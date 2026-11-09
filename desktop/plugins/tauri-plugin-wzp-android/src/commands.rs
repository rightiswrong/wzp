//! Webview-facing commands (`plugin:wzp-android|...`). Thin wrappers over the
//! Rust API; async so they never run on the main thread.

use tauri::{AppHandle, Runtime, command};

use crate::{PickedFile, Result, WzpAndroidExt};

#[command]
pub(crate) async fn pick_files<R: Runtime>(app: AppHandle<R>) -> Result<Vec<PickedFile>> {
    app.wzp_android().pick_files().await
}

#[command]
pub(crate) async fn take_shared_files<R: Runtime>(app: AppHandle<R>) -> Result<Vec<PickedFile>> {
    app.wzp_android().take_shared_files()
}

#[command]
pub(crate) async fn request_notification_permission<R: Runtime>(app: AppHandle<R>) -> Result<bool> {
    app.wzp_android().request_notification_permission().await
}
