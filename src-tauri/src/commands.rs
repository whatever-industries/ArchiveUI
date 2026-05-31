use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager};

// ── Shared helpers ─────────────────────────────────────────────────────────────

pub fn emit_log(app: &AppHandle, msg: impl Into<String>) {
    let _ = app.emit("log", msg.into());
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
            let _ = Command::new("kill").arg(pid.to_string()).status();
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

/// Confirm the `ia` CLI is installed, returning a friendly error otherwise.
fn ensure_ia() -> Result<(), String> {
    Command::new("ia")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|_| {
            "The 'ia' CLI is not installed.\nInstall it with:  pip install internetarchive"
                .to_string()
        })?;
    Ok(())
}

/// Sign in once per batch: write the archive.org S3 keys via `ia configure`.
#[tauri::command]
pub async fn configure_account(username: String, password: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        ensure_ia()?;
        let cfg = Command::new("ia")
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
        let out = Command::new("curl")
            .args(["-sS", "--max-time", "15", &url])
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
        ensure_ia()?;
        let id = identifier.trim();
        if id.is_empty() {
            return Err("No identifier provided.".to_string());
        }
        let out = Command::new("ia")
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

    let mut child = Command::new("ia")
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
