# Hardware testing

OpenAir talks to devices whose firmware we cannot read, so hardware testing is
not a formality here — it is the only way most of this code is ever verified.
This is the checklist.

Two things make a session worth the time:

- **Record the model string and features hex for every device.** They are what
  a future bug report gets diagnosed against.
- **Run with `--log`.** A log you did not keep is a test you have to repeat.

---

## Why a licensed third-party receiver matters most

It is tempting to rank receivers by how exotic they are. The useful ranking is
by how *strict* they are:

| Receiver | Implementation | Strictness |
|---|---|---|
| Shairport Sync | Reverse-engineered, deliberately tolerant | Lowest — forgives protocol sloppiness |
| Apple TV / HomePod | Apple's own | High, but Apple's sender is the only one it is tested against |
| **Denon / Marantz / Sonos / B&O** | **Apple's licensed stack, integrated by a third party** | **Highest in practice** |

A licensed third-party receiver runs Apple's actual state machine without
Apple's freedom to special-case its own sender. That makes it the closest
thing to a conformance test that exists outside the MFi programme.

"It only has to play music, it won't be fussy" is the wrong instinct.
Shairport Sync passing tells you little. A Denon passing tells you a lot.

---

## Session order

Do these in order. Each result changes whether the next is worth running.

### 1. Is the pairing store healthy? (2 min)

```
openair discover
openair capture "Living Room" --log
```

- **Audio plays** → the `HTTP 470` seen previously was one-time credential
  loss, not a live bug. Close it.
- **`HTTP 470 — device requires user authorization` again** → this is a real
  bug, and finding it outranks the rest of this list. Keep the log. The
  question it answers is whether the run used pair-verify or fell back to
  Transient; grep the log for `using stored HomeKit pairing`.

Then note the `pairing_id` in `%APPDATA%\OpenAir\pairings.json`. If it has
changed since last session and you did not forget a pairing, the store is
being rebuilt underneath us — its own bug, and a data-durability one.

### 2. New Apple TV 4K — baseline (10 min)

Record the model string first: the 2022 model reports `AppleTV14,1`, the 2021
`AppleTV11,1`. Both are new to us — every Apple TV result on file came from an
`AppleTV5,3` or `AppleTV6,2`.

```
openair discover
openair capture "<new atv>" --log
openair capture "<new atv>" --buffered --log
```

Watch for the PIN prompt appearing at all, audio starting within a few
seconds, and the session surviving past **40 seconds** — that is the window
where a missing event-channel reply used to cause a silent teardown (DEVLOG
session 8). Newer tvOS is the most likely place for that contract to have
moved.

### 3. Does #29 reproduce on new hardware? (10 min)

The open bug: an Apple TV shows no AirPlay UI after the first session following
a reboot. Testing it here is worthwhile because it may be specific to the older
models.

```
# Reboot the Apple TV. Then, WITHOUT rebooting it again:
python tools/atv_trace.py capture     # run 1 — expect the AirPlay UI
python tools/atv_trace.py capture     # run 2 — does the UI appear?
```

Both traces are the deliverable. If run 2 shows the UI on the new Apple TV but
not the old one, #29 is a firmware-version bug and we can stop chasing it on
current hardware.

### 4. Denon — first licensed third-party receiver (15 min)

The highest-information test in this session. Nothing about our conformance to
the licensed stack is currently known.

```
openair discover
openair capture "<denon>" --log
openair capture "<denon>" --buffered --log
```

Check these specifically, because they are where a tolerant receiver would
have hidden a bug:

- **Pairing mode.** Features bit 43 or 48 means Transient is offered. If the
  amp advertises neither, it wants normal pairing and should prompt for a PIN.
- **Ports.** We must use the ports from the SETUP response, never a hardcoded
  7000. The Apple TV quirk where `timingPort=0` means "use the sender's port"
  may not apply here — if timing fails, suspect this first.
- **Codec choice.** Bit 40 set means prefer AAC. Confirm the log's choice
  matches the advertised bits.
- **Volume.** Does the amp's display follow OpenAir's volume, and does its own
  remote reach us?
- **Teardown.** Quit with `q`, then start again immediately. A licensed
  receiver is stricter about a session that was not closed cleanly and will
  refuse the second connection if our TEARDOWN is wrong.

### 5. MacBook Air as a receiver (10 min)

Enable it first: **System Settings → General → AirDrop & Handoff → AirPlay
Receiver**, set to *Anyone on the same network*.

A Mac is the only receiver we have whose now-playing state is directly
inspectable, which makes it the best place to debug metadata instead of
guessing at it.

```
openair capture "<macbook>" --log
```

Confirm title, artist and cover art appear in the Mac's Control Centre
now-playing tile. If metadata is wrong anywhere, find out why here — a TV
screen tells you less.

### 6. Multi-room, mixed types (15 min)

The actual differentiator, and the thing no competing project does.

```
openair capture "<new atv>" "<denon>" "<macbook>" --log
```

Three different implementations on one shared clock is the hardest thing this
codebase does. Look for all three reaching Connected, one receiver's failure
not killing the others, and per-room volume and offset working mid-stream.

**Measure the sync rather than judging it by ear.** Put two receivers in the
same room, play something percussive, record a few seconds on a phone, and
open the recording in Audacity. Two transients per beat means an audible
offset, and the gap between them is the number — in samples, so divide by the
sample rate. Under ~5 ms is inaudible; a repeatable 20 ms+ offset is a bug
worth a log. This turns "sounds fine" into a figure comparable between
sessions.

### 7. Metadata and remote control (10 min)

Play from a real source — Spotify, Apple Music, a browser — not a test tone.

- Title, artist, album and cover art on the receiver's screen
- Cover art updates on track *change*, not on every tick
- From an Apple TV remote: pause, play, next, previous reaching the PC

### 8. `--handoff` and the new late-handoff ordering (10 min)

Handoff now fires when the group goes live rather than at startup, so the
silence window should be shorter. This is the first time that runs against
hardware.

```
openair capture "<new atv>" --handoff --log
```

- **Time from Enter to audio**, roughly, by counting. Shrinking this was the
  entire point of the change; if it feels unchanged, the ready hook may not be
  firing.
- **PC speakers go quiet only once the receiver is actually playing.** Local
  silence while still connecting is the old behaviour.
- **Volume keys and the Windows slider** reach the receiver.
- **Toggle handoff off and back on mid-stream** from the settings overlay.
  Volume mirroring must still work afterwards; this path used to lose it.
- **Multi-room + handoff:** speakers mute when the *whole group* is live, not
  when the first receiver connects.

---

## After the session

Update, in this order:

1. `STATUS.md` — the *Receiver Compatibility* and *Test Devices* tables, with
   model strings and features hex.
2. `DEVLOG.md` — anything surprising, especially Denon quirks. This is where a
   future session finds out what already went wrong.
3. `docs/troubleshooting.md` — any failure a user could hit, with its fix.
4. The README's "Hardware-verified against…" line, if the list grew.

---

## Not yet testable

**HomePod.** Untested, and the only receiver requiring PTP with no NTP
fallback, so it exercises a path nothing else does. Until one is available the
honest move is a pinned "hardware testers wanted" issue with this checklist
attached, rather than an untested claim in the README.
