use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use tauri::{AppHandle, Emitter, Manager};

// ── Shared helpers ─────────────────────────────────────────────────────────────

pub fn emit_log(app: &AppHandle, msg: impl Into<String>) {
    let _ = app.emit("log", msg.into());
}

/// Stop Windows from flashing a console window open for each spawned child
/// process (`ia`, `curl`, …). No-op on other platforms.
fn hide_window(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// Tracks the in-flight `ia upload` child so it can be cancelled mid-upload.
#[derive(Default)]
pub struct UploadState {
    /// PID of the running upload, if any.
    pid: Mutex<Option<u32>>,
    /// Set when the user cancels, so the upload result is reported as cancelled
    /// rather than a generic failure.
    cancelled: Mutex<bool>,
}

/// Kill the in-flight upload, if one is running.
#[tauri::command]
pub fn cancel_upload(app: AppHandle) -> Result<(), String> {
    let state = app.state::<UploadState>();
    let pid = *state.pid.lock().unwrap();
    match pid {
        Some(pid) => {
            *state.cancelled.lock().unwrap() = true;
            // SIGTERM stops the `ia` python process cleanly mid-transfer.
            let _ = hide_window(Command::new("kill").arg(pid.to_string())).status();
            Ok(())
        }
        None => Err("No upload is currently in progress.".to_string()),
    }
}

/// A single file slated for upload, with a display name and size.
#[derive(Serialize)]
pub struct FileEntry {
    path: String,
    name: String,
    size: u64,
}

/// Recursively gather files under `dir`, skipping hidden entries (e.g. .DS_Store).
fn walk_dir(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let hidden = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.starts_with('.'))
            .unwrap_or(false);
        if hidden {
            continue;
        }
        if path.is_dir() {
            walk_dir(&path, out);
        } else if path.is_file() {
            out.push(path);
        }
    }
}

/// Resolve the source selection (a folder and/or an explicit file list) into a
/// flat, de-duplicated list of files to upload, with display metadata.
#[tauri::command]
pub fn collect_sources(files: Vec<String>, folder: Option<String>) -> Result<Vec<FileEntry>, String> {
    let mut paths: Vec<PathBuf> = Vec::new();

    if let Some(folder) = folder.as_ref().filter(|f| !f.trim().is_empty()) {
        let dir = Path::new(folder);
        if !dir.is_dir() {
            return Err(format!("Not a folder: {folder}"));
        }
        walk_dir(dir, &mut paths);
    }

    for f in files {
        let p = PathBuf::from(&f);
        if p.is_file() {
            paths.push(p);
        }
    }

    paths.sort();
    paths.dedup();

    let entries = paths
        .into_iter()
        .map(|p| {
            let size = fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            let name = p
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            FileEntry {
                path: p.to_string_lossy().to_string(),
                name,
                size,
            }
        })
        .collect();

    Ok(entries)
}

// ── Upload ──────────────────────────────────────────────────────────────────────

/// Metadata for an archive.org item, mirrored from the UI form.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadMeta {
    identifier: String,
    title: String,
    description: String,
    mediatype: String,
    #[serde(default)]
    subjects: Vec<String>,
    #[serde(default)]
    creator: String,
    #[serde(default)]
    collection: String,
    #[serde(default)]
    date: String,
    #[serde(default)]
    license_url: String,
    #[serde(default)]
    language: String,
}

/// Locate the `ia` executable. GUI apps launched from Finder/Dock on macOS do
/// not inherit the user's shell `PATH`, so a bare `Command::new("ia")` fails to
/// find `ia` when it lives somewhere like `~/.local/bin` or a Homebrew prefix.
/// Resolve the absolute path once: prefer whatever a login shell reports (it
/// sources the user's profile and full PATH), then fall back to common install
/// locations. Returns `None` only if `ia` genuinely can't be found.
fn ia_bin() -> Option<&'static str> {
    static CACHE: OnceLock<Option<String>> = OnceLock::new();
    CACHE.get_or_init(resolve_ia).as_deref()
}

fn resolve_ia() -> Option<String> {
    // 1. Already on PATH (e.g. `cargo tauri dev` launched from a terminal).
    let on_path = hide_window(
        Command::new("ia")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
    )
    .status()
    .map(|s| s.success())
    .unwrap_or(false);
    if on_path {
        return Some("ia".to_string());
    }

    // 2. Ask the user's login shell, which loads their profile and real PATH.
    if let Ok(shell) = std::env::var("SHELL") {
        if let Ok(out) = hide_window(Command::new(&shell).args(["-lic", "command -v ia"])).output() {
            // Profile scripts may print noise; take the last line that is a
            // real, existing path.
            let resolved = String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .rfind(|l| Path::new(l).is_file())
                .map(str::to_string);
            if resolved.is_some() {
                return resolved;
            }
        }
    }

    // 3. Probe common install locations directly.
    let home = std::env::var("HOME").unwrap_or_default();
    let mut candidates = vec![
        format!("{home}/.local/bin/ia"),
        "/opt/homebrew/bin/ia".to_string(),
        "/usr/local/bin/ia".to_string(),
        "/usr/bin/ia".to_string(),
    ];
    // pip `--user` installs land under ~/Library/Python/<ver>/bin on macOS.
    if let Ok(entries) = fs::read_dir(format!("{home}/Library/Python")) {
        for e in entries.flatten() {
            candidates.push(e.path().join("bin").join("ia").to_string_lossy().to_string());
        }
    }
    candidates.into_iter().find(|p| Path::new(p).is_file())
}

