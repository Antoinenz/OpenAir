# OpenAir — Development Log

> Append-only. Each session gets one entry at the top. Records decisions, what broke, what worked, and why.

---

## 2026-05-20 — Session 3: pairing + rtsp crates (Steps 2–3 code complete)

### What we did
- Implemented `pairing` crate: TLV8 encoder/decoder, `TransientPairing` M1–M4 state machine, HKDF key derivation into write/read channel keys
- Implemented `rtsp` crate: `RtspConnection` (sync TCP, plain + encrypted read/write), `pair_and_get_info` end-to-end function
- Wired `pair_and_get_info` into `apps/cli`: discovers devices, picks first, pairs, fires encrypted GET /info
- All unit tests pass (7 pairing, 10 crypto, 4 discovery)

### Key design notes
- **RTSP is synchronous** (blocking `TcpStream`): the pairing handshake is strictly sequential — no benefit from async here until we layer audio streaming. Async promotion in Step 4 when we need concurrent RTP + RTSP keepalives.
- **Encrypted read**: the frame format is `uint16_le(ciphertext_len) || ciphertext || tag(16)`. We read the 2-byte header first, then read exactly `len + 16` more bytes to get the full frame.
- **M1 includes Flags=0x00**: required for Transient mode (some receivers check for it).
- **TLV8 fragmentation**: values >255 bytes must be split into consecutive TLVs with the same type and reassembled on decode — handled in `tlv8::encode`/`decode`.

### What needs hardware verification
- `pair_and_get_info` against Pool Room (Shairport Sync) and Living Room (Apple TV 4K)
- Expected: encrypted GET /info returns a binary plist with device capabilities

---

## 2026-05-20 — Session 2: Hardware verification + Step 2 crypto (SRP-6a, HKDF, ChaCha20)

### What we did
- Verified discovery on real hardware: Pool Room (Shairport Sync @ 192.168.1.106) and Living Room (AppleTV5,3)
- Fixed IPv4 preference in browser (was returning fe80:: link-local first)
- Suppressed benign mdns-sd v0.11 shutdown race error
- Implemented `crypto` crate: SRP-6a 3072-bit, HKDF-SHA-512, ChaCha20-Poly1305 RTSP framing

### Key decisions / bugs caught
- **N_HEX was malformed**: original hex string was 3552 bits, not 3072. Root cause: copy-pasted RFC lines with inconsistent lengths (some 63, some 65 chars). Fixed by using exact 48-char rows from RFC 5054 / RFC 3526 §7 (16 rows × 48 chars = 768 hex = 3072 bits).
- **SRP compute_m1**: `H(N) XOR H(g)` uses SHA-512 of the raw big-endian bytes of N and g respectively, not padded.
- **ChaCha nonce**: 4 zero bytes + 8-byte little-endian counter. Both directions need independent counters; never share state between read and write channels.

### What's working (10 tests)
- `SrpGroup::rfc5054_3072()` — N is confirmed 3072 bits
- `SrpClient::new()` — random ephemeral a in range, unique per instance
- `hkdf::derive()` — deterministic, different info → different keys
- `ChaChaChannel` — roundtrip, counter advancement, wrong key rejection, replay rejection

### What's next
- `pairing` crate: pair-setup M1 (POST /pair-setup with TLV8), M2–M4 (SRP challenge/response), derive RTSP keys via HKDF
- `rtsp` crate: TCP connection, RTSP framing, encrypted GET /info

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

### Hardware test results (2026-05-20)
- **Pool Room** (Shairport Sync @ 192.168.1.106:7000) — discovered correctly; PTP=true, AAC, Transient
- **Living Room** (AppleTV5,3) — discovered correctly; PTP=true, AAC, Transient
- IPv4 preference fix applied: devices were initially resolving to fe80:: link-local, now correctly picks IPv4
- mdns-sd v0.11 shutdown race suppressed (benign, log was `ERROR mdns_sd::service_daemon: exit: failed to send response`)

### Known issues / observations
- Both devices report `transient=true` — Transient pairing is the right target for Step 2
- Apple TV 4K also accepts Normal pairing (Step 7), but Transient first

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
