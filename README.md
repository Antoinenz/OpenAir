<div align="center">

# OpenAir

**Stream Windows audio to HomePod, Apple TV and AirPlay 2 speakers — multi-room, lossless, open source. The whole protocol stack written from scratch in Rust.**

_No Apple hardware required on the sending side._

Pick your speakers, type a PIN if one is asked for, and your PC's audio plays in every room on one shared clock. Discovery, HomeKit pairing, encrypted RTSP, PTP timing, ALAC and AAC are all implemented here rather than borrowed — which is why an Apple TV and a Shairport Sync box can stay in sync with each other, and why the now-playing title reaches the receiver's screen.

</div>

```
┌ latency ──────────┐┌ bandwidth ─────────────┐┌ now playing ──────────────────────────────────────────┐
│500 ms             ││4.28 Mbit/s             ││Weightless — Marconi Union                             │
│505 ms ahead       ││340 KB                  ││Ambient Transmissions Vol. 2                           │
└───────────────────┘└────────────────────────┘└───────────────────────────────────────────────────────┘
┌ bandwidth over time ─────────────────────────────────────────────────────────────────────────────────┐
│▆▇▇▅▆▇█▅▆▇▇▅▆                                                                                         │
│█████████████                                                                                         │
└──────────────────────────────────────────────────────────────────────────────────────────────────────┘
  now 4.28 Mbit/s
┌ receivers (3)   [+/-] vol · [<>] offset · [a] add · [r] retry · [d] drop · [s] settings ─────────────┐
│ ▸ Living Room              +0 dB    +0 ms  ██████████  connected                                     │
│   Pool Room                -3 dB   +80 ms  ██████████  connected                                     │
│   Denon AVR-X1700H         +0 dB   -40 ms  ██████████  connected                                     │
└──────────────────────────────────────────────────────────────────────────────────────────────────────┘
┌ logs   [PgUp/PgDn] scroll ───────────────────────────────────────────────────────────────────────────┐
│ 19:42:01 INFO  PTP: yielding to receiver clock (BMCA) grandmaster=Living Room                        │
│ 19:42:02 INFO  3 receivers on a shared clock, buffered AAC PT=103                                    │
│ 19:42:02 INFO  now playing sent: cover art 41.2 kB                                                   │
└──────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

## Features

- **Multi-room on a shared clock** — name several receivers and the same audio plays in every room, mixing types freely; an Apple TV and a Shairport Sync box stay in sync with each other
- **Lossless, if you want it** — realtime ALAC for a single receiver, or buffered AAC when you want to choose your own latency, multi-room and metadata ([which is which](docs/audio.md))
- **Latency you control** — from ~300 ms up, raised automatically if the network turns bad, and a buffer that refills itself by trimming playback speed a fraction of a percent rather than by skipping audio
- **Pairing that works on an Apple TV** — HomeKit Transient *and* normal PIN pairing, with credentials persisted so it is asked once per receiver
- **Now-playing metadata** — title, artist, album and cover art reach the receiver's screen, with art sent on track change rather than on every tick
- **The receiver's remote drives your PC** — pause, play and skip from an Apple TV remote reach whatever is playing on Windows
- **Speaker handoff** — `--handoff` silences your PC speakers and hands the Windows volume slider, volume keys and mute to AirPlay
- **Per-receiver trim** — volume and play offset per room, adjustable mid-stream, so a slow soundbar or a bright speaker can be corrected in place
- **A terminal UI that runs the whole session** — picker, PIN entry, live dashboard, settings. No config files to write
- **Real-time hardening** — DSCP EF marking, MMCSS "Pro Audio" thread scheduling, and retransmit handling for lost packets
- **Written from scratch** — pure userland mDNS with no Avahi dependency, and no vendored third-party AirPlay crate

## Status

**Prerelease.** The protocol side is the well-tested part: multi-room, both
codecs, both pairing modes and metadata all work against real hardware. The
interface is a terminal one, the binary is unsigned, and a desktop UI is
[being designed](docs/design/2026-09-29-desktop-ui-options.md).

Hardware-verified against **Apple TV** (HD and 4K) and **Shairport Sync**.
HomePod is untested — it is the one receiver that requires PTP with no NTP
fallback, so if you have one, [a test report](docs/hardware-testing.md) would
genuinely help.

[STATUS.md](STATUS.md) is honest about what is solid and what is not.

## Install

No binary release yet — the first prerelease is close, and will appear on the
[Releases](https://github.com/Antoinenz/OpenAir/releases) page as a portable
zip. Until then it is two commands.

**Prerequisites:** [Rust](https://rustup.rs) (stable), and a C toolchain for
the AAC encoder — on Windows, Visual Studio Build Tools with "Desktop
development with C++".

```console
git clone https://github.com/Antoinenz/OpenAir
cd OpenAir
cargo build --release
```

The binary lands at `target/release/openair` (`.exe` on Windows). **Use the
release build** — the pairing handshake does 3072-bit modular arithmetic and is
roughly 20× slower in a debug build.

## Use it

```console
openair
```

That opens the picker. `space` to choose receivers, Enter to start.

```
┌ OpenAir (3 found) ───────────────────────────────────────────────────────────────────────────────────┐
│ [ ] Living Room           Apple TV 4K (3rd gen)  192.168.1.61:7000                                   │
│ [ ] Denon AVR-X1700H      AVR-X1700H             192.168.1.88:7000                                   │
│ [ ] Pool Room             Shairport Sync         192.168.1.106:7000                                  │
│                                                                                        ┌──────────┐  │
│                                                                                        │  ⏎ READY │  │
└────────────────────────────────────────────────────────────────────────────────────────└──────────┘──┘
  handoff on · 500 ms · -8 dB · 0 selected   space select · h handoff · <> latency · s settings
