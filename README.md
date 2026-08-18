# OpenAir

Stream your PC's audio to AirPlay 2 speakers. Open source, written in Rust.

OpenAir is a desktop utility that sends high-fidelity, low-latency system audio
from Windows (Linux planned) to AirPlay 2-compatible receivers — HomePods,
Apple TVs, AirPort Express, Shairport Sync, and other third-party devices —
with no Apple hardware required on the sending side.

## What works today

Hardware-verified against **Apple TV** (HD + 4K) and **Shairport Sync**:

```console
# One-time pairing with an Apple TV (PIN shown on screen); after this it
# connects automatically — Shairport receivers need no pairing step at all
openair pair "Living Room"

# Stream live system audio (WASAPI loopback) until Ctrl+C, ~sub-second latency
openair capture "Living Room" --buffered --latency 300

# Windows only: silence the PC speakers and let the Windows volume control AirPlay
# (needs VB-CABLE installed — see the --handoff section below)
openair capture "Living Room" --handoff

# Play a WAV file (any sample rate/bit depth — resampled automatically)
openair play "Pool Room" song.wav --buffered

# Test tone
openair tone "Living Room" 10 --volume -14

# Multi-room: list several receivers to play the same audio, synchronized,
# to all of them at once (mixes receiver types freely — e.g. Apple TV + Shairport)
openair capture "Living Room" "Pool Room" --buffered

# Receivers can be named (discovered via mDNS) or given as ip:port
openair capture 192.168.1.106:7000

# No arguments: interactive picker, then a live dashboard
openair
```

- **Two pipelines**: realtime ALAC (protocol-fixed ~2 s latency) and buffered
  AAC-LC over TCP (`--buffered`, sender-chosen latency via `--latency <ms>`,
  default 500 ms)
- **Multi-room**: name several receivers and the same audio plays
  time-synchronized on all of them, even a mix of Apple TV and Shairport —
  each anchored on the clock it actually follows, at one shared instant
- **Full AirPlay 2 stack**: mDNS discovery + feature bits, HomeKit pairing
  (Transient *and* Normal with on-screen PIN + persisted Ed25519 identity /
  pair-verify), ChaCha20-Poly1305-encrypted RTSP with binary plists,
  per-packet AEAD audio, PTP (IEEE 1588) timing with BMCA yield — OpenAir
  masters the clock for receivers that follow it (Shairport) and slaves to
  receivers that insist on their own (Apple TV)
- **Sources**: live system capture (WASAPI loopback), WAV files, test tone —
  all resampled/converted to the pipeline format automatically

## Commands & flags

A `<receiver>` is either a discovered device **name** (case-insensitive
substring match over mDNS, e.g. `pool`) or an explicit **`ip:port`** (e.g.
`192.168.1.106:7000`). Streaming commands accept **several** receivers — two or
more plays the same audio synchronized to all of them (multi-room), which
automatically uses the buffered pipeline.

| Command | What it does |
|---------|--------------|
| `openair` | Open the **interactive picker**: receivers appear as they answer, arrow keys and space to choose, Enter to stream. Contacts nothing until you press Enter. With `--no-tui`, instead scans for 5 s and tries Transient pairing + `GET /info` on every device found (diagnostic). |
| `openair <ip:port>` | Connect straight to one address, pair, and `GET /info` — no discovery (diagnostic). |
| `openair pair <receiver>` | One-time **Normal HomeKit pairing**: shows a PIN on the device, prompts for it, and persists credentials so future connections are automatic. Needed for Apple TV / HomePod; Shairport needs no pairing. |
| `openair capture <receiver>… [seconds]` | Stream **live system audio** (WASAPI loopback of the default output device). Runs until `Ctrl+C`, or for `seconds` if given. Pausing PC audio auto-pauses/resumes the stream. |
| `openair play <receiver>… <file.wav>` | Stream a **WAV file** (the last argument). Any sample rate / 16-bit int or 32-bit float, mono or stereo — resampled/converted automatically. |
| `openair tone <receiver>… [seconds]` | Stream a 440 Hz **test tone** (default 10 s). Hardware smoke test. |
| `openair devices` (**Windows**) | List audio output devices and show which one `--handoff` would route through. Read-only — changes nothing. |
| `openair restore-audio` (**Windows**) | Put the default output device back if a `--handoff` run was killed before it could restore it. |

