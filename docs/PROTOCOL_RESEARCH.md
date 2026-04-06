# Dante Multicast Protocol Research

## What This Document Is

Notes on the Dante multicast protocol as used by Inferno and observed from the Shure MXWANI8.
This is a reverse-engineered, incomplete picture — the official protocol is proprietary.
The primary reference is `inferno_aoip/src/device_server/info_mcast_server.rs` in the Inferno source.

**Authoritative source**: the packet captures in `../captures/` — always prefer capture data
over speculation in this document.

---

## Network Topology

| Address | Port | Direction | Purpose |
|---------|------|-----------|---------|
| 224.0.0.233 | 8708 | Device → all | Heartbeat (stats, peaks, latency) |
| 224.0.0.231 | 8702 | Device → all | Device info (board, product, clock, network) |
| 224.0.0.231 | 8702 | Dante Controller → Device | Info requests (queries device responds to) |
| Unicast | 8700 | Dante Control | ARC protocol (subscription management) |
| Unicast | 4455 | Dante Audio | CMC protocol |
| Unicast | varies | Audio | RTP audio flows |

**Inferno ALT_PORT offsets** (inferno uses alternate ports — standard Dante ports are also listened on):

| ALT_PORT offset | Port (base 6000) | Purpose |
|-----------------|------------------|---------|
| +4 | 6004 | ARC server (alt) |
| +5 | 6005 | CMC server (alt) |
| +6 | 6006 | Device info multicast send |
| +7 | 6007 | Info request port (unicast — receives reboot/identify conmon) |
| +8 | 6008 | Heartbeat multicast send |
| +9 | 6009 | (reserved) |
| +10 | 6010 | Audio RTP (alt base) |
| +11 | 6011 | Info request port 2 |

---

## Packet Outer Structure

All Dante multicast packets share a common 32-byte header:

```
Offset  Size  Description
0x00    2     Start code (0xffff = device info, 0xfffe = heartbeat)
0x02    2     Sequence number (increments per device)
0x04    4     ??? (observed: 0x00000000 or small values)
0x08    8     DEVICE_ID (16 hex chars from MAC: MAC_no_colons + "0000")
0x10    8     Vendor string (8 bytes, space-padded)
0x18    8     Opcode (8 bytes — identifies message type)
--- Content follows (opcode-specific) ---
```

The `make_packet()` function in `src/protocol/mcast.rs` writes this header.

---

## Opcode Reference

### Heartbeat opcodes (start code 0xfffe, dst 224.0.0.233:8708)

| Opcode bytes | Description |
|--------------|-------------|
| `00 08 00 01 10 00 00 00` | Regular heartbeat (sent every 1 second) |

### Device info opcodes (start code 0xffff, dst 224.0.0.231:8702)

| Opcode bytes | Sent by | Triggered by |
|--------------|---------|--------------|
| `07 2a 00 60 00 00 00 00` | Device | Startup + request `07 ?? 00 61` |
| `07 2a 00 c0 00 00 00 00` | Device | Startup + request `07 ?? 00 c1` |
| `07 2a 00 20 00 00 00 00` | Device | Request `07 ?? 00 21` (clock stats) |
| `07 2a 00 11 00 00 00 00` | Device | Request `07 ?? 00 13` (network info) |
| `07 2a 00 78 00 00 00 00` | Device | Request `07 ?? 00 77` (→ responds `[0,0,0,3,0,0,0,0]`) |
| `07 2a 00 80 00 00 00 00` | Device → 224.0.0.231 | Sample rate info (sent proactively; also in response to `07 3d 00 81 .. 64` ARC query) ✅ |
| `07 2a 00 82 00 00 00 00` | Device → 224.0.0.231 | Encoding info (Shure does NOT send this; DC shows encoding as blank — expected) |
| `07 2a 10 07 00 00 00 00` | Device | Unknown (Shure sends, not yet decoded) |
| `07 2a 10 09 00 00 00 00` | Device | Unknown (Shure sends, not yet decoded) |

---

## Heartbeat Content Structure

