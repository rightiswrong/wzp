// Commands exposed to the webview (JS). The Rust API (`WzpAndroidExt`) is the
// primary interface; only side-effect-free / user-mediated calls are exposed
// to JS. save_to_downloads, app_data_dir and the transfer service are
// deliberately Rust-only (a compromised webview must not be able to copy
// internal files such as identity keys into public storage).
const COMMANDS: &[&str] = &["pick_files", "take_shared_files", "request_notification_permission"];

fn main() {
    tauri_plugin::Builder::new(COMMANDS)
        .android_path("android")
        .build();
}
