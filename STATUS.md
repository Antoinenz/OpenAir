# OpenAir — Implementation Status

> Updated by Claude at the end of each session. Reflects what is actually working, not just written.

## Implementation Phases

| # | Phase | Status | Notes |
|---|-------|--------|-------|
| 1 | mDNS discovery + TXT feature-bit parsing | ✅ Done | 4 unit tests pass; verified on LAN (Shairport Sync + Apple TV 4K) |
| 2 | HomeKit Transient pairing (SRP-6a, PIN "3939") | 🔄 In Progress | |
| 3 | Encrypted RTSP (`GET /info` over ChaCha20-Poly1305) | ⬜ Not Started | |
| 4 | NTP timing + realtime ALAC PT=96 | ⬜ Not Started | Target: AirPort Express first |
| 5 | Buffered AAC PT=103 | ⬜ Not Started | |
| 6 | PTP timing (HomePod, BMCA yield) | ⬜ Not Started | Requires ptp-helper binary |
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
| `crypto` | ✅ Done | — | SRP-6a 3072-bit, HKDF-SHA-512, ChaCha20-Poly1305 framing; 10 tests pass |
| `pairing` | 🔄 In Progress | — | Transient pair-setup M1–M4 |
| `rtsp` | ⬜ Stub | — | |
| `audio-codec` | ⬜ Stub | — | Will vendor FDK-AAC + ALAC C sources |
| `audio-rtp` | ⬜ Stub | — | |
| `timing` | ⬜ Stub | — | |
| `capture` | ⬜ Stub | — | WASAPI / PipeWire / CoreAudio via cpal |
| `ptp-helper` | ⬜ Stub | — | Privileged binary, IPC to main |
| `client` | ⬜ Stub | — | |
| `apps/cli` | ⬜ Stub | — | Boots, logs, exits |
| `apps/tui` | ⬜ Stub | — | |

---

## Known Issues / Blockers

_None yet._

---

## Next Steps

1. **Step 2** — HomeKit Transient pairing: SRP-6a 3072-bit in `crypto`, HKDF-SHA-512 key derivation, pair-setup M1–M4 in `pairing`
2. **Step 3** — Encrypted RTSP: ChaCha20-Poly1305 framing, `GET /info` to verify channel works

---

## Test Devices

| Device | Model string | Features hex | Reachable | Notes |
|--------|-------------|--------------|-----------|-------|
| Pool Room (Shairport Sync) | `Shairport Sync` | — | ✅ 192.168.1.106:7000 | Software receiver on LAN; PTP + AAC + Transient |
| Living Room | `AppleTV5,3` | — | ✅ 192.168.1.x:7000 | Apple TV 4K (1st gen); PTP + AAC + Transient |