The heartbeat content is a concatenation of typed blocks. Each block:

```
Offset  Size  Description
0x00    2     Total block length (including this header)
0x02    2     Type (0x8001, 0x8002, 0x8003, 0x8000, 0x8004)
0x04    2     Sub-type or format indicator (always 0x0004 observed)
0x06    2     Content length (block length - 12)
0x08    2     Sequence counter (same as outer packet seqnum)
0x0a    2     Zero padding
--- Type-specific content follows ---
```

Blocks are sent only when `freq_offset_opt` is `Some(...)` — i.e. when PTP clock is synced.
If clock is not available: heartbeat is sent but with EMPTY content (no blocks). This is why
DC shows "no values" for a device that just started before PTP sync.

---

## Heartbeat Type 0x8001 — Clock Frequency Offset

**Status: ✅ Implemented and working**

Total length: 16 bytes

```
0x00  u16  16 (length)
0x02  u16  0x8001 (type)
0x04  u16  4
0x06  u16  4 (content length)
0x08  u16  seqnum
0x0a  u16  0
0x0c  i32  freq_offset_ppb  (PLL offset in parts-per-billion)
```

This appears in DC as the clock sync offset display.

---

## Heartbeat Type 0x8002 — Audio Peak Levels

**Status: ✅ Implemented and working**

Variable length (depends on channel count).

```
0x00  u16  24 + total_peaks (+ padding to 4-byte align)
0x02  u16  0x8002 (type)
0x04  u16  4
0x06  u16  12 + total_peaks (content length)
0x08  u16  seqnum
0x0a  u16  0
0x0c  u16  tx_channel_count
0x0e  u16  0
0x10  u16  rx_channel_count
0x12  u16  0
0x14  u16  24 (???)
0x16  u16  0
0x18  u8[] tx_peaks (one byte per TX channel, log scale 0-255)
      u8[] rx_peaks (one byte per RX channel, log scale 0-255)
      u8[] padding to 4-byte boundary
```

Peak level scale: `peaks.rs::peaks_of_buffers()` computes `20 * log10(peak/max)` mapped to 0-255.
Value 255 = full scale, 0 = silence or -inf dB.

---

## Heartbeat Type 0x8003 — RX Latency

**Status: ✅ Working — DC shows peak/average/late correctly**

Format (confirmed from Shure MXWANI8 capture and Inferno implementation):

```
0x00  u16  24 + flows_count * 4   (total block length)
0x02  u16  0x8003                 (type)
0x04  u16  4                      (sub-type)
0x06  u16  12 + flows_count * 4   (content length)
0x08  u16  seqnum                 (independent latency_block_seqnum counter)
0x0a  u16  0
0x0c  u16  flows_count            (32 slots = Shure fixed, Inferno uses actual count)
0x0e  u16  0
0x10  u16  24                     (data start offset from block start = 0x18)
0x12  u16  0
0x14  u32  sample_rate            (e.g. 48000 = 0x0000bb80)
--- Per-flow data (flows_count × 4 bytes each): ---
      u32  actual_latency_samples (peak since last heartbeat, reset to 0 after read)
```

DC tracks peak/average itself over a display window from the per-heartbeat u32 values.
"Late" comes from 0x8004. Inferno sends one u32 per actual flow (not the fixed 32-slot array
the Shure uses — this is fine, DC handles variable slot counts).

**Key fix**: 0x8003 alone is not enough. DC requires both 0x8003 AND 0x8004 in the same
heartbeat tick. These must be sent as separate UDP datagrams (see datagram split below).

---

## Heartbeat Type 0x8000 — Network Statistics (TX/RX Utilization)

**Status: ✅ Implemented and working**

```
0x00  u16  36                 (total block length)
0x02  u16  0x8000             (type)
0x04  u16  4                  (sub-type)
0x06  u16  4                  (content length = 4 bytes per field × 4 fields + header...)
0x08  u16  seqnum             (util_block_seqnum counter)
0x0a  u16  0
0x0c  u32  0x00100000         (fixed header field — meaning unknown)
0x10  u32  0x00010010         (fixed header field — meaning unknown)
0x14  u32  tx_bytes_per_sec   (read-and-reset AtomicU64, divided by tick interval)
0x18  u32  rx_bytes_per_sec
0x1c  u32  tx_error_count     (read-and-reset AtomicU32)
0x20  u32  rx_error_count
```

