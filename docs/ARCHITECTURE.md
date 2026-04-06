# Inferno AoIP — Architecture

## Overview

Inferno is a Dante-compatible AoIP device implemented in Rust. It presents as an ALSA PCM device
to applications on Linux and transmits/receives audio over the Dante network protocol.

**Repository (upstream)**: https://gitlab.com/lumifaza/inferno  
**Repository (fork)**: https://github.com/legopc/inferno (DC integration work, `dev` branch)  
**Rust edition**: 2021, workspace with 3 crates

---

## Workspace Crates

| Crate | Purpose |
|-------|---------|
| `inferno_aoip/` | Core library — all protocol logic, audio routing, Dante communication |
| `alsa_pcm_inferno/` | ALSA plugin — thin wrapper that calls `inferno_aoip` from ALSA PCM API |
| `inferno2pipe/` | CLI utility — pipes Dante audio to stdout, not relevant for DC stats work |

The `alsa_pcm_inferno` plugin is compiled to `libasound_module_pcm_inferno.so` and loaded by ALSA
when an application opens a PCM named `inferno_...`. It calls into `inferno_aoip` to do the work.

---

## `inferno_aoip` Internal Structure

### Entry point: `DeviceServer`

`src/device_server/mod.rs` — the top-level struct. Instantiated by the ALSA plugin.

```
DeviceServer::start(Settings)
├── Spawns: arc_server (ARC protocol handler)
├── Spawns: cmc_server (CMC protocol handler)  
├── Spawns: info_mcast_server (multicast heartbeat + device info)
├── Spawns: mdns_server (mDNS for Dante device discovery)
└── On receive_to_external_buffer() / transmit_from_external_buffer():
    ├── FlowsReceiver  (RX: Dante network → ALSA buffer)
    ├── FlowsTransmitter (TX: ALSA buffer → Dante network)
    └── ChannelsSubscriber (subscribes/unsubscribes Dante flows)
```

### Clock system

`src/media_clock.rs` — wraps the Statime PTP clock via Unix socket `/tmp/ptp-usrvclock`.
The `MediaClock` provides timestamps in audio sample units (e.g. 1/48000 s per tick).
**Critical**: `DeviceServer::start()` blocks on `make_shared_media_clock()` until the clock is
available. The service sleeps up to 10s waiting for Statime to establish PTP sync.

---

## Data Flow: Transmit (Spotify → Dante)

```
librespot → ALSA write → alsa_pcm_inferno plugin
  └── DeviceServer::transmit_from_external_buffer()
       └── FlowsTransmitter thread (real-time, direct-to-UDP)
            ├── Reads samples from ring buffer at PTP-stamped positions
            ├── Packetizes into RTP/AES67 frames
            └── Sends UDP unicast to each subscribed Dante receiver
```

The TX path stamps each packet with the current PTP timestamp. Dante receivers use this to
schedule playback at exactly the right time (within the configured latency budget).

---

## Data Flow: Receive (Dante → analog out)

```
Dante transmitter (e.g. Shure MXWANI8) → UDP multicast/unicast
  └── FlowsReceiver thread (real-time, mio poll loop)
       ├── Reads UDP packets, extracts PTP timestamp from each
       ├── Calculates latency: now - packet_timestamp (in sample units)
       │    → stored in actual_latency_samples via fetch_max (keeps the peak)
       ├── Writes samples to ring buffer at scheduled timestamp
       └── ChannelsSubscriber → SamplesCollector → ALSA read
                                                         └── alsa_pcm_inferno plugin
                                                              └── arecord / alsaloop
```

The **latency measurement** happens here: every received UDP packet has a PTP timestamp.
The receiver compares `clock.now()` to that timestamp. The difference (in samples) is the
actual receive latency — how much earlier than "now" the packet claims to be.

---

## Key Files

### `device_server/info_mcast_server.rs` — **Most relevant for DC stats work**

The `Multicaster` struct handles all multicast communication:

| Function | Destination | Port | Trigger |
|----------|-------------|------|---------|
| `send_heartbeat()` | 224.0.0.233 | 8708 | Every 1 second |
| `send_board_info()` | 224.0.0.231 | 8702 | On startup + request opcode `0x07??0061` |
| `send_product_info()` | 224.0.0.231 | 8702 | On startup + request opcode `0x07??00c1` |
| `send_clock_stats()` | 224.0.0.231 | 8702 | On request opcode `0x07??0021` |
| `send_network_info()` | 224.0.0.231 | 8702 | On request opcode `0x07??0013` |

The main loop also handles incoming requests on the info port (listening for Dante Controller
queries) and routes them to the appropriate send function.

### `device_server/flows_rx.rs` — **Latency measurement lives here**

Key structs:

