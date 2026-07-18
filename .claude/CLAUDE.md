# Project: OpenAir

## Git Commits
Do not add "Co-Authored-By: Claude" or any Claude attribution to commit messages.

## Goal
To develop and release a cross-platform, open-source desktop utility that allows users to stream high-fidelity, low-latency system audio from Windows and Linux to AirPlay 2-compatible receivers, achieving feature parity with proprietary alternatives.

## Research
@.claude/docs/airplay2_transmitter_brief.md

## Existing AirPlay Solutions
https://github.com/omarroth/doubletake
https://github.com/mikebrady/shairport-sync
https://github.com/lmcgartland/airplay2-rs/
https://nto.github.io/AirPlay.html

## Architecture

Implemented as a **Rust workspace** (Steps 1–5 of §16 are DONE and hardware-verified —
see STATUS.md for live state and DEVLOG.md for protocol findings before changing
protocol code):

crates/
  core/          # shared types, errors, traits
  discovery/     # mDNS / DNS-SD (pure userland — mdns-sd crate, no Avahi dependency)
  crypto/        # SRP-6a 3072-bit, Ed25519, Curve25519, ChaCha20-Poly1305, HKDF-SHA512
  pairing/       # HomeKit pair-setup/verify (Transient + Normal)
  rtsp/          # encrypted RTSP framing + binary plist
  audio-codec/   # ALAC + FDK-AAC (vendor C source, compile via cc crate)
  audio-rtp/     # RTP packetization, AEAD, retransmit handling
  timing/        # NTP-style (single-room) + PTP/IEEE 1588 with BMCA yield
  capture/       # platform audio: WASAPI / PipeWire / CoreAudio via cpal
  ptp-helper/    # separate privileged binary, IPC to main process
  client/        # high-level API, multi-room group streaming
apps/
  cli/
  tui/           # ratatui (optional)

## Build Commands (once scaffolded)

```bash
cargo build
cargo test
cargo test -p <crate-name>
cargo clippy -- -D warnings

Implementation Order

Follow §16 of the research brief (✅ = done, hardware-verified):
1. ✅ mDNS discovery + TXT feature-bit parsing
2. ✅ HomeKit Transient pairing (SRP-6a, PIN "3939", skip M5/M6)
3. ✅ Encrypted RTSP (GET /info over ChaCha20-Poly1305 channel)
4. ✅ Realtime ALAC PT=96 — over PTP, not NTP (Shairport can't do AP2-NTP; see DEVLOG)
5. ✅ Buffered AAC PT=103 (FDK-AAC over TCP, tunable --latency)
6. 🔄 PTP timing — master + Delay_Resp + BMCA yield/foreign-timeline anchoring done (Apple TV-verified); Linux ptp-helper remains
7. ✅ Normal pairing (Apple TV + PIN, persist identity to disk) — pair-verify + %APPDATA% store; `openair pair`
8. Multi-room streaming ← NEXT
9. Real-time hardening (SCHED_FIFO, DSCP EF, retransmit <5ms)

Hardware-verified protocol knowledge lives in DEVLOG.md — READ IT before touching
protocol code (anchoring, PTP, TLV formats, AEAD framing were all hard-won).

Critical Protocol Rules
- Port negotiation: always use ports from SETUP response — never hardcode 7000. Apple TV quirk: timingPort=0 in response → use sender's configured port.
- RTP marker bit: must be set on the first packet of a new session.
- SRP group: 3072-bit (RFC 5054 Appendix A) — not 2048-bit.
- PTP ports 319/320 are privileged — handle via ptp-helper binary with CAP_NET_BIND_SERVICE (Linux) or UAC elevation
(Windows).
- HomePod requires PTP — no NTP fallback.
- Bit 40 in features → prefer AAC PT=103; else ALAC PT=96. Bit 41 → PTP required. Bit 43 or 48 → Transient pairing.

Crypto Test Vectors

Verify against RFCs before testing on hardware: SRP-6a (RFC 5054-B), HKDF (RFC 5869-A), Curve25519 (RFC 7748 §6.1),
Ed25519 (RFC 8032 §7.1), ChaCha20-Poly1305 (RFC 8439-A).

Out of Scope for v1

Screen mirroring, FairPlay, AirPlay video, AWDL peer-to-peer, DACP remote control.
