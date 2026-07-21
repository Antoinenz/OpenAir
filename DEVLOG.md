# OpenAir — Development Log

> Append-only. Each session gets one entry at the top. Records decisions, what broke, what worked, and why.

---

## 2026-07-21 — Session 10: robustness — pause/resume, per-receiver offset, auto-reconnect

Three features (tasks #10–#12). **Code complete, clippy-clean, 71 tests pass —
NOT yet hardware-verified.** Test plan handed to user.

### Pause/resume on silence (fixes: pausing PC audio → AirPlay silent forever)
Root cause: the buffered timeline maps `rtptime` to frames-sent; during a pause
frames-sent stalls while wall-clock advances, so every post-resume packet is
"in the past" of the anchor and the receiver drops it. Fix (live capture only —
gated by new `AudioSource::is_live`; a quiet passage in a WAV is music, not a
pause): detect silence by packet peak (< ~−54 dBFS / `SILENCE_PEAK`), after
`PAUSE_AFTER_SILENCE` (1.5 s) pause via `SETRATEANCHORTIME rate=0`, and on
audio's return re-anchor at a fresh instant (rate=1) + reset the pacing
baseline. `/feedback` keepalive continues through the pause. Also shortened the
blocking-capture `fill()` wait 1000 ms → 60 ms so a pause is noticed within a
couple packets instead of stalling the loop.
*Protocol-uncertain:* whether the Apple TV cleanly resumes from rate=0 →
re-anchor rate=1, or needs a FLUSH first. Watch the hardware test.

### Per-receiver latency offset (`--offset "Pool Room=+80ms"`)
`GroupTarget.offset_ms` added into the anchor (+ = play later) on top of the
per-peer clock-timeline translation. Compensates downstream amp/DSP delay so
rooms line up audibly — the residual gap heard in Session 9. Repeatable, signed,
optional `ms` suffix; matches the receiver argument case-insensitively.

### Auto-reconnect dropped receivers (live streams)
A dropped receiver (TV off, Wi-Fi blip) is re-established on a BACKGROUND thread
(pair → SETUP → event → SETPEERS → TCP), up to 3× with increasing backoff, so
healthy receivers keep playing uninterrupted during the seconds-long re-pair.
On success the main loop RECORDs + anchors the rejoiner onto the group's current
anchor LINE (tracked as `anchor_rtptime` heard at `anchor_t_local`, refreshed on
each resume) so it lands in sync. Crucial detail: `rtptime` keeps advancing with
wall-clock even while the group is momentarily empty (encode is skipped, position
still advances), so the line stays valid and a rejoin never needs a full-group
re-anchor. Stop condition is now "group empty AND no reconnects pending".

New helpers in client: `prepare_receiver` / `spawn_reconnect` /
`finish_reconnect` / `reap_dead` / `spawn_writer`; `stream_audio_buffered_multi`
now takes `&[GroupTarget]`.

### Auto-latency on underrun (#13)
Buffered latency now auto-raises when the stream starts cutting out. Key
insight: for a LIVE capture the receiver's jitter buffer is ≈ the anchor
latency (we can't send audio before we've captured it), so a deeper anchor =
more headroom. Sender-side signal = the *lead*: `play_deadline(rtptime) − now`
for the newest queued frame (`play_deadline_ns` walks the current anchor line).
Healthy live stream ⇒ lead ≈ latency (well above floor); TCP backpressure /
network can't keep up ⇒ rtptime stalls while now advances ⇒ lead collapses.
When the *window-minimum* lead stays under `UNDERRUN_LEAD_FLOOR` (120 ms) for a
1 s window, step latency +250 ms (cap 2 s, bump-only) and re-anchor the whole
group deeper — same anchor-line machinery as resume/reconnect, so multi-room
sync is preserved. 5 s cooldown + window-min gate stop a transient blip from
ratcheting latency up. `--latency` is now the *starting* value.
*Note:* re-anchoring deeper causes a one-time ~step gap as the buffer refills —
expected cost of stabilising. Lowering-when-stable is deliberately not done
(oscillation risk); revisit if wanted.

### Still queued
#14 Step 9 hardening (DSCP EF / thread priority), #15 metadata to receiver
(deferred).

---

## 2026-07-20 — Session 9: MULTI-ROOM 🔊🔊 — Step 8 done (grouped streaming)

Hardware-verified: `openair capture pool test --buffered` plays synchronized system
audio on a Shairport receiver and an Apple TV simultaneously. Residual inter-room
offset matches the iPhone's own AirPlay on the same pair (it's the pool amp's DSP
latency, downstream of the receiver) — i.e. parity with Apple's sender.

