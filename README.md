# OpenAir

Stream your PC's audio to AirPlay 2 speakers. Open source, written in Rust.

OpenAir is a desktop utility that sends high-fidelity, low-latency system audio
from Windows (Linux planned) to AirPlay 2-compatible receivers — HomePods,
Apple TVs, AirPort Express, Shairport Sync, and other third-party devices —
with no Apple hardware required on the sending side.

## What works today

Hardware-verified against Shairport Sync (AirPlay 2 mode):

```console
# Stream live system audio (WASAPI loopback) until Ctrl+C, ~sub-second latency
openair capture "Pool Room" --buffered --latency 300

# Play a WAV file (any sample rate/bit depth — resampled automatically)
openair play "Pool Room" song.wav --buffered

# Test tone
openair tone "Pool Room" 10 --volume -14

# Receivers can be named (discovered via mDNS) or given as ip:port
openair capture 192.168.1.106:7000

# No arguments: scan the network and list AirPlay receivers
openair
```

- **Two pipelines**: realtime ALAC (protocol-fixed ~2 s latency) and buffered
  AAC-LC over TCP (`--buffered`, sender-chosen latency via `--latency <ms>`,
  default 500 ms)
- **Full AirPlay 2 stack**: mDNS discovery + feature bits, HomeKit Transient
  pairing (SRP-6a 3072), ChaCha20-Poly1305-encrypted RTSP with binary plists,
  per-packet AEAD audio, PTP (IEEE 1588) master timing
- **Sources**: live system capture (WASAPI loopback), WAV files, test tone —
  all resampled/converted to the pipeline format automatically

## Not yet

- Apple TV / HomePod "Normal" HomeKit pairing (on-screen PIN + persisted
  identity) — Transient pairing works with Shairport Sync today; tvOS demands
  the full pairing flow (next up)
- Multi-room (grouped) streaming
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
