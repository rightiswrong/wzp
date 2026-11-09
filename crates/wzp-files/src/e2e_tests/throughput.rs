//! Relay throughput: two clients behind a UDP delay proxy (simulated RTT)
//! push a file through the in-process relay.
//!
//! `quick_relay_throughput_with_rtt` runs in CI (`--lib`); the heavy
//! variant is `#[ignore]`d:
//! `cargo test --release -p wzp-files --lib heavy_relay_throughput -- --ignored --nocapture`

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use super::common::*;
use crate::*;

fn big_udp_socket(bind: SocketAddr) -> UdpSocket {
    let s = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::DGRAM, None).unwrap();
    let _ = s.set_recv_buffer_size(16 << 20);
    let _ = s.set_send_buffer_size(16 << 20);
    s.bind(&bind.into()).unwrap();
    s.set_nonblocking(true).unwrap();
    UdpSocket::from_std(s.into()).unwrap()
}

/// Spawn a delayed sender: packets pushed into the channel go out after
/// `delay` (constant delay keeps order).
fn delayed_sender(
    sock: Arc<UdpSocket>,
    delay: Duration,
) -> mpsc::UnboundedSender<(Instant, Vec<u8>, SocketAddr)> {
    let (tx, mut rx) = mpsc::unbounded_channel::<(Instant, Vec<u8>, SocketAddr)>();
    tokio::spawn(async move {
        while let Some((at, pkt, dst)) = rx.recv().await {
            let due = at + delay;
            if due > Instant::now() {
                tokio::time::sleep_until(due.into()).await;
            }
            let _ = sock.send_to(&pkt, dst).await;
        }
    });
    tx
}

/// UDP proxy adding `one_way` delay in each direction. Returns its address.
pub async fn delay_proxy(target: SocketAddr, one_way: Duration) -> SocketAddr {
    let front = Arc::new(big_udp_socket("127.0.0.1:0".parse().unwrap()));
    let addr = front.local_addr().unwrap();
    let to_client = delayed_sender(front.clone(), one_way);
    tokio::spawn(async move {
        let mut ups: HashMap<SocketAddr, mpsc::UnboundedSender<(Instant, Vec<u8>, SocketAddr)>> =
            HashMap::new();
        let mut buf = vec![0u8; 65536];
        loop {
            let Ok((n, client)) = front.recv_from(&mut buf).await else {
                return;
            };
            let tx = ups.entry(client).or_insert_with(|| {
                let up = Arc::new(big_udp_socket("127.0.0.1:0".parse().unwrap()));
                let to_client = to_client.clone();
                let up2 = up.clone();
                tokio::spawn(async move {
                    let mut b = vec![0u8; 65536];
                    while let Ok((n, _)) = up2.recv_from(&mut b).await {
                        let _ = to_client.send((Instant::now(), b[..n].to_vec(), client));
                    }
                });
                delayed_sender(up, one_way)
            });
            let _ = tx.send((Instant::now(), buf[..n].to_vec(), target));
        }
    });
    addr
}

/// Transfer `size` bytes through the relay with `rtt` on both legs;
/// returns MiB/s measured from `Started` to completion.
pub async fn relay_throughput(size: usize, rtt: Duration) -> f64 {
    let relay = start_relay().await;
    let entry = if rtt.is_zero() {
        relay.addr
    } else {
        delay_proxy(relay.addr, rtt / 2).await
    };
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let path = src.path().join("tp.bin");
    random_file(&path, size);
    let room = random_room();
    let (s30, s31) = (seed(30), seed(31));
    let (sa, sb) = tokio::join!(
        connect_via_relay(entry, &room, &s30),
        connect_via_relay(entry, &room, &s31),
    );
    let cfg = |d: &std::path::Path| FileConfig {
        download_dir: d.to_path_buf(),
        ..Default::default()
    };
    let a = FileEndpoint::new(sa.unwrap(), cfg(src.path()));
    let b = FileEndpoint::new(sb.unwrap(), cfg(dst.path()));
    let mut ev = a.events();
    let id = a.send(&path).await.unwrap();
    let offer = timeout(30, b.next_offer()).await.unwrap();
    offer.accept();
    let start = timeout(30, async {
        loop {
            if let Ok(Progress::Started { .. }) = ev.recv().await {
                return Instant::now();
            }
        }
    })
    .await;
    let (r, s) = tokio::join!(timeout(600, b.wait(id)), timeout(600, a.wait(id)));
    let secs = start.elapsed().as_secs_f64();
    let p = r.unwrap();
    s.unwrap();
    assert_eq!(std::fs::metadata(&p).unwrap().len(), size as u64);
    let mibs = size as f64 / (1024.0 * 1024.0) / secs;
    eprintln!(
        "relay throughput: {} MiB, RTT {} ms: {:.2} s, {:.1} MiB/s",
        size >> 20,
        rtt.as_millis(),
        secs,
        mibs
    );
    a.close().await;
    mibs
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quick_relay_throughput_with_rtt() {
    init_tracing();
    let mibs = relay_throughput(16 << 20, Duration::from_millis(50)).await;
    // Pre-tuning this was ~2 MiB/s (VoIP flow-control windows). Keep the
    // bound loose: CI machines are slow and shared.
    assert!(mibs > 4.0, "{mibs:.1} MiB/s");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn heavy_relay_throughput() {
    init_tracing();
    for rtt in [0u64, 20, 50, 100] {
        relay_throughput(256 << 20, Duration::from_millis(rtt)).await;
    }
}