```

If a receiver wants a PIN, it takes over the screen and asks for it. This
happens once per receiver.

```
┌ OpenAir ─────────────────────────────────────────────────────────────────────────────────────────────┐
│                                                                                                      │
│                                       pairing with Living Room                                       │
│                                   enter the PIN shown on the device                                  │
│                                                                                                      │
│                                           ○    ○    ○    ○                                           │
│                                                                                                      │
│                   digits to enter · backspace to correct · esc to skip this device                   │
└──────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

Naming receivers on the command line skips the picker:

```console
openair capture "Living Room" "Pool Room" --handoff
```

> **Windows: install [VB-CABLE](https://vb-audio.com/Cable/) too.** It is free,
> and it is what makes `--handoff` work: your PC speakers go quiet and the
> Windows volume control drives AirPlay instead. Without it OpenAir still
> streams, but your speakers keep playing along and the volume slider does
> nothing to the receiver. Details in [docs/windows.md](docs/windows.md).

**Platform support:** Windows is the only tested platform. Linux and macOS
should build — the protocol stack is platform-independent — but system capture
is not implemented on either, so there is nothing yet to capture *from*. Linux
is planned; see [the roadmap](docs/roadmap.md).

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

Working on the protocol itself:

| | |
|---|---|
| [docs/airplay2-protocol.md](docs/airplay2-protocol.md) | The AirPlay 2 stack: pairing, RTSP, timing, codecs, feature bits |
| [docs/hardware-testing.md](docs/hardware-testing.md) | The receiver test checklist, and which receivers prove the most |
| [docs/design/](docs/design/) | Design notes arguing the trade-offs behind each subsystem |
| [DEVLOG.md](DEVLOG.md) | What broke, what fixed it, and what the hardware actually did |

## Roadmap

Next up: published binaries, a desktop UI, **Apple TV remote control built in**
(drive the Apple TV itself, not just receive what its remote sends), and
**per-receiver graphic EQ**.

After that, Linux (PipeWire capture and the privileged PTP helper) and HomePod
verification.

Eventually, once the audio side is genuinely a fine art rather than merely
working: **video and screen mirroring**.

The [full roadmap](docs/roadmap.md) has the reasoning, and the known gaps.

## Building on it

A Rust workspace. `crates/` holds the protocol stack — `discovery`, `crypto`,
`pairing`, `rtsp`, `timing`, `audio-codec`, `audio-rtp`, `capture`, `client`,
`tui` — and `apps/cli` is the front end.

```console
cargo test
cargo clippy --workspace --all-targets
```

The screens above are not hand-drawn. `cargo run -p openair-tui --example
screens` renders them through the real render functions, so they cannot drift
from the interface without someone noticing.

## License

[GPL-3.0](LICENSE) © Antoine Rossi
