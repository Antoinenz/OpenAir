# Windows integration

Windows is the platform OpenAir is developed and tested on, and it gets three
things the protocol does not require: local speaker handoff with volume
mirroring, now-playing metadata, and support for the receiver's own remote.

## `--handoff`: silent speakers, and the Windows volume controls AirPlay

Without it, streaming to AirPlay leaves your PC speakers playing too, and the
Windows volume slider does nothing to the receiver.

`--handoff` switches the Windows **default output device** to a virtual audio
cable, captures from that cable, and restores your original device on exit.
Because nothing is muted, there is no fight with Windows over device state.

You get three things:

- **Silent local speakers** — the audio goes to the cable, not to them.
- **The Windows volume controls AirPlay** — slider, volume keys and the mute
  key all reach the receiver.
- **Per-app routing, free** — audio now flows through a virtual device, so
  Settings → System → Sound → Volume mixer can send individual apps somewhere
  else.

`--volume` sets the initial level until you first touch the Windows volume.
`--handoff` implies `--buffered`, because live volume changes only exist on the
buffered pipeline.

### Setup, once

Install [VB-CABLE](https://vb-audio.com/Cable/) (free). Then check it is seen:

```console
openair devices
#   CABLE Input (VB-Audio Virtual Cable) ← --handoff would use this
```

```console
openair capture "Living Room" --handoff
```

Your speakers go quiet, audio plays on the receiver, and the Windows volume
controls it.

If auto-detection picks the wrong device, name one:

```console
openair capture "Living Room" --handoff-device "CABLE Input"
```

> **Set the cable to 44.1 kHz.** Sound Control Panel → CABLE Input →
> Properties → Advanced. AirPlay carries 44.1 kHz, so a cable already running
> at that rate means no resampling at all. See [audio.md](audio.md).

### If your audio stays silent after a crash

OpenAir restores your output device on exit, including on `Ctrl+C`. If it is
killed outright it never gets the chance, and the PC is left routed to a silent
virtual cable with no obvious cause.

```console
openair restore-audio
```

OpenAir also warns you on the next run if it detects this.

## Now-playing metadata

`capture` reads the current track from Windows and pushes it to the receiver —
title, artist, album and cover art. An Apple TV shows it on its now-playing
screen.

It comes from **System Media Transport Controls**, the same source the Windows
media overlay uses, so it works with any player that reports there — Spotify,
browsers, Apple Music, foobar2000 — with no per-app integration.

Sent on track change rather than continuously. `--no-metadata` turns it off,
as does the metadata row in settings.

> Metadata is a buffered-pipeline feature. The realtime ALAC path has no
> channel for it.

## The receiver's remote

An Apple TV's remote does not control the Apple TV while it is acting as an
AirPlay receiver — it asks the **sender** to do something. OpenAir acts on
those requests, and since it streams system audio rather than owning any
playback of its own, it forwards them to whatever Windows is actually playing:

```
Apple TV remote  →  OpenAir  →  Windows media session  →  Spotify
```

Play, pause, play/pause toggle, next, previous and stop are handled. Anything
that fails is logged and ignored — a media key is never worth dropping audio
for. `--no-media-controls` turns it off.

## Other platforms

Linux and macOS are not supported yet. See [roadmap.md](roadmap.md) for what
each needs.
