mod commands;

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(commands::UploadState::default())
        .invoke_handler(tauri::generate_handler![
            commands::collect_sources,
            commands::configure_account,
            commands::check_identifier,
            commands::inspect_item,
            commands::upload_to_archive,
            commands::cancel_upload,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
