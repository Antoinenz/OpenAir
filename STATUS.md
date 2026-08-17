# OpenAir — Implementation Status

> Updated by Claude at the end of each session. Reflects what is actually working, not just written.

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
| 9 | Real-time hardening (SCHED_FIFO, DSCP EF, retransmit <5ms) | ⬜ Not Started | |

**Legend:** ✅ Done · 🔄 In Progress · ⚠️ Partial / Known Issues · ⬜ Not Started

---

## Per-Crate Status

| Crate | Status | Tested Against Hardware | Notes |
|-------|--------|------------------------|-------|
| `core` | ✅ Scaffolded | — | `Features` bitmask, `AudioMode`, `OpenAirError` |
| `discovery` | ✅ Done | Yes | `browse()`, `AirPlayDevice`, `AirPlayTxt`, feature-bit decoder; 4 tests pass; verified on LAN |
| `crypto` | ✅ Done | Yes | SRP-6a 3072-bit (N fingerprint-guarded), HKDF-SHA-512, ChaCha20-Poly1305 (channel + labeled one-shot); 12 tests |
| `pairing` | ✅ Done | Yes | TLV8, `TransientPairing` M1–M4, `NormalPairing` M1–M6 + `PairVerify` (Ed25519/X25519); 12 tests |
| `rtsp` | ✅ Done | Yes | Transient + Normal pair flows, SETUP×2, SETPEERS, RECORD, full SETRATEANCHORTIME, SET_PARAMETER, TEARDOWN |
| `audio-codec` | ✅ Done | Yes | Verbatim ALAC + FDK-AAC (CBR 256k) both play on hardware |
| `audio-rtp` | ✅ Done | Yes | RTP+AEAD packetizer, PTP anchor packets (0xD7) with timeline translation, NTP sync (0xD4), retransmit backlog |
| `timing` | ✅ Done | Yes | NTP responder + PTP master with BMCA yield: tracks foreign grandmaster (offset EWMA), answers Delay_Req |
| `capture` | ✅ Done (Win) | Yes | WASAPI loopback verified with live Spotify; PipeWire/CoreAudio later |
| `ptp-helper` | ⬜ Stub | — | Privileged binary, IPC to main (Linux ports 319/320; not needed on Windows) |
| `client` | ✅ Done (v1) | Yes | realtime + buffered pipelines, pairing store + auto-dispatch (pair-verify vs transient), event channel |
| `apps/cli` | ✅ Done (v1) | Yes | scan, `pair` (PIN), tone/play/capture, devices, restore-audio; name resolution, --volume, --buffered, --latency <ms>, --offset <name=ms>, --handoff[-device] (Windows), --bind <ip>, --no-metadata, Ctrl+C |
| `apps/tui` | ⬜ Stub | — | |

---

## Receiver Compatibility (hardware-verified)

| Receiver | Pairing | Realtime ALAC | Buffered AAC | Notes |
|----------|---------|---------------|--------------|-------|
| Shairport Sync 4.x | Transient | ✅ | ✅ | We are PTP master (nqptp follows us) |
| Apple TV (AppleTV5,3 + 6,2) | Normal (PIN, one-time) | ✅ | ✅ | Needs SETPEERS + event channel + full anchor + BMCA yield (we follow ITS clock) — see DEVLOG Session 8 |
| HomePod | — | — | — | Untested; expected same path as Apple TV |

---

## Known Issues / Blockers

### 🔴 Apple TV data connection dies every 30 s (blocks metadata display)

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

**Next diagnostic:** we connect the reverse event channel and have always
*discarded* everything the receiver sends on it. It is now logged in full (text
+ hex). If the Apple TV is asking us something and giving up ~30 s later, that
is where the evidence will be. Failing that: check whether the receiver expects a
periodic `SETRATEANCHORTIME`/rate refresh (we send it once at RECORD and never
again), and test whether `tone`/`play` also die at 30 s to isolate the RTSP
session from the capture/handoff path.



- Timeline offset to a foreign grandmaster is captured once at session start; sender/receiver
  crystal drift (~ppm) accumulates over very long sessions (hours). Fine for typical use.
- Bare `openair` scan mode still tries Transient against everything (does not consult the
  pairing store) — cosmetic; `tone`/`play`/`capture` dispatch correctly.

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

Wire format hardware-confirmed: title + artist rendered on AppleTV6,2 on a
freshly-restarted receiver. **Currently blocked by the 30 s drop above** — after
the first drop the receiver accepts metadata but stops displaying it.

**Cover art is currently always skipped** (`cover art too large — skipping`).
The encrypted RTSP frame header is 2 bytes, so one frame carries at most 64 KiB
and real album art is 70–210 KB. Needs multi-frame chunking (task #21).

- **Text** — play a track; title/artist/album appear on the Apple TV.
- **Cover art** — blocked on #21; expect it to be skipped with a WARN for now.
- **Track change** — skip; the display updates within ~1 s.
- **No spam** — pausing/resuming must not re-send; expect one
  "sending now-playing metadata" log line per track.
- **Rejoin** — drop and restore a receiver mid-stream; it should show the
  current track, not a blank screen. Also exercises the Session 14 rejoin-anchor
  fix — audio must actually resume.
- **`--no-metadata`** — nothing is sent.
- **Shairport** — accepts or cleanly ignores; audio unaffected either way.

## Next Steps

1. **🔴 30 s Apple TV data-connection timeout** (see Known Issues) — highest
   priority; it interrupts audio every 30 s and blocks metadata display
2. **#21 encrypted frame chunking** (>64 KiB bodies) — unblocks cover art, and
   is general correctness: any large RTSP body is currently impossible
3. **Step 9** — hardening (DSCP EF, thread priority, retransmit tuning)
4. **#19** Pool Room (Shairport) refuses connections from the Wi-Fi subnet —
   server-side, reproducible without OpenAir
5. Linux capture (PipeWire) + ptp-helper for privileged ports
6. HomePod hardware test when available; realtime-ALAC multi-room (buffered-only today)

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
| Living Room | `AppleTV5,3` | — | ✅ 192.168.1.64:7000 | Apple TV HD; AirTunes/670.5.1; Normal pairing ✅ |
| test | `AppleTV6,2` | — | ✅ 192.168.1.152:7000 | Apple TV 4K; AirTunes/870.14.1; full streaming ✅ |
