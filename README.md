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

# Play a WAV file (any sample rate/bit depth — resampled automatically)
openair play "Pool Room" song.wav --buffered

# Test tone
openair tone "Living Room" 10 --volume -14

# Multi-room: list several receivers to play the same audio, synchronized,
# to all of them at once (mixes receiver types freely — e.g. Apple TV + Shairport)
openair capture "Living Room" "Pool Room" --buffered

# Receivers can be named (discovered via mDNS) or given as ip:port
openair capture 192.168.1.106:7000

# No arguments: scan the network and list AirPlay receivers
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
| `openair` | Scan the LAN for 5 s, list AirPlay receivers, and try Transient pairing + `GET /info` on each (discovery/diagnostic). |
| `openair <ip:port>` | Connect straight to one address, pair, and `GET /info` — no discovery (diagnostic). |
| `openair pair <receiver>` | One-time **Normal HomeKit pairing**: shows a PIN on the device, prompts for it, and persists credentials so future connections are automatic. Needed for Apple TV / HomePod; Shairport needs no pairing. |
| `openair capture <receiver>… [seconds]` | Stream **live system audio** (WASAPI loopback of the default output device). Runs until `Ctrl+C`, or for `seconds` if given. Pausing PC audio auto-pauses/resumes the stream. |
| `openair play <receiver>… <file.wav>` | Stream a **WAV file** (the last argument). Any sample rate / 16-bit int or 32-bit float, mono or stereo — resampled/converted automatically. |
| `openair tone <receiver>… [seconds]` | Stream a 440 Hz **test tone** (default 10 s). Hardware smoke test. |

| Flag | Applies to | Default | What it does |
|------|-----------|---------|--------------|
| `--buffered` | capture / play / tone | off | Use the buffered AAC-LC pipeline (lower, sender-chosen latency) instead of realtime ALAC (~2 s fixed). Auto-enabled when you name more than one receiver. |
| `--latency <ms>` | buffered only | `500` | End-to-end buffered latency (the anchor lead). Lower = tighter sync but more prone to underruns; below ~300 ms is risky. Ignored without `--buffered`. |
| `--volume <dBFS>` | capture / play / tone | `-8` | Playback volume in dBFS: `0` = full scale, negative = quieter (e.g. `-14`), very low mutes. |
| `--offset <name=ms>` | buffered / multi-room | `0` | Per-receiver play delay in milliseconds (`+` later, `-` earlier), e.g. `--offset "pool=+80ms"`. Repeatable; the `name` matches the receiver argument case-insensitively. Compensates downstream amp/DSP delay so rooms line up audibly. |

Notes:
- Flags can appear anywhere in the command line.
- `--latency` and `--offset` only affect the buffered pipeline; the realtime
  (default single-receiver) pipeline has a protocol-fixed ~2 s latency.
- HomeKit credentials are stored at `%APPDATA%\OpenAir\pairings.json`.

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

MIT