Byte counters live in `flows_tx.rs` (tx_bytes, tx_errors) and `flows_rx.rs` (rx_bytes, rx_errors)
as `Arc<AtomicU64/U32>`, threaded through `mod.rs` into `info_mcast_server`.

---

## Heartbeat Type 0x8004 — Missed Packets

**Status: ✅ Implemented and working (all zeros — no late packets in normal operation)**

Format (confirmed from Shure MXWANI8 capture):

```
0x00  u16  20 + flows_count * 4   (total block length — 4 bytes shorter than 0x8003, no sample_rate)
0x02  u16  0x8004                 (type)
0x04  u16  4                      (sub-type)
0x06  u16  8 + flows_count * 4    (content length)
0x08  u16  seqnum                 (same latency_block_seqnum as 0x8003)
0x0a  u16  0
0x0c  u16  flows_count
0x0e  u16  0
0x10  u16  20                     (data start offset = 0x14, no sample_rate field)
0x12  u16  0
--- Per-flow data (flows_count × 4 bytes each): ---
      u32  missed_packets         (packets arriving after deadline, read-and-reset)
```

**Note**: Shure sends 32 fixed slots; Inferno sends actual flow count. Both work with DC.
A packet is "missed" when measured latency > configured latency_samples threshold.

---

## Device Info — Board Info (opcode 07 2a 00 60)

Sent to 224.0.0.231:8702. Content is 200 bytes. Key offsets:

```
0x00..0x03  Firmware version (bytes, e.g. [4, 1, 0, 6])
0x04..0x07  Hardware version
0x0c..0x13  Board name (8 chars, null-padded)
0x14        Feature flags byte 1  — Shure: 0x86 (AES67, device lock + others)
0x15        Feature flags byte 2  — Shure: 0x7c
0x16        Feature flags byte 3  — Shure: 0xd4 (0x10=Manufacturer name, 0x40=static IP + others)
            Inferno sets: 0x10 (Manufacturer name only)
0x17        Feature flags byte 4  — Shure: 0xcb = 0b11001011
            Inferno sets: 0x4b = 0b01001011 (Identify + Reboot + companion bits 0x01|0x02)
            Known bits:
              0x01: companion bit — required alongside 0x40 (Reboot) for DC to activate button
              0x02: companion bit — required alongside 0x40 (Reboot) for DC to activate button
              0x08: Identify button (LED blink) — set; 0x0BC8 handler TODO (T3.10)
              0x10: ⚠️ Allow DC to CHANGE sample rate — makes rate editable, do NOT set (T3.13)
              0x40: Reboot — ✅ set and implemented (conmon 0x0090, ack 0x0092, then exit(0))
              0x80: Factory reset — intentionally NOT set; DC greys the button regardless
0x38..0x47  Board name again (16 chars)
0xbb        0x1f — critical: if 0, device is flooded with 1/s info multicast requests
```

---

## Device Info — Sample Rate Response (opcode 07 2a 00 80)

**Status: ✅ Implemented — shows as display-only (non-editable) in DC**

Sent proactively to 224.0.0.231:8702. Also sent in response to ARC query `07 3d 00 81 .. 64`
(DC sends this unicast to ARC port 8700; device responds to multicast port 8702).

**Confirmed wire format** (from dante6.pcapng — Shure MXWANI8 capture, Apr 2026):

```
byte 0-1:  0x00 0x18   ← fixed header field (NOT a length indicator)
byte 2-3:  0x00 0x01   ← type identifier for sample rate (MUST be 0x01, not 0x04)
byte 4-7:  current sample rate as u32 big-endian (48000 = 0x0000bb80)
byte 8-11: default sample rate as u32 big-endian
byte 12-13: count of supported rates as u16 big-endian
byte 14-15: 0x0000 padding
byte 16+:  u32 per supported rate
```

