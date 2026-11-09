//! Relay access control: static access tokens and room-scoped tickets.
//!
//! * **Access tokens** (`--access-token`, `--access-tokens <file>`): when
//!   any is configured, every client connection must present one (as
//!   `SignalMessage::AuthToken`) before the relay handshake. The token file
//!   is re-read on SIGHUP and whenever its mtime changes. Tokens are stored
//!   only as SHA-256 digests, compared in constant time, and never logged.
//! * **Room tickets**: a client holding a token may ask for a ticket for a
//!   `file-*` room (`RoomTicketRequest`). A ticket is
//!   `wzpt1.<expiry_unix>.<max_participants>.<hex HMAC-SHA256(secret, …)>`
//!   and admits its holder to exactly that room (two-party), only until
//!   the expiry (the session may outlive it). It cannot be used for the
//!   direct-call signaling channel or any other room. The HMAC secret is
//!   random per relay and persisted next to the relay identity.
//! * `--auth-url` (HTTP validation) keeps working as an alternative backend:
//!   a presented token that is neither a local token nor a ticket is sent
//!   to the URL.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use tracing::{info, warn};
use wzp_proto::{MediaTransport, SignalMessage, default_signal_version};

use crate::file_relay::room_is_file_session;

/// Prefix of room tickets.
pub const TICKET_PREFIX: &str = "wzpt1.";
/// QUIC application close code for unauthenticated / refused clients.
pub const CLOSE_UNAUTHORIZED: u32 = 0x5741;
/// Participants a ticket admits (file rooms are two-party).
pub const TICKET_MAX_PARTICIPANTS: u32 = 2;

type HmacSha256 = Hmac<Sha256>;

/// Parse a token file: one token per line, `#` starts a comment, blank
/// lines ignored, surrounding whitespace trimmed.
pub fn parse_tokens(text: &str) -> Vec<String> {
    text.lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

fn digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

fn ct_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut d = 0u8;
    for i in 0..32 {
        d |= a[i] ^ b[i];
    }
    d == 0
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Load the ticket HMAC secret from `path`, creating it (mode 0600) if
/// missing.
pub fn load_or_create_secret(path: &Path) -> std::io::Result<[u8; 32]> {
    if let Ok(text) = std::fs::read_to_string(path) {
        if let Ok(v) = hex::decode(text.trim()) {
            if let Ok(s) = <[u8; 32]>::try_from(v.as_slice()) {
                return Ok(s);
            }
        }
        warn!(path = %path.display(), "corrupt ticket secret, regenerating");
    }
    let mut s = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut s);
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    use std::io::Write;
    opts.open(path)?.write_all(hex::encode(s).as_bytes())?;
    Ok(s)
}

/// Token + ticket state shared by all connection handlers.
pub struct AccessControl {
    static_tokens: Vec<[u8; 32]>,
    file: Option<PathBuf>,
    file_tokens: RwLock<Vec<[u8; 32]>>,
    file_mtime: Mutex<Option<SystemTime>>,
    secret: [u8; 32],
    ticket_ttl: Duration,
}

impl AccessControl {
    /// `tokens`: from `--access-token`; `file`: `--access-tokens`.
    /// Configuring a file (even an empty one) enables enforcement.
    pub fn new(
        tokens: &[String],
        file: Option<PathBuf>,
        secret: [u8; 32],
        ticket_ttl: Duration,
    ) -> std::io::Result<Self> {
        let ac = Self {
            static_tokens: tokens.iter().map(|t| digest(t.trim())).collect(),
            file,
            file_tokens: RwLock::new(Vec::new()),
            file_mtime: Mutex::new(None),
            secret,
            ticket_ttl,
        };
        if ac.file.is_some() {
            ac.reload()?;
        }
        Ok(ac)
    }

    /// No tokens: open relay (tickets are still issued, but not needed).
    pub fn open(secret: [u8; 32]) -> Self {
        Self::new(&[], None, secret, Duration::from_secs(600)).expect("no file")
    }

    /// True when clients must authenticate.
    pub fn enabled(&self) -> bool {
        !self.static_tokens.is_empty() || self.file.is_some()
    }

    pub fn ticket_ttl(&self) -> Duration {
        self.ticket_ttl
    }

