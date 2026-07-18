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

# Receivers can be named (discovered via mDNS) or given as ip:port
openair capture 192.168.1.106:7000

# No arguments: scan the network and list AirPlay receivers
openair
```

- **Two pipelines**: realtime ALAC (protocol-fixed ~2 s latency) and buffered
  AAC-LC over TCP (`--buffered`, sender-chosen latency via `--latency <ms>`,
  default 500 ms)
- **Full AirPlay 2 stack**: mDNS discovery + feature bits, HomeKit pairing
  (Transient *and* Normal with on-screen PIN + persisted Ed25519 identity /
  pair-verify), ChaCha20-Poly1305-encrypted RTSP with binary plists,
  per-packet AEAD audio, PTP (IEEE 1588) timing with BMCA yield — OpenAir
  masters the clock for receivers that follow it (Shairport) and slaves to
  receivers that insist on their own (Apple TV)
- **Sources**: live system capture (WASAPI loopback), WAV files, test tone —
  all resampled/converted to the pipeline format automatically

## Not yet

- HomePod (expected to work like Apple TV — untested, no hardware on hand)
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
