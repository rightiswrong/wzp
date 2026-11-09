//! Token-protected relay: access tokens, room tickets, rotation.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use wzp_proto::{MediaTransport, SignalMessage};
use wzp_relay::access::AccessControl;

use super::common::*;
use crate::*;

fn access(tokens: &[&str], ttl: u64) -> Arc<AccessControl> {
    let t: Vec<String> = tokens.iter().map(|s| s.to_string()).collect();
    Arc::new(AccessControl::new(&t, None, [5u8; 32], Duration::from_secs(ttl)).unwrap())
}

fn with_token(t: &str) -> ConnectOptions {
    ConnectOptions {
        auth_token: Some(t.to_string()),
        peer_wait: Duration::from_secs(3),
        ..Default::default()
    }
}

fn no_token() -> ConnectOptions {
    ConnectOptions {
        peer_wait: Duration::from_secs(3),
        ..Default::default()
    }
}

async fn pair(
    relay: std::net::SocketAddr,
    room: &str,
    oa: &ConnectOptions,
    ob: &ConnectOptions,
) -> (Result<Session>, Result<Session>) {
    let (a, b) = (seed(40), seed(41));
    timeout(30, async {
        tokio::join!(
            connect_via_relay_with(relay, room, &a, oa),
            connect_via_relay_with(relay, room, &b, ob)
        )
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tokens_required_when_configured() {
    init_tracing();
    let relay = start_relay_with_access(access(&["good-token"], 600)).await;
    let room = random_room();
    // No token / wrong token: clean Auth error.
    let r = connect_via_relay_with(relay.addr, &room, &seed(1), &no_token()).await;
    assert!(matches!(r, Err(FileError::Auth(_))), "{r:?}");
    let r = connect_via_relay_with(relay.addr, &room, &seed(1), &with_token("bad")).await;
    assert!(matches!(r, Err(FileError::Auth(_))), "{r:?}");
    // Ticket request with a bad token is refused.
    let r = request_room_ticket(relay.addr, &room, "bad").await;
    assert!(matches!(r, Err(FileError::Auth(_))), "{r:?}");
    // Valid token on both ends.
    let t = with_token("good-token");
    let (a, b) = pair(relay.addr, &room, &t, &t).await;
    a.unwrap();
    b.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ticket_admits_only_its_room_and_two_participants() {
    init_tracing();
    let relay = start_relay_with_access(access(&["master"], 600)).await;
    let invite = create_invite(Some(relay.addr), Some("master"), vec![], &seed(40))
        .await
        .unwrap();
    let r = invite.relay.clone().unwrap();
    let ticket = r.auth_token.clone().expect("ticket in invite");
    assert!(ticket.starts_with("wzpt1."));
    assert!(
        !invite.to_json().contains("master"),
        "master token must not leak"
    );

    // Host with its master token, joiner with only the invite.
    let host_opts = with_token("master");
    let dflt = no_token();
    let (s40, s41) = (seed(40), seed(41));
    let (h, j) = timeout(30, async {
        tokio::join!(
            host_invite(&invite, None, &s40, &host_opts),
            join_invite(&invite, &s41, &dflt)
        )
    })
    .await;
    let (h, j) = (h.unwrap(), j.unwrap());
    // Transfer works on the ticket session.
    let d = tempfile::tempdir().unwrap();
    let src = d.path().join("t.bin");
    let data = random_file(&src, 200_000);
    let out = tempfile::tempdir().unwrap();
    let a = FileEndpoint::new(
        h,
        FileConfig {
            download_dir: d.path().into(),
            ..Default::default()
        },
    );
    let b = FileEndpoint::new(
        j,
        FileConfig {
            download_dir: out.path().into(),
            ..Default::default()
        },
    );
    let id = a.send(&src).await.unwrap();
    timeout(10, b.next_offer()).await.unwrap().accept();
    let p = timeout(30, b.wait(id)).await.unwrap();
    assert_eq!(std::fs::read(p).unwrap(), data);

    // Third participant with the same ticket: room is full.
    let r3 = connect_via_relay_with(relay.addr, &r.room, &seed(42), &with_token(&ticket)).await;
    assert!(r3.is_err(), "third participant admitted");

    // Ticket for another file room: refused.
    let other = random_room();
    let r = connect_via_relay_with(relay.addr, &other, &seed(43), &with_token(&ticket)).await;
    assert!(matches!(r, Err(FileError::Auth(_))), "{r:?}");

    // Ticket on a call room / tickets for call rooms: refused.
    let r = connect_via_relay_with(relay.addr, "call-xyz", &seed(43), &with_token(&ticket)).await;
    assert!(matches!(r, Err(FileError::Auth(_))), "{r:?}");
    assert!(
        request_room_ticket(relay.addr, "call-xyz", "master")
            .await
            .is_err()
    );

    // Ticket on the direct-call signaling channel: refused.
    let ep = wzp_transport::create_endpoint("127.0.0.1:0".parse().unwrap(), None).unwrap();
    let conn = wzp_transport::connect(&ep, relay.addr, "_signal", wzp_transport::client_config())
        .await
        .unwrap();
    let t = wzp_transport::QuinnTransport::new(conn.clone());
    t.send_signal(&SignalMessage::AuthToken {
        version: 1,
        token: ticket.clone(),
    })
    .await
    .unwrap();
    match timeout(5, conn.closed()).await {
        quinn::ConnectionError::ApplicationClosed(c) => {
            assert_eq!(
                c.error_code.into_inner(),
                wzp_relay::access::CLOSE_UNAUTHORIZED as u64
            )
        }
        other => panic!("{other:?}"),
    }
    // ...while the master token is accepted there.
    let conn = wzp_transport::connect(&ep, relay.addr, "_signal", wzp_transport::client_config())
        .await
        .unwrap();
    let t = wzp_transport::QuinnTransport::new(conn.clone());
    t.send_signal(&SignalMessage::AuthToken {
        version: 1,
        token: "master".into(),
    })
    .await
    .unwrap();
    assert!(matches!(
        timeout(5, t.recv_signal()).await,
        Ok(Some(SignalMessage::Pong { .. }))
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ticket_expires() {
    init_tracing();
    let relay = start_relay_with_access(access(&["master"], 1)).await;
    let invite = create_invite(Some(relay.addr), Some("master"), vec![], &seed(40))
        .await
        .unwrap();
    let r = invite.relay.unwrap();
    tokio::time::sleep(Duration::from_millis(2100)).await;
    let res = connect_via_relay_with(
        relay.addr,
        &r.room,
        &seed(41),
        &with_token(&r.auth_token.unwrap()),
    )
    .await;
    assert!(matches!(res, Err(FileError::Auth(_))), "{res:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn token_rotation_via_file_reload() {
    init_tracing();
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("tokens");
    std::fs::write(&path, "# relay tokens\nold-token\n").unwrap();
    let ac = Arc::new(
        AccessControl::new(&[], Some(path.clone()), [6u8; 32], Duration::from_secs(600)).unwrap(),
    );
    let relay = start_relay_with_access(ac.clone()).await;
    assert!(
        request_room_ticket(relay.addr, &random_room(), "old-token")
            .await
            .is_ok()
    );
    std::fs::write(&path, "new-token # rotated\n").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(SystemTime::now() + Duration::from_secs(5))
        .unwrap();
    assert!(ac.reload_if_changed());
    let r = request_room_ticket(relay.addr, &random_room(), "old-token").await;
    assert!(matches!(r, Err(FileError::Auth(_))), "{r:?}");
    assert!(
        request_room_ticket(relay.addr, &random_room(), "new-token")
            .await
            .is_ok()
    );
}
