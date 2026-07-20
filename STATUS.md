# OpenAir — Implementation Status

> Updated by Claude at the end of each session. Reflects what is actually working, not just written.

## Implementation Phases

| # | Phase | Status | Notes |
|---|-------|--------|-------|
| 1 | mDNS discovery + TXT feature-bit parsing | ✅ Done | 4 unit tests pass; verified on LAN (Shairport Sync + Apple TV 4K) |
| 2 | HomeKit Transient pairing (SRP-6a, PIN "3939") | ✅ Done | Hardware-verified vs Shairport Sync 2026-07-07 (after N typo + Flags TLV fixes) |
| 3 | Encrypted RTSP (`GET /info` over ChaCha20-Poly1305) | ✅ Done | Hardware-verified 2026-07-07: encrypted GET /info → 701 bytes (580 B plist) |
| 4 | Timing + realtime ALAC PT=96 | ✅ Done | Hardware-verified 2026-07-08 (Shairport) + 2026-07-19 (Apple TV); PTP, collinear type-215 anchors |
| 5 | Buffered AAC PT=103 | ✅ Done | Hardware-verified 2026-07-14 (Shairport) + 2026-07-19 (Apple TV); FDK-AAC over TCP, --buffered/--latency |
| 6 | PTP timing (HomePod, BMCA yield) | 🔄 Mostly done | Master (Announce+Sync/Follow_Up) ✅, Delay_Resp ✅, **BMCA yield + foreign-timeline anchoring ✅ (ATV-verified)**; ptp-helper (Linux privileged ports) remains |
| 7 | Normal pairing (Apple TV + PIN, persist identity) | ✅ Done | Hardware-verified 2026-07-19 on AppleTV5,3 + AppleTV6,2: pair-setup M1–M6 w/ PIN, pair-verify, %APPDATA% persistence, `openair pair` |
| 8 | Multi-room group streaming | ✅ Done | Hardware-verified 2026-07-20: Shairport + Apple TV synchronized group (buffered); per-receiver timelines anchored at one shared instant; receiver-drop resilience tested live |
| 9 | Real-time hardening (SCHED_FIFO, DSCP EF, retransmit <5ms) | ⬜ Not Started | |

**Legend:** ✅ Done · 🔄 In Progress · ⚠️ Partial / Known Issues · ⬜ Not Started

---

## Per-Crate Status

| Crate | Status | Tested Against Hardware | Notes |
|-------|--------|------------------------|-------|
| `core` | ✅ Scaffolded | — | `Features` bitmask, `AudioMode`, `OpenAirError` |
| `discovery` | ✅ Done | Yes | `browse()`, `AirPlayDevice`, `AirPlayTxt`, feature-bit decoder; 4 tests pass; verified on LAN |
| `crypto` | ✅ Done | Yes | SRP-6a 3072-bit (N fingerprint-guarded), HKDF-SHA-512, ChaCha20-Poly1305 (channel + labeled one-shot); 12 tests |
| `pairing` | ✅ Done | Yes | TLV8, `TransientPairing` M1–M4, `NormalPairing` M1–M6 + `PairVerify` (Ed25519/X25519); 12 tests |
| `rtsp` | ✅ Done | Yes | Transient + Normal pair flows, SETUP×2, SETPEERS, RECORD, full SETRATEANCHORTIME, SET_PARAMETER, TEARDOWN |
| `audio-codec` | ✅ Done | Yes | Verbatim ALAC + FDK-AAC (CBR 256k) both play on hardware |
| `audio-rtp` | ✅ Done | Yes | RTP+AEAD packetizer, PTP anchor packets (0xD7) with timeline translation, NTP sync (0xD4), retransmit backlog |
| `timing` | ✅ Done | Yes | NTP responder + PTP master with BMCA yield: tracks foreign grandmaster (offset EWMA), answers Delay_Req |
| `capture` | ✅ Done (Win) | Yes | WASAPI loopback verified with live Spotify; PipeWire/CoreAudio later |
| `ptp-helper` | ⬜ Stub | — | Privileged binary, IPC to main (Linux ports 319/320; not needed on Windows) |
| `client` | ✅ Done (v1) | Yes | realtime + buffered pipelines, pairing store + auto-dispatch (pair-verify vs transient), event channel |
| `apps/cli` | ✅ Done (v1) | Yes | scan, `pair` (PIN), tone/play/capture; name resolution, --volume, --buffered, --latency <ms>, Ctrl+C |
| `apps/tui` | ⬜ Stub | — | |

---

## Receiver Compatibility (hardware-verified)

| Receiver | Pairing | Realtime ALAC | Buffered AAC | Notes |
|----------|---------|---------------|--------------|-------|
| Shairport Sync 4.x | Transient | ✅ | ✅ | We are PTP master (nqptp follows us) |
| Apple TV (AppleTV5,3 + 6,2) | Normal (PIN, one-time) | ✅ | ✅ | Needs SETPEERS + event channel + full anchor + BMCA yield (we follow ITS clock) — see DEVLOG Session 8 |
| HomePod | — | — | — | Untested; expected same path as Apple TV |

---

## Known Issues / Blockers

- Timeline offset to a foreign grandmaster is captured once at session start; sender/receiver
  crystal drift (~ppm) accumulates over very long sessions (hours). Fine for typical use.
- Bare `openair` scan mode still tries Transient against everything (does not consult the
  pairing store) — cosmetic; `tone`/`play`/`capture` dispatch correctly.

---

## Next Steps

1. **Step 9** — hardening (adaptive resample, retransmit tuning, DSCP EF)
2. Linux capture (PipeWire) + ptp-helper for privileged ports
3. Per-receiver latency offset (`--offset pool=+80ms`) for downstream amp/DSP delay
4. HomePod hardware test when available; realtime-ALAC multi-room (buffered-only today)

---

## Reference Tooling (`tools/`)

| Script | Purpose |
|--------|---------|
| `hap_probe.py` | Known-good transient pairing + encrypted GET /info client (pyatv math, raw RTSP) |
| `hap_oracle_server.py` | Local pair-setup M1–M4 server (srptools) for offline differential tests |
| `mitm_proxy.py` | TCP proxy hex-dumping both directions (wire-level diffing) |
| `pyatv_probe.py` | Drive pyatv end-to-end with debug logs (needs SelectorEventLoop on Windows) |

---

## Test Devices

| Device | Model string | Features hex | Reachable | Notes |
|--------|-------------|--------------|-----------|-------|
| Pool Room (Shairport Sync) | `Shairport Sync` | — | ✅ 192.168.1.106:7000 | Software receiver on LAN; PTP + AAC + Transient |
| Living Room | `AppleTV5,3` | — | ✅ 192.168.1.64:7000 | Apple TV HD; AirTunes/670.5.1; Normal pairing ✅ |
| test | `AppleTV6,2` | — | ✅ 192.168.1.152:7000 | Apple TV 4K; AirTunes/870.14.1; full streaming ✅ |
