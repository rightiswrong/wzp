//! Android implementation: forwards to the Kotlin plugin
//! `com.wzp.plugin.WzpAndroidPlugin` (see `android/`).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tauri::{
    AppHandle, Emitter, Runtime,
    ipc::{Channel, InvokeResponseBody},
    plugin::{PluginApi, PluginHandle},
};

use crate::{Error, PickedFile, Result, SHARED_EVENT};

const PLUGIN_IDENTIFIER: &str = "com.wzp.plugin";
const PLUGIN_CLASS: &str = "WzpAndroidPlugin";

pub fn init<R: Runtime, C: DeserializeOwned>(
    app: &AppHandle<R>,
    api: PluginApi<R, C>,
) -> Result<WzpAndroid<R>> {
    let handle = api.register_android_plugin(PLUGIN_IDENTIFIER, PLUGIN_CLASS)?;
    register_shared_listener(app.clone(), handle.clone());
    Ok(WzpAndroid(handle))
}

/// Subscribes to the Kotlin "shared" event and re-emits it as the Tauri app
/// event [`SHARED_EVENT`]. Done from a task because `setup` runs on the main
/// thread, where waiting for a Kotlin reply would deadlock.
fn register_shared_listener<R: Runtime>(app: AppHandle<R>, handle: PluginHandle<R>) {
    let channel: Channel = Channel::new(move |body| {
        if let InvokeResponseBody::Json(json) = body {
            if let Ok(r) = serde_json::from_str::<FilesResponse>(&json) {
                let _ = app.emit(SHARED_EVENT, r.files);
            }
        }
        Ok(())
    });
    tauri::async_runtime::spawn(async move {
        let _ = handle
            .run_mobile_plugin_async::<serde_json::Value>(
                "registerListener",
                RegisterListenerArgs {
                    event: "shared",
                    handler: channel,
                },
            )
            .await;
    });
}

/// Blocking calls wait for the Kotlin side, which runs on the UI thread. On
/// Android the UI thread is the process' initial thread (tid == pid).
fn ensure_not_main_thread() -> Result<()> {
    // SAFETY: plain syscalls without arguments.
    if unsafe { libc::gettid() == libc::getpid() } {
        Err(Error::MainThread)
    } else {
        Ok(())
    }
}

/// Handle to the Android bridge. Get it with
/// [`WzpAndroidExt::wzp_android`](crate::WzpAndroidExt::wzp_android).
pub struct WzpAndroid<R: Runtime>(PluginHandle<R>);

impl<R: Runtime> WzpAndroid<R> {
    /// Opens the system document picker (ACTION_OPEN_DOCUMENT, multiple) and
    /// copies every chosen file into the app cache. Returns an empty Vec if the
    /// user cancels. Fails with a clear message if the cache volume lacks room.
    pub async fn pick_files(&self) -> Result<Vec<PickedFile>> {
        let r: FilesResponse = self.0.run_mobile_plugin_async("pickFiles", ()).await?;
        Ok(r.files)
    }

    /// Drains the queue of files received through the share sheet
    /// (ACTION_SEND / ACTION_SEND_MULTIPLE) since the previous call.
    pub fn take_shared_files(&self) -> Result<Vec<PickedFile>> {
        ensure_not_main_thread()?;
        let r: FilesResponse = self.0.run_mobile_plugin("takeSharedFiles", ())?;
        Ok(r.files)
    }

    /// Copies `path` to the public Downloads/WarzonePhone/ folder as `name`
    /// (renamed "name (1).ext" etc. on collision). Returns a human-readable
    /// location such as `Download/WarzonePhone/name.ext`. The source file is
    /// left in place; the caller deletes it afterwards.
    ///
    /// API 29+: MediaStore.Downloads (no permission). API 26-28: public
    /// Downloads dir after a WRITE_EXTERNAL_STORAGE prompt; if the user denies
    /// it, falls back to the app-specific external Downloads dir
    /// (`Android/data/<pkg>/files/Download/WarzonePhone/`).
    pub async fn save_to_downloads(
        &self,
        path: &Path,
        name: &str,
        mime: Option<&str>,
    ) -> Result<String> {
        std::fs::metadata(path)?;
        let r: SaveResponse = self
            .0
            .run_mobile_plugin_async(
                "saveToDownloads",
                SaveArgs {
                    path: path.to_string_lossy().into_owned(),
                    name: name.to_owned(),
                    mime: mime.map(str::to_owned),
                },
            )
            .await?;
        Ok(r.location)
    }

    /// Starts (or, if already running, updates) the `dataSync` foreground
    /// service with an ongoing notification. Must be called while the app is
    /// in the foreground (Android 12+ background-start restrictions).
    pub fn start_transfer_service(&self, title: &str, text: &str) -> Result<()> {
        ensure_not_main_thread()?;
        self.0.run_mobile_plugin::<serde_json::Value>(
            "startTransferService",
            StartArgs { title, text },
        )?;
        Ok(())
    }

    /// Updates the notification text and progress (`(done, total)` bytes;
    /// `None` hides the bar, `total == 0` shows an indeterminate bar). No-op
    /// if the service is not running. Rate-limited on the Kotlin side, so it is
    /// fine to call on every progress tick.
    pub fn update_transfer_service(&self, text: &str, progress: Option<(u64, u64)>) -> Result<()> {
        ensure_not_main_thread()?;
        self.0.run_mobile_plugin::<serde_json::Value>(
            "updateTransferService",
            UpdateArgs {
                text,
                current: progress.map(|p| p.0),
                total: progress.map(|p| p.1),
            },
        )?;
        Ok(())
    }

    /// Stops the foreground service and removes its notification. Idempotent.
    pub fn stop_transfer_service(&self) -> Result<()> {
        ensure_not_main_thread()?;
        self.0
            .run_mobile_plugin::<serde_json::Value>("stopTransferService", ())?;
        Ok(())
    }

    /// Asks for POST_NOTIFICATIONS (API 33+). Returns `true` if granted
    /// (always `true` below API 33).
    pub async fn request_notification_permission(&self) -> Result<bool> {
        let r: PermissionResponse = self
            .0
            .run_mobile_plugin_async("requestNotificationPermission", ())
            .await?;
        Ok(r.granted)
    }

    /// Internal, private storage dir for identity/contacts/partials:
    /// `Context.getFilesDir()/wzp` (created if missing).
    pub fn app_data_dir(&self) -> Result<PathBuf> {
        ensure_not_main_thread()?;
        let r: PathResponse = self.0.run_mobile_plugin("appDataDir", ())?;
        let dir = PathBuf::from(r.path);
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }
}

#[derive(Serialize)]
struct RegisterListenerArgs {
    event: &'static str,
    handler: Channel,
}

#[derive(Deserialize)]
struct FilesResponse {
    #[serde(default)]
    files: Vec<PickedFile>,
}

#[derive(Serialize)]
struct SaveArgs {
    path: String,
    name: String,
    mime: Option<String>,
}

#[derive(Deserialize)]
struct SaveResponse {
    location: String,
}

#[derive(Serialize)]
struct StartArgs<'a> {
    title: &'a str,
    text: &'a str,
}

#[derive(Serialize)]
struct UpdateArgs<'a> {
    text: &'a str,
    current: Option<u64>,
    total: Option<u64>,
}

#[derive(Deserialize)]
struct PermissionResponse {
    granted: bool,
}

#[derive(Deserialize)]
struct PathResponse {
    path: String,
}
