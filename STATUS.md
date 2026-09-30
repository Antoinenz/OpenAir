# OpenAir — Implementation Status

> Updated at the end of each working session. Reflects what is actually working, not just written.

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
| 9 | Real-time hardening (SCHED_FIFO, DSCP EF, retransmit <5ms) | 🔄 Mostly done | DSCP EF on all audio sockets ✅ (Windows strips it unless DisableUserTOSSetting=0 — detected and reported); MMCSS "Pro Audio" thread priority ✅ (verified Raised on hardware); retransmit turnaround now timed and reported at teardown (realtime path only — buffered is TCP) ✅; SCHED_FIFO (Linux) remains, and no real-world retransmit figure has been captured yet |

**Legend:** ✅ Done · 🔄 In Progress · ⚠️ Partial / Known Issues · ⬜ Not Started

---

## Per-Crate Status

| Crate | Status | Tested Against Hardware | Notes |
|-------|--------|------------------------|-------|
| `core` | ✅ Scaffolded | — | `Features` bitmask, `AudioMode`, `OpenAirError` |
| `discovery` | ✅ Done | Yes | `browse()`, `browse_live()` (non-blocking, for the picker), `DeviceSet` collation, `display_name()`, feature-bit decoder; 15 tests; verified on LAN |
| `crypto` | ✅ Done | Yes | SRP-6a 3072-bit (N fingerprint-guarded), HKDF-SHA-512, ChaCha20-Poly1305 (channel + labeled one-shot); 12 tests |
| `pairing` | ✅ Done | Yes | TLV8, `TransientPairing` M1–M4, `NormalPairing` M1–M6 + `PairVerify` (Ed25519/X25519); 12 tests |
| `rtsp` | ✅ Done | Yes | Transient + Normal pair flows, SETUP×2, SETPEERS, RECORD, full SETRATEANCHORTIME, SET_PARAMETER, TEARDOWN |
| `audio-codec` | ✅ Done | Yes | Verbatim ALAC + FDK-AAC (CBR 256k) both play on hardware |
| `audio-rtp` | ✅ Done | Yes | RTP+AEAD packetizer, PTP anchor packets (0xD7) with timeline translation, NTP sync (0xD4), retransmit backlog |
| `timing` | ✅ Done | Yes | NTP responder + PTP master with BMCA yield: tracks foreign grandmaster (offset EWMA), answers Delay_Req |
| `capture` | ✅ Done (Win) | Yes | WASAPI loopback verified with live Spotify; PipeWire/CoreAudio later |
| `ptp-helper` | ⬜ Stub | — | Privileged binary, IPC to main (Linux ports 319/320; not needed on Windows) |
| `client` | ✅ Done (v1) | Yes | windowed-sinc resampling (rubato 5), realtime + buffered pipelines, pairing store + auto-dispatch (pair-verify vs transient), event channel, `StreamStats` snapshot for observers, live `SetLatency`/`SetMasterVolume`/`SetMetadataEnabled` commands, capture rate followed through a shared atomic |
| `apps/cli` | ✅ Done (v1) | Yes | scan, `pair` (PIN), tone/play/capture, devices, restore-audio; name resolution, --volume, --buffered, --latency <ms>, --offset <name=ms>, --handoff[-device] (Windows), --bind <ip>, --no-metadata, --log, --debug [0-2], --no-tui, --help/-h/`help <command>`, Ctrl+C |
| `tui` | ✅ Done (unified flow) | Yes | Library, not a binary — `openair` drives it. One App owns the terminal for the whole run: picker → pairing (in-TUI PIN) → connecting → dashboard, never dropping to a shell. Per-receiver volume/offset/buffer bar, add/remove/retry mid-stream, ready button, responsive layout, row cap on large networks, settings overlay (`s`, live handoff/latency/volume/metadata), settings persistence, log panel, panic-safe restore; 186 tests |

---

## Receiver Compatibility (hardware-verified)

| Receiver | Pairing | Realtime ALAC | Buffered AAC | Notes |
|----------|---------|---------------|--------------|-------|
| Shairport Sync 4.x | Transient | ✅ | ✅ | We are PTP master (nqptp follows us) |
| Apple TV (AppleTV5,3 + 6,2) | Normal (PIN, one-time) | ✅ | ✅ | Needs SETPEERS + event channel + full anchor + BMCA yield (we follow ITS clock) — see DEVLOG Session 8 |
| Denon AVR-X2700H | Transient | ⚠️ silent | ❌ | Licensed stack, AirTunes/366.0. `cn: 0,1` (no AAC) and `et: 0,4` (not AirPlay 2 encryption) — see DEVLOG session 24 |
| macOS (AirPlay Receiver) | — | — | — | Answers 403 to Transient pair-setup; untested past that |
| HomePod | — | — | — | Untested; expected same path as Apple TV |

