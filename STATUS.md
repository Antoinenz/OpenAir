# OpenAir — Implementation Status

> Updated by Claude at the end of each session. Reflects what is actually working, not just written.

## Implementation Phases

| # | Phase | Status | Notes |
|---|-------|--------|-------|
| 1 | mDNS discovery + TXT feature-bit parsing | ✅ Done | 4 unit tests pass; verified on LAN (Shairport Sync + Apple TV 4K) |
| 2 | HomeKit Transient pairing (SRP-6a, PIN "3939") | ✅ Done | Hardware-verified vs Shairport Sync 2026-07-07 (after N typo + Flags TLV fixes) |
| 3 | Encrypted RTSP (`GET /info` over ChaCha20-Poly1305) | ✅ Done | Hardware-verified 2026-07-07: encrypted GET /info → 701 bytes (580 B plist) |
| 4 | Timing + realtime ALAC PT=96 | ✅ Done | Hardware-verified 2026-07-08: 440 Hz tone audible on Pool Room; PTP (pulled fwd from Step 6), collinear type-215 anchors |
| 5 | Buffered AAC PT=103 | ✅ Done | Hardware-verified 2026-07-14: FDK-AAC over TCP, full SETRATEANCHORTIME anchor, --buffered flag |
| 6 | PTP timing (HomePod, BMCA yield) | 🔄 Partial | Minimal PTP master done (Announce+Sync/Follow_Up, nqptp-verified); BMCA yield + Delay_Req + ptp-helper (Linux) remain |
| 7 | Normal pairing (Apple TV + PIN, persist identity) | ⬜ Not Started | |
| 8 | Multi-room group streaming | ⬜ Not Started | |
| 9 | Real-time hardening (SCHED_FIFO, DSCP EF, retransmit <5ms) | ⬜ Not Started | |

**Legend:** ✅ Done · 🔄 In Progress · ⚠️ Partial / Known Issues · ⬜ Not Started

---

## Per-Crate Status

| Crate | Status | Tested Against Hardware | Notes |
|-------|--------|------------------------|-------|
| `core` | ✅ Scaffolded | — | `Features` bitmask, `AudioMode`, `OpenAirError` |
| `discovery` | ✅ Done | Yes | `browse()`, `AirPlayDevice`, `AirPlayTxt`, feature-bit decoder; 4 tests pass; verified on LAN |
| `crypto` | ✅ Done | Yes | SRP-6a 3072-bit (N fingerprint-guarded), HKDF-SHA-512, ChaCha20-Poly1305 with AAD; 11 tests |
| `pairing` | ✅ Done | Yes | TLV8 (Flags=0x13), `TransientPairing` M1–M4 incl. M2-proof verify; 7 tests |
| `rtsp` | ✅ Done | Yes | `pair_and_get_info` verified vs Shairport Sync (AirTunes/366.0) |
| `audio-codec` | ✅ Done | Yes | Verbatim ALAC + FDK-AAC (CBR 256k) both play on hardware |
| `audio-rtp` | ✅ Done | Yes | RTP+AEAD packetizer, PTP anchor packets (0xD7), NTP sync (0xD4), retransmit backlog |
| `timing` | ✅ Done | Yes | NTP responder + minimal PTP master (nqptp-verified) |
| `capture` | ✅ Done (Win) | Yes | WASAPI loopback verified with live Spotify; PipeWire/CoreAudio later |
| `ptp-helper` | ⬜ Stub | — | Privileged binary, IPC to main |
| `client` | ✅ Done (v1) | Yes | stream_audio (realtime) + stream_audio_buffered (AAC, tunable latency); AudioSource: sine/WAV/live capture |
| `apps/cli` | ✅ Done (v1) | Yes | scan, pair, tone/play/capture; name resolution, --volume, --buffered, --latency <ms>, Ctrl+C |
| `apps/tui` | ⬜ Stub | — | |

---

## Known Issues / Blockers

- **Apple TV 4K returns 470 at M1 even with AirPlay access = "Everyone"** (tested 2026-07-08).
  tvOS refuses *Transient* pairing from unknown senders — it requires Normal HomeKit pairing
  (on-screen PIN + persisted Ed25519 identity + pair-verify) = **Step 7**. Not a code bug.

---

## Next Steps

1. **Step 7** — Normal HomeKit pairing (M1–M6, pair-verify, persist Ed25519 identity) → Apple TV
2. Step 8 multi-room; Step 9 hardening (adaptive resample, retransmit tuning, DSCP)

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
| Living Room | `AppleTV5,3` | — | ✅ 192.168.1.x:7000 | Apple TV 4K (1st gen); PTP + AAC + Transient |
