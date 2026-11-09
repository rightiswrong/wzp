//! Non-Android targets: every call returns [`Error::Unsupported`].

use std::{
    marker::PhantomData,
    path::{Path, PathBuf},
};

use serde::de::DeserializeOwned;
use tauri::{AppHandle, Runtime, plugin::PluginApi};

use crate::{Error, PickedFile, Result};

pub fn init<R: Runtime, C: DeserializeOwned>(
    _app: &AppHandle<R>,
    _api: PluginApi<R, C>,
) -> Result<WzpAndroid<R>> {
    Ok(WzpAndroid(PhantomData))
}

/// Handle to the Android bridge (inert on this platform).
pub struct WzpAndroid<R: Runtime>(PhantomData<fn() -> R>);

impl<R: Runtime> WzpAndroid<R> {
    pub async fn pick_files(&self) -> Result<Vec<PickedFile>> {
        Err(Error::Unsupported)
    }

    pub fn take_shared_files(&self) -> Result<Vec<PickedFile>> {
        Err(Error::Unsupported)
    }

    pub async fn save_to_downloads(
        &self,
        _path: &Path,
        _name: &str,
        _mime: Option<&str>,
    ) -> Result<String> {
        Err(Error::Unsupported)
    }

    pub fn start_transfer_service(&self, _title: &str, _text: &str) -> Result<()> {
        Err(Error::Unsupported)
    }

    pub fn update_transfer_service(
        &self,
        _text: &str,
        _progress: Option<(u64, u64)>,
    ) -> Result<()> {
        Err(Error::Unsupported)
    }

    pub fn stop_transfer_service(&self) -> Result<()> {
        Err(Error::Unsupported)
    }

    pub async fn request_notification_permission(&self) -> Result<bool> {
        Err(Error::Unsupported)
    }

    pub fn app_data_dir(&self) -> Result<PathBuf> {
        Err(Error::Unsupported)
    }
}