---

## Known Issues / Blockers

### ⚠️ An Apple TV shows no AirPlay UI after the first session per reboot

Audio plays normally — the television simply shows nothing. **The first session
after a receiver reboot works; every session after it fails, until the next
reboot.**

One bit predicts it. The receiver's `updateInfo` on the reverse event channel
carries `flags`, and across nine runs `0x120644` meant the UI appeared while
`0x20644` alone meant it never did. The difference is `0x100000` —
`ReceiverSessionIsActive` in the reverse-engineering notes. The correlation is
9/9 and machine-checkable, which makes it the cheapest triage available: if
`0x120644` never arrives, the metadata path is not the thing that is broken.

Four theories are dead, each killed by experiment rather than argument:
orphaned sessions from earlier drops, an unanswered MediaRemote `play`, a stale
per-sender record keyed to our identity (`--random-sender-id` ruled it out in
two runs), and anything at all about *what we send* — the RTSP exchange is
byte-identical between a working run and a failing one. So the state lives
inside the receiver, is not keyed to our identity, and is cleared only by a
reboot.

What remains is whether a third-party sender gets a UI where we do not.
`tools/atv_trace.py` settles it: run `capture` twice without rebooting the
receiver and diff the traces. Second session still shows the UI → the gap is in
what we send, and pyatv's trace shows what we omit. Second session shows nothing
either → the receiver does this to everyone and this closes as upstream
behaviour. Tracked as **#29**.

### ⚠️ The TUI only attaches to `capture`

`play` and `tone` still print plain scrolling text. Nothing structural stops
them — they call the same `stream_fn` — it just wasn't wired up.

<details><summary>RESOLVED 2026-08-21: receiver transport buttons were answered but not obeyed</summary>

The Apple TV's own pause/play buttons send RTSP `POST /command` on the event
channel. We answered 200 OK — which kept the session alive — but ignored the
content, so the television paused its UI while we kept streaming.

