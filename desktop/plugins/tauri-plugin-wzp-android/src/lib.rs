//! WarzonePhone Android bridge (Tauri 2 mobile plugin).
//!
//! Rust API: [`WzpAndroidExt::wzp_android`] on any [`Manager`] (`AppHandle`,
//! `App`, `Window`, ...) returns a [`WzpAndroid`] handle. On every target other
//! than Android all methods return [`Error::Unsupported`].
//!
//! Threading: the synchronous methods block until the Kotlin side answers, and
//! Kotlin commands run on the Android main (UI) thread. Calling a synchronous
//! method *from* the main thread would deadlock, so it returns
//! [`Error::MainThread`] instead. Call them from async Tauri commands, tokio
//! tasks or other background threads (plain non-async `#[tauri::command]`s and
//! the `setup` closure run on the main thread).
//!
//! Events: when files are shared to the app ("Share to WarzonePhone"), the
//! plugin copies them into the app cache, queues them and emits
//! [`SHARED_EVENT`] (`"wzp-android://shared"`) with the new `Vec<PickedFile>`
//! as payload. The files stay queued until [`WzpAndroid::take_shared_files`]
//! drains the queue, so a share that arrived before anyone listened (cold
//! start) is not lost. Recommended use: call `take_shared_files()` once at
//! startup and again whenever the event fires, and use its result (not the
//! event payload) as the source of truth.
//!
//! File lifetime: every [`PickedFile`] lives alone in its own unique directory
//! under the app cache (`cache/picked/<id>/<name>` or `cache/shared/<id>/<name>`).
//! Delete `path.parent()` when done with it.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tauri::{
    Manager, Runtime,
    plugin::{Builder, TauriPlugin},
};

mod commands;
mod error;

#[cfg(not(target_os = "android"))]
mod desktop;
#[cfg(target_os = "android")]
mod mobile;

#[cfg(not(target_os = "android"))]
pub use desktop::WzpAndroid;
pub use error::{Error, Result};
#[cfg(target_os = "android")]
pub use mobile::WzpAndroid;

/// Tauri event emitted when files are shared to the app while it is running.
/// Payload: `Vec<PickedFile>` (the newly arrived files).
pub const SHARED_EVENT: &str = "wzp-android://shared";

/// A file that was copied from a `content://` URI into the app cache.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PickedFile {
    /// Absolute path of the cached copy (readable with `std::fs`).
    pub path: PathBuf,
    /// Display name as reported by the content provider (sanitised).
    pub name: String,
    /// Size in bytes of the cached copy.
    pub size: u64,
    /// MIME type reported by the content provider, if any.
    #[serde(default)]
    pub mime: Option<String>,
}

/// Extension trait giving access to the plugin's API.
pub trait WzpAndroidExt<R: Runtime> {
    fn wzp_android(&self) -> &WzpAndroid<R>;
}

impl<R: Runtime, T: Manager<R>> WzpAndroidExt<R> for T {
    fn wzp_android(&self) -> &WzpAndroid<R> {
        self.state::<WzpAndroid<R>>().inner()
    }
}

/// Initializes the plugin. Register with `.plugin(tauri_plugin_wzp_android::init())`.
pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("wzp-android")
        .invoke_handler(tauri::generate_handler![
            commands::pick_files,
            commands::take_shared_files,
            commands::request_notification_permission,
        ])
        .setup(|app, api| {
            #[cfg(target_os = "android")]
            let handle = mobile::init(app, api)?;
            #[cfg(not(target_os = "android"))]
            let handle = desktop::init(app, api)?;
            app.manage(handle);
            Ok(())
        })
        .build()
}
