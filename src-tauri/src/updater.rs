//! In-app updates for Archive UI itself, via tauri-plugin-updater. Releases
//! publish a signed `latest.json`; the app checks it on launch and offers an
//! Update button when a newer version is out.

use crate::commands::{emit_log, emit_progress, human_size};
use serde::Serialize;
use std::time::{Duration, Instant};
use tauri::AppHandle;
use tauri_plugin_updater::UpdaterExt;

#[derive(Serialize)]
pub struct UpdateInfo {
    version: String,
    current: String,
}

/// Returns the newer version on offer, or `None` when already up to date.
#[tauri::command]
pub async fn check_for_update(app: AppHandle) -> Result<Option<UpdateInfo>, String> {
    let update = app
        .updater()
        .map_err(|e| e.to_string())?
        .check()
        .await
        .map_err(|e| e.to_string())?;
    Ok(update.map(|u| UpdateInfo {
        version: u.version.clone(),
        current: u.current_version.clone(),
    }))
}

/// Download + install the latest version, then relaunch into it.
#[tauri::command]
pub async fn install_update(app: AppHandle) -> Result<(), String> {
    let update = app
        .updater()
        .map_err(|e| e.to_string())?
        .check()
        .await
        .map_err(|e| e.to_string())?
        .ok_or("Archive UI is already up to date.")?;

    emit_log(&app, format!("Downloading Archive UI v{}…", update.version));
    let mut got: u64 = 0;
    let mut last = Instant::now() - Duration::from_secs(1);
    let app_dl = app.clone();
    let app_done = app.clone();
    update
        .download_and_install(
            move |chunk, total| {
                got += chunk as u64;
                if last.elapsed() >= Duration::from_millis(150) {
                    last = Instant::now();
                    let of = total.map(|t| format!(" / {}", human_size(t))).unwrap_or_default();
                    emit_progress(&app_dl, format!("  {}{of}", human_size(got)));
                }
            },
            move || emit_log(&app_done, "Download complete — installing…"),
        )
        .await
        .map_err(|e| format!("Update failed: {e}"))?;

    emit_log(&app, "Update installed — restarting…");
    app.restart();
}
