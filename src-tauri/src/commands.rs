use crate::archive::{self, PutError, S3Keys};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};

// ── Shared helpers ─────────────────────────────────────────────────────────────

pub fn emit_log(app: &AppHandle, msg: impl Into<String>) {
    let _ = app.emit("log", msg.into());
}

/// Live, in-place progress line in the UI log (replaced on each update).
pub fn emit_progress(app: &AppHandle, msg: impl Into<String>) {
    let _ = app.emit("upload-progress", msg.into());
}

pub fn human_size(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let units = ["KB", "MB", "GB", "TB"];
    let mut n = bytes as f64 / 1024.0;
    let mut i = 0;
    while n >= 1024.0 && i < units.len() - 1 {
        n /= 1024.0;
        i += 1;
    }
    if n >= 10.0 {
        format!("{n:.0} {}", units[i])
    } else {
        format!("{n:.1} {}", units[i])
    }
}

// ── Session state ──────────────────────────────────────────────────────────────

/// The signed-in account's upload keys, held in memory for this session only.
#[derive(Default)]
pub struct AuthState {
    keys: Mutex<Option<S3Keys>>,
}

/// Tracks the in-flight upload so it can be cancelled mid-transfer.
#[derive(Default)]
pub struct UploadState {
    active: AtomicBool,
    cancel: AtomicBool,
}

/// Clears `active` when an upload ends, however it ends.
struct ActiveGuard<'a>(&'a AtomicBool);
impl Drop for ActiveGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Abort the in-flight upload, if one is running.
#[tauri::command]
pub fn cancel_upload(app: AppHandle) -> Result<(), String> {
    let state = app.state::<UploadState>();
    if state.active.load(Ordering::SeqCst) {
        state.cancel.store(true, Ordering::SeqCst);
        Ok(())
    } else {
        Err("No upload is currently in progress.".to_string())
    }
}

// ── Sources ────────────────────────────────────────────────────────────────────

/// A single file slated for upload: its local path, the remote name it will have
/// on archive.org (preserving folder structure), a display name, and size.
#[derive(Serialize)]
pub struct FileEntry {
    path: String,
    name: String,
    remote: String,
    size: u64,
}

/// Recursively gather files under `dir` (skipping hidden entries like .DS_Store),
/// pairing each local path with its remote name = the path relative to `base`, so
/// the folder's structure is preserved on archive.org.
fn walk_dir(dir: &Path, base: &Path, out: &mut Vec<(PathBuf, String)>) {
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
            walk_dir(&path, base, out);
        } else if path.is_file() {
            let remote = path
                .strip_prefix(base)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            out.push((path, remote));
        }
    }
}

/// Resolve the source selection into a de-duplicated list of files to upload.
/// Folders keep their structure (remote name = path relative to the folder's
/// parent, so the folder name is retained); standalone files use their basename.
#[tauri::command]
pub fn collect_sources(files: Vec<String>, folder: Option<String>) -> Result<Vec<FileEntry>, String> {
    // (local path, remote name)
    let mut items: Vec<(PathBuf, String)> = Vec::new();

    if let Some(folder) = folder.as_ref().filter(|f| !f.trim().is_empty()) {
        let dir = Path::new(folder);
        if !dir.is_dir() {
            return Err(format!("Not a folder: {folder}"));
        }
        walk_dir(dir, dir.parent().unwrap_or(dir), &mut items);
    }

    for f in files {
        let p = PathBuf::from(&f);
        if p.is_dir() {
            // Keep the folder name plus its internal structure.
            let base = p.parent().map(|b| b.to_path_buf()).unwrap_or_else(|| p.clone());
            walk_dir(&p, &base, &mut items);
        } else if p.is_file() {
            let remote = p
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            items.push((p, remote));
        }
    }

    // De-duplicate by local path, then sort by remote name for a tidy display.
    items.sort_by(|a, b| a.0.cmp(&b.0));
    items.dedup_by(|a, b| a.0 == b.0);
    items.sort_by(|a, b| a.1.cmp(&b.1));

    let entries = items
        .into_iter()
        .map(|(p, remote)| {
            let size = fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            FileEntry {
                path: p.to_string_lossy().to_string(),
                name: remote.clone(),
                remote,
                size,
            }
        })
        .collect();

    Ok(entries)
}

// ── Account ────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct SignInResult {
    screenname: String,
}

