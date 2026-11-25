mod archive;
mod commands;
mod updater;

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(commands::UploadState::default())
        .manage(commands::AuthState::default())
        .invoke_handler(tauri::generate_handler![
            commands::collect_sources,
            commands::sign_in,
            commands::sign_out,
            commands::check_identifier,
            commands::inspect_item,
            commands::upload_to_archive,
            commands::cancel_upload,
            updater::check_for_update,
            updater::install_update,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
