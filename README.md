# OpenAir

Stream your PC's audio to AirPlay 2 speakers. Open source, written in Rust.

OpenAir sends high-fidelity, low-latency system audio from Windows to AirPlay
2 receivers — HomePods, Apple TVs, AirPort Express, Shairport Sync and other
third-party devices — with **no Apple hardware required on the sending side**.
The whole protocol stack is implemented from scratch: discovery, HomeKit
pairing, encrypted RTSP, PTP timing, the lot.

> **No binary release yet.** Build it from source — it is two commands, and
> they are below. Releases are coming.

## What it does

- **Multi-room.** Name several receivers and the same audio plays in every room
  on one shared clock, mixing types freely — an Apple TV and a Shairport box
  stay in sync with each other.
- **Lossless, if you want it.** Realtime ALAC for a single receiver, or
  buffered AAC when you want to choose your own latency, multi-room and
  metadata. [Which is which](docs/audio.md).
- **Latency you control**, from ~300 ms up, raised automatically if the network
  turns bad — and a buffer that refills itself by trimming playback speed by a
  fraction of a percent rather than by skipping audio.
- **A terminal UI** that runs the whole session: pick receivers, type a pairing
  PIN, watch them connect, then a live dashboard with per-room volume, offset
  and buffer headroom. No config files to write.
- **Windows integration.** `--handoff` silences your PC speakers and hands the
  Windows volume slider, keys and mute to AirPlay. Now-playing metadata reaches
  the receiver's screen, and an Apple TV's remote controls your music.
- **Per-receiver trim.** Volume and play offset per room, adjustable while
  streaming, so a slow soundbar or a bright speaker can be corrected in place.

Hardware-verified against **Apple TV** (HD and 4K) and **Shairport Sync**.

## Quick start

**Prerequisites**

- [Rust](https://rustup.rs) (stable).
- A C toolchain — the AAC and ALAC encoders are compiled from source.
  - **Windows:** Visual Studio Build Tools with "Desktop development with C++"
    (the MSVC toolchain, which `rustup` selects by default).
  - **Linux:** `build-essential` and `libasound2-dev`.
  - **macOS:** Xcode command line tools.

**Build**

```console
git clone https://github.com/Antoinenz/OpenAir
cd OpenAir
cargo build --release
```

The binary lands at `target/release/openair` (`.exe` on Windows). **Use the
release build** — the pairing handshake does 3072-bit modular arithmetic and is
roughly 20× slower in a debug build.

**Run**

```console
openair
```

That opens the picker. Choose a receiver with `space`, press Enter, and type
the four-digit PIN if it asks for one. Everything else has a sensible default.

> **Windows: install [VB-CABLE](https://vb-audio.com/Cable/) too.** It is free,
> and it is what makes `--handoff` work: your PC speakers go quiet and the
> Windows volume control drives AirPlay instead. Without it OpenAir still
> streams, but your speakers keep playing along and the volume slider does
> nothing to the receiver. Details in [docs/windows.md](docs/windows.md).

**Platform support:** Windows is the only tested platform. Linux and macOS
should build — the protocol stack is platform-independent, pure userland, with
no Avahi dependency — but system capture is not implemented on either, so there
is nothing yet to capture *from*. See [the roadmap](docs/roadmap.md).

## Documentation

`openair --help` covers the flags. For anything longer:

| | |
|---|---|
| [docs/cli.md](docs/cli.md) | Every command and flag, and where settings are kept |
| [docs/tui.md](docs/tui.md) | The terminal UI: picker, dashboard, settings, keys |
| [docs/audio.md](docs/audio.md) | ALAC vs AAC, latency, resampling, how gaps close |
| [docs/windows.md](docs/windows.md) | Handoff, VB-CABLE, metadata, the receiver's remote |
| [docs/troubleshooting.md](docs/troubleshooting.md) | When it does not work |
| [docs/roadmap.md](docs/roadmap.md) | What is coming, and what is missing |

## Roadmap

Next up: published binaries, managing stored pairings from the TUI, **Apple TV
remote control built in** (drive the Apple TV itself, not just receive what its
remote sends), **per-receiver graphic EQ**, and real-time network hardening.

After that, Linux (PipeWire capture and the privileged PTP helper) and HomePod
verification.

Eventually, once the audio side is genuinely a fine art rather than merely
working: **video and screen mirroring**.

The [full roadmap](docs/roadmap.md) has the reasoning, and the known gaps.

## Project layout

A Rust workspace. `crates/` holds the protocol stack — `discovery`, `crypto`,
`pairing`, `rtsp`, `timing`, `audio-codec`, `audio-rtp`, `capture`, `client`,
`tui` — and `apps/cli` is the front end.

```console
cargo test
cargo clippy --workspace --all-targets
```

[STATUS.md](STATUS.md) tracks per-phase implementation state.
[DEVLOG.md](DEVLOG.md) is the full development history, including the protocol
details reverse-verified against shairport-sync and pyatv.

## License

GPL-3.0
