//! Relay access tokens for client connections.
//!
//! A token-protected relay (`wzp-relay --access-token(s)`) requires every
//! connection to send `SignalMessage::AuthToken` as its first signal. The
//! app registers the token for the one relay address it belongs to; every
//! code path that opens a relay connection (room join, `_signal`, the relay
//! leg of a direct call, reflection probes) calls [`authenticate`] right
//! after connecting. Tokens are only ever sent to the address they were
//! registered for, never to other relays.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{OnceLock, RwLock};

use wzp_proto::packet::default_signal_version;
use wzp_proto::{MediaTransport, SignalMessage};

fn registry() -> &'static RwLock<HashMap<SocketAddr, String>> {
    static REG: OnceLock<RwLock<HashMap<SocketAddr, String>>> = OnceLock::new();
    REG.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Replace all registered tokens with `token` for each of `addrs` (pass an
/// empty slice or `None` to clear). Called whenever relay settings change.
pub fn set_tokens(addrs: &[SocketAddr], token: Option<&str>) {
    let mut reg = registry().write().unwrap_or_else(|e| e.into_inner());
    reg.clear();
    if let Some(t) = token.filter(|t| !t.is_empty()) {
        for a in addrs {
            reg.insert(*a, t.to_string());
        }
    }
}

/// Token registered for this relay address, if any.
pub fn token_for(addr: SocketAddr) -> Option<String> {
    let reg = registry().read().unwrap_or_else(|e| e.into_inner());
    reg.get(&addr)
        .or_else(|| {
            // An IPv4 relay reached through a v4-mapped v6 socket.
            match addr {
                SocketAddr::V6(v6) => v6
                    .ip()
                    .to_ipv4_mapped()
                    .and_then(|ip| reg.get(&SocketAddr::new(ip.into(), v6.port()))),
                SocketAddr::V4(_) => None,
            }
        })
        .cloned()
}

/// Send the registered token (if any) as the connection's first signal.
/// A no-op for relays without a registered token (open relays).
pub async fn authenticate<T: MediaTransport + ?Sized>(
    transport: &T,
    relay: SocketAddr,
) -> Result<(), wzp_proto::TransportError> {
    if let Some(token) = token_for(relay) {
        transport
            .send_signal(&SignalMessage::AuthToken {
                version: default_signal_version(),
                token,
            })
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_scoped_to_their_address() {
        let a: SocketAddr = "203.0.113.5:4433".parse().unwrap();
        let b: SocketAddr = "198.51.100.7:4433".parse().unwrap();
        set_tokens(&[a], Some("t0k"));
        assert_eq!(token_for(a).as_deref(), Some("t0k"));
        assert_eq!(token_for(b), None);
        let mapped: SocketAddr = "[::ffff:203.0.113.5]:4433".parse().unwrap();
        assert_eq!(token_for(mapped).as_deref(), Some("t0k"));
        set_tokens(&[a], None);
        assert_eq!(token_for(a), None);
    }
}
