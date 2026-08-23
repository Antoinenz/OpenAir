# Command-line reference

Everything here is also available from the program itself:

```console
openair --help            # the overview
openair help capture      # one command in detail
```

## Naming a receiver

A `<receiver>` is either:

- a **discovered name** — matched case-insensitively against any part of it,
  so `pool` finds "Pool Room"; or
- an explicit **`ip:port`**, e.g. `192.168.1.106:7000`, which skips discovery
  entirely.

Streaming commands accept **several** receivers. Two or more plays the same
audio, time-synchronized, in every room at once — and that automatically uses
the buffered pipeline, since it is the one that can hold a group together.

## Commands

| Command | What it does |
|---------|--------------|
| `openair` | Open the [terminal UI](tui.md): pick receivers, pair any that need a PIN, watch them connect, then stream — all in one screen. Contacts nothing until you press Enter. With `--no-tui`, instead scans for 5 s and tries pairing plus `GET /info` on every device found (diagnostic). |
| `openair <ip:port>` | Connect straight to one address, pair, and `GET /info` — no discovery (diagnostic). |
| `openair capture <receiver>… [seconds]` | Stream **live system audio** (WASAPI loopback of the default output device). Runs until `Ctrl+C`, or for `seconds` if given. Pausing PC audio pauses the stream; resuming resumes it. |
| `openair play <receiver>… <file.wav>` | Stream a **WAV file** (the last argument). Any sample rate, 16-bit int or 32-bit float, mono or stereo — converted automatically. |
| `openair tone <receiver>… [seconds]` | Stream a 440 Hz **test tone** (default 10 s). The quickest hardware smoke test. |
| `openair pair <receiver>` | One-time **HomeKit pairing** from the command line. Rarely needed now — the TUI pairs a receiver as part of selecting it. Persists credentials either way. Apple TV and HomePod need it; Shairport Sync does not. |
| `openair devices` (**Windows**) | List audio output devices and mark the one `--handoff` would route through. Read-only. |
| `openair restore-audio` (**Windows**) | Put the default output device back if a `--handoff` run was killed before it could. |
| `openair help [command]` | The built-in help. |

## Flags

Flags may appear **anywhere** in the command line.

### Audio

| Flag | Applies to | Default | What it does |
|------|-----------|---------|--------------|
| `--buffered` | capture / play / tone | off | Use the buffered AAC pipeline (latency you choose) instead of realtime ALAC (~2 s fixed). Auto-enabled by `--handoff` and by naming more than one receiver. See [audio.md](audio.md). |
| `--latency <ms>` | buffered only | `500` | **Starting** end-to-end latency. Lower is tighter but more fragile; below ~300 ms is risky. If the stream starts cutting out, OpenAir raises this in 250 ms steps on its own. Ignored without `--buffered`. |
| `--volume <dBFS>` | capture / play / tone | `-8` | Playback volume. `0` is full scale, negative is quieter (e.g. `-14`), very low mutes. |
| `--offset <name=ms>` | buffered / multi-room | `0` | Per-receiver play delay (`+` later, `-` earlier), e.g. `--offset "pool=+80ms"`. Repeatable; `name` matches the receiver argument case-insensitively. Compensates a downstream amp or DSP so rooms line up audibly. |

### Windows

| Flag | Applies to | Default | What it does |
|------|-----------|---------|--------------|
| `--handoff` | capture only | off | Route system audio through a **virtual audio device** so your speakers go silent and audio comes only from AirPlay, and **mirror the Windows master volume** — slider, volume keys and mute all control the AirPlay volume. Needs a virtual audio cable; see [windows.md](windows.md). Implies `--buffered`. |
| `--handoff-device <name>` | with `--handoff` | auto | Force an output device by name substring (e.g. `"CABLE Input"`) instead of auto-detecting the cable. |
| `--no-metadata` | capture | off | Stop sending now-playing info. By default `capture` reads the current track from Windows (title, artist, album, cover art) and pushes it to the receiver. |
| `--no-media-controls` | capture | off | Ignore play/pause/skip sent by a receiver's own remote. By default those are forwarded to the Windows media session. |

### Output and logging

| Flag | Applies to | Default | What it does |
|------|-----------|---------|--------------|
| `--no-tui` | any | off | Plain scrolling text: no picker, no dashboard. Selected automatically when stdout is not a terminal (pipes, scripts, CI), so redirection keeps working. |
| `--log` | any | off | Also write this run to `logs/openair-YYYYMMDD-HHMMSS.log`. Plain text, no colour codes, UTC timestamps. The file keeps full detail even when the console is quiet, so you get a clean terminal *and* a complete log. Attach this to a bug report rather than pasting a scrollback. |
| `--debug [0-2]` | any | `0` | Console verbosity. `0` shows narration plus warnings and errors. `--debug` (= `1`) adds protocol detail — pairing, SETUP, anchors, PTP. `--debug 2` adds everything the receiver sends, including the decrypted body of each event-channel message. Bare `--debug` means level 1, so `tone x --debug 10` still plays for 10 seconds. |
| `--help`, `-h` | any | — | Print help and exit. |

### Network

| Flag | Applies to | Default | What it does |
|------|-----------|---------|--------------|
| `--bind <ip>` | all streaming commands | auto | Force the local IP that receiver connections originate from. OpenAir normally picks the interface on the receiver's subnet by asking the OS how it would route there. Use this only if it guesses wrong — see [troubleshooting.md](troubleshooting.md). |

### Diagnostics

These exist to answer open questions about receiver behaviour. They change what
OpenAir announces itself as, which is protocol-visible, so neither is a default.

| Flag | What it does |
|------|--------------|
| `--random-sender-id` | Announce a freshly generated sender identity for this run instead of the fixed placeholder. One identity per run, shared across a group — a multi-room group must look like a single sender. |
| `--impersonate-iphone` | Announce an iPhone's model and OS instead of OpenAir's own, copied field-for-field from pyatv. Tests whether a receiver treats an unknown sender model differently. |

## Examples

```console
# Pair once with an Apple TV (PIN on screen); Shairport needs no pairing
openair pair "Living Room"

# Live system audio, sub-second latency
openair capture "Living Room" --buffered --latency 300

# Multi-room, mixing receiver types freely
openair capture "Living Room" "Pool Room" --offset "pool=+80ms"

# Windows: silence the PC speakers, hand the volume slider to AirPlay
openair capture "Living Room" --handoff

# A WAV file, any rate or depth
openair play "Pool Room" song.wav --buffered

# Prove a receiver works
openair tone "Living Room" 10 --volume -14

# Straight to an address, no discovery
openair capture 192.168.1.106:7000
```

## Where things are stored

| What | Where |
|------|-------|
| HomeKit pairing credentials | `%APPDATA%\OpenAir\pairings.json` |
| Preferences (handoff, latency, volume, metadata) | `settings.json`, beside it |
| Logs, with `--log` | `logs/openair-<timestamp>.log` |

Command-line flags override the settings file for that run without rewriting
it. Chosen receivers are deliberately **not** remembered between runs.