| Flag | Applies to | Default | What it does |
|------|-----------|---------|--------------|
| `--buffered` | capture / play / tone | off | Use the buffered AAC-LC pipeline (lower, sender-chosen latency) instead of realtime ALAC (~2 s fixed). Auto-enabled when you name more than one receiver. |
| `--latency <ms>` | buffered only | `500` | **Starting** end-to-end buffered latency (the anchor lead). Lower = tighter sync but more prone to underruns; below ~300 ms is risky. If the stream starts cutting out, OpenAir automatically raises the latency in 250 ms steps (up to 2 s) until it's stable. Ignored without `--buffered`. |
| `--volume <dBFS>` | capture / play / tone | `-8` | Playback volume in dBFS: `0` = full scale, negative = quieter (e.g. `-14`), very low mutes. |
| `--offset <name=ms>` | buffered / multi-room | `0` | Per-receiver play delay in milliseconds (`+` later, `-` earlier), e.g. `--offset "pool=+80ms"`. Repeatable; the `name` matches the receiver argument case-insensitively. Compensates downstream amp/DSP delay so rooms line up audibly. |
| `--handoff` | capture only (**Windows**) | off | Routes system audio through a **virtual audio device** so your speakers go silent and audio only comes out of AirPlay, and **mirrors the Windows master volume** — the slider, volume keys and mute key all control the AirPlay volume. Requires a virtual audio cable (see below). `--volume` sets the initial level until you first touch the Windows volume. Implies `--buffered`. |
| `--handoff-device <name>` | with `--handoff` | auto | Force a specific output device by name substring (e.g. `--handoff-device "CABLE Input"`) instead of auto-detecting the virtual cable. |
| `--bind <ip>` | all streaming commands | auto | Force the local IP that receiver connections originate from. OpenAir normally picks the interface on the receiver's subnet automatically; use this only if it guesses wrong on an unusual setup. |
| `--no-metadata` | capture (**Windows**) | off | Stop sending now-playing info. By default `capture` reads the current track from Windows (title, artist, album, cover art) and pushes it to the receiver — an Apple TV shows it on its now-playing screen. |
| `--log` | any command | off | Also write this run's log to `logs/openair-YYYYMMDD-HHMMSS.log`. Plain text, no colour codes, UTC timestamps — greppable and diffable between runs. The file always keeps full detail even when the console is quiet, so you get a clean terminal *and* a complete log. Invaluable for reporting a bug: attach the file instead of pasting a scrollback. |
| `--no-tui` | any command | off | Turn off the terminal UI: no picker, no dashboard, plain scrolling text. Selected automatically when stdout isn't a terminal (pipes, scripts, CI), so redirection keeps working without it. |
| `--debug [0-2]` | any command | `0` | Console verbosity. `0` (default) shows only the normal narration plus warnings and errors. `--debug` (= `1`) adds protocol detail — pairing, SETUP, anchors, PTP. `--debug 2` adds everything the receiver sends, including the full decrypted body of each event-channel message. Bare `--debug` means level 1, so `tone x --debug 10` still plays for 10 seconds. |

Notes:
- Flags can appear anywhere in the command line.
- `--latency` and `--offset` only affect the buffered pipeline; the realtime
  (default single-receiver) pipeline has a protocol-fixed ~2 s latency.
- On a multi-homed machine, OpenAir asks the OS which local address it would
  route to the receiver from, and binds RTSP and PTP to that same address so the
  receiver's clock daemon sees us consistently. If a connection is reset during
  pair-setup, you're most likely on an interface that can't reach the receiver's
  network — OpenAir will suggest a `--bind <ip>` to try.
