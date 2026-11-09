//! File sharing for the Tauri app: one [`wzp_share::ShareNode`] per app,
//! persisted settings, thin Tauri commands, and event forwarding.
//!
//! * Identity: the node uses the same 32-byte seed as calls
//!   (`<app data>/.wzp/identity`), so calls and files share one identity.
//! * Settings: `<app data>/.wzp/share-settings.json`, written with mode 0600
//!   because it holds the WZP relay token. The token is never sent back to
//!   the front end: [`SettingsView`] only says whether one is set.
//! * Events: every [`ShareEvent`] is emitted to the front end as
//!   `share://event` (same JSON as wzp-share). Extra app events:
//!   `share://status` ([`NodeStatus`]) and `share://saved` (Android, after a
//!   received file was copied to Downloads).
//!
//! See docs/SHARE.md for the wzp-share flow.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use tauri::{Emitter, Manager};
use tokio::sync::RwLock;
use wzp_share::{
    Direction, EndpointId, RelayChoice, ShareConfig, ShareEvent, ShareId, ShareNode, TransferInfo,
    TransferState,
};

use crate::platform;

pub const EVENT: &str = "share://event";
pub const STATUS_EVENT: &str = "share://status";
pub const SAVED_EVENT: &str = "share://saved";
const SETTINGS_FILE: &str = "share-settings.json";

// ─── Settings ────────────────────────────────────────────────────────

/// iroh relay servers used for contacts / presence / signaling.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum IrohRelay {
    /// number 0's public relays.
    #[default]
    N0,
    /// A self-hosted `iroh-relay`.
    Custom { url: String },
    /// No iroh relays (LAN / direct only).
    Off,
}

impl IrohRelay {
    fn to_choice(&self) -> RelayChoice {
        match self {
            IrohRelay::N0 => RelayChoice::N0,
            IrohRelay::Custom { url } => RelayChoice::Custom(vec![url.clone()]),
            IrohRelay::Off => RelayChoice::Disabled,
        }
    }
}

/// Persisted file-sharing settings. `Debug` redacts the relay token.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Name in our contact string. Empty = the seed-derived alias.
    pub display_name: String,
    /// WZP relay `host:port` used as fallback for files. Empty = none.
    pub relay_addr: String,
    /// **Secret** WZP relay access token.
    pub relay_token: Option<String>,
    pub iroh_relay: IrohRelay,
    /// Desktop download folder. `None` = OS Downloads folder.
    pub download_dir: Option<PathBuf>,
    /// Accept offers from *verified* contacts without asking.
    pub auto_accept_verified: bool,
}

impl std::fmt::Debug for Settings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Settings")
            .field("display_name", &self.display_name)
            .field("relay_addr", &self.relay_addr)
            .field(
                "relay_token",
                &self.relay_token.as_ref().map(|_| "<redacted>"),
            )
            .field("iroh_relay", &self.iroh_relay)
            .field("download_dir", &self.download_dir)
            .field("auto_accept_verified", &self.auto_accept_verified)
            .finish()
    }
}

/// What the front end sees. Never contains the relay token.
#[derive(Debug, Clone, Serialize)]
pub struct SettingsView {
    pub display_name: String,
    /// Name actually used (alias if `display_name` is empty).
    pub effective_display_name: String,
    pub relay_addr: String,
    pub relay_token_set: bool,
    pub iroh_relay: IrohRelay,
    /// Folder received files go to (Android: internal staging folder).
    pub download_dir: String,
    pub download_dir_is_default: bool,
    pub auto_accept_verified: bool,
    pub platform: &'static str,
}

/// Partial update from the front end. `None` keeps the current value.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SettingsUpdate {
    pub display_name: Option<String>,
    pub relay_addr: Option<String>,
    /// Replace the token (empty string is ignored; use `clear_relay_token`).
    pub relay_token: Option<String>,
    pub clear_relay_token: bool,
    pub iroh_relay: Option<IrohRelay>,
    /// Empty string = back to the default folder.
    pub download_dir: Option<String>,
    pub auto_accept_verified: Option<bool>,
}

fn validate_relay_addr(s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Ok(());
    }
    let (host, port) = s
        .rsplit_once(':')
        .ok_or_else(|| format!("relay address {s:?} must be host:port"))?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.is_empty() || host.contains(char::is_whitespace) {
        return Err(format!("relay address {s:?} has no valid host"));
    }
    match port.parse::<u16>() {
        Ok(p) if p != 0 => Ok(()),
        _ => Err(format!("relay address {s:?} has no valid port")),
    }
}

