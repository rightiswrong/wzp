//! Platform glue for file sharing: everything Android-specific goes through
//! this module so the bridge to `tauri-plugin-wzp-android` is wired in one
//! place (see `notes/android-bridge.md` / the plugin's `WzpAndroidExt`).
//!
//! On desktop targets nothing here touches the plugin.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use serde::Serialize;

/// A file chosen by the user (picker or "Share to WarzonePhone").
#[derive(Debug, Clone, Serialize)]
pub struct PickedFile {
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
    pub mime: Option<String>,
}

impl PickedFile {
    pub fn from_path(path: PathBuf) -> Self {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        Self {
            path,
            name,
            size,
            mime: None,
        }
    }
}

pub const IS_ANDROID: bool = cfg!(target_os = "android");

/// Short platform name for the front end.
pub fn name() -> &'static str {
    if cfg!(target_os = "android") {
        "android"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "linux"
    }
}

// ─── Android ─────────────────────────────────────────────────────────
//
// Thin wrappers over `tauri-plugin-wzp-android` (registered in lib.rs).
// The sync methods must not run on the Android main thread (they return
// `Error::MainThread`): call them only from async commands / tokio tasks.

#[cfg(target_os = "android")]
mod imp {
    use super::*;
    use tauri_plugin_wzp_android::WzpAndroidExt;

    fn conv(f: tauri_plugin_wzp_android::PickedFile) -> PickedFile {
        PickedFile {
            path: f.path,
            name: f.name,
            size: f.size,
            mime: f.mime,
        }
    }

    pub async fn pick_files(app: &tauri::AppHandle) -> Result<Vec<PickedFile>, String> {
        app.wzp_android()
            .pick_files()
            .await
            .map(|v| v.into_iter().map(conv).collect())
            .map_err(|e| e.to_string())
    }

    pub fn take_shared_files(app: &tauri::AppHandle) -> Result<Vec<PickedFile>, String> {
        app.wzp_android()
            .take_shared_files()
            .map(|v| v.into_iter().map(conv).collect())
            .map_err(|e| e.to_string())
    }

    pub async fn save_to_downloads(
        app: &tauri::AppHandle,
        path: &Path,
        name: &str,
        mime: Option<&str>,
    ) -> Result<String, String> {
        app.wzp_android()
            .save_to_downloads(path, name, mime)
            .await
            .map_err(|e| e.to_string())
    }

    pub fn start_transfer_service(
        app: &tauri::AppHandle,
        title: &str,
        text: &str,
    ) -> Result<(), String> {
        app.wzp_android()
            .start_transfer_service(title, text)
            .map_err(|e| e.to_string())
    }

    pub fn update_transfer_service(
        app: &tauri::AppHandle,
        text: &str,
        progress: Option<(u64, u64)>,
    ) -> Result<(), String> {
        app.wzp_android()
            .update_transfer_service(text, progress)
            .map_err(|e| e.to_string())
    }

    pub fn stop_transfer_service(app: &tauri::AppHandle) -> Result<(), String> {
        app.wzp_android()
            .stop_transfer_service()
            .map_err(|e| e.to_string())
    }

    pub async fn request_notification_permission(app: &tauri::AppHandle) -> Result<bool, String> {
        app.wzp_android()
            .request_notification_permission()
            .await
            .map_err(|e| e.to_string())
    }
}

// ─── Desktop ─────────────────────────────────────────────────────────

#[cfg(not(target_os = "android"))]
mod imp {
    use super::*;

    /// Native multi-file picker (tauri-plugin-dialog).
    pub async fn pick_files(app: &tauri::AppHandle) -> Result<Vec<PickedFile>, String> {
        use tauri_plugin_dialog::DialogExt;
        let (tx, rx) = tokio::sync::oneshot::channel();
        app.dialog()
            .file()
            .set_title("Choose files to send")
            .pick_files(move |files| {
                let _ = tx.send(files);
            });
        let files = rx.await.map_err(|_| "file dialog closed".to_string())?;
        Ok(files
            .unwrap_or_default()
            .into_iter()
            .filter_map(|f| f.into_path().ok())
            .map(PickedFile::from_path)
            .collect())
    }

    pub fn take_shared_files(_app: &tauri::AppHandle) -> Result<Vec<PickedFile>, String> {
        Ok(Vec::new())
    }

    pub async fn save_to_downloads(
        _app: &tauri::AppHandle,
        path: &Path,
        _name: &str,
        _mime: Option<&str>,
    ) -> Result<String, String> {
        // Desktop writes straight into the download folder.
        Ok(path.display().to_string())
    }

    pub fn start_transfer_service(
        _app: &tauri::AppHandle,
        _title: &str,
        _text: &str,
    ) -> Result<(), String> {
        Ok(())
    }

    pub fn update_transfer_service(
        _app: &tauri::AppHandle,
        _text: &str,
        _progress: Option<(u64, u64)>,
    ) -> Result<(), String> {
        Ok(())
    }

    pub fn stop_transfer_service(_app: &tauri::AppHandle) -> Result<(), String> {
        Ok(())
    }

    pub async fn request_notification_permission(_app: &tauri::AppHandle) -> Result<bool, String> {
        Ok(true)
    }
}

pub use imp::*;

/// Desktop folder picker (download folder setting). `None` = cancelled.
#[cfg(not(target_os = "android"))]
pub async fn pick_folder(app: &tauri::AppHandle) -> Result<Option<PathBuf>, String> {
    use tauri_plugin_dialog::DialogExt;
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_title("Download folder")
        .pick_folder(move |f| {
            let _ = tx.send(f);
        });
    let f = rx.await.map_err(|_| "folder dialog closed".to_string())?;
    Ok(f.and_then(|f| f.into_path().ok()))
}

#[cfg(target_os = "android")]
pub async fn pick_folder(_app: &tauri::AppHandle) -> Result<Option<PathBuf>, String> {
    Err("received files are saved to Downloads/WarzonePhone on Android".into())
}
