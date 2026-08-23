# The audio pipeline

## Two pipelines

AirPlay 2 offers a sender two ways to send audio, and OpenAir implements both.
They are not variations on a theme — they differ in codec, transport, latency
and what they can do.

| | **Realtime ALAC** | **Buffered AAC** |
|---|---|---|
| Codec | ALAC — **lossless** | AAC-LC, 256 kbps CBR — lossy |
| Payload type | 96 | 103 |
| Transport | UDP, with retransmit | TCP |
| Latency | fixed by the protocol, ~2 s | **you choose** (default 500 ms) |
| Receivers | one | one or many, synchronized |
| Metadata | no | yes — track, artist, album, cover art |
| Live latency/volume changes | no | yes |
| How to get it | the default for a single receiver on the command line | `--buffered`, or anything else |

### Which one am I getting?

```console
openair capture "Living Room"                # realtime ALAC (lossless)
openair capture "Living Room" --buffered     # buffered AAC
openair capture "Living Room" "Pool Room"    # buffered AAC (multi-room needs it)
openair capture "Living Room" --handoff      # buffered AAC (handoff implies it)
openair                                      # buffered AAC (the TUI always does)
```

The **terminal UI always uses the buffered pipeline.** Everything it offers —
choosing a latency, adjusting it while streaming, per-receiver volume and
offset, adding a room mid-stream, now-playing metadata — exists only there. So
if you want lossless, ask for it explicitly on the command line, with one
receiver and no `--buffered`.

That is a real trade, not a hedge. Lossless costs you a fixed two seconds of
latency, multi-room, and metadata.

### Is it actually lossless?

The ALAC path is, in the sense that matters: ALAC is a lossless codec and the
audio reaches the receiver bit-for-bit as it was encoded.

Two things upstream of the encoder can still change the samples, and both are
under your control:

- **Volume.** `--volume` scales the samples before encoding, and the default is
  `-8` dBFS. For a bit-exact chain pass `--volume 0`.
- **Sample-rate conversion.** AirPlay carries 44.1 kHz. If your capture device
  runs at 48 kHz — the Windows default — the audio is resampled, and resampling
  is not lossless no matter how good the filter is. Set your capture device to
  44.1 kHz and nothing is resampled at all (see below).

So: 44.1 kHz capture device, `--volume 0`, one receiver, no `--buffered`, and
the receiver gets exactly what the application produced.

## Latency, and why it is a safety margin

The latency setting is not a delay for its own sake. At 2000 ms the receivers
are holding two seconds of audio that has not played yet, and that stock is
what absorbs network trouble: a Wi-Fi hiccup eats into the margin instead of
into the music.

Lower is tighter but more fragile. Below about 300 ms is risky on Wi-Fi.

If the stream starts running short, OpenAir raises the latency by 250 ms at a
time until it is stable. You will see it in the log panel.

## How a gap gets closed

A stall spends the margin. If the sender is blocked for a second, wall-clock
time moves on and your send position does not, so the margin is a second
shorter than it was — and it stays that way, because live capture arrives at
exactly realtime and never brings a surplus.

Getting it back means sending **faster than realtime** for a while, and the only
place that audio can come from is the capture ring.

So OpenAir does not discard from that ring. It nudges the resample ratio
instead: consuming slightly more source audio per output frame drains a
backlog, slightly less lets it refill. The correction is capped at **0.5 %** —
about 8.6 cents of pitch, where a semitone is 100 — and closes a half-second
deficit over roughly a minute and a half. You are not meant to hear it.

The same mechanism handles ordinary clock drift, which is a real effect: your
sound card's 48 kHz and the receiver's are never exactly equal, and over hours
that difference would otherwise grow without bound.

Turn it off with **smooth fix** in the settings overlay, and audio above a
threshold is discarded instead — the older behaviour, audible when it fires.

> Sources already at 44.1 kHz are copied bit-for-bit and cannot be trimmed at
> all: there is no filter to retune. They use the discard path regardless.
> Bit-exact and self-correcting are, unavoidably, alternatives.

## Sample-rate conversion

The pipeline runs at 44.1 kHz because that is what AirPlay carries. Windows
usually runs at 48 kHz, so most streams are resampled, and OpenAir uses a
256-tap windowed sinc (via `rubato`).

That choice is worth the cycles. The obvious cheap alternative, linear
interpolation, does two audible things: it dulls the top of the band, and it
folds ultrasonic content back down into it as tones that were never in the
source.

**If your capture device already runs at 44.1 kHz, nothing is resampled** — the
samples are passed through untouched. On Windows you can set this per device in
Sound Control Panel → device → Properties → Advanced, and it is worth doing:
set the virtual cable to 44.1 kHz if you use `--handoff`.

## Multi-room timing

Every receiver in a group is anchored at one shared instant, on the clock it
actually follows. That matters because receivers disagree about who keeps time:
Shairport Sync will follow OpenAir's clock, while an Apple TV insists on its
own, and OpenAir yields to it (PTP/IEEE 1588 BMCA) and translates.

The practical result is that an Apple TV and a Shairport box stay in sync with
each other in the same group.

If a room still sounds late — a soundbar with its own DSP, an AV receiver doing
room correction — that delay is downstream of AirPlay and nothing in the
protocol knows about it. Correct it with `--offset "name=+80ms"`, or with `<`
and `>` on that receiver's row in the dashboard.
