use serde::{Serialize, Serializer};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The plugin only does something on Android.
    #[error("wzp-android: not supported on this platform")]
    Unsupported,
    /// A blocking method was called on the Android main thread, where it would
    /// deadlock (the Kotlin side runs on that thread).
    #[error(
        "wzp-android: blocking call on the Android main thread would deadlock; \
         call it from an async command or a background thread"
    )]
    MainThread,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Error reported by the Kotlin side (message is user-readable, e.g.
    /// "Not enough free space ...").
    #[cfg(mobile)]
    #[error(transparent)]
    PluginInvoke(#[from] tauri::plugin::mobile::PluginInvokeError),
}

impl Serialize for Error {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}
