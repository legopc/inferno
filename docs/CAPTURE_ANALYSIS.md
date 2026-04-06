# Packet Capture Analysis

**Captures** (all in `./captures/`):

| File | Contents | Key use |
|------|----------|---------|
| `dante-filtered.pcap` | 150KB filtered; port 8708 heartbeats from Shure + dante-doos | Initial latency analysis |
| `dante4.pcapng` | Broader capture; port 8708 + ARC port 8700 | Routing tab investigation |
| `dante5.pcapng` | Port 8708 heartbeats only; no 8702 traffic | Heartbeat-only snapshot |
| `dante6.pcapng` | **First capture with port 8702 device-info** (Shure 0x80 packet) | Sample rate / feature flags |

**Devices in all captures**:
- `192.168.1.34` — Shure MXWANI8 (reference hardware Dante device)
- `192.168.1.25` — dante-doos (Inferno RX-only, software Dante)
- `192.168.1.43` — EliteDesk-01 (Inferno TX-only, software Dante)

---

## Key Findings

### 1. Inferno's 0x8003 format is CORRECT and data IS valid

The 0x8003 (RX latency) block sent by dante-doos **matches the Shure's structure exactly**:
- 32 fixed slots (one per possible Dante flow)
- One `u32` per slot = latency in samples
- Same header layout (slots, cfg_latency, sample_rate fields)

dante-doos flow[0] latency over 81 heartbeat samples:
```
avg  = 2.632ms
peak = 3.729ms
min  = 1.958ms
```

The data is continuously updated and non-zero. Inferno is correctly measuring and reporting
latency. This is NOT the problem.

### 2. The only structural difference: Inferno was missing 0x8004

The Shure sends TWO blocks every heartbeat: `0x8003` (latency values) AND `0x8004` (missed
packets). Inferno previously sent only `0x8003`.

DC requires `0x8004` to be present alongside `0x8003` before displaying the latency panel values.
**This is fixed** — both blocks are now sent in datagram 2.

### 3. 0x8004 block structure (confirmed from Shure capture)

```
Offset  Size  Value       Meaning
0x00    u16   0x0094      Total block length = 148 bytes
0x02    u16   0x8004      Type
0x04    u16   0x0004      Sub-type
0x06    u16   0x0088      Content length = 136
0x08    u16   seqnum      Same seqnum counter as 0x8003 in same datagram
0x0a    u16   0x0000      Zero
0x0c    u16   0x0020      Slot count = 32 (same as 0x8003)
0x0e    u16   0x0000      Zero
0x10    u16   0x0014      Data start offset = 20 (vs 24 in 0x8003 — no sample_rate field)
0x12    u16   0x0000      Zero
0x14    u32×32           Per-slot missed/late packet count (all zero = no late packets)
```

Differences from `0x8003`:
- No `sample_rate` u32 field (content sub-header is 8 bytes, not 12)
- Data start offset = 20 (not 24)
- Total length = 148 (not 152)

Inferno implements this with actual flow count (not fixed 32 slots). DC handles variable counts.

### 4. Heartbeat datagram split is required

All four captures confirm the Shure always sends its heartbeat blocks in two separate datagrams:
- Datagram 1 (224.0.0.233:8708): `[0x8000 utilization] + [0x8001 clock PPB]`
- Datagram 2 (224.0.0.233:8708): `[0x8003 latency] + [0x8004 missed packets]`

Inferno previously sent all blocks in one combined datagram. This caused DC to silently
ignore the latency data. **Fixed** by splitting into two separate UDP sends.

### 5. How Dante Controller computes peak/average/late

The Shure sends a single `u32` per slot in `0x8003` (~38 samples = 0.79ms per heartbeat),
yet DC shows `peak: 1.8ms` and `average: 0.866ms`. This means:

- **DC tracks peak itself** over a display window (max of all received 0x8003 values)
- **DC tracks average itself** (rolling average of 0x8003 values over the window)
- **late** comes from the `0x8004` block (sum of missed packet counts)

Inferno only needs to send the current peak latency per flow (which it already does) — DC
handles the display aggregation.

### 6. dante-doos latency is 2-3ms vs Shure's 0.79ms

This is expected: dante-doos uses **software PTP** (RTL8111 NIC, ~500µs offset), while the
Shure uses hardware PTP clocking. Higher software PTP jitter → higher measured latency.

---

## dante6.pcapng — Device Info (port 8702) Findings

This was the first capture to include port 8702 traffic (device-info multicast). Key findings:

### Shure 0x80 (sample rate) packet — confirmed wire format

DC sends `07 3d 00 81 00 00 00 64` unicast to Shure on port 8700 (ARC).
Shure responds by sending opcode 0x80 to 224.0.0.231:8702.

**Shure 0x80 payload** (opcode header + 20 bytes content):
```
00 18 00 01  ← header bytes 0-3 (type indicator — 0x01 = display-only)
00 00 bb 80  ← current sample rate: 0xbb80 = 48000
00 00 bb 80  ← default sample rate: 48000
00 01 00 00  ← count of supported rates = 1
00 00 bb 80  ← supported rate[0]: 48000
```

Our earlier implementation used `00 10 00 04` for the first 4 bytes. This made DC treat
the sample rate as **editable**. The Shure uses `00 18 00 01` which makes it **display-only**.

### Feature flags byte 0x17 — confirmed meaning of bit 0x10

Shure board info (opcode 0x60) byte 0x17 = `0xcb` = 0b11001011. Bit 0x10 is NOT set.
When Inferno had `0x17 = 0x18` (bit 0x10 SET), DC showed sample rate as editable.
After removing bit 0x10 (`0x17 = 0x08`), DC shows sample rate as display-only. ✅

Confirmed: **bit 0x10 in byte 0x17 = "DC may change sample rate" (make editable)**

### Shure does NOT send opcode 0x82 (encoding)

DC shows encoding as blank for the Shure. This is not a bug — DC simply does not display
encoding from the 0x82 packet content. Inferno sends 0x82 with non-editable header; DC also
shows it blank. Behaviour matches Shure exactly. No further action needed.

---

## Implementation Status — All Complete

| Feature | Status | Notes |
|---------|--------|-------|
| DC latency display (0x8003 + 0x8004) | ✅ Working | Peak/avg/late all show in DC |
| Datagram split | ✅ Fixed | Two separate UDP sends per tick |
| TX/RX utilization (0x8000) | ✅ Working | Real byte counters from flows |
| Routing tab visibility | ✅ Fixed | ARC server always reports rx_channels.len() |
| Sample rate display (0x80) | ✅ Working | Display-only, shows 48kHz |
| Sample rate non-editable | ✅ Fixed | Correct header bytes + feature flags |
| Encoding display (0x82) | ✅ Acceptable | No value shown — matches Shure behaviour |
| dante-doos latency ~2-3ms | ✅ Expected | Software PTP limitation, not a bug |

---

## Files Modified

- `inferno/inferno_aoip/src/device_server/info_mcast_server.rs`
  - Two-datagram heartbeat split
  - 0x8004 block added
  - 0x8000 TX/RX utilization with real counters
  - 0x80 sample rate header fixed to `00 18 00 01`
  - 0x82 encoding header `00 18 00 03` (non-editable)
  - `content[0x17] = 0x08` (remove bit 0x10 = rate configurable)

- `inferno/inferno_aoip/src/device_server/arc_server.rs`
  - Always returns `rx_channels.len()` regardless of subscriber state (routing tab fix)
