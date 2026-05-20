# Cross-Platform AirPlay 2 Transmitter — Research Brief

> A technical handoff document summarizing the AirPlay 2 protocol stack, reference open-source projects, and an implementation plan for a cross-platform sender (transmitter).

---

## 1. Project Goal

Build a cross-platform (Linux, macOS, Windows) **AirPlay 2 Transmitter** that can:

- Discover AirPlay 2 receivers on the local network
- Pair with HomePods, Apple TVs, and third-party AirPlay 2 speakers
- Stream encrypted audio (and optionally mirrored video) using the modern AirPlay 2 protocol stack
- Support both single-room and multi-room (PTP-synchronized) playback

---

## 2. Reference Open-Source Projects

| Project | Language | Role | What to learn from it |
|---|---|---|---|
| [omarroth/doubletake](https://github.com/omarroth/doubletake) | Go | Linux screen-mirroring sender | Full AirPlay 2 mirroring flow, FairPlay SAP via snapshot execution, ChaCha20-Poly1305 framing, GStreamer-based H.264 capture |
| [lmcgartland/airplay2-rs](https://github.com/lmcgartland/airplay2-rs) | Rust | Cross-platform audio sender | Clean crate layout, PTP/NTP, HomeKit pairing, real-time threading, [`AIRPLAY_2_SPEC.md`](https://github.com/lmcgartland/airplay2-rs/blob/master/AIRPLAY_2_SPEC.md) (best implementer's reference available) |
| [owntone/owntone-server](https://github.com/owntone/owntone-server) | C | Production AirPlay 2 audio output | `src/outputs/airplay.c` is the canonical battle-tested sender code |
| [ejurgensen/pair_ap](https://github.com/ejurgensen/pair_ap) | C | HomeKit pairing library | SRP-6a 3072-bit, Ed25519, Curve25519, ChaCha20-Poly1305 — the de-facto reference |
| [mikebrady/shairport-sync](https://github.com/mikebrady/shairport-sync) | C | AirPlay 2 receiver | Receiver perspective; [`AIRPLAY2.md`](https://github.com/mikebrady/shairport-sync/blob/master/AIRPLAY2.md) describes supported formats |
| [mikebrady/nqptp](https://github.com/mikebrady/nqptp) | C | "Not Quite PTP" daemon | PTP master/slave implementation pattern |
| [openairplay/airplay2-receiver](https://github.com/openairplay/airplay2-receiver) | Python | Reverse-engineering prototype | Easy to read; great for protocol experimentation |
| [openairplay/ap2-sender](https://github.com/openairplay/ap2-sender) | Python | Sender prototype | Early-stage but useful for cross-checking |
| [SteeBono/airplayreceiver](https://github.com/SteeBono/airplayreceiver) | C# | AirPlay 2 receiver | Shows how to wrap ALAC/AAC C libraries from .NET |

### Documentation references

- **Classic AirPlay (pre-AirPlay 2):** [nto.github.io/AirPlay.html](https://nto.github.io/AirPlay.html) — still the baseline for RTSP/RTP framing, sync packets, retransmit packets, photo/video flows
- **AirPlay 2 internals (reverse-engineering writeups):** https://emanuelecozzi.net/docs/airplay2/ (discovery, features, RTSP, RTP)
- **OpenAirplay community spec:** https://openairplay.github.io/airplay-spec/ (pairing/HKP, /auth-setup, status flags)
- **Best single document:** `lmcgartland/airplay2-rs/AIRPLAY_2_SPEC.md` — read this first

---

## 3. Protocol Stack Overview

```
┌──────────────────────────────────────────────────────────┐
│  Application / Audio source (mic, file, system capture)  │
├──────────────────────────────────────────────────────────┤
│  Codec layer:  ALAC (realtime) / AAC-LC (buffered)       │
├──────────────────────────────────────────────────────────┤
│  RTP transport (PT=96 realtime, PT=103 buffered)         │
│  AEAD: ChaCha20-Poly1305 per packet (seq-based nonce)    │
├──────────────────────────────────────────────────────────┤
│  RTSP-like control (binary plist bodies, encrypted)      │
│  ChaCha20-Poly1305 framing on TCP after pairing          │
├──────────────────────────────────────────────────────────┤
│  HomeKit pairing (TLV8: SRP-6a, Ed25519, Curve25519)     │
│  + Optional: /auth-setup (MFi), /fp-setup (FairPlay)     │
├──────────────────────────────────────────────────────────┤
│  Timing: NTP-style (single-room) OR PTP/IEEE 1588        │
│  (multi-room, UDP 319/320, privileged)                   │
├──────────────────────────────────────────────────────────┤
│  mDNS / DNS-SD discovery (_airplay._tcp, _raop._tcp)     │
└──────────────────────────────────────────────────────────┘
```

---

## 4. Discovery (mDNS / Bonjour)

### Service types to browse

- `_airplay._tcp.local.` — primary AirPlay control endpoint
- `_raop._tcp.local.` — legacy/RAOP; consult for codec hints + fallback
- `_airplay-p2p._tcp.local.` — peer-to-peer (AWDL), out of scope

### Critical TXT keys on `_airplay._tcp`

| Key | Meaning |
|---|---|
| `deviceid` | MAC-like identifier |
| `model` | Model string (e.g., `AudioAccessory1,1` → HomePod-class) |
| `srcvers` | Firmware version — used as gate for some features |
| `features` | 64-bit bitmask encoded as `0xLOWER,0xUPPER` |
| `flags` / `sf` | Device state bitmask (PIN required, password required, etc.) |
| `pk` | Device public key (pairing) |
| `pi`, `psi` | Pairing/system identities |
| `gid`, `igl`, `gcgl`, `pgid` | Multi-room group membership |
| `hkid`, `hgid`, `hmid` | HomeKit home / group / household UUIDs |
| `acl` | Access control level |

### Feature bits that gate behavior

| Bit | Capability | Version gate |
|---|---|---|
| 9 | SupportsAirPlayAudio | required |
| 26 | HasUnifiedAdvertiserInfo | triggers MFi auth |
| 30 | RAOP (no separate AirTunes needed) | — |
| 40 | SupportsBufferedAudio | `srcvers >= 354.54.6` |
| 41 | SupportsPTP | `srcvers >= 366` |
| 43 | SupportsSystemPairing (implies 48) | — |
| 46 | SupportsHKPairingAndAccessControl | — |
| 48 | SupportsTransientPairing | — |
| 51 | SupportsUnifiedPairSetupAndMFi | — |

### Decision rules

- If `model` starts with `AudioAccessory` → HomePod-class
- If bit 40 → prefer PT=103 (buffered AAC)
- Else → PT=96 (realtime ALAC)
- If bit 41 (and targeting multi-room) → must implement PTP
- If bit 43 || 48 → use Transient pairing (X-Apple-HKP: 4)
- Else → use Normal pairing (X-Apple-HKP: 3)
- If bit 26 → likely needs `/auth-setup` before SETUP

### Recommended libraries (pure-userland, no Avahi/Bonjour)

- **Rust:** `mdns-sd`
- **Go:** `hashicorp/mdns` or `grandcat/zeroconf`
- **Python:** `zeroconf`
- **C/C++:** `mDNSResponder` (Apple open source), or `tinysvcmdns`

---

## 5. Pairing (HomeKit-Based)

### Two flavors, signaled by the `X-Apple-HKP` HTTP header

#### `X-Apple-HKP: 4` — Transient (HomePod, AirPort Express, most third-party)

1. `POST /pair-setup` (M1) with `kTLVType_Flags = 0x00` (Transient)
2. SRP-6a M1–M4 with username `"Pair-Setup"`, PIN `"3939"` (hardcoded)
3. Derive control-channel keys directly from SRP shared secret via HKDF-SHA-512
4. **Skip M5/M6 and pair-verify entirely**
5. Switch to encrypted RTSP

#### `X-Apple-HKP: 3` — Normal (Apple TV, persistent)

1. `POST /pair-pin-start` → triggers PIN display on the receiver
2. User enters PIN
3. Full SRP M1–M6, with M5/M6 swapping Ed25519 long-term identity keys
4. Persist controller identity to disk (keyed by device id)
5. **Future sessions:** `/pair-verify` (M1–M4) using Curve25519 ECDH with saved identity

### SRP-6a parameters (HomeKit-specific)

| Parameter | Value |
|---|---|
| Group | **3072-bit** (RFC 5054 Appendix A) — *not* 2048-bit |
| Hash | SHA-512 |
| Salt | 16 bytes |
| Proof | 64 bytes |
| Session key | 64 bytes |
| Username | literal `"Pair-Setup"` |

### TLV8 type codes (subset)

| Type | Name |
|---|---|
| 0x00 | Method |
| 0x01 | Identifier |
| 0x02 | Salt |
| 0x03 | PublicKey |
| 0x04 | Proof |
| 0x05 | EncryptedData |
| 0x06 | State (M1=0x01 .. M6=0x06) |
| 0x07 | Error |
| 0x0B | Signature |
| 0x10 | Flags (0x00 = Transient) |

### HKDF-SHA-512 derivations

| Purpose | Salt | Info | Output |
|---|---|---|---|
| Control write (client→server) | `"Control-Salt"` | `"Control-Write-Encryption-Key"` | 32 bytes |
| Control read (server→client) | `"Control-Salt"` | `"Control-Read-Encryption-Key"` | 32 bytes |
| Pair-Setup M5/M6 encryption | `"Pair-Setup-Encrypt-Salt"` | `"Pair-Setup-Encrypt-Info"` | 32 bytes |
| Pair-Verify encryption | `"Pair-Verify-Encrypt-Salt"` | `"Pair-Verify-Encrypt-Info"` | 32 bytes |

---

## 6. Encrypted Control Channel

After pairing, every RTSP request and response is wrapped:

```
uint16_le length N || ciphertext (N bytes) || Poly1305 tag (16 bytes)
```

- Algorithm: **ChaCha20-Poly1305**
- Each direction uses its own key + monotonically increasing counter
- Nonce (12 bytes): `[0x00, 0x00, 0x00, 0x00]` + counter (8 bytes, little-endian, starts at 0)
- **Independent counters per direction** — never reuse a nonce under the same key

---

## 7. RTSP Control Flow

```
GET /info                  →  capabilities
POST /pair-setup           →  TLV8 SRP handshake
POST /pair-verify          →  (normal pairing only)
POST /auth-setup           →  (only if bit 26 / Sonos / new AirPort Express)
POST /fp-setup             →  (mirroring path; FairPlay-specific receivers)
SETUP   (phase 1)          →  binary plist: timing + events + crypto
SETUP   (phase 2)          →  binary plist: stream definitions
RECORD                     →  begin streaming
SET_PARAMETER              →  volume, metadata, artwork (DAAP-tagged)
POST /feedback             →  keepalive (periodic)
FLUSH                      →  pause / discontinuity
TEARDOWN                   →  end session
```

### Required headers

- `CSeq: <monotonic int>`
- `User-Agent: AirPlay/<version>`
- `X-Apple-ProtocolVersion: 1`
- `X-Apple-Device-ID`, `X-Apple-Session-ID` (some receivers strict)
- `DACP-ID`, `Active-Remote` (for remote-control integration)
- `X-Apple-HKP: 3` or `4` (on pairing requests)
- `X-Apple-ET: 32` (on fp-setup)

---

## 8. SETUP — Two-Phase Binary Plist Negotiation

Content-Type: `application/x-apple-binary-plist`

### Phase 1 request (timing + events)

```
{
  deviceID: "AA:BB:CC:DD:EE:FF",
  sessionUUID: "<UUID>",
  timingPort: <sender's NTP port>,
  timingProtocol: "PTP" or "NTP",
  eiv: <16 bytes>,       // if AES
  ekey: <16 bytes>,      // if AES
  et: 32                 // 32 = ChaCha20-Poly1305
}
```

Response:
```
{ eventPort: <int>, timingPort: <int> }
```

### Phase 2 request (audio streams)

```
{
  streams: [
    {
      type: 96 or 103,           // 96=realtime, 103=buffered
      ct: 2 or 3,                 // 2=ALAC, 3=AAC, 4=AAC-ELD, 32=Opus
      audioFormat: 0x40000        // ALAC/44100/16/2  (bitmask)
                  | 0x400000      // AAC-LC/44100/2
                  | ...
      audioMode: "default",
      sr: 44100,                  // sample rate
      spf: 352,                   // samples per frame (352 ALAC, 1024 AAC)
      latencyMin: 11025,          // ~250ms @ 44.1kHz
      latencyMax: 88200,          // ~2s @ 44.1kHz
      shk: <32 bytes>,            // RTP encryption key
      controlPort: <sender's RTCP port>,
      isMedia: true,
      streamConnectionID: <uint>,
      supportsDynamicStreamID: true
    }
  ]
}
```

Response:
```
{
  streams: [
    {
      dataPort: <receiver's RTP audio port>,
      controlPort: <receiver's RTCP port>,
      eventPort: <int>,
      timingPort: <int>
    }
  ]
}
```

**Always use negotiated ports** — never hardcode 7000 or anything else.

---

## 9. RTP Audio Transport

### Packet layout (on the wire)

```
┌─────────────────┬─────────────────────────┬──────────────┬──────────────┐
│ RTP Header (12) │  Encrypted Payload (N)  │  Tag (16)    │  Nonce (8)   │
└─────────────────┴─────────────────────────┴──────────────┴──────────────┘
                                            ↑              ↑
                                       offset L-24    offset L-8
```

### Encryption

- **Algorithm:** ChaCha20-Poly1305 AEAD
- **Key:** `shk` from SETUP phase 2 (32 bytes)
- **Internal nonce (12 bytes):**
  ```
  nonce[0..3]  = 0x00000000
  nonce[4..5]  = RTP seq number (u16, LITTLE-endian)
  nonce[6..11] = 0x000000000000
  ```
- **Transmitted nonce:** only `nonce[4..11]` (8 bytes) is appended to the packet
- **AAD (8 bytes):** RTP timestamp (4 bytes BE) ∥ SSRC (4 bytes BE)
- **Tag:** 16-byte Poly1305 MAC

### RTP payload types

| PT | Purpose |
|---|---|
| 82 | Timing request (NTP-style, timing port) |
| 83 | Timing reply (timing port) |
| 84 | Sync packets (control port, 1/sec, correlates RTP↔NTP) |
| 85 | Retransmit request (control port) |
| 86 | Retransmit reply (control port) |
| 87 | PTP sync packet (28 bytes, PTP time + master clock ID) |
| 96 | Realtime audio (ALAC) |
| 103 | Buffered audio (AAC) |

### Critical gotchas

- **Set the RTP marker bit on the FIRST packet of a new session.** Some receivers refuse to play without it.
- **Use sequence-based nonces** so retransmissions produce identical ciphertext.
- **Listen for retransmit requests (PT=85)** and respond within ~5ms with PT=86 replies.
- Default Apple stack uses ~2s latency for realtime, ~500ms for buffered. Match these.

---

## 10. Timing Synchronization

### Single-room: NTP-style on negotiated port

- Receiver sends timing requests (PT=82) to sender's `timingPort`
- Sender records receive time `t2`, then transmit time `t3`, responds with PT=83
- Receiver computes offset using its `t1` (send) and `t4` (receive)
- **NTP epoch:** Jan 1, 1900 — NOT Unix epoch
  - `ntp_seconds = unix_seconds + 0x83AA7E80`

### Multi-room: PTP (IEEE 1588)

- **UDP ports 319 (event) and 320 (general)** — privileged, needs sudo / `CAP_NET_BIND_SERVICE`
- BMCA yield flow:
  1. Sender starts as master, Priority1 = 250
  2. Sends 3 Syncs + 2 Announces
  3. Yields to receiver (Priority1 = 248)
  4. Continues as PTP slave, syncing to receiver's clock
- Uses PT=87 sync packets in the audio stream
- HomePod **requires** PTP — won't accept audio without it

### Privilege strategy (cross-platform)

- **Linux:** `setcap CAP_NET_BIND_SERVICE+ep` on a small PTP helper binary
- **Windows:** UAC elevation, or run as service
- **macOS:** must use sudo (macOS already binds these ports — this is why shairport-sync can't run AP2 mode on Macs)

---

## 11. Audio Codecs

### What receivers actually accept (per shairport-sync)

| Format | Latency | Lossless | Surround |
|---|---|---|---|
| ALAC/S16/44100/2 (realtime, PT=96) | ~2s | Yes | No |
| AAC/F24/44100/2 (buffered, PT=103) | ~500ms | No | No |
| AAC/F24/48000/2 (buffered) | ~500ms | No | No |
| ALAC/S24/48000/2 (buffered) | ~500ms | Yes | No |
| AAC/F24/48000/5.1 and 7.1 (buffered) | ~500ms | No | Yes |

### Encoder libraries

- **ALAC:** Apple's open-source reference (Apache 2.0), or pure-Rust `alac-encoder`
- **AAC:** **FDK-AAC** (industry standard for buffered AirPlay 2). Vendor the C source and compile via `cc`.
- **Audio decoding for input (file playback):** `symphonia` (Rust) or `ffmpeg-next` covers MP3/FLAC/WAV/AAC/ALAC/Ogg

---

## 12. Recommended Cross-Platform Architecture

```
your-airplay2-sender/
├── crates/  (or pkg/ for Go, modules for C++)
│   ├── core/         # types, errors, traits
│   ├── discovery/    # mDNS / DNS-SD — pure userland
│   ├── crypto/       # SRP-6a 3072, Ed25519, Curve25519, ChaCha20-Poly1305, HKDF-SHA512
│   ├── pairing/      # HomeKit pair-setup/verify (Transient + Normal)
│   ├── rtsp/         # encrypted RTSP framing + binary plist
│   ├── audio-codec/  # ALAC + FDK-AAC encoding
│   ├── audio-rtp/    # RTP packetization, AEAD wrapping, retransmit handling
│   ├── timing/       # NTP-style + PTP (IEEE 1588) with BMCA yield
│   ├── capture/      # platform-specific audio capture (cpal, etc.)
│   ├── ptp-helper/   # separate privileged binary, talks to main over IPC
│   └── client/       # high-level API + multi-room group streaming
├── apps/
│   ├── cli/          # command-line sender
│   └── tui/          # optional terminal UI (ratatui)
└── docs/
```

### Language choice rationale

- **Rust** — best fit. Pure-language crypto, async networking, real-time safety, easy cross-compile. airplay2-rs already proves this works.
- **Go** — viable. doubletake demonstrates feasibility. Easier learning curve, slightly worse real-time guarantees.
- **C/C++** — most production-tested (OwnTone), but most maintenance burden.

### Cross-platform shims needed

| Concern | Linux | macOS | Windows |
|---|---|---|---|
| Audio capture | PipeWire / ALSA | CoreAudio / AudioUnit | WASAPI |
| Real-time priority | `SCHED_FIFO` + `setcap` | `pthread_setschedparam` | `AvSetMmThreadCharacteristics` |
| Privileged ports (PTP) | `CAP_NET_BIND_SERVICE` | sudo (and conflict with macOS) | run as Administrator |
| mDNS | Pure-userland library; do NOT depend on Avahi | Same; ignore mDNSResponder | Same |
| Screen capture (mirroring) | xdg-desktop-portal + PipeWire | CGDisplayStream / ScreenCaptureKit | Desktop Duplication API |
| H.264 encoding (mirroring) | VA-API / NVENC / x264 | VideoToolbox | Media Foundation / NVENC |

---

## 13. Real-Time Audio Practices (from airplay2-rs)

These are not optional polish — they're what separates "usable" from "drops audio every few seconds":

1. **Dedicated sender thread** with `clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME)` + `SCHED_FIFO` real-time priority → ~5µs jitter vs ~1.5ms with async sleep
2. **Retransmit handling within 5ms** — parse Apple's 8-byte compact retransmit format (PT=85) and respond promptly
3. **DSCP EF marking** on audio UDP sockets for WiFi WMM Voice priority
4. **CPU governor = performance** on the host (Linux), **disable WiFi power management**, **reduce vm.swappiness**
5. **Packet bursting** on shared WiFi/Bluetooth radios (e.g., Pi Zero 2 W): send 4 packets rapidly then wait ~32ms, instead of one packet every 8ms

---

## 14. Device-Specific Quirks (collect these in a table)

| Device | Quirk | Workaround |
|---|---|---|
| Sonos Beam | Rejects SETUP without `/auth-setup` | Send minimal auth-setup payload first |
| AirPort Express (firmware ≥ 7.8) | Same | Same |
| Apple TV 4 | Returns `timingPort=0` in SETUP response | Ignore; use sender's configured port |
| Sonos volume | Volume `SET_PARAMETER` must be **last** in a sequence | Reorder updates |
| HomePod | Requires PTP, ignores NTP-only | Implement PTP, no exceptions |
| HomePod | Blocked by Home app "Speakers & TV Access" policy | Surface as diagnostic, not protocol failure |

---

## 15. FairPlay / Screen Mirroring (Optional)

The audio-only sender does **not** need FairPlay. Skip FairPlay if you only care about audio.

For screen mirroring:

- Separate HTTP server on port 7100 (per nto's spec — may have evolved in AP2)
- `POST /stream` with binary plist containing `param1` (FairPlay-wrapped AES key) and `param2` (IV)
- H.264 video over TCP with 128-byte custom packet headers
  - Type 0: video bitstream
  - Type 1: codec data (H.264 SPS/PPS in avcC format)
  - Type 2: heartbeat (1/sec)
- AAC-ELD audio over the regular AirTunes RTP channel
- NTP timing on UDP 7010/7011
- AirPlay 2 mirroring wraps the video stream with ChaCha20-Poly1305 (was AES-CTR in AP1)

### FairPlay client logic

This is Apple's proprietary handshake. There is no clean re-implementation. Practical paths:

- **doubletake approach:** ship a snapshot/VM of compiled Go ARM64 that executes the FairPlay logic in a sandbox
- **Historical "OmgHax" approach:** reverse-engineered C code that does the handshake (legally murky to redistribute)
- **Skip mirroring entirely** for v1

---

## 16. Recommended Build Order

1. **mDNS discovery + TXT parsing** — list devices on the network with their feature bits, decide which to target
2. **HomeKit pairing — Transient mode first** (works with HomePod immediately, no PIN UI needed)
3. **Encrypted RTSP framing** — get to a working `GET /info` over the encrypted channel
4. **NTP-style timing + realtime ALAC (PT=96)** — get audio playing to an AirPort Express or pre-HomePod device. This is the easiest receiver target.
5. **Buffered AAC (PT=103)** — switch to the modern path
6. **PTP timing** — get HomePod working. Test with the BMCA yield flow before debugging anything else.
7. **Normal pairing (Apple TV with PIN)** — adds persistent identity
8. **Multi-room group streaming** — single sender, multiple receivers, all PTP-synchronized
9. **Real-time scheduling, DSCP, retransmit handling** — production hardening
10. *(Optional)* Screen mirroring + FairPlay

---

## 17. Test Vectors

All standard, available in public RFCs:

- SRP-6a: RFC 5054 Appendix B
- HKDF: RFC 5869 Appendix A
- Curve25519: RFC 7748 §6.1
- Ed25519: RFC 8032 §7.1
- ChaCha20-Poly1305: RFC 8439 Appendix A

Build unit tests against these vectors before testing against real devices.

---

## 18. Quick "Known Device" Feature Signatures

For your test matrix:

| Device | Model | Features hex |
|---|---|---|
| Apple TV 4K | `AppleTV5,3` | `0x5A7FDFD5,0x3C155FDE` |
| HomePod | `AudioAccessory1,1` | `0x4A7FCA00,0x3C356BD0` |
| AirPort Express 2 | `AirPort10,115` | `0x445D0A00,0x1C340` |
| Sonos Symfonisk | — | `0x445F8A00,0x1C340` |
| Roku (3810X) | — | `0x7F8AD0,0x10BCF46` |
| Samsung TV (UNU7090) | — | `0x7F8AD0,0x38BCB46` |

A minimal multi-room signature is `0x40000a00,0x80300` (bits 9, 11, 30, 40, 41, 51).

---

## 19. Out-of-Scope for v1

- AirPlay video v1/v2 playback (separate protocol family)
- AirPlay Mirroring (FairPlay required)
- AirPlay peer-to-peer (`_airplay-p2p`, AWDL)
- iTunes-style remote control (DACP server)
- Photo sharing
- Windows iTunes interop (shairport-sync explicitly notes this doesn't work even on the receiver side)

---

## 20. Starting Point

When kicking this off in Claude Code, the natural first commands are:

```bash
# scaffold a Rust workspace (recommended)
cargo new --lib airplay2-sender
cd airplay2-sender
# add the per-crate split from §12 above

# OR scaffold a Go module
go mod init github.com/<you>/airplay2-sender
```

Then implement §16 step 1 (mDNS discovery + feature-bit decoding) as the first vertical slice. Use real devices on your LAN for verification — `dns-sd -B _airplay._tcp .` on macOS or `avahi-browse -r _airplay._tcp` on Linux will show you what you're up against.
