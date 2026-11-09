# WarzonePhone

Custom lossy VoIP protocol built in Rust. E2E encrypted, FEC-protected, adaptive quality, designed for hostile network conditions.

This fork adds **end-to-end encrypted file sharing** (up to 10 GiB per file) to the WarzonePhone app on Android, Linux (x64 + arm64), Windows and macOS:

- **Contacts over iroh**: your identity is your WZP seed; share a `wzp1…` contact string or QR code, see who's online.
- **Direct first, relay fallback**: files go peer-to-peer when possible, otherwise through your own token-protected WZP relay (it only ever sees ciphertext). Recipients join with a one-transfer ticket, never your relay token.
- **Resumable and verified**: every chunk is BLAKE3-checked on arrival; interrupted transfers resume.

Downloads: see [Releases](../../releases) and [docs/INSTALL.md](docs/INSTALL.md) (builds are unsigned). Using it: [docs/USER_GUIDE.md](docs/USER_GUIDE.md#sending-files). Running a relay: [docs/ADMINISTRATION.md](docs/ADMINISTRATION.md). Protocol: [docs/FILE-TRANSFER.md](docs/FILE-TRANSFER.md), [docs/SHARE.md](docs/SHARE.md), [docs/IROH.md](docs/IROH.md).

## Quick Start

```bash
# Build
cargo build --release

# Run relay (token-protected)
./target/release/wzp-relay --listen 0.0.0.0:4433 --access-token "$(openssl rand -hex 32)"

# Send a test tone
./target/release/wzp-client --send-tone 5 relay-addr:4433

# Web bridge (browser calls)
./target/release/wzp-web --port 8080 --relay 127.0.0.1:4433 --tls
# Open https://localhost:8080/room-name in two browser tabs
```

## Architecture

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the full system architecture with Mermaid diagrams covering:

- System overview and data flow
- Crate dependency graph (8 crates)
- Wire formats (MediaHeader, MiniHeader, TrunkFrame, SignalMessage)
- Cryptographic handshake (X25519 + Ed25519 + ChaCha20-Poly1305)
- Identity model (BIP39 seed)
- Quality profiles (GOOD/DEGRADED/CATASTROPHIC)
- FEC protection (RaptorQ with interleaving)
- Adaptive jitter buffer (NetEq-inspired)
- Telemetry stack (Prometheus + Grafana)
- Deployment topology

## Features

- **3 quality tiers**: Opus 24k (28.8 kbps) / Opus 6k (9 kbps) / Codec2 1200 (2.4 kbps)
- **RaptorQ FEC**: Recovers from 20-100% packet loss depending on tier
- **E2E encryption**: ChaCha20-Poly1305 with X25519 key exchange
- **Adaptive jitter buffer**: EMA-based playout delay tracking
- **Silence suppression**: VAD + comfort noise (~50% bandwidth savings)
- **ML noise removal**: RNNoise (nnnoiseless pure Rust port)
- **Mini-frames**: 67% header compression for steady-state packets
- **Trunking**: Multiplex sessions into batched datagrams
- **Token auth hook**: relays can validate client tokens against any HTTP endpoint (`--auth-url`)
- **Prometheus metrics**: Relay, web bridge, inter-relay probes
- **Grafana dashboard**: Pre-built JSON with 18 panels

## Documentation

| Document | Description |
|----------|-------------|
| [ARCHITECTURE.md](docs/ARCHITECTURE.md) | Full system architecture with diagrams |
| [TELEMETRY.md](docs/TELEMETRY.md) | Prometheus metrics specification |
| [grafana-dashboard.json](docs/grafana-dashboard.json) | Importable Grafana dashboard |

## Binaries

| Binary | Description |
|--------|-------------|
| `wzp-relay` | Relay daemon (SFU room mode, forward mode, probes) |
| `wzp-client` | CLI client (send-tone, record, live mic, echo-test, drift-test, sweep) |
| `wzp-web` | Browser bridge (HTTPS + WebSocket + AudioWorklet) |
| `wzp-bench` | Component benchmarks |

## Linux Build

```bash
./scripts/build-linux.sh --prepare   # Create Hetzner VM + install deps
./scripts/build-linux.sh --build     # Build release binaries
./scripts/build-linux.sh --transfer  # Download to target/linux-x86_64/
./scripts/build-linux.sh --destroy   # Delete VM
```

## Tests

```bash
cargo test --workspace   # 272 tests
```

## License

MIT OR Apache-2.0