fn validate_iroh_relay(r: &IrohRelay) -> Result<(), String> {
    if let IrohRelay::Custom { url } = r {
        let ok = (url.starts_with("https://") || url.starts_with("http://"))
            && url.len() > "https://".len()
            && !url.contains(char::is_whitespace);
        if !ok {
            return Err(format!("iroh relay URL {url:?} must start with https://"));
        }
    }
    Ok(())
}

impl Settings {
    pub fn load(path: &Path) -> Settings {
        match std::fs::read_to_string(path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
                tracing::warn!("ignoring unreadable {}: {e}", path.display());
                Settings::default()
            }),
            Err(_) => Settings::default(),
        }
    }

    /// Atomic write (temp + rename), mode 0600 on Unix.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let json = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        }
        let tmp = path.with_extension("json.tmp");
        write_private(&tmp, &json).map_err(|e| format!("write {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("rename {}: {e}", path.display()))?;
        Ok(())
    }

    /// Apply an update. Returns whether the node must restart.
    pub fn apply(&mut self, u: SettingsUpdate) -> Result<bool, String> {
        let mut next = self.clone();
        if let Some(n) = u.display_name {
            let n = n.trim().to_string();
            if n.len() > 64 {
                return Err("display name is too long (max 64 bytes)".into());
            }
            next.display_name = n;
        }
        if let Some(a) = u.relay_addr {
            let a = a.trim().to_string();
            validate_relay_addr(&a)?;
            next.relay_addr = a;
        }
        if u.clear_relay_token {
            next.relay_token = None;
        }
        if let Some(t) = u.relay_token {
            let t = t.trim().to_string();
            if !t.is_empty() {
                next.relay_token = Some(t);
            }
        }
        if let Some(r) = u.iroh_relay {
            let r = match r {
                IrohRelay::Custom { url } => IrohRelay::Custom {
                    url: url.trim().to_string(),
                },
                o => o,
            };
            validate_iroh_relay(&r)?;
            next.iroh_relay = r;
        }
        if let Some(d) = u.download_dir {
            let d = d.trim();
            next.download_dir = if d.is_empty() {
                None
            } else {
                Some(PathBuf::from(d))
            };
        }
        if let Some(a) = u.auto_accept_verified {
            next.auto_accept_verified = a;
        }
        let restart = next.relay_addr != self.relay_addr
            || next.relay_token != self.relay_token
            || next.iroh_relay != self.iroh_relay
            || next.download_dir != self.download_dir;
        *self = next;
        Ok(restart)
    }
}

/// Write a file readable only by the owner (0600 on Unix).
pub fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    let mut f = o.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    f.write_all(data)?;
    f.sync_all()
}

