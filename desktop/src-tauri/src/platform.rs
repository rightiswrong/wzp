//! Platform glue for file sharing: everything Android-specific goes through
//! this module so the bridge to `tauri-plugin-wzp-android` is wired in one
//! place (see `notes/android-bridge.md` / the plugin's `WzpAndroidExt`).
//!
//! Until the plugin lands on main, the Android functions are stubs that
//! return errors / do nothing, so the Android build keeps compiling. On
//! desktop targets nothing here touches the plugin.

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
// Stubs until `tauri-plugin-wzp-android` exists on main. Wiring it up:
//   1. add the plugin crate under `[target.'cfg(target_os = "android")'.dependencies]`;
//   2. register it in `register_plugins` below;
//   3. replace each stub body with `app.wzp_android().<same name>(..)`.

#[cfg(target_os = "android")]
mod imp {
    use super::*;

    const NOT_YET: &str = "Android bridge plugin not available in this build";

    pub fn register_plugins<R: tauri::Runtime>(b: tauri::Builder<R>) -> tauri::Builder<R> {
        b
    }

    pub async fn pick_files(_app: &tauri::AppHandle) -> Result<Vec<PickedFile>, String> {
        Err(NOT_YET.into())
    }

    pub fn take_shared_files(_app: &tauri::AppHandle) -> Result<Vec<PickedFile>, String> {
        Ok(Vec::new())
    }

    pub async fn save_to_downloads(
        _app: &tauri::AppHandle,
        _path: &Path,
        _name: &str,
        _mime: Option<&str>,
    ) -> Result<String, String> {
        Err(NOT_YET.into())
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
        Ok(false)
    }

    pub fn app_data_dir(_app: &tauri::AppHandle) -> Result<PathBuf, String> {
        Err(NOT_YET.into())
    }
}

// ─── Desktop ─────────────────────────────────────────────────────────

#[cfg(not(target_os = "android"))]
mod imp {
    use super::*;

    pub fn register_plugins<R: tauri::Runtime>(b: tauri::Builder<R>) -> tauri::Builder<R> {
        b
    }

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

    pub fn app_data_dir(_app: &tauri::AppHandle) -> Result<PathBuf, String> {
        Err("not used on desktop".into())
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