### The design that works: per-receiver timelines, one shared instant
Rooms are synchronized because every session's SETRATEANCHORTIME describes the
SAME physical moment — each expressed on the clock that receiver actually
follows (ours for Shairport, its own grandmaster for Apple), NOT because the
receivers share a clock. One `PtpMaster::start_multi(all_peer_ips)` node
(319/320 can only bind once per process); audio encoded once, encrypted
per-receiver (each SETUP has its own AEAD key); bounded per-receiver TCP writer
threads so a stalled/dead receiver is dropped while the rest keep playing
(hardware-tested live: the ATV aborted its TCP mid-stream and pool kept going).

### Two shared-clock designs that hardware killed first
1. **Global yield**: Shairport's nqptp ANNOUNCES a clock identity
   (002324fffeb60750, p1=247 class=248 accuracy=254 — real values now logged)
   but never serves Sync/Follow_Up for it. Yielding to any announcing master
   left the whole group clockless. Yield (and timeline choice) must require an
   actively-SYNCING master: ≥3 offset samples.
2. **Group-wide foreign anchor**: anchoring the Shairport receiver on the
   Apple TV's grandmaster left it silent — the ATV's clock never reaches other
   receivers, and receiver-side BMCA isn't ours to control. Hence: SETPEERS
   stays `[receiver, us]` per session (deliberately NOT the whole group — keep
   each receiver's timing world down to {itself, us}), PTP yield is per-peer
   (quiet toward the ATV, mastering toward pool simultaneously), and
   `timeline_for(peer_ip)` picks per-receiver.

### Other hardware finds
- **Playout drain before teardown**: the send-ahead loop runs up to the whole
  lead window ahead of wall clock; tearing down at source-end made receivers
  dump the unplayed tail. A 1s tone sent in 4 ms and torn down before its
  500 ms anchor even arrived = perfect silence with "✓ success" logs. Now we
  sleep until sent-frames + latency (+250 ms margin) have actually played.
- The "test" ATV auto-updated tvOS overnight (AirTunes 870.14.1 → 950.7.1),
  new clock identity, uptime reset — protocol still works on 950.
- tvOS announce quality: p1=248 class=248 accuracy=33 variance=17258 p2=243.
- CLI: `capture/play/tone` take multiple receivers (one shared browse);
  >1 receiver auto-selects buffered; trailing integer = seconds (beware:
  `tone pool test 1` is a ONE-second tone, which is how the drain bug surfaced).

### Future niceties (not planned yet)
- Per-receiver latency offset (`--offset pool=+80ms`) to compensate downstream
  amp/DSP delay — the residual sync gap the user hears is exactly this.
- Timeline offset captured once per session; ppm drift over hours → slew t0.

---

## 2026-07-19 — Session 8: APPLE TV 📺 — Step 7 done (Normal pairing) + Apple-receiver streaming

Both pipelines (realtime ALAC tone, buffered AAC capture) hardware-verified on
AppleTV6,2 (AirTunes/870.14.1); pairing also verified on AppleTV5,3 (670.5.1).
Shairport (Pool Room) regression-checked OK. Five stacked discoveries, each only
visible after fixing the previous one:

### 1. Normal HomeKit pairing (worked first try)
- Wire format from pyatv (`auth/hap_srp.py`, `protocols/airplay/auth/hap.py`), all `X-Apple-HKP: 3`:
  - `POST /pair-pin-start` (empty body) → PIN appears on the TV.
  - M1 `{Method:0, State:1}` (NO transient flag) → M2 salt+B → SRP-6a 3072/SHA-512 with
    user `"Pair-Setup"`, password = on-screen PIN → M3 A+proof → M4 verify server proof.
  - M5/M6: sub-TLVs sealed with ChaCha20-Poly1305, key = HKDF(K, "Pair-Setup-Encrypt-Salt",
    "Pair-Setup-Encrypt-Info"), nonce = 4 zero bytes || "PS-Msg05"/"PS-Msg06", **no AAD**.
    M5 carries `{Identifier: our-UUID, PublicKey: Ed25519 LTPK, Signature over
    HKDF(K, Controller-Sign salts) || id || LTPK}`. M6 returns the accessory's identity;
    we verify its signature (pyatv skips this — spec-conformant receivers pass).
- Pair-verify (every reconnect): X25519 ephemerals, "PV-Msg02"/"PV-Msg03" sealed sub-TLVs,
  Ed25519 signatures both ways, **channel keys from the RAW shared secret** (not the
  Pair-Verify encrypt key) via HKDF("Control-Salt", "Control-Write/Read-Encryption-Key").
- Credentials persist in `%APPDATA%\OpenAir\pairings.json`; streaming auto-dispatches:
  stored peer → pair-verify, else transient. `openair pair <name>` is the one-time step.
- Apple TV "Allow Access: Everyone" does NOT stop the 470 on transient — Normal pairing
  is simply required for tvOS.

### 2. RECORD stalls without SETPEERS (timeout after 10 s)
- Method `SETPEERS`, body = binary plist ARRAY of IP strings `[receiver, sender]`,
  Content-Type `/peer-list-changed` (genuine Apple quirk). Receiver's PTP daemon
  otherwise has no timing-peer list. Shairport ignores SETPEERS entirely.

### 3. RECORD also stalls without the reverse event channel
- TCP connect to `eventPort` from SETUP 1 **before RECORD**, hold open all session.
  We send nothing; a drain thread discards inbound. (owntone: "reverse connection,
  used to receive playback events".)

### 4. Rate-only SETRATEANCHORTIME → HTTP 400
- Real Apple receivers require the full anchor plist (networkTimeTimelineID/Secs/Frac,
  rtpTime, rate) to START. Rate-only `{rate:0}` for stop is accepted. Shairport accepts
  rate-only for both.

### 5. The big one: Apple receivers never slave to a third-party PTP master
- The ATV runs its own grandmaster (Announce + two-step Sync/Follow_Up + Signaling
  type-0xC spam at ~4 Hz) and sent us **zero Delay_Reqs** — it flatly ignores our master.
  Session accepted, audio buffered, but playback never starts because our anchor
  referenced a timeline (our clock) that isn't the elected grandmaster. **No error is
  ever reported — the failure mode is pure silence.** (It does wake the TV from sleep.)
- Fix = BMCA yield (pulled forward from Step 6): parse their Announce (grandmaster ID),
  timestamp their Sync locally, match Follow_Up origin timestamps → offset =
  origin − local_rx (EWMA/8; path delay sub-ms on LAN, absorbed). While a foreign
  master is active (seen <5 s ago) we stop sending our own Announce/Sync, and ALL
  anchors — SETRATEANCHORTIME and type-215 control packets — are expressed on THEIR
  timeline: `networkTimeTimelineID` = their GM ID, times = local + offset.
- Offset to the ATV was ≈ −20.6 days: its PTP epoch is roughly its uptime, nothing
  like wall-clock. Never assume the receiver's timeline resembles ours.
- Timeline is captured ONCE after the 1.5 s warm-up (needs ≥3 offset samples).
  Deliberately not updated live: bending the anchor line mid-session causes receiver
  resyncs. Cost: sender/receiver crystal drift (~ppm) accumulates — fine for
  minutes-long sessions, revisit for hours (slew t0 instead of the offset).
- We also answer Delay_Req with Delay_Resp now, for receivers that DO slave to us.
- Shairport unaffected by all of the above: no foreign master → we stay master,
  offset 0, identical wire behavior to before.

### Debugging pattern that worked
Each layer was found by logging the receive path we previously ignored. "Receiver
accepts everything but nothing happens" on Apple gear = look at what THEIR daemons
are transmitting at you (the Signaling/Announce spam was the tell).

---

## 2026-07-14 — Session 7b: buffered latency tuning (glitch-free, user-controllable)

- **Glitchy buffered-capture start fixed**: the send-ahead loop raced 2 s ahead of a live
  source, padding the lead window with silence dribbles. `CaptureSource::with_blocking()`
  now rate-limits buffered pipelines by waiting for real ring data.
- **PTP warm-up**: 100 ms sync cadence for the first ~3 s + 1.5 s settle between SETUP and
  RECORD (nqptp resets clock records at SETUP; smoothing needs follow_ups to converge).
- **Latency is now sender-controlled**: buffered anchor lead default 500 ms, `--latency <ms>`
  flag (realtime stays ~2 s — protocol constants). Stale setup-time capture audio dropped at
  stream start (was carrying up to 4 s of ring backlog).
- A/B verified by stopwatch: 300 vs 1500 ms clearly track the flag.
- Debugging gotcha: an A/B test ran a stale `target\debug` binary — our hand-rolled arg
  parser silently ignores unknown flags, masking it. (Consider warning on unknown args.)

---

## 2026-07-14 — Session 7: Step 5 done (buffered AAC) + CLI QoL

### Results (all hardware-verified on Pool Room)
- **Buffered AAC (type 103)**: `tone <dest> 10 --buffered` and `play <wav> --buffered`
  play cleanly — FDK-AAC (CBR 256k, raw frames, receiver adds ADTS), TCP transport,
  full-form SETRATEANCHORTIME anchoring, 2 s send-ahead pacing.
- **CLI QoL**: receiver by name (`openair tone pool 3`), `--volume <db>`, indefinite
  `capture` until Ctrl+C.

### Buffered protocol notes (from shairport-sync source, hardware-confirmed)
- SETUP 2: {type:103, ct:4, audioFormat:0x400000, spf:1024, shk, controlPort, …} →
  response dataPort is **TCP**, plus audioBufferSize (informational, ~8 MB).
- Block wire format: `u16 BE length (incl. itself) || u32 BE 0x80800000|(seq&0x7FFFFF)
  || u32 BE rtptime || u32 BE ssrc || ct+tag || nonce8`; AAD = bytes[4..12]; same shk
  and AEAD scheme as realtime; SSRC signals the codec per block (0x16000000 = AAC-LC
  44.1 kHz stereo); rtptime += 1024 per block.
- Anchoring: full SETRATEANCHORTIME plist (networkTimeTimelineID = our PTP clock id,
  Secs/Frac = 2^-64 fraction, rtpTime, rate) — NOT the control-port 215 packets
  (those are the realtime mechanism; sending both would create competing anchors).
- fdk-aac crate (0.8) builds on Windows MSVC in ~13 s; `encode()` counts individual
  i16 samples (not frames); first call(s) return empty output while priming — skip
  them without advancing rtptime.

### Delegation model working well
Both features implemented one-shot by Sonnet subagents from tight, protocol-complete
specs; supervisor did research (shairport/nqptp source), hardware tests, and review.

### Next
- Step 7: Normal HomeKit pairing (M1–M6 + pair-verify + persisted Ed25519 identity)
  → unlocks Apple TV. Then Step 8 (multi-room) / Step 9 (hardening).

---

## 2026-07-13 — Session 6: SYSTEM AUDIO CAPTURE 🎧→🔊 — the core product feature works

### Result
`openair capture 192.168.1.106:7000 20` streams live Windows system audio (Spotify →
laptop speakers, WASAPI loopback) to the Pool Room speaker. Capture health telemetry:
ring steady ~27k frames, zero silence-padded frames over the whole run.

### What we built
- `capture` crate: `SystemCapture::start()` — cpal input stream on the default OUTPUT
  device (WASAPI loopback), F32/I16/U16 + any channel count, 4 s ring
  (Arc<Mutex<VecDeque<i16>>>, drop-oldest). `cpal::Stream` is !Send — stays in the
  binary; only the ring crosses threads.
- `client`: `CaptureSource` (AudioSource) — 200 ms prebuffer, silence-fill on dry ring
  (live capture must never starve RTP pacing), >1 s drift-guard drain, optional duration;
  `LinearResampler` extracted from WavSource and shared. 46 tests green.
- CLI: `openair capture <ip:port> [seconds]`.
- Fixed TEARDOWN 451: shairport requires a binary-plist body (empty dict = close
  connection); sessions now end 200.
- Implementation again delegated to a Sonnet subagent from a tight spec (one-shot).

### Gotchas
- WASAPI loopback captures the post-mix signal: a muted/idle PC yields silence (and
  possibly no callbacks at all) — first "silent" test run was exactly that; the
  `capture health` debug log (ring level + silence-padded count) now makes it obvious.
- PowerShell background-job SoundPlayer playback is unreliable as a test signal; use
  real foreground audio (Spotify) for loopback testing.

### Next
- Step 5: buffered AAC PT=103; Step 7: Normal pairing (Apple TV); Ctrl+C for
  indefinite capture; volume CLI flag; adaptive resampling for long sessions (Step 9).

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