    /// Re-read the token file. Returns the number of tokens loaded.
    pub fn reload(&self) -> std::io::Result<usize> {
        let Some(path) = &self.file else { return Ok(0) };
        let meta = std::fs::metadata(path)?;
        let text = std::fs::read_to_string(path)?;
        let toks: Vec<[u8; 32]> = parse_tokens(&text).iter().map(|t| digest(t)).collect();
        let n = toks.len();
        *self.file_tokens.write().unwrap() = toks;
        *self.file_mtime.lock().unwrap() = meta.modified().ok();
        info!(path = %path.display(), tokens = n, "access tokens loaded");
        Ok(n)
    }

    /// Reload if the file's mtime changed. Returns true if reloaded.
    pub fn reload_if_changed(&self) -> bool {
        let Some(path) = &self.file else { return false };
        let m = std::fs::metadata(path).ok().and_then(|m| m.modified().ok());
        if m.is_some() && m != *self.file_mtime.lock().unwrap() {
            match self.reload() {
                Ok(_) => return true,
                Err(e) => warn!(path = %path.display(), "token reload failed: {e}"),
            }
        }
        false
    }

    /// Watch the token file (mtime poll every 2 s, plus SIGHUP on Unix).
    pub fn spawn_reloader(self: std::sync::Arc<Self>) {
        if self.file.is_none() {
            return;
        }
        tokio::spawn(async move {
            #[cfg(unix)]
            let mut hup =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()).ok();
            loop {
                #[cfg(unix)]
                {
                    let hup_fut = async {
                        match hup.as_mut() {
                            Some(h) => {
                                h.recv().await;
                            }
                            None => std::future::pending::<()>().await,
                        }
                    };
                    tokio::select! {
                        _ = hup_fut => {
                            if let Err(e) = self.reload() {
                                warn!("token reload on SIGHUP failed: {e}");
                            }
                            continue;
                        }
                        _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                    }
                }
                #[cfg(not(unix))]
                tokio::time::sleep(Duration::from_secs(2)).await;
                self.reload_if_changed();
            }
        });
    }

    /// Constant-time check against all configured tokens.
    pub fn check_token(&self, token: &str) -> bool {
        let d = digest(token.trim());
        let mut ok = false;
        for t in &self.static_tokens {
            ok |= ct_eq(t, &d);
        }
        for t in self.file_tokens.read().unwrap().iter() {
            ok |= ct_eq(t, &d);
        }
        ok
    }

    fn mac(&self, room: &str, expiry: u64, max: u32) -> HmacSha256 {
        let mut m = HmacSha256::new_from_slice(&self.secret).expect("hmac key");
        m.update(b"wzp-ticket-v1\0");
        m.update(room.as_bytes());
        m.update(b"\0");
        m.update(&expiry.to_be_bytes());
        m.update(&max.to_be_bytes());
        m
    }

    /// Issue a ticket for `room`, valid for the configured TTL from `now`.
    pub fn issue_ticket(&self, room: &str, now: u64) -> (String, u64) {
        let expiry = now + self.ticket_ttl.as_secs();
        let tag = self
            .mac(room, expiry, TICKET_MAX_PARTICIPANTS)
            .finalize()
            .into_bytes();
        (
            format!(
                "{TICKET_PREFIX}{expiry}.{TICKET_MAX_PARTICIPANTS}.{}",
                hex::encode(tag)
            ),
            expiry,
        )
    }

    /// True if `ticket` was issued by this relay for `room` and has not
    /// expired at `now`.
    pub fn verify_ticket(&self, ticket: &str, room: &str, now: u64) -> bool {
        if !room_is_file_session(room) {
            return false;
        }
        let Some(rest) = ticket.strip_prefix(TICKET_PREFIX) else {
            return false;
        };
        let mut parts = rest.splitn(3, '.');
        let (Some(exp), Some(max), Some(tag)) = (parts.next(), parts.next(), parts.next()) else {
            return false;
        };
        let (Ok(exp), Ok(max), Ok(tag)) =
            (exp.parse::<u64>(), max.parse::<u32>(), hex::decode(tag))
        else {
            return false;
        };
        if exp < now || max != TICKET_MAX_PARTICIPANTS {
            return false;
        }
        self.mac(room, exp, max).verify_slice(&tag).is_ok()
    }
}

/// How a client was admitted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Admission {
    /// Open relay, local access token, or `--auth-url` (with its fingerprint).
    Full { http_fingerprint: Option<String> },
    /// Room ticket: only the connection's own `file-*` room.
    Ticket,
}

