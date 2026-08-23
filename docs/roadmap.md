# Roadmap

Ordered roughly by when it is likely to land, not by size. Nothing here has a
date attached.

## Next

**Published binaries.** There is no release yet — everything is built from
source. Windows first, since that is the platform that works end to end.

**Pairing management.** See which receivers you have credentials for and forget
one, from settings. Today the only way to drop a pairing is to edit
`pairings.json` by hand. Alongside it: when a receiver rejects credentials it
previously accepted (an Apple TV that has been factory reset, say), offer to
re-pair from the TUI instead of failing with an error.

**Apple TV remote control, built in.** Drive the Apple TV *itself* from the
OpenAir TUI — navigation, playback, the lot — rather than only receiving what
its remote sends us. This is a different protocol from AirPlay (MRP/Companion),
so it is real new work, not a flag.

**Per-receiver graphic EQ.** A room with a bright ceiling speaker and a room
with a boomy soundbar want different curves, and the receiver is the only place
that distinction exists. Applied per receiver before encoding, adjustable on
that receiver's row.

**Real-time hardening.** DSCP EF marking so Wi-Fi access points treat the audio
as the real-time traffic it is, sender thread priority, and tighter retransmit
turnaround. This is the difference between "usually fine" and "fine on a busy
network".

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

## Known gaps

- **An Apple TV shows no AirPlay UI at all** for any session after the first
  one following a reboot. Under investigation; nine experiments have ruled out
  orphaned sessions, stale sender records, and anything about what we send —
  the RTSP exchange is byte-identical between a working and a failing run.
  Audio is unaffected.
- **Multi-room is buffered-only.** The realtime ALAC pipeline streams to one
  receiver. This is a limit of our implementation, not the protocol.
- **Cover art is re-sent periodically** rather than only on track change, which
  wastes bandwidth on a stream that has none to spare.

## Not planned

Screen mirroring aside, this is a v1 boundary rather than a permanent one:
FairPlay-protected content, AWDL peer-to-peer, and DACP remote control are all
out of scope.