The payload was decoded during the #29 investigation: `type =
sendMediaRemoteCommand` wrapping a `modernMediaRemoteCommand`, with
`kMRMediaRemoteOptionSenderID`, `kMRMediaRemoteOptionCommandID` and
`kMRMediaRemoteOptionSendOptionsNumber`. `crates/client/src/mediaremote.rs`
parses it now.

Acting on it does *not* mean pausing our stream. OpenAir streams system audio,
so there is no OpenAir playback to pause — "play" from the television means
"play whatever this machine is playing", and the only sensible target is the
platform's own media session (on Windows, the same SMTC the now-playing
metadata is read from). Pausing our own stream instead would leave the music
player running into a void. The handler is process-wide rather than
per-stream, because a machine has one media session and a command from any
receiver in a group means the same thing.

Still unconfirmed on hardware: whether obeying the command also stops the
receiver resetting the connection after a few toggles, which session 14
observed and which may or may not have shared a cause.

</details>

<details><summary>RESOLVED 2026-08-20: pairing required leaving the TUI</summary>

A receiver needing a HomeKit PIN was refused by the picker with an instruction
to run `openair pair` separately, because the PIN prompt was a `stdin` read and
the TUI owned the screen. Pairing now happens inside the flow, after the user
confirms their selection, on a worker whose PIN provider waits on a channel.

</details>

<details><summary>RESOLVED 2026-08-18: bare <code>openair</code> paired with every device it found</summary>

Bare `openair` blocked for a fixed 5 s mDNS browse and then attempted Transient
pairing plus `GET /info` against *every* device discovered. Slow at home; on a
shared network it opened handshakes with strangers' receivers. Replaced by the
TUI picker, which contacts nothing until Enter. The old behaviour survives
behind `--no-tui`, where it is a deliberate diagnostic rather than the default.

</details>

<details><summary>RESOLVED 2026-08-17: 30 s teardown + cover art (kept for the record)</summary>

**30 s teardown — fixed.** The Apple TV pushes RTSP `POST /command` (an
`updateInfo` binary plist describing its capabilities and attached display) on
the reverse event channel and waits for a 200 OK. We connected the socket,
discarded every byte, and never replied; ~30 s later it tore the session down —
closing the event channel first, then the data socket 40–60 ms after.

Key direction was hardware-determined: the accessory encrypts with
`Events-Write-Encryption-Key`, so the `Events-*` labels are from *its*
perspective, the reverse of the control channel. Trying both directions and
logging the winner settled it in one run.

Result: sessions went from a hard 30.0 s ceiling to 167 s and then 4m33s,
ending only when stopped or by the unrelated pause issue above.

**Cover art — fixed.** The encrypted frame limit is **1024 bytes of plaintext**,
not the 65535 the 2-byte length prefix implies. A single oversized frame makes
the Apple TV drop the connection. Every request we sent was under 1 KiB until
artwork, so this stayed hidden; an earlier 60 KB cap derived from the u16 limit
was simply the wrong number. `encrypt()` now chunks at 1024 transparently.
Verified: 5 consecutive track changes, all with art, no failures.

</details>

<details><summary>Earlier measurements (kept for the record)</summary>

**The single most important open bug.** Reproduced consistently against
AppleTV6,2 (AirTunes/960.13.1). Measured from session start (RECORD/anchor) to
`data write failed ... (os error 10053)`:

| Session | Delta |
|---------|-------|
| 08:59 initial | 30.03 s |
| 09:00 rejoin 1 | 30.03 s |
| 09:00 rejoin 2 | 30.03 s |
| 09:01 rejoin 3 | 30.06 s |
| 09:01 rejoin 4 | 30.03 s |

Five consecutive drops within 30 ms of each other, and a *rejoined* session
restarts the clock. This is a fixed timeout, not packet loss.

**Only the data socket dies.** The RTSP control connection stays healthy
throughout — `/feedback` runs every 2 s and never fails across any drop. So the
receiver is closing the buffered-audio TCP connection specifically. Audio
recovers on reconnect, so the user hears a ~2 s gap every 30 s.

**Metadata display is downstream of this.** After restarting the Apple TV:
- first-ever session → title/artist **displayed** ✅
- after the 30 s drop and rejoin → accepted (200 OK), **never displays again**
- restarting OpenAir does not help; only restarting the *receiver* does

So metadata only displays on a session that has never dropped. Leading
explanation: every drop orphaned a session on the receiver (we abandoned it
without `TEARDOWN`), so ours stopped being the foreground session. A best-effort
`TEARDOWN` on the drop path is now sent — unverified whether it restores display.

**Not the cause** (each ruled out by evidence, not reasoning):
- DMAP encoding — hex decoded by hand and verified correct; byte-structure is
  identical to a bundle that *did* display
- Wi-Fi vs Ethernet — happens identically on both
- Artwork — predates any artwork being sent, and occurs with `has_art=false`
- Source-address selection — happens with the correct LAN source

*(That diagnostic is what found the root cause above.)*

</details>

### Minor, known and accepted

- **Long-session drift.** The timeline offset to a foreign grandmaster is
  captured once at session start, so sender/receiver crystal drift (~ppm)
  accumulates over sessions measured in hours. Fine for typical use.
- **`--no-tui` scan mode still tries Transient against everything**, without
  consulting the pairing store. That is the pre-picker behaviour, kept
  deliberately as a diagnostic; the default path and `tone`/`play`/`capture`
  all dispatch correctly.

---

## Awaiting hardware verification (code complete, Sessions 10–13)

- **Pause/resume on silence** — pausing PC audio pauses AirPlay (`rate=0`) and
  auto-resumes on sound. Verify the Apple TV resumes cleanly from rate=0 →
  re-anchor (may need a FLUSH — see DEVLOG).
- **Per-receiver `--offset "name=ms"`** — verify a room shifts by the given ms.
- **Auto-reconnect** — kill a receiver mid-`capture` (switch the TV off/on); it
  should rejoin in sync within a few seconds while the other room keeps playing.
- **Auto-latency** — force underruns (start with `--latency 200` on Wi-Fi);
  expect "underrun risk — raising latency" logs stepping 200→…, then stability.

### `--handoff` v2 — virtual device routing (Windows, Session 12)

Requires VB-CABLE. v1's endpoint-mute approach was scrapped (glitched on every
volume change — Windows auto-unmutes and we could only re-mute after the fact).
Device detection already verified on hardware via `openair devices`.

- **Route + play** — `openair capture "<room>" --handoff` → speakers silent,
  AirPlay plays, and Windows' output device shows the cable while streaming.
- **Volume mirror** — slider / volume keys → AirPlay volume follows (~50 ms lag
  ok). **This is where v1 glitched — verify it's now clean.**
- **Mute key** — Windows mute → AirPlay silent; press again → returns.
- **Restore** — Ctrl+C puts the original output device back (check Windows
  sound settings).
- **Crash recovery** — kill the process (Task Manager, not Ctrl+C), then confirm
  the next run warns and `openair restore-audio` fixes it.
- **Split tunneling** — with a handoff stream running, route one app to the
  speakers (Settings → System → Sound → Volume mixer); it should play locally
  while everything else goes to AirPlay.
- **Multi-room** — two rooms both track the volume; a reconnecting room comes
  back at the current level.

### Source-address binding (Session 13) — needs a repro attempt

- **The original failure** — reconnect after a stream on Wi-Fi with virtual
  adapters present. Previously RST at pair-setup; should now connect every time.
- **Check the log line** — `connected from selected local address src=192.168.1.108`
  should match what `Find-NetRoute -RemoteIPAddress <receiver>` reports.
- **PTP** — `PTP sockets bound bind_ip=192.168.1.x` should match the source
  address, and the receiver should reach "NQPTP master clock" as usual.
- **`--bind <ip>`** — forcing a deliberately wrong IP should fail to connect
  (proves the override is actually applied).

### Now-playing metadata (Windows, Session 14)

✅ **WORKING as of 2026-08-17** on AppleTV6,2: title, artist, album and cover art
all display, and survive track changes. Verified over a 4m33s session with five
consecutive track changes, every one carrying art, with no
`set_metadata`/`set_artwork` failures.

Note the display still depends on the receiver having attached a screen to the
session at all — see #29 above, which only a receiver reboot clears. A session
that gets the AirPlay UI is the precondition.

- **Text** — play a track; title/artist/album appear on the Apple TV.
- **Cover art** — album image appears alongside (slight compression visible).
- **Track change** — skip; the display updates within ~1 s.
- **No spam** — pausing/resuming must not re-send; expect one
  "sending now-playing metadata" log line per track.
- **Rejoin** — drop and restore a receiver mid-stream; it should show the
  current track, not a blank screen. Also exercises the Session 14 rejoin-anchor
  fix — audio must actually resume.
- **`--no-metadata`** — nothing is sent.
- **Shairport** — accepts or cleanly ignores; audio unaffected either way.

## Next Steps

1. **#29** — run `python tools/atv_trace.py capture` twice without rebooting the
   Apple TV, watching the television both times, then `diff` the two traces.
   One experiment, and it decides whether there is any work left here at all.
2. **Linux** — PipeWire capture, `ptp-helper` for the privileged PTP ports, and
   `SCHED_FIFO`. The largest gap between what the project promises and what it
   ships: today it captures on Windows only.
3. **A real-world retransmit figure.** The turnaround is timed and reported at
   teardown on the realtime path, but no number has been captured from a lossy
   network yet, so the <5 ms target is still unmeasured rather than met.
4. **HomePod**, when hardware is available — expected to take the Apple TV path
   (PTP, no NTP fallback).
5. **Realtime-ALAC multi-room.** Grouping is buffered-only today.
6. **Attach the TUI to `play` and `tone`**, which still print scrolling text.

---

## Reference Tooling (`tools/`)

| Script | Purpose |
|--------|---------|
| `hap_probe.py` | Known-good transient pairing + encrypted GET /info client (pyatv math, raw RTSP) |
| `hap_oracle_server.py` | Local pair-setup M1–M4 server (srptools) for offline differential tests |
| `mitm_proxy.py` | TCP proxy hex-dumping both directions (wire-level diffing) |
| `pyatv_probe.py` | Drive pyatv end-to-end with debug logs (needs SelectorEventLoop on Windows) |
| `atv_trace.py` | Capture a normalised pyatv protocol trace for #29; `capture` twice without rebooting, then `diff` |

---

## Test Devices

| Device | Model string | Features hex | Reachable | Notes |
|--------|-------------|--------------|-----------|-------|
| Pool Room (Shairport Sync) | `Shairport Sync` | — | ✅ 192.168.1.106:7000 | Software receiver on LAN; PTP + AAC + Transient |
| Living Room | `AppleTV5,3` | — | ✅ 192.168.1.64:7000 | Apple TV HD; AirTunes/670.5.1; Normal pairing ✅ |
| test | `AppleTV6,2` | — | ✅ 192.168.1.152:7000 | Apple TV 4K; AirTunes/870.14.1; full streaming ✅ |
| Living Room | `AppleTV14,1` | `0x3C175FDE4A7FDFD5` | ✅ 192.168.1.165:7000 | Apple TV 4K (3rd gen); AirTunes/980.77.2; Normal pairing ✅ |
| Denon Amp | `Denon AVR-X2700H` | `0x0801C340445F8A00` | ✅ 192.168.1.100:7000 | AirTunes/366.0; MFi bit 26; control plane ✅, audio ❌ |
| Antoine's MacBook Air | `MacBookAir10,1` | `0x38174FDE4A7FCFD5` | ✅ 192.168.1.55:7000 | AirPlay Receiver; 403 to Transient; also the reference *sender* |
