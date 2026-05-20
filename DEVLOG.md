# OpenAir — Development Log

> Append-only. Each session gets one entry at the top. Records decisions, what broke, what worked, and why.

---

## 2026-05-20 — Session 1: Workspace scaffold + mDNS discovery (Step 1)

### What we did
- Created `.gitignore` (Rust + OS + editor)
- Scaffolded the full Rust workspace: 11 crates + `apps/cli` + `apps/tui`
- `cargo build` passes clean across the whole workspace
- Started implementing `openair-discovery` (Step 1)

### Key decisions
- **`mdns-sd` v0.11** chosen for pure-userland mDNS — no Avahi/Bonjour dependency, works on Windows and Linux out of the box.
- **`core` crate** owns `Features(u64)` bitmask and `AudioMode` enum so every crate can decode device capabilities without importing `discovery`.
- Workspace-level `Cargo.toml` pins all shared deps via `[workspace.dependencies]` — no version drift between crates.

### What's working
- Full workspace compiles, zero warnings
- `Features(u64)` bitmask + named accessors (`supports_airplay_audio`, `requires_ptp`, etc.) in `core::types`
- `AirPlayTxt::parse()` — TXT record parser handles `0xLOWER,0xUPPER` feature format, plain hex, and decimal
- `AirPlayDevice` — resolved device with address, port, txt, and helpers (`preferred_audio_mode`, `uses_transient_pairing`)
- `browse(timeout, callback)` — browses `_airplay._tcp` + `_raop._tcp`, deduplicates by device ID, filters out devices missing bit 9
- `apps/cli` wired up: `cargo run -p openair-cli` scans for 5s and prints found devices
- 4 unit tests pass (feature parse, TXT parse)

### What's not working / TODO
- No hardware test yet — needs a real AirPlay device on the LAN to verify

### Protocol notes
- Feature bits are transmitted as `0xLOWER,0xUPPER` hex strings in the `features` TXT key.
  Parse: split on `,`, parse each as hex u32, combine as `(upper as u64) << 32 | lower as u64`.
- Key feature bits for routing decisions:
  - Bit 9 → supports AirPlay audio at all (required)
  - Bit 40 → prefer AAC PT=103; else ALAC PT=96
  - Bit 41 → PTP required (HomePod)
  - Bit 43 || 48 → Transient pairing
  - Bit 26 → needs `/auth-setup` (Sonos, newer AirPort Express)

---
