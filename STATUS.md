# OpenAir — Implementation Status

> Updated by Claude at the end of each session. Reflects what is actually working, not just written.

## Implementation Phases

| # | Phase | Status | Notes |
|---|-------|--------|-------|
| 1 | mDNS discovery + TXT feature-bit parsing | ✅ Done | 4 unit tests pass; verified on LAN (Shairport Sync + Apple TV 4K) |
| 2 | HomeKit Transient pairing (SRP-6a, PIN "3939") | ✅ Done | TLV8, M1–M4 state machine, key derivation; 7 tests pass |
| 3 | Encrypted RTSP (`GET /info` over ChaCha20-Poly1305) | ⚠️ Partial | Code complete; needs hardware verification (run `cargo run -p openair-cli`) |
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
| `pairing` | ✅ Done | No | TLV8 encode/decode, `TransientPairing` M1–M4, key derivation; 7 tests |
| `rtsp` | ⚠️ Partial | No | `RtspConnection`, plain + encrypted framing, `pair_and_get_info`; needs hw test |
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

1. **Hardware test Steps 2+3** — run `cargo run -p openair-cli` and verify Transient pairing + encrypted `GET /info` against a real device
2. **Step 4** — NTP timing + realtime ALAC PT=96 (target: Shairport Sync @ Pool Room first)

---

## Test Devices

| Device | Model string | Features hex | Reachable | Notes |
|--------|-------------|--------------|-----------|-------|
| Pool Room (Shairport Sync) | `Shairport Sync` | — | ✅ 192.168.1.106:7000 | Software receiver on LAN; PTP + AAC + Transient |
| Living Room | `AppleTV5,3` | — | ✅ 192.168.1.x:7000 | Apple TV 4K (1st gen); PTP + AAC + Transient |
