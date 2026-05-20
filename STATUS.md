# OpenAir — Implementation Status

> Updated by Claude at the end of each session. Reflects what is actually working, not just written.

## Implementation Phases

| # | Phase | Status | Notes |
|---|-------|--------|-------|
| 1 | mDNS discovery + TXT feature-bit parsing | ✅ Done | 4 unit tests pass; needs hardware verification |
| 2 | HomeKit Transient pairing (SRP-6a, PIN "3939") | ⬜ Not Started | |
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
| `discovery` | ✅ Done | No | `browse()`, `AirPlayDevice`, `AirPlayTxt`, feature-bit decoder; 4 tests pass |
| `crypto` | ⬜ Stub | — | |
| `pairing` | ⬜ Stub | — | |
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

1. **Hardware test** — run `cargo run -p openair-cli` on a network with AirPlay devices; verify devices appear with correct model/features
2. **Step 2** — HomeKit Transient pairing: SRP-6a 3072-bit in `crypto` crate, pair-setup M1–M4 in `pairing` crate

---

## Test Devices (add yours here)

| Device | Model string | Features hex | Reachable | Notes |
|--------|-------------|--------------|-----------|-------|
| _none yet_ | | | | |