**What makes it read-only vs editable in DC:**
- Header bytes `0x00 0x18 0x00 0x01` → display-only ✅
- Header bytes `0x00 0x10 0x00 0x04` → editable (wrong, from our earlier attempt)
- Feature flag bit 0x10 set in byte 0x17 → editable regardless of header
- Feature flag bit 0x10 NOT set + correct header → display-only ✅

Inferno sends count=1 with only 48000 Hz listed.

---

## Device Info — Encoding Response (opcode 07 2a 00 82)

**Status: ✅ Sent (non-editable), but DC shows no value — expected**

The Shure MXWANI8 does NOT send opcode 0x82 at all. DC shows encoding as blank for the
Shure too. This appears to be a DC limitation — it does not display encoding value from
the 0x82 packet, regardless of content.

Inferno sends 0x82 with header `0x00 0x18 0x00 0x03` (non-editable format), count=1,
24-bit only. DC shows it as non-editable with no value — same as Shure. Acceptable.

---

## Two-Datagram Heartbeat Split (critical)

**Status: ✅ Implemented**

DC requires heartbeat blocks to arrive in exactly two separate UDP datagrams per second:

```
Datagram 1 → 224.0.0.233:8708:  [0x8000 utilization] + [0x8001 clock PPB]
Datagram 2 → 224.0.0.233:8708:  [0x8003 rx latency]  + [0x8004 missed packets]
```

When all blocks arrive in one combined datagram, DC silently ignores latency data.
Each datagram group uses an independent seqnum counter (`util_block_seqnum` and
`latency_block_seqnum`), matching the Shure's observed behaviour.

0x8002 (audio peaks) was removed from heartbeats — the Shure never sends it in
multicast heartbeats.

---

## Community-Sourced Conmon Opcode Reference

Opcodes sourced from `chris-ritsen/network-audio-controller` packet dissector. Not all are implemented in inferno.

| Opcode | Name | Inferno status |
|--------|------|----------------|
| `0x0077` | clear_config | Received — no-op handler (T3.9 pending) |
| `0x0081` | set_sample_rate | Not handled (T3.13 — blocked, see scaffold) |
| `0x0090` | reboot | ✅ Implemented (exit(0) + ack 0x0092) |
| `0x0092` | reboot_ack | Sent as reboot response |
| `0x01FE` | metering_data | Not handled |
| `0x0326` | set_output_gain | Not handled (T3.12 — blocked, see scaffold) |
| `0x0344` | set_input_gain | Not handled (T3.12 — blocked, see scaffold) |
| `0x03D7` | set_encoding | Not handled |
| `0x0BC8` | identify | Not handled (T3.10 — low complexity) |
| `0x1008` | heartbeat_query | ✅ Handled — logged at trace level (no response needed) |
| `0x22DC` | set_aes67 | Not handled |
| `0x40FE` | metering_data_extended | Not handled |

---

## Known Unknowns — Resolved

| Item | Resolution |
|------|-----------|
| Exact 0x8003 format | ✅ Confirmed: 1×u32 per flow, variable count |
| Whether 0x8004 is separate from 0x8003 | ✅ Confirmed: separate block, same datagram |
| What `0x10 0x00 0x01 0x00 0x10` means in 0x8000 | Unknown but harmless — fixed values work |
| What DC sends for opcode `07 2a 10 07` / `07 2a 10 09` | Still unknown; Shure sends them, DC doesn't query us for these |
| Feature flag bits at 0x14..0x17 | ✅ Confirmed from dante6.pcapng capture |
| Whether 0x10 bit in 0x17 enables sample rate UI | ✅ Confirmed: it makes rate *editable*, not just visible |
| 0x80 header bytes for read-only display | ✅ Confirmed: `00 18 00 01` from Shure capture |
| Whether Shure sends 0x82 encoding | ✅ Confirmed: it does NOT — DC shows encoding blank by design |