- Now-playing metadata is read from Windows' System Media Transport Controls,
  so it works with any player that reports there (Spotify, browsers, Apple
  Music, foobar2000, …) — no per-app integration. It is sent once per track
  change, not continuously.
- HomeKit credentials are stored at `%APPDATA%\OpenAir\pairings.json`.

### The terminal UI

Running `openair` with no arguments opens a **picker**:

```
┌ OpenAir — 4 found ──────────────────────────────────────────┐
│  [x] Living Room       AppleTV6,2      192.168.1.106:7000 ✓ │
│  [ ] Pool Room         ShairportSync   192.168.1.51:7000    │
│> [x] Kitchen           AudioAccessory  192.168.1.88:7000    │
└─────────────────────────────────────────────────────────────┘
  handoff ON   latency 500 ms   volume -8 dB   selected 2
  ↑↓ move · space select · h handoff · +/- latency · ⏎ start · q quit
```

Receivers appear as they answer — there's no fixed wait — and **nothing is
contacted until you press Enter**: names, models and capabilities all come from
the mDNS record, and the ✓ comes from your local pairings file. Devices needing
a one-time `openair pair` are marked, and pressing Enter on one tells you so
rather than failing mid-handshake.

Press Enter and it becomes a **dashboard**: latency, bandwidth and now-playing
across the top, a rolling **buffer-health** graph (press `b` for bandwidth), the
live state of each receiver, and a log panel. Buffer health is the number that
predicts a dropout — it's what auto-latency watches to decide when to step up —
so you can usually see trouble coming.

`q` (or Ctrl+C) stops, restoring the terminal and your audio device, and prints
a one-line summary of the run.

Preferences — handoff, latency, volume, graph choice — persist in
`settings.json` beside `pairings.json`. Command-line flags override the file for
that run without rewriting it. Chosen receivers are deliberately *not*
remembered.

> `--no-tui` gives the plain scrolling output, and is selected automatically
> when stdout isn't a terminal.

### `--handoff`: silent speakers + Windows volume control

`--handoff` switches the Windows **default output device** to a virtual audio
cable, captures from that cable, and restores your original device on exit.
Because nothing is muted, there's no fight with Windows — and since the audio
now flows through a virtual device, you also get **per-app routing** for free
(Settings → System → Sound → Volume mixer lets you send individual apps
somewhere else).

**Setup (one time):** install [VB-CABLE](https://vb-audio.com/Cable/) (free).
Then check it's detected:

```bash
openair devices
#   CABLE Input (VB-Audio Virtual Cable) ← --handoff would use this
```

```bash
openair capture "Living Room" --handoff
```

Your speakers go quiet, audio plays on the AirPlay receiver, and the Windows
volume controls it.

> **If your audio stays silent after a crash:** OpenAir restores your output
> device on exit (including Ctrl+C), but if it's killed hard it can't. Run
> `openair restore-audio` to put it back — OpenAir also warns you on the next
> run if it detects this.

## Not yet

- HomePod (expected to work like Apple TV — untested, no hardware on hand)
- Linux capture (PipeWire) and the privileged PTP helper it needs
- macOS

## Building

Rust stable; on Windows the MSVC toolchain (FDK-AAC is compiled from source
via `cc`).

```console
cargo build --release
cargo test
# binary at target/release/openair.exe
```

Use the release binary: the SRP-6a pairing math is ~20× slower in debug builds.

## Project layout

Rust workspace: `crates/` holds the protocol stack (`discovery`, `crypto`,
`pairing`, `rtsp`, `timing`, `audio-codec`, `audio-rtp`, `capture`, `client`),
`apps/cli` is the command-line front end. See [STATUS.md](STATUS.md) for
per-phase implementation state and [DEVLOG.md](DEVLOG.md) for the full
development history (including the protocol details that were reverse-verified
against shairport-sync and pyatv).

## License

GPL-3.0