/// A `Command` for the resolved `ia` binary, or a friendly error if it's missing.
fn ia_command() -> Result<Command, String> {
    let bin = ia_bin().ok_or(
        "The 'ia' CLI is not installed.\nInstall it with:  pip install internetarchive",
    )?;
    let mut cmd = Command::new(bin);
    hide_window(&mut cmd);
    Ok(cmd)
}

/// Confirm the `ia` CLI can be found, returning a friendly error otherwise.
fn ensure_ia() -> Result<(), String> {
    ia_command().map(|_| ())
}

/// Sign in once per batch: write the archive.org S3 keys via `ia configure`.
#[tauri::command]
pub async fn configure_account(username: String, password: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let cfg = ia_command()?
            .args([
                "configure",
                &format!("--username={}", username),
                &format!("--password={}", password),
            ])
            .output()
            .map_err(|e| format!("ia configure failed: {e}"))?;
        if cfg.status.success() {
            Ok(())
        } else {
            Err(format!(
                "Sign-in failed: {}",
                String::from_utf8_lossy(&cfg.stderr).trim()
            ))
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Whether an identifier is free to use on archive.org.
#[derive(Serialize)]
pub struct IdentifierStatus {
    available: bool,
    message: String,
}

/// Probe archive.org for an identifier using the same availability service the
/// web uploader uses. `ia metadata` is unreliable here — it returns `{}` for
/// reserved-but-unpublished identifiers (e.g. `test`), which the dedicated
/// endpoint correctly reports as taken:
///
///   check_identifier.php?identifier=<id>  →  {"code":"available"|"not_available", ...}
#[tauri::command]
pub async fn check_identifier(identifier: String) -> Result<IdentifierStatus, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let id = identifier.trim();
        if id.is_empty() {
            return Err("Enter an identifier first.".to_string());
        }
        let url = format!(
            "https://archive.org/services/check_identifier.php?identifier={id}&output=json"
        );
        let out = hide_window(Command::new("curl").args(["-sS", "--max-time", "15", &url]))
            .output()
            .map_err(|e| format!("Could not reach archive.org: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "Lookup failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }

        let body = String::from_utf8_lossy(&out.stdout);
        let json: serde_json::Value = serde_json::from_str(&body)
            .map_err(|_| format!("Unexpected response from archive.org: {}", body.trim()))?;
        let code = json.get("code").and_then(|c| c.as_str()).unwrap_or("");
        let available = code == "available";

        // Prefer the service's own message; fall back to a sensible default.
        let message = json
            .get("message")
            .and_then(|m| m.as_str())
            .map(|m| format!("'{id}': {m}"))
            .unwrap_or_else(|| {
                if available {
                    format!("'{id}' is available.")
                } else {
                    format!("'{id}' is not available — choose another.")
                }
            });

        Ok(IdentifierStatus { available, message })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Ownership details for a pre-existing archive.org item.
#[derive(Serialize)]
pub struct ItemInfo {
    exists: bool,
    /// Email of the account that created the item ("" if unknown / not exposed).
    uploader: String,
    title: String,
}

/// Look up an existing item's owner so the UI can confirm the signed-in account
/// matches before adding files to it. Uses `ia metadata`, whose `metadata.uploader`
/// field carries the creating account's email.
#[tauri::command]
pub async fn inspect_item(identifier: String) -> Result<ItemInfo, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let id = identifier.trim();
        if id.is_empty() {
            return Err("No identifier provided.".to_string());
        }
        let out = ia_command()?
            .args(["metadata", id])
            .output()
            .map_err(|e| format!("ia metadata failed: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "Lookup failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let body = String::from_utf8_lossy(&out.stdout);
        let json: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
        let meta = json.get("metadata");
        // A populated `metadata` object means the item already exists.
        let exists = meta.map(|m| m.as_object().is_some_and(|o| !o.is_empty())).unwrap_or(false);
        let field = |k: &str| {
            meta.and_then(|m| m.get(k))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        };
        Ok(ItemInfo {
            exists,
            uploader: field("uploader"),
            title: field("title"),
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn upload_to_archive(
    app: AppHandle,
    meta: UploadMeta,
    files: Vec<String>,
) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || upload_blocking(&app, &meta, &files))
        .await
        .map_err(|e| e.to_string())?
}

/// Read a child stream, treating `\n` as a committed line and `\r` as an
/// in-place progress update (the convention CLI progress bars like `ia`'s use).
fn stream_cr_lf<R: Read>(
    reader: R,
    mut on_line: impl FnMut(&str),
    mut on_progress: impl FnMut(&str),
) {
    let mut reader = BufReader::new(reader);
    let mut buf: Vec<u8> = Vec::new();
    let mut byte = [0u8; 1];
    let flush = |buf: &[u8]| String::from_utf8_lossy(buf).trim_end().to_string();

    while let Ok(n) = reader.read(&mut byte) {
        if n == 0 {
            break;
        }
        match byte[0] {
            b'\n' => {
                let s = flush(&buf);
                if !s.is_empty() {
                    on_line(&s);
                }
                buf.clear();
            }
            b'\r' => {
                let s = flush(&buf);
                if !s.is_empty() {
                    on_progress(&s);
                }
                buf.clear();
            }
            b => buf.push(b),
        }
    }
    let s = flush(&buf);
    if !s.is_empty() {
        on_line(&s);
    }
}

fn upload_blocking(app: &AppHandle, meta: &UploadMeta, files: &[String]) -> Result<(), String> {
    if files.is_empty() {
        return Err("No files selected to upload.".to_string());
    }

    ensure_ia()?;

    // Hold the system awake for the duration of the upload so a long transfer
    // isn't cut short by the machine going to sleep. Best-effort — released
    // automatically when this guard drops at the end of the upload.
    let _keep_awake = keepawake::Builder::default()
        .idle(true)
        .sleep(true)
        .reason("Uploading to archive.org")
        .app_name("Archive UI")
        .app_reverse_domain("com.whatev-indus.archive-ui")
        .create()
        .ok();

    // ── Build the metadata argument list ────────────────────────────────────────
    let mut md: Vec<String> = Vec::new();
    let mut push = |key: &str, val: &str| {
        if !val.trim().is_empty() {
            md.push(format!("--metadata={key}:{}", val.trim()));
        }
    };
    push("mediatype", &meta.mediatype);
    push("title", &meta.title);
    push("description", &meta.description);
    push("creator", &meta.creator);
    push("collection", &meta.collection);
    push("date", &meta.date);
    push("licenseurl", &meta.license_url);
    push("language", &meta.language);
    // Each subject becomes its own --metadata so `ia` stores them as an array.
    for s in &meta.subjects {
        if !s.trim().is_empty() {
            md.push(format!("--metadata=subject:{}", s.trim()));
        }
    }

    emit_log(app, format!("Uploading to archive.org as '{}'…", meta.identifier));
    emit_log(app, format!("  identifier : {}", meta.identifier));
    emit_log(app, format!("  mediatype  : {}", meta.mediatype));
    emit_log(app, format!("  files      : {}", files.len()));

    let mut args: Vec<String> = vec!["upload".into(), meta.identifier.clone()];
    args.extend(files.iter().cloned());
    args.extend(md);
    args.push("--checksum".into());
    args.push("--retries=10".into());

    let mut child = ia_command()?
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to spawn ia upload: {e}"))?;

    // Publish this upload's PID so `cancel_upload` can stop it; clear the
    // cancelled flag for a fresh run.
    let state = app.state::<UploadState>();
    *state.pid.lock().unwrap() = Some(child.id());
    *state.cancelled.lock().unwrap() = false;

    let stdout = child.stdout.take().expect("stdout");
    let stderr = child.stderr.take().expect("stderr");

    let app_a = app.clone();
    let t_out = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            emit_log(&app_a, line);
        }
    });
    // `ia` writes status + a `\r`-updated progress bar to stderr. Surface
    // committed lines normally, and stream the progress bar as live, in-place
    // updates (throttled) so it doesn't look stalled then dump all at once.
    let app_b = app.clone();
    let t_err = std::thread::spawn(move || {
        let mut last = std::time::Instant::now() - std::time::Duration::from_secs(1);
        stream_cr_lf(
            stderr,
            |line| {
                let _ = app_b.emit("log", line.to_string());
            },
            |prog| {
                if last.elapsed() >= std::time::Duration::from_millis(120) {
                    let _ = app_b.emit("upload-progress", prog.to_string());
                    last = std::time::Instant::now();
                }
            },
        );
    });

    let status = child.wait().map_err(|e| format!("ia wait failed: {e}"))?;
    t_out.join().ok();
    t_err.join().ok();

    // This upload is no longer cancellable; read whether it was cancelled.
    *state.pid.lock().unwrap() = None;
    let was_cancelled = *state.cancelled.lock().unwrap();

    if was_cancelled {
        emit_log(app, format!("Upload of '{}' cancelled.", meta.identifier));
        Err("cancelled".to_string())
    } else if status.success() {
        emit_log(
            app,
            format!(
                "Upload complete!  https://archive.org/details/{}",
                meta.identifier
            ),
        );
        Ok(())
    } else {
        Err("Upload failed — see log for details".to_string())
    }
}