```rust
struct SocketData<P> {
    // ... socket, channels ...
    latency_samples: usize,          // configured latency (from DeviceInfo)
    actual_latency_samples: Arc<AtomicI32>,  // measured peak latency (shared with FlowInfo)
}

pub struct FlowInfo {
    pub actual_latency_samples: Arc<AtomicI32>,  // read by info_mcast_server
    pub channels_map: BitArray<...>,
    pub last_packet_time: Arc<AtomicUsize>,
}
```

In the UDP receive loop (hot path, real-time thread):
```rust
if let Some(now) = clock.wrapping_now_in_timebase(sample_rate.into()) {
    let latency = wrapped_diff(now, timestamp).clamp(0, i32::MAX as _);
    sd.actual_latency_samples.fetch_max(latency as _, Ordering::Relaxed);
}
```
This keeps the **maximum latency** seen since the last heartbeat. On each heartbeat,
`send_heartbeat()` reads it with `swap(0, Ordering::Relaxed)` — atomically reads + resets.

### `device_server/peaks.rs`

`peaks_of_buffers()` — iterates ring buffer samples, converts peak amplitude to
a logarithmic 0-255 scale (0 = silence, 255 = full scale). One byte per channel.
Used for the `0x8002` heartbeat type (audio level meters in DC).

### `device_server/channels_subscriber.rs`

Handles Dante subscription logic — discovers TX devices via mDNS, negotiates flows,
connects them to ring buffer inputs. Holds `Arc<FlowsReceiver>` and shares `flows_info`
(a `Vec<Option<FlowInfo>>`, one slot per flow, indexed by local flow index) with
`info_mcast_server`.

---

## Configuration (from ALSA plugin / `~/.asoundrc`)

The ALSA plugin reads these parameters from the `~/.asoundrc` PCM definition:

| Parameter | Type | Purpose |
|-----------|------|---------|
| `NAME` | string | Device name shown in Dante Controller |
| `DEVICE_ID` | hex string | 16-hex-digit unique ID (MAC-derived: `<mac_no_colons>0000`) |
| `BIND_IP` | IP string | NIC IP address to bind to |
| `SAMPLE_RATE` | u32 | Audio sample rate (48000) |
| `PROCESS_ID` | u8 | For multiple instances: 1, 2, 3 |
| `ALT_PORT` | u16 | Base UDP port (6000 = primary, 6004 = aux TX, 6008 = aux RX) |
| `TX_CHANNELS` | u16 | Number of transmit channels (0 for RX-only device) |
| `RX_CHANNELS` | u16 | Number of receive channels (0 for TX-only device) |
| `CLOCK_PATH` | path | Path to Statime Unix socket (`/tmp/ptp-usrvclock`) |
| `BITS_PER_SAMPLE` | u8 | Audio bit depth (16/24/32, default 24) — controls `send_encoding()` |
| `TX_LATENCY_NS` | u32 | TX latency advertised in mDNS channel/bundle records (default 10000000 = 10ms) |

---

## What DC Statistics Currently Shows (and Why)

| DC Panel | Shows for Inferno | Root cause |
|----------|-------------------|------------|
| Latency → Setting | ✅ Shows (e.g. 10ms) | Comes from Dante subscription negotiation |
| Latency → Peak | ✅ Shows | `0x8003` heartbeat block, measured per packet |
| Latency → Average | ✅ Shows | `0x8004` heartbeat block — running average tracked (T2.2 done) |
| Latency → Late | ✅ Shows | `0x8004` heartbeat block — late-packet counter tracked (T2.2 done) |
| TX/RX Util | ✅ Shows | `0x8000` heartbeat block, real byte counters |
| Sample Rate | ✅ Shows (read-only) | `0x80` device info, configured rate |
| Encoding | ✅ (blank, matches Shure) | Bit depth advertised in `0x82` device info; DC displays blank — correct |
| Audio Levels | ✅ Sends (0x8002) | `get_peaks` called per heartbeat — TX+RX peaks per channel, 0–255 log-scale |
| Clock offset | ✅ Shows | `0x8001` is fully implemented |
| Routing tab | ✅ Shows RX channels | `arc_server` always returns rx_channels |

---

## Build System

Standard Cargo workspace. No unusual build steps.

```bash
# Build ALSA plugin (the main deliverable):
cd alsa_pcm_inferno && cargo build --release
# → target/release/libasound_module_pcm_inferno.so

# Run tests:
cargo test

# Check for errors without building:
cargo check
```

Requires: `rustup` with Rust 1.94.0, `libasound2-dev` (or `alsa-lib-devel`).

### Build Reproducibility

A source-neutral change (docs, patches, no Rust code) produces an identical `.so` binary.
Verify with a hash-check workflow:

```bash
# Before any merge that might affect Rust source:
sha256sum ~/.local/lib/alsa-lib/libasound_module_pcm_inferno.so

# After rebuild on dante-doos:
cd ~/inferno && cargo build --release
sha256sum target/release/libasound_module_pcm_inferno.so

# Identical hashes = safe to deploy, no behaviour change
# Different hashes = Rust source changed — review diff before deploying
```