/// Outcome of [`gate_room_connection`].
pub enum Gate {
    /// Proceed with the relay handshake using `offer` (the client's first
    /// non-auth signal, normally `CallOffer`).
    Join {
        admission: Admission,
        offer: SignalMessage,
    },
    /// A ticket was issued; the connection is done.
    TicketIssued,
    /// Refused (the client was told and the connection closed).
    Rejected(String),
}

/// Send a Hangup and close with [`CLOSE_UNAUTHORIZED`].
pub async fn refuse(transport: &wzp_transport::QuinnTransport, reason: &str) {
    let _ = transport
        .send_signal(&SignalMessage::Hangup {
            version: default_signal_version(),
            reason: wzp_proto::HangupReason::Error,
            call_id: None,
        })
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    transport.connection().close(
        quinn::VarInt::from_u32(CLOSE_UNAUTHORIZED),
        reason.as_bytes(),
    );
}

async fn recv_first(transport: &wzp_transport::QuinnTransport) -> Option<SignalMessage> {
    match tokio::time::timeout(Duration::from_secs(10), transport.recv_signal()).await {
        Ok(Ok(Some(m))) => Some(m),
        _ => None,
    }
}

/// Is `token` a valid local token, or accepted by `auth_url`?
/// Returns `Some(http_fingerprint)` on success.
async fn token_ok(
    access: &AccessControl,
    auth_url: Option<&str>,
    token: &str,
) -> Option<Option<String>> {
    if access.check_token(token) {
        return Some(None);
    }
    if let Some(url) = auth_url {
        if let Ok(c) = crate::auth::validate_token(url, token).await {
            return Some(Some(c.fingerprint));
        }
    }
    None
}

/// Authenticate a room (media) connection before the relay handshake.
///
/// First signal:
/// * `RoomTicketRequest` → token checked, ticket for this connection's own
///   `file-*` room returned, connection closed;
/// * `AuthToken` → ticket for this room, local token, or `--auth-url`;
/// * anything else → allowed only on an open relay.
pub async fn gate_room_connection(
    transport: &wzp_transport::QuinnTransport,
    room: &str,
    access: &AccessControl,
    auth_url: Option<&str>,
) -> Gate {
    let enforce = access.enabled() || auth_url.is_some();
    let Some(first) = recv_first(transport).await else {
        return Gate::Rejected("no signal".into());
    };
    let admission = match first {
        SignalMessage::RoomTicketRequest { room: r, token, .. } => {
            if r != room || !room_is_file_session(room) {
                refuse(
                    transport,
                    "tickets are only issued for the connection's file-* room",
                )
                .await;
                return Gate::Rejected("ticket request for wrong room".into());
            }
            if enforce && token_ok(access, auth_url, &token).await.is_none() {
                refuse(transport, "unauthorized").await;
                return Gate::Rejected("ticket request with invalid token".into());
            }
            let (ticket, expires_unix) = access.issue_ticket(room, now_unix());
            let _ = transport
                .send_signal(&SignalMessage::RoomTicket {
                    version: default_signal_version(),
                    room: room.to_string(),
                    ticket,
                    expires_unix,
                })
                .await;
            info!(room, "issued room ticket");
            return Gate::TicketIssued;
        }
        SignalMessage::AuthToken { token, .. } => {
            if token.starts_with(TICKET_PREFIX) {
                if access.verify_ticket(&token, room, now_unix()) {
                    Admission::Ticket
                } else if !enforce {
                    Admission::Full {
                        http_fingerprint: None,
                    }
                } else {
                    refuse(transport, "invalid or expired ticket for this room").await;
                    return Gate::Rejected("bad ticket".into());
                }
            } else {
                match token_ok(access, auth_url, &token).await {
                    Some(fp) => Admission::Full {
                        http_fingerprint: fp,
                    },
                    None if !enforce => Admission::Full {
                        http_fingerprint: None,
                    },
                    None => {
                        refuse(transport, "unauthorized").await;
                        return Gate::Rejected("invalid token".into());
                    }
                }
            }
        }
        other => {
            if enforce {
                refuse(
                    transport,
                    "unauthorized: this relay requires an access token",
                )
                .await;
                return Gate::Rejected("no token".into());
            }
            return Gate::Join {
                admission: Admission::Full {
                    http_fingerprint: None,
                },
                offer: other,
            };
        }
    };
    match recv_first(transport).await {
        Some(offer) => Gate::Join { admission, offer },
        None => Gate::Rejected("closed after auth".into()),
    }
}

