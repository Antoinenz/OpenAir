# OpenAir — Development Log

> Append-only. Each session gets one entry at the top. Records decisions, what broke, what worked, and why.

---

## 2026-07-08 — Session 5b: WAV file playback (first real audio content)

### What we did
- Refactored `stream_tone` into generic `stream_audio(addr, device_id, &mut dyn AudioSource, volume)`;
  `AudioSource::fill(&mut [i16]) -> usize` (interleaved stereo 44.1 kHz frames, 0 = end).
- New `WavSource` (hound): 16-bit int / 32-bit float, mono/stereo, any sample rate with a
  minimal linear-interpolation resampler; streams incrementally (no whole-file buffering).
- CLI: `openair play <ip:port> <file.wav>`; 6 new unit tests (43 total).
- Hardware-verified with a 48 kHz stereo melody WAV (exercises the resampler) on Pool Room.
- Implementation delegated to a Sonnet subagent from a tight spec — clean one-shot delivery.

### Next
- System audio capture (WASAPI loopback via cpal + 48k→44.1k resample) — the "stream what my
  PC is playing" milestone; then Step 5 (buffered AAC).

---

## 2026-07-08 — Session 5: FIRST AUDIO 🔊 — Step 4 done (realtime ALAC over AP2, PTP pulled forward)

### Result
`cargo run -p openair-cli -- tone 192.168.1.106:7000 12` plays a 440 Hz tone on the
Pool Room speaker. Full chain: pair → SETUP(PTP) → SETUP(stream ALAC PT=96) →
RECORD → SETRATEANCHORTIME(rate=1) → encrypted RTP + control anchors + PTP master.
Receiver log: "first frame sync error: 0.612 mS" — sync is essentially perfect.