// ─── Views ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct NodeStatus {
    pub running: bool,
    pub starting: bool,
    pub error: Option<String>,
    /// Non-fatal problem, e.g. the WZP relay address didn't resolve.
    pub warning: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct IdentityView {
    pub id: String,
    pub fingerprint: String,
    pub contact: String,
    pub display_name: String,
    pub platform: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContactView {
    pub id: String,
    pub name: String,
    pub fingerprint: String,
    pub verified: bool,
    pub presence: wzp_share::PresenceState,
    pub last_seen: Option<u64>,
    pub added_at: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TransferView {
    #[serde(flatten)]
    pub info: TransferInfo,
    /// Android: where the received file was saved (MediaStore Downloads).
    pub saved_to: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ContactRequestView {
    pub id: String,
    pub fingerprint: String,
    pub name: Option<String>,
    pub contact: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SendResult {
    pub path: String,
    pub id: Option<String>,
    pub error: Option<String>,
}

// ─── State ───────────────────────────────────────────────────────────

pub struct ShareState {
    app: tauri::AppHandle,
    seed: [u8; 32],
    alias: String,
    settings_path: PathBuf,
    share_dir: PathBuf,
    settings: Mutex<Settings>,
    node: RwLock<Option<Arc<ShareNode>>>,
    pump: Mutex<Option<tokio::task::JoinHandle<()>>>,
    restart: tokio::sync::Mutex<()>,
    status: Mutex<NodeStatus>,
    requests: Mutex<Vec<ContactRequestView>>,
    saved: Mutex<HashMap<ShareId, String>>,
    service_running: AtomicBool,
    service_updated: Mutex<Instant>,
}

impl Drop for ShareState {
    fn drop(&mut self) {
        self.seed.fill(0);
    }
}

fn fmt_size(n: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

fn parse_contact_id(s: &str) -> Result<EndpointId, String> {
    wzp_iroh::identity::parse_endpoint_id(s).ok_or_else(|| format!("invalid contact id {s:?}"))
}

fn parse_share_id(s: &str) -> Result<ShareId, String> {
    s.parse::<ShareId>().map_err(|e| e.to_string())
}

impl ShareState {
    /// `base_dir` holds the identity and settings; share data (contacts,
    /// partial downloads on Android) goes to `share_dir`.
    pub fn new(
        app: tauri::AppHandle,
        seed: [u8; 32],
        alias: String,
        base_dir: &Path,
        share_dir: PathBuf,
    ) -> Arc<Self> {
        let settings_path = base_dir.join(SETTINGS_FILE);
        let settings = Settings::load(&settings_path);
        tracing::info!(?settings, "file-sharing settings loaded");
        Arc::new(Self {
            app,
            seed,
            alias,
            settings_path,
            share_dir,
            settings: Mutex::new(settings),
            node: RwLock::new(None),
            pump: Mutex::new(None),
            restart: tokio::sync::Mutex::new(()),
            status: Mutex::new(NodeStatus {
                running: false,
                starting: true,
                error: None,
                warning: None,
            }),
            requests: Mutex::new(Vec::new()),
            saved: Mutex::new(HashMap::new()),
            service_running: AtomicBool::new(false),
            service_updated: Mutex::new(Instant::now() - Duration::from_secs(10)),
        })
    }

    fn settings(&self) -> Settings {
        self.settings.lock().unwrap().clone()
    }

    fn display_name(&self) -> String {
        let n = self.settings.lock().unwrap().display_name.clone();
        if n.is_empty() { self.alias.clone() } else { n }
    }

    fn download_dir(&self, s: &Settings) -> PathBuf {
        if platform::IS_ANDROID {
            // Staged internally, then copied to MediaStore Downloads.
            return self.share_dir.join("incoming");
        }
        if let Some(d) = &s.download_dir {
            return d.clone();
        }
        self.app
            .path()
            .download_dir()
            .unwrap_or_else(|_| self.share_dir.join("downloads"))
    }

    fn view(&self) -> SettingsView {
        let s = self.settings();
        SettingsView {
            effective_display_name: self.display_name(),
            display_name: s.display_name.clone(),
            relay_addr: s.relay_addr.clone(),
            relay_token_set: s.relay_token.is_some(),
            iroh_relay: s.iroh_relay.clone(),
            download_dir: self.download_dir(&s).display().to_string(),
            download_dir_is_default: s.download_dir.is_none(),
            auto_accept_verified: s.auto_accept_verified,
            platform: platform::name(),
        }
    }

    fn set_status(&self, st: NodeStatus) {
        *self.status.lock().unwrap() = st.clone();
        let _ = self.app.emit(STATUS_EVENT, st);
    }

    async fn node(&self) -> Result<Arc<ShareNode>, String> {
        if let Some(n) = self.node.read().await.as_ref() {
            return Ok(n.clone());
        }
        let st = self.status.lock().unwrap().clone();
        Err(match st.error {
            Some(e) => format!("file sharing is not running: {e}"),
            None => "file sharing is starting, try again in a moment".into(),
        })
    }

    async fn build_config(&self, s: &Settings) -> (ShareConfig, Option<String>) {
        let mut cfg = ShareConfig::new(&self.share_dir);
        cfg.iroh.relay = s.iroh_relay.to_choice();
        cfg.files.download_dir = self.download_dir(s);
        cfg.relay_token = s.relay_token.clone();
        cfg.auto_accept = false; // handled here (verified contacts only)
        let mut warning = None;
        if !s.relay_addr.is_empty() {
            let lookup = tokio::time::timeout(
                Duration::from_secs(5),
                tokio::net::lookup_host(s.relay_addr.clone()),
            )
            .await;
            match lookup {
                Ok(Ok(addrs)) => {
                    let addrs: Vec<_> = addrs.collect();
                    cfg.relay = addrs
                        .iter()
                        .find(|a| a.is_ipv4())
                        .or(addrs.first())
                        .copied();
                    if cfg.relay.is_none() {
                        warning = Some(format!("relay {} did not resolve", s.relay_addr));
                    }
                }
                Ok(Err(e)) => warning = Some(format!("relay {}: {e}", s.relay_addr)),
                Err(_) => warning = Some(format!("relay {}: DNS timeout", s.relay_addr)),
            }
            if cfg.relay.is_some() && s.relay_token.is_none() {
                warning =
                    Some("WZP relay set but no relay token: files go direct only".to_string());
            }
        }
        (cfg, warning)
    }

    /// (Re)start the node with the current settings.
    pub async fn restart(self: &Arc<Self>) {
        let _guard = self.restart.lock().await;
        self.set_status(NodeStatus {
            running: false,
            starting: true,
            error: None,
            warning: None,
        });
        self.stop_node().await;
        let s = self.settings();
        let (cfg, warning) = self.build_config(&s).await;
        tracing::info!(?cfg, "starting file-sharing node");
        match ShareNode::start(&self.seed, cfg).await {
            Ok(node) => {
                let rx = node.events();
                *self.node.write().await = Some(Arc::new(node));
                let me = self.clone();
                *self.pump.lock().unwrap() = Some(tokio::spawn(async move { me.pump(rx).await }));
                self.set_status(NodeStatus {
                    running: true,
                    starting: false,
                    error: None,
                    warning,
                });
            }
            Err(e) => {
                tracing::warn!("file-sharing node failed to start: {e}");
                self.set_status(NodeStatus {
                    running: false,
                    starting: false,
                    error: Some(e.to_string()),
                    warning,
                });
            }
        }
    }

    async fn stop_node(&self) {
        if let Some(p) = self.pump.lock().unwrap().take() {
            p.abort();
        }
        let Some(mut old) = self.node.write().await.take() else {
            return;
        };
        // Commands hold short-lived clones; wait for them to finish.
        for _ in 0..30 {
            match Arc::try_unwrap(old) {
                Ok(n) => {
                    n.shutdown().await;
                    return;
                }
                Err(a) => {
                    old = a;
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
        for t in old.transfers() {
            if !t.state.is_finished() {
                let _ = old.cancel(t.id).await;
            }
        }
        old.iroh().close().await;
    }

    fn notify(&self, title: &str, body: &str) {
        use tauri_plugin_notification::NotificationExt;
        if let Err(e) = self
            .app
            .notification()
            .builder()
            .title(title)
            .body(body)
            .show()
        {
            tracing::debug!("notification failed: {e}");
        }
    }

    async fn pump(self: Arc<Self>, mut rx: tokio::sync::broadcast::Receiver<ShareEvent>) {
        use tokio::sync::broadcast::error::RecvError;
        loop {
            let ev = match rx.recv().await {
                Ok(ev) => ev,
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!("file-sharing UI dropped {n} events");
                    continue;
                }
                Err(RecvError::Closed) => return,
            };
            if let Err(e) = self.app.emit(EVENT, &ev) {
                tracing::warn!("emit share event: {e}");
            }
            self.handle(&ev).await;
        }
    }

    async fn handle(&self, ev: &ShareEvent) {
        let Ok(node) = self.node().await else { return };
        match ev {
            ShareEvent::ContactRequest {
                id,
                fingerprint,
                name,
                contact,
            } => {
                let req = ContactRequestView {
                    id: id.to_string(),
                    fingerprint: fingerprint.clone(),
                    name: name.clone(),
                    contact: contact.clone(),
                };
                let mut v = self.requests.lock().unwrap();
                let known = v.iter().any(|r| r.id == req.id);
                // A later request carrying a contact string (Hello) upgrades
                // an earlier bare one.
                v.retain(|r| !(r.id == req.id && r.contact.is_none() && req.contact.is_some()));
                if !v.iter().any(|r| r.id == req.id) {
                    let who = req.name.clone().unwrap_or_else(|| req.fingerprint.clone());
                    v.push(req);
                    drop(v);
                    if !known {
                        self.notify(
                            "Contact request",
                            &format!("{who} wants to share files with you"),
                        );
                    }
                }
            }
            ShareEvent::TransferOffered { transfer: t }
                if t.direction == Direction::Incoming
                    && t.state == TransferState::AwaitingDecision =>
            {
                let verified = node
                    .contacts()
                    .iter()
                    .any(|c| c.id == t.contact && c.trust == wzp_iroh::Trust::Verified);
                if verified && self.settings.lock().unwrap().auto_accept_verified {
                    tracing::info!(id = %t.id, "auto-accepting offer from verified contact");
                    if let Err(e) = node.accept(t.id).await {
                        tracing::warn!("auto-accept {}: {e}", t.id);
                    }
                } else {
                    self.notify(
                        "Incoming file",
                        &format!(
                            "{} wants to send {} ({})",
                            t.contact_name,
                            t.name,
                            fmt_size(t.size)
                        ),
                    );
                }
            }
            ShareEvent::TransferCompleted { transfer: t } => {
                if t.direction == Direction::Incoming {
                    self.notify(
                        "File received",
                        &format!("{} from {}", t.name, t.contact_name),
                    );
                    if platform::IS_ANDROID
                        && let Some(p) = &t.path
                    {
                        self.save_android(t.id, p, &t.name).await;
                    }
                } else {
                    self.notify("File sent", &format!("{} to {}", t.name, t.contact_name));
                }
            }
            ShareEvent::TransferFailed { transfer: t } => {
                self.notify(
                    "Transfer failed",
                    &format!("{}: {}", t.name, t.error.clone().unwrap_or_default()),
                );
            }
            _ => {}
        }
        if matches!(
            ev,
            ShareEvent::TransferOffered { .. }
                | ShareEvent::TransferStarted { .. }
                | ShareEvent::TransferProgress { .. }
                | ShareEvent::TransferCompleted { .. }
                | ShareEvent::TransferRejected { .. }
                | ShareEvent::TransferFailed { .. }
                | ShareEvent::TransferCancelled { .. }
        ) {
            let force = !matches!(ev, ShareEvent::TransferProgress { .. });
            self.update_service(&node, force);
        }
    }

    /// Android: copy a received file into Downloads/WarzonePhone and remove
    /// the internal copy.
    async fn save_android(&self, id: ShareId, path: &Path, name: &str) {
        match platform::save_to_downloads(&self.app, path, name, None).await {
            Ok(loc) => {
                let _ = std::fs::remove_file(path);
                self.saved.lock().unwrap().insert(id, loc.clone());
                let _ = self.app.emit(
                    SAVED_EVENT,
                    serde_json::json!({ "id": id.to_string(), "location": loc }),
                );
            }
            Err(e) => {
                tracing::warn!("save_to_downloads {}: {e}", path.display());
                let _ = self.app.emit(
                    SAVED_EVENT,
                    serde_json::json!({ "id": id.to_string(), "location": null, "error": e }),
                );
            }
        }
    }

    /// Keep the Android foreground service running while transfers are
    /// active (no-op on desktop).
    fn update_service(&self, node: &ShareNode, force: bool) {
        if !platform::IS_ANDROID {
            return;
        }
        {
            let mut last = self.service_updated.lock().unwrap();
            if !force && last.elapsed() < Duration::from_secs(1) {
                return;
            }
            *last = Instant::now();
        }
        let active: Vec<TransferInfo> = node
            .transfers()
            .into_iter()
            .filter(|t| !t.state.is_finished())
            .collect();
        let running = self.service_running.load(Ordering::SeqCst);
        if active.is_empty() {
            if running {
                if let Err(e) = platform::stop_transfer_service(&self.app) {
                    tracing::warn!("stop transfer service: {e}");
                }
                self.service_running.store(false, Ordering::SeqCst);
            }
            return;
        }
        let text = if active.len() == 1 {
            format!("{} — {}", active[0].name, active[0].contact_name)
        } else {
            format!("{} transfers active", active.len())
        };
        let done: u64 = active.iter().map(|t| t.done).sum();
        let total: u64 = active.iter().map(|t| t.size).sum();
        if !running {
            match platform::start_transfer_service(&self.app, "WarzonePhone file transfer", &text) {
                Ok(()) => self.service_running.store(true, Ordering::SeqCst),
                Err(e) => tracing::warn!("start transfer service: {e}"),
            }
        } else if let Err(e) =
            platform::update_transfer_service(&self.app, &text, Some((done, total)))
        {
            tracing::debug!("update transfer service: {e}");
        }
    }

    fn transfer_view(&self, info: TransferInfo) -> TransferView {
        let saved_to = self.saved.lock().unwrap().get(&info.id).cloned();
        TransferView { info, saved_to }
    }

    async fn transfer(&self, id: &str) -> Result<TransferInfo, String> {
        let id = parse_share_id(id)?;
        self.node()
            .await?
            .transfers()
            .into_iter()
            .find(|t| t.id == id)
            .ok_or_else(|| format!("unknown transfer {id}"))
    }

    /// Existing local path of a finished transfer (received file or sent source).
    async fn transfer_path(&self, id: &str) -> Result<PathBuf, String> {
        let t = self.transfer(id).await?;
        let p = t.path.ok_or("this transfer has no local file")?;
        if !p.exists() {
            return Err(match self.saved.lock().unwrap().get(&t.id) {
                Some(loc) => format!("saved to {loc}"),
                None => format!("{} no longer exists", p.display()),
            });
        }
        Ok(p)
    }
}

/// Start the node at app launch (called from `setup`).
pub fn start(state: Arc<ShareState>) {
    tauri::async_runtime::spawn(async move {
        if let Ok(false) = platform::request_notification_permission(&state.app).await {
            tracing::debug!("notification permission not granted");
        }
        state.restart().await;
    });
}

// ─── Commands ────────────────────────────────────────────────────────

type St<'a> = tauri::State<'a, Arc<ShareState>>;

#[tauri::command]
pub async fn share_status(state: St<'_>) -> Result<NodeStatus, String> {
    Ok(state.status.lock().unwrap().clone())
}

/// Our file-sharing identity (same key as calls).
#[tauri::command]
pub async fn get_share_identity(state: St<'_>) -> Result<IdentityView, String> {
    let node = state.node().await?;
    let name = state.display_name();
    Ok(IdentityView {
        id: node.id().to_string(),
        fingerprint: node.fingerprint(),
        contact: node.contact_string(&name),
        display_name: name,
        platform: platform::name(),
    })
}

#[tauri::command]
pub async fn add_contact(state: St<'_>, contact: String) -> Result<ContactView, String> {
    let node = state.node().await?;
    let c = node
        .add_contact(contact.trim())
        .await
        .map_err(|e| e.to_string())?;
    let id = c.id;
    state
        .requests
        .lock()
        .unwrap()
        .retain(|r| r.id != id.to_string());
    // Introduce ourselves so the peer gets a contact request with our
    // contact string (or, if they already have us, just an ack).
    let name = state.display_name();
    let n2 = node.clone();
    tokio::spawn(async move {
        if let Err(e) = n2.iroh().send_hello(id, &name).await {
            tracing::debug!(%id, "hello: {e}");
        }
    });
    Ok(contact_view(&node, &c))
}

fn contact_view(node: &ShareNode, c: &wzp_share::Contact) -> ContactView {
    ContactView {
        id: c.id.to_string(),
        name: c.name.clone(),
        fingerprint: c.fingerprint.clone(),
        verified: c.trust == wzp_iroh::Trust::Verified,
        presence: node.iroh().presence(c.id),
        last_seen: c.last_seen,
        added_at: c.added_at,
    }
}

#[tauri::command]
pub async fn remove_contact(state: St<'_>, id: String) -> Result<(), String> {
    let id = parse_contact_id(&id)?;
    state
        .node()
        .await?
        .remove_contact(id)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn verify_contact(state: St<'_>, id: String) -> Result<(), String> {
    let id = parse_contact_id(&id)?;
    state
        .node()
        .await?
        .iroh()
        .verify_contact(id)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn list_contacts(state: St<'_>) -> Result<Vec<ContactView>, String> {
    let node = state.node().await?;
    let mut v: Vec<_> = node
        .contacts()
        .iter()
        .map(|c| contact_view(&node, c))
        .collect();
    v.sort_by_key(|c| c.name.to_lowercase());
    Ok(v)
}

#[tauri::command]
pub async fn list_contact_requests(state: St<'_>) -> Result<Vec<ContactRequestView>, String> {
    Ok(state.requests.lock().unwrap().clone())
}

#[tauri::command]
pub async fn dismiss_contact_request(state: St<'_>, id: String) -> Result<(), String> {
    state.requests.lock().unwrap().retain(|r| r.id != id);
    Ok(())
}

#[tauri::command]
pub async fn send_files(
    state: St<'_>,
    contact: String,
    paths: Vec<String>,
) -> Result<Vec<SendResult>, String> {
    let node = state.node().await?;
    let id = parse_contact_id(&contact)?;
    let mut out = Vec::with_capacity(paths.len());
    for p in paths {
        let r = node.send_file(id, &p).await;
        out.push(match r {
            Ok(h) => SendResult {
                path: p,
                id: Some(h.id().to_string()),
                error: None,
            },
            Err(e) => SendResult {
                path: p,
                id: None,
                error: Some(e.to_string()),
            },
        });
    }
    Ok(out)
}

#[tauri::command]
pub async fn accept(state: St<'_>, id: String) -> Result<(), String> {
    let id = parse_share_id(&id)?;
    state
        .node()
        .await?
        .accept(id)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn reject(state: St<'_>, id: String, reason: Option<String>) -> Result<(), String> {
    let id = parse_share_id(&id)?;
    let reason = reason
        .filter(|r| !r.trim().is_empty())
        .unwrap_or_else(|| "declined".into());
    state
        .node()
        .await?
        .reject(id, &reason)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn cancel(state: St<'_>, id: String) -> Result<(), String> {
    let id = parse_share_id(&id)?;
    state
        .node()
        .await?
        .cancel(id)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn list_transfers(state: St<'_>) -> Result<Vec<TransferView>, String> {
    let node = state.node().await?;
    Ok(node
        .transfers()
        .into_iter()
        .map(|t| state.transfer_view(t))
        .collect())
}

#[tauri::command]
pub async fn clear_finished(state: St<'_>) -> Result<usize, String> {
    let node = state.node().await?;
    let n = node.remove_finished();
    let live: Vec<ShareId> = node.transfers().iter().map(|t| t.id).collect();
    state
        .saved
        .lock()
        .unwrap()
        .retain(|id, _| live.contains(id));
    Ok(n)
}

#[tauri::command]
pub async fn get_settings(state: St<'_>) -> Result<SettingsView, String> {
    Ok(state.view())
}

#[tauri::command]
pub async fn set_settings(state: St<'_>, update: SettingsUpdate) -> Result<SettingsView, String> {
    let restart = {
        let mut s = state.settings.lock().unwrap();
        let mut next = s.clone();
        let restart = next.apply(update)?;
        next.save(&state.settings_path)?;
        *s = next;
        restart
    };
    if restart {
        let st = state.inner().clone();
        tauri::async_runtime::spawn(async move { st.restart().await });
    }
    Ok(state.view())
}

/// Native file picker (desktop dialog, Android SAF through the bridge).
#[tauri::command]
pub async fn pick_files(app: tauri::AppHandle) -> Result<Vec<platform::PickedFile>, String> {
    platform::pick_files(&app).await
}

/// Desktop: choose the download folder. `None` if cancelled.
#[tauri::command]
pub async fn pick_download_folder(app: tauri::AppHandle) -> Result<Option<String>, String> {
    Ok(platform::pick_folder(&app)
        .await?
        .map(|p| p.display().to_string()))
}

/// Android: files shared to the app ("Share to WarzonePhone").
#[tauri::command]
pub async fn take_shared_files(app: tauri::AppHandle) -> Result<Vec<platform::PickedFile>, String> {
    platform::take_shared_files(&app)
}

#[tauri::command]
pub async fn open_file(state: St<'_>, id: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let p = state.transfer_path(&id).await?;
    state
        .app
        .opener()
        .open_path(p.to_string_lossy().into_owned(), None::<&str>)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn reveal_in_folder(state: St<'_>, id: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let p = state.transfer_path(&id).await?;
    state
        .app
        .opener()
        .reveal_item_in_dir(p)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn open_download_folder(state: St<'_>) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    if platform::IS_ANDROID {
        return Err("received files are in Downloads/WarzonePhone".into());
    }
    let dir = state.download_dir(&state.settings());
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    state
        .app
        .opener()
        .open_path(dir.to_string_lossy().into_owned(), None::<&str>)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_token() -> Settings {
        Settings {
            display_name: "alice".into(),
            relay_addr: "relay.example.org:4433".into(),
            relay_token: Some("s3cret-token-value".into()),
            iroh_relay: IrohRelay::Custom {
                url: "https://iroh.example.org".into(),
            },
            download_dir: Some(PathBuf::from("/tmp/dl")),
            auto_accept_verified: true,
        }
    }

    #[test]
    fn settings_roundtrip_and_private_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SETTINGS_FILE);
        assert_eq!(Settings::load(&path), Settings::default());
        let s = with_token();
        s.save(&path).unwrap();
        assert_eq!(Settings::load(&path), s);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "settings file must be owner-only");
        }
        // Overwrite keeps 0600 and the content.
        let mut s2 = s.clone();
        s2.display_name = "bob".into();
        s2.save(&path).unwrap();
        assert_eq!(Settings::load(&path).display_name, "bob");
    }

    #[test]
    fn corrupt_settings_fall_back_to_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SETTINGS_FILE);
        std::fs::write(&path, b"{not json").unwrap();
        assert_eq!(Settings::load(&path), Settings::default());
    }

    #[test]
    fn debug_and_view_never_leak_token() {
        let s = with_token();
        let dbg = format!("{s:?}");
        assert!(!dbg.contains("s3cret"), "{dbg}");
        assert!(dbg.contains("<redacted>"));
        let view = SettingsView {
            display_name: s.display_name.clone(),
            effective_display_name: s.display_name.clone(),
            relay_addr: s.relay_addr.clone(),
            relay_token_set: s.relay_token.is_some(),
            iroh_relay: s.iroh_relay.clone(),
            download_dir: "/tmp/dl".into(),
            download_dir_is_default: false,
            auto_accept_verified: true,
            platform: "linux",
        };
        let json = serde_json::to_string(&view).unwrap();
        assert!(!json.contains("s3cret"), "{json}");
        assert!(json.contains("\"relay_token_set\":true"));
    }

    #[test]
    fn token_keep_replace_clear() {
        let mut s = with_token();
        // Untouched by an unrelated update; empty string doesn't clear.
        let r = s
            .apply(SettingsUpdate {
                display_name: Some(" carol ".into()),
                relay_token: Some("  ".into()),
                ..Default::default()
            })
            .unwrap();
        assert!(!r, "name change needs no restart");
        assert_eq!(s.display_name, "carol");
        assert_eq!(s.relay_token.as_deref(), Some("s3cret-token-value"));
        // Replace.
        let r = s
            .apply(SettingsUpdate {
                relay_token: Some("new".into()),
                ..Default::default()
            })
            .unwrap();
        assert!(r);
        assert_eq!(s.relay_token.as_deref(), Some("new"));
        // Clear.
        let r = s
            .apply(SettingsUpdate {
                clear_relay_token: true,
                ..Default::default()
            })
            .unwrap();
        assert!(r);
        assert_eq!(s.relay_token, None);
        // Update JSON from the front end.
        let u: SettingsUpdate =
            serde_json::from_str(r#"{"iroh_relay":{"mode":"off"},"auto_accept_verified":false}"#)
                .unwrap();
        assert!(s.apply(u).unwrap());
        assert_eq!(s.iroh_relay, IrohRelay::Off);
        assert!(!s.auto_accept_verified);
    }

    #[test]
    fn validation() {
        let mut s = Settings::default();
        for bad in [
            "relay",
            "relay:",
            "relay:0",
            ":4433",
            "relay:99999",
            "a b:1",
        ] {
            let u = SettingsUpdate {
                relay_addr: Some(bad.into()),
                ..Default::default()
            };
            assert!(s.apply(u).is_err(), "{bad} should be rejected");
        }
        for good in [
            "",
            "relay.example.org:4433",
            "203.0.113.1:4433",
            "[2001:db8::1]:4433",
        ] {
            let u = SettingsUpdate {
                relay_addr: Some(good.into()),
                ..Default::default()
            };
            assert!(s.apply(u).is_ok(), "{good} should be accepted");
        }
        let u = SettingsUpdate {
            iroh_relay: Some(IrohRelay::Custom {
                url: "relay.example.org".into(),
            }),
            ..Default::default()
        };
        assert!(s.apply(u).is_err());
        // A failed update changes nothing.
        assert_eq!(s.iroh_relay, IrohRelay::N0);
        assert_eq!(s.relay_addr, "[2001:db8::1]:4433");
    }
}
