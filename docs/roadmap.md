# Roadmap

Ordered roughly by when it is likely to land, not by size. Nothing here has a
date attached.

## Next

**Published binaries.** There is no release yet — everything is built from
source. Windows first, since that is the platform that works end to end.

**Apple TV remote control, built in.** Drive the Apple TV *itself* from the
OpenAir TUI — navigation, playback, the lot — rather than only receiving what
its remote sends us. This is a different protocol from AirPlay (MRP/Companion),
so it is real new work, not a flag.

**Per-receiver graphic EQ.** A room with a bright ceiling speaker and a room
with a boomy soundbar want different curves, and the receiver is the only place
that distinction exists. Applied per receiver before encoding, adjustable on
that receiver's row.

## After that

**Linux.** PipeWire capture, plus the privileged helper that PTP needs — ports
319 and 320 are below 1024, so the timing code has to be split into a small
separate binary with `CAP_NET_BIND_SERVICE`. The protocol stack itself is
already platform-independent and pure userland; there is no Avahi dependency.

**HomePod.** Expected to work already — it needs PTP, which is implemented and
verified against an Apple TV — but nobody has run it against real hardware.

**macOS.** CoreAudio capture. Lowest priority: macOS can already do this
natively.

## Eventually

**Video and screen mirroring.** Once the audio side is genuinely a fine art —
not just working, but robust on a bad network, on every receiver type, with
nothing left to explain away. AirPlay video is a separate protocol layered on
the same discovery and pairing, so the foundations carry over; the work is
H.264 encoding, its own timing model, and FairPlay.

## Done recently

**Real-time hardening.** DSCP EF marking on every socket carrying audio, and
the sender thread registered with the platform's audio scheduler. See
[audio.md](audio.md) — including the honest caveat that Windows ignores the
marking unless a registry value is set, which OpenAir reads and reports rather
than assuming.

**Pairing management.** See and forget stored pairings from settings, and a
receiver that rejects credentials it once accepted now offers to pair again
instead of failing with a dead end.

## Known gaps

- **An Apple TV shows no AirPlay UI at all** for any session after the first
  one following a reboot. Under investigation; nine experiments have ruled out
  orphaned sessions, stale sender records, and anything about what we send —
  the RTSP exchange is byte-identical between a working and a failing run.
  Audio is unaffected.
- **Multi-room is buffered-only.** The realtime ALAC pipeline streams to one
  receiver. This is a limit of our implementation, not the protocol.
- **Retransmit turnaround is not measured.** The backlog answers requests, but
  nothing tracks how quickly, so "under 5 ms" is an intention rather than a
  number anyone has checked.

## Not planned

Screen mirroring aside, this is a v1 boundary rather than a permanent one:
FairPlay-protected content, AWDL peer-to-peer, and DACP remote control are all
out of scope.