### Major plan change: PTP pulled forward from Step 6
Shairport Sync **cannot do AP2-NTP realtime** (`warn("Shairport Sync can not handle
NTP streams.")` in rtsp.c) and the Apple TV requires PTP too — Step 4's NTP plan
targeted an AirPort Express we don't have. Implemented a minimal PTP master instead:
- nqptp (shairport's clock daemon) is **listen-only**: no BMCA, no Delay_Req. Sender
  just unicasts Announce (1 Hz, port 320) + two-step Sync/Follow_Up (4 Hz, 319/320).
- Windows has no privileged-port concept → binds 319/320 without elevation
  (Linux will need the ptp-helper + CAP_NET_BIND_SERVICE later).
- NTP responder (`TimingResponder`) kept for AirPort-Express-class receivers.

### Protocol discoveries (all verified against shairport-sync source + hardware)
- **Realtime AP2 anchoring** is NOT SETRATEANCHORTIME (that's the buffered path).
  It's control-port packets **type 215 (0xD7)**: `[0]=0x80/0x90, [1]=0xD7,
  [4..8]=frame-77175, [8..16]=PTP-ns u64 BE, [16..20]=frame, [20..28]=clock_id`.
  SETRATEANCHORTIME is still needed but **rate-only** — it flips ap2_play_enabled.
- **Anchors must be collinear**: compute T0 (PTP ns of frame 0) once and extrapolate
  `time(frame)=T0+frame/44100`. Re-measuring the clock per anchor produced ±250 ms
  "frame adjustments" → receiver resynced/muted continuously (root cause of silence #2).
- **Audibility trap**: airplay volume −20 dB ⇒ −43 dB software attenuation in
  shairport; with a −10 dB test tone that's ≈ −53 dB — inaudible (silence #1).
  Tone now 0.6 amplitude, volume −8 dB.
- RTP audio packet: `hdr(12) || ct||tag || nonce8`, AAD=hdr[4..12], nonce=LE counter
  in bytes 4..12; shairport decrypts with `shk` from SETUP 2 ("Hammerton Decoder
  used on encrypted audio" confirms ALAC ct=2 + AEAD both correct).
- Uncompressed/verbatim ALAC frames (owntone-style, 23-bit header + BE samples) play fine.
- SETUP 1 (PTP): timingProtocol=PTP + groupUUID + timingPeerInfo{Addresses,ID};
  shairport auto-registers the sender IP with nqptp ("/nqptp T <ip>").

### Debug method
SSH to the receiver, `log_verbosity = 2`, journalctl. Receiver-side logs found in
minutes what wire-level guesswork couldn't: clock qualified ✓, decrypt ✓, anchor
drift ✗, volume ✗. (Config restored afterwards.)

### Known quirks / follow-ups
- TEARDOWN response reads as 451 on our side though shairport's handler always
  returns 200 + `Connection: close` — probably our encrypted-frame read on a
  closing socket; cosmetic, investigate with Step 5.
- Pacing uses `Instant` while anchors use `SystemTime` — fine for minutes-long
  streams; unify for long sessions (Step 9 hardening).

### Next
- Step 5: buffered AAC PT=103 (needs FDK-AAC + event channel + FLUSHBUFFERED).
- Real audio input (WAV/capture) instead of the tone generator; retest Apple TV
  after on-screen authorization.

---

## 2026-07-07 — Session 4: Step 3 hardware-verified (three bugs found via differential testing)

### Result
`cargo run -p openair-cli -- 192.168.1.106:7000` → Transient pairing M1–M4 accepted by
Shairport Sync (AirTunes/366.0), server M2 proof verified, encrypted channel up,
**encrypted GET /info returned 701 bytes (580-byte binary plist decrypted)**. Step 3 done.

### The three bugs (all found by differential testing against pyatv/srptools)
1. **N_HEX had a one-hex-digit typo** — `…95581716 3995…` instead of RFC 3526's
   `…95581718 3995…` (pos 447). `bits()==3072` passed, all self-consistent tests passed,
   but every SRP value was computed in the wrong group → receiver always returned
   kTLVError_Authentication (0x02). Worse: the Python oracle server we first validated
   against had the same typo copy-pasted in, so it *confirmed* the broken math.
   Regression guard added: SHA-256 fingerprint test of N (`n_matches_rfc3526_fingerprint`).
2. **M1 TLV wire format was wrong** — Flags must be tag **0x13** (19) with value **0x10**
   (kPairingFlag_Transient); we sent tag 0x10 value 0x00. Without the Transient flag,
   pair_ap treats the session as normal pairing (different PIN policy) and rejects the
   proof. Correct M1 is exactly `Method=0x00, State=0x01, Flags=0x10` — no Identifier,
   no PublicKey (A goes in M3). Also fixed Signature tag 0x0B → 0x0A.
3. **ChaCha20-Poly1305 framing was missing AAD** — the 2-byte little-endian length prefix
   must be passed as associated data on every encrypt/decrypt. Without it the Poly1305
   tag never verifies.

### Debugging method (worth repeating)
- Built a Python "oracle" pair-setup server on srptools → validated Rust math… falsely
  (shared typo). Lesson: **an oracle must come from an independent source**, never
  copy-paste constants from the code under test.
- Wrote `tools/hap_probe.py` — pyatv-exact TLVs over raw RTSP → paired successfully with
  real hardware, proving receiver + math and isolating the delta to our client.
- Wrote `tools/mitm_proxy.py` — captured both clients' wire bytes; transcripts were
  structurally identical → difference had to be in the crypto values.
- Deterministic cross-check: same fixed `a`, same captured salt/B in Rust and srptools →
  `A` differed → `g^a mod N` differs → N differs → found the typo in seconds.

### Also learned
- Apple TV 4K returns **470 Connection Authorization Required** at M1 — on-device
  approval / Home-app "Speakers & TV Access" setting; retest Living Room after approving.
- Shairport Sync responds 400 to `/pair-pin-start` (pyatv sends it; not needed for us).
- `tools/` now has: `hap_probe.py` (known-good reference client), `hap_oracle_server.py`
  (local pair-setup server, typo fixed), `mitm_proxy.py`, `pyatv_probe.py`.

### Next
- Step 4: NTP timing + realtime ALAC PT=96 (SETUP two-phase plists, RECORD, RTP+AEAD).
- Retest Apple TV after on-screen authorization (Normal pairing is Step 7 anyway).

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