/// Authenticate a direct-call signaling (`_signal`) connection. Tickets are
/// not accepted here. Returns the `--auth-url` fingerprint if any.
pub async fn gate_signal_connection(
    transport: &wzp_transport::QuinnTransport,
    access: &AccessControl,
    auth_url: Option<&str>,
) -> Result<Option<String>, String> {
    if !access.enabled() && auth_url.is_none() {
        return Ok(None);
    }
    match recv_first(transport).await {
        Some(SignalMessage::AuthToken { token, .. }) if !token.starts_with(TICKET_PREFIX) => {
            match token_ok(access, auth_url, &token).await {
                Some(fp) => Ok(fp),
                None => {
                    refuse(transport, "unauthorized").await;
                    Err("invalid token".into())
                }
            }
        }
        _ => {
            refuse(
                transport,
                "unauthorized: this relay requires an access token",
            )
            .await;
            Err("expected AuthToken".into())
        }
    }
}

/// Check a token presented on the WebSocket bridge for `room`.
pub async fn check_ws_token(
    access: &AccessControl,
    auth_url: Option<&str>,
    room: &str,
    token: &str,
) -> Option<Option<String>> {
    if token.starts_with(TICKET_PREFIX) {
        return access
            .verify_ticket(token, room, now_unix())
            .then_some(None);
    }
    token_ok(access, auth_url, token).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ac(tokens: &[&str]) -> AccessControl {
        let t: Vec<String> = tokens.iter().map(|s| s.to_string()).collect();
        AccessControl::new(&t, None, [7u8; 32], Duration::from_secs(600)).unwrap()
    }

    #[test]
    fn parse_token_file() {
        let t = parse_tokens("# admins\nalpha\n\n  beta  # laptop\n#gamma\n");
        assert_eq!(t, vec!["alpha", "beta"]);
    }

    #[test]
    fn tokens_checked() {
        let a = ac(&["s3cret"]);
        assert!(a.enabled());
        assert!(a.check_token("s3cret"));
        assert!(!a.check_token("s3cre"));
        assert!(!ac(&[]).enabled());
    }

    #[test]
    fn tickets_scoped_and_expiring() {
        let a = ac(&["x"]);
        let (t, exp) = a.issue_ticket("file-abc", 1000);
        assert_eq!(exp, 1600);
        assert!(a.verify_ticket(&t, "file-abc", 1000));
        assert!(a.verify_ticket(&t, "file-abc", 1600));
        assert!(!a.verify_ticket(&t, "file-abc", 1601), "expired");
        assert!(!a.verify_ticket(&t, "file-abd", 1000), "other room");
        assert!(!a.verify_ticket(&t, "call-abc", 1000));
        // tampered expiry / max
        let forged = t.replacen("1600", "9999", 1);
        assert!(!a.verify_ticket(&forged, "file-abc", 1000));
        let forged = t.replacen(".2.", ".3.", 1);
        assert!(!a.verify_ticket(&forged, "file-abc", 1000));
        // other relay's secret
        let b = AccessControl::new(&[], None, [8u8; 32], Duration::from_secs(600)).unwrap();
        assert!(!b.verify_ticket(&t, "file-abc", 1000));
        // call rooms never get tickets accepted
        let (tc, _) = a.issue_ticket("call-1", 1000);
        assert!(!a.verify_ticket(&tc, "call-1", 1000));
    }

    #[test]
    fn token_file_reload() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("tokens");
        std::fs::write(&p, "one\n").unwrap();
        let a = AccessControl::new(&[], Some(p.clone()), [1; 32], Duration::from_secs(60)).unwrap();
        assert!(a.check_token("one") && !a.check_token("two"));
        std::fs::write(&p, "two\n").unwrap();
        // force an mtime difference on coarse filesystems
        let later = SystemTime::now() + Duration::from_secs(5);
        let f = std::fs::File::options().write(true).open(&p).unwrap();
        f.set_modified(later).unwrap();
        assert!(a.reload_if_changed());
        assert!(!a.check_token("one") && a.check_token("two"));
        assert!(!a.reload_if_changed());
    }

    #[test]
    fn secret_persisted() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("sub/secret");
        let s1 = load_or_create_secret(&p).unwrap();
        let s2 = load_or_create_secret(&p).unwrap();
        assert_eq!(s1, s2);
    }
}