/// Sign in to archive.org and keep the account's upload keys for this session.
#[tauri::command]
pub async fn sign_in(app: AppHandle, email: String, password: String) -> Result<SignInResult, String> {
    let login = tauri::async_runtime::spawn_blocking(move || archive::login(email.trim(), &password))
        .await
        .map_err(|e| e.to_string())??;
    *app.state::<AuthState>().keys.lock().unwrap() = Some(login.keys);
    Ok(SignInResult { screenname: login.screenname })
}

/// Forget the session's upload keys.
#[tauri::command]
pub fn sign_out(app: AppHandle) {
    *app.state::<AuthState>().keys.lock().unwrap() = None;
}

// ── Identifier / item lookups ──────────────────────────────────────────────────

/// Whether an identifier is free to use on archive.org.
#[derive(Serialize)]
pub struct IdentifierStatus {
    available: bool,
    message: String,
}

/// Probe archive.org's identifier-availability service (the one the web
/// uploader uses — it correctly reports reserved-but-unpublished identifiers as
/// taken, which a plain metadata lookup does not).
#[tauri::command]
pub async fn check_identifier(identifier: String) -> Result<IdentifierStatus, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let id = identifier.trim();
        if id.is_empty() {
            return Err("Enter an identifier first.".to_string());
        }
        let (available, message) = archive::check_identifier(id)?;
        let message = message.map(|m| format!("'{id}': {m}")).unwrap_or_else(|| {
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
/// matches before adding files to it (`metadata.uploader` is the creator's email).
#[tauri::command]
pub async fn inspect_item(identifier: String) -> Result<ItemInfo, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let id = identifier.trim();
        if id.is_empty() {
            return Err("No identifier provided.".to_string());
        }
        let json = archive::item_metadata(id)?;
        let meta = &json["metadata"];
        // A populated `metadata` object means the item already exists.
        let exists = meta.as_object().is_some_and(|o| !o.is_empty());
        let field = |k: &str| meta[k].as_str().unwrap_or("").to_string();
        Ok(ItemInfo {
            exists,
            uploader: field("uploader"),
            title: field("title"),
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

// ── Upload ─────────────────────────────────────────────────────────────────────

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

/// A file to upload: its local path plus the remote name it should have on
/// archive.org (which preserves any folder structure).
#[derive(Deserialize)]
pub struct UploadFile {
    path: String,
    remote: String,
}

#[tauri::command]
pub async fn upload_to_archive(
    app: AppHandle,
    meta: UploadMeta,
    files: Vec<UploadFile>,
) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || upload_blocking(&app, &meta, &files))
        .await
        .map_err(|e| e.to_string())?
}

/// Retries per file on transient failures (archive.org answers `503 SlowDown`
/// when it's busy) — matches the old `ia upload --retries=10`.
const MAX_RETRIES: u32 = 10;

/// Sleep for `secs`, waking early (returning false) if the upload is cancelled.
fn wait_or_cancel(secs: u64, cancel: &AtomicBool) -> bool {
    let until = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < until {
        if cancel.load(Ordering::SeqCst) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    !cancel.load(Ordering::SeqCst)
}

fn upload_blocking(app: &AppHandle, meta: &UploadMeta, files: &[UploadFile]) -> Result<(), String> {
    if files.is_empty() {
        return Err("No files selected to upload.".to_string());
    }
    let keys = app
        .state::<AuthState>()
        .keys
        .lock()
        .unwrap()
        .clone()
        .ok_or("Sign in to your archive.org account before uploading.")?;

    let state = app.state::<UploadState>();
    state.cancel.store(false, Ordering::SeqCst);
    state.active.store(true, Ordering::SeqCst);
    let _active = ActiveGuard(&state.active);
    let cancel = &state.cancel;
    let cancelled = || {
        emit_log(app, format!("Upload of '{}' cancelled.", meta.identifier));
        Err("cancelled".to_string())
    };

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

    let id = meta.identifier.as_str();
    emit_log(app, format!("Uploading to archive.org as '{id}'…"));
    emit_log(app, format!("  identifier : {id}"));
    emit_log(app, format!("  mediatype  : {}", meta.mediatype));
    emit_log(app, format!("  files      : {}", files.len()));

    // ── Skip files already in the item (same name, size and MD5) ────────────────
    let existing = archive::existing_files(id).unwrap_or_else(|e| {
        emit_log(app, format!("  (could not list existing files: {e} — uploading everything)"));
        Default::default()
    });
    let mut plan: Vec<(&UploadFile, u64)> = Vec::new();
    for f in files {
        let size = fs::metadata(&f.path)
            .map_err(|e| format!("Cannot read {}: {e}", f.path))?
            .len();
        if let Some((remote_size, remote_md5)) = existing.get(&f.remote) {
            if *remote_size == size {
                emit_progress(app, format!("Verifying {}…", f.remote));
                match archive::md5_file(Path::new(&f.path), cancel) {
                    Ok(md5) if &md5 == remote_md5 => {
                        emit_log(app, format!("  {} is already on archive.org — skipped", f.remote));
                        continue;
                    }
                    Ok(_) => {}
                    Err(_) if cancel.load(Ordering::SeqCst) => return cancelled(),
                    Err(e) => return Err(e),
                }
            }
        }
        plan.push((f, size));
    }
    if plan.is_empty() {
        emit_log(app, "All files are already on archive.org — nothing to upload.");
        emit_log(app, format!("Upload complete!  https://archive.org/details/{id}"));
        return Ok(());
    }

    // ── Metadata (sent with the first file, which creates the item) ─────────────
    let mut fields: Vec<(&str, &str)> = vec![
        ("mediatype", &meta.mediatype),
        ("title", &meta.title),
        ("description", &meta.description),
        ("creator", &meta.creator),
        ("collection", &meta.collection),
        ("date", &meta.date),
        ("licenseurl", &meta.license_url),
        ("language", &meta.language),
    ];
    for s in &meta.subjects {
        fields.push(("subject", s)); // repeated → stored as an array
    }
    let scanner = format!("Archive UI {}", env!("CARGO_PKG_VERSION"));
    fields.push(("scanner", &scanner));
    let meta_headers = archive::metadata_headers(&fields);
    let total_bytes: u64 = plan.iter().map(|(_, s)| s).sum();

    // Upload one file at a time. Derive (archive.org's post-processing) is
    // queued only with the final file so the item is processed just once.
    let n = plan.len();
    for (i, (file, size)) in plan.iter().enumerate() {
        if cancel.load(Ordering::SeqCst) {
            return cancelled();
        }
        let label = file.remote.as_str();
        emit_log(app, format!("[{}/{}] Uploading {} ({})…", i + 1, n, label, human_size(*size)));

        let mut headers: Vec<(String, String)> = vec![
            ("x-archive-auto-make-bucket".into(), "1".into()),
            ("x-archive-queue-derive".into(), if i + 1 < n { "0" } else { "1" }.into()),
            (
                "x-archive-size-hint".into(),
                if i == 0 { total_bytes } else { *size }.to_string(),
            ),
        ];
        if i == 0 {
            headers.extend(meta_headers.iter().cloned());
        }

        let mut attempt = 0;
        loop {
            let started = Instant::now();
            let mut last = Instant::now() - Duration::from_secs(1);
            let mut on_progress = |sent: u64| {
                if last.elapsed() < Duration::from_millis(150) && sent < *size {
                    return;
                }
                last = Instant::now();
                let pct = if *size == 0 { 100 } else { sent * 100 / size };
                let secs = started.elapsed().as_secs_f64().max(0.001);
                let rate = human_size((sent as f64 / secs) as u64);
                emit_progress(
                    app,
                    format!("  {label}  {pct:>3}%  ·  {} / {}  ·  {rate}/s", human_size(sent), human_size(*size)),
                );
            };

            match archive::put_file(&keys, id, Path::new(&file.path), label, *size, &headers, cancel, &mut on_progress) {
                Ok(()) => break,
                Err(PutError::Cancelled) => return cancelled(),
                Err(PutError::Fatal(msg)) => {
                    emit_log(app, format!("  error: {msg}"));
                    return Err(format!("Upload failed on '{label}': {msg}"));
                }
                Err(PutError::Retry(msg)) => {
                    attempt += 1;
                    if attempt > MAX_RETRIES {
                        emit_log(app, format!("  error: {msg}"));
                        return Err(format!("Upload failed on '{label}' after {MAX_RETRIES} retries: {msg}"));
                    }
                    let wait = (5u64 << (attempt - 1).min(3)).min(30); // 5, 10, 20, 30, 30…
                    emit_log(app, format!("  {msg} — retrying in {wait}s ({attempt}/{MAX_RETRIES})"));
                    if !wait_or_cancel(wait, cancel) {
                        return cancelled();
                    }
                }
            }
        }
    }

    emit_log(app, format!("Upload complete!  https://archive.org/details/{id}"));
    Ok(())
}
