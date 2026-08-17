# Design: now-playing metadata (artist / track / album art) to the receiver

**Date:** 2026-08-15
**Status:** Approved (design)
**Task:** #15
**Scope:** Windows only, `capture` mode; text metadata + cover art (no scrub bar)

## Summary

Send now-playing information — track title, artist, album, and cover art — to
the AirPlay receiver so an Apple TV shows what is playing instead of a blank
screen. Read from Windows' System Media Transport Controls (SMTC), which every
mainstream player (Spotify, browsers, Apple Music, foobar2000, …) already
reports to, so this works regardless of which app is making sound.

Enabled by default for `openair capture`; `--no-metadata` turns it off.

## Motivation

`--handoff` silences the PC while streaming, which makes the receiver's screen
the only now-playing display available. Today it shows nothing. This closes
that gap and is the last obvious "feels like real AirPlay" gap for v1.

## Non-goals (v1)

- **Progress / scrub bar.** With system-audio capture our stream position is the
  capture timeline, not the song's, so a naively-derived bar would disagree with
  the actual track. Deferred until the text path is proven.
- Transport controls (receiver-side play/pause/skip driving the PC) — that is
  DACP remote control, explicitly out of scope for v1 per CLAUDE.md.
- Non-Windows platforms (Linux would read MPRIS over D-Bus).
- `play` / `tone` metadata (a WAV file has no meaningful now-playing state).

## Key technical facts

- **SMTC** (`Windows.Media.Control.GlobalSystemMediaTransportControlsSessionManager`)
  exposes the current session's `Title`, `Artist`, `AlbumTitle`, and a
  `Thumbnail` as an `IRandomAccessStreamReference`. Available via the `windows`
  crate's `Windows_Media_Control` feature; 0.54 is already in the tree.
- **Transport is the existing RTSP session.** `SET_PARAMETER` already carries
  volume (`crates/rtsp/src/stream.rs::set_volume`); metadata and artwork are the
  same request with different content types. The research brief confirms:
  `SET_PARAMETER → volume, metadata, artwork (DAAP-tagged)`.
- **DMAP framing** is 4-byte ASCII tag + 4-byte big-endian length + payload,
  nested for containers.

## ✅ Confirmed wire format (hardware-verified 2026-08-17)

Probed against **AppleTV6,2 (AirTunes/960.13.1)** mid-stream. The format this
design assumed is correct — no iteration was needed:

- `SET_PARAMETER` with `Content-Type: application/x-dmap-tagged` and
  `RTP-Info: rtptime=<n>` → **200 OK**
- Body: `mlit` container wrapping `minm` (title), `asar` (artist),
  `asal` (album) — 90 bytes for the probe strings
- **Title and artist rendered on the Apple TV's screen.** Album art was not
  part of the probe.

Variants 2 (bare items, no container) and 3 (`mlit` + leading `mikd`) were
prepared but never needed. `encode_now_playing` stands as written.

Metadata is cleared from the receiver's screen when the session drops.

### ⚠️ Load-bearing uncertainty — probe before building (RESOLVED, see above)

The DMAP **tag set and container framing** come from reverse-engineering notes,
not a specification we can check. The tag/length/payload framing is well
established; what is *not* certain is whether Apple TV requires the `mlit`
container wrapper and which tags it actually honours.

> **Step 1 is a hardware probe:** send a minimal metadata bundle mid-stream and
> see whether the Apple TV renders it. Only build the pipeline once the wire
> format is confirmed.

If the minimal bundle does not display, iterate on framing (bare items vs
`mlit`-wrapped, tag spellings) against the device before writing the rest. This
mirrors `--handoff`'s Phase 0 and exists for the same reason: three sessions of
this project were spent building on unverified premises.

## Architecture

Mirrors the `--handoff` volume bridge, which is already proven in this codebase.

```
SMTC (WinRT)  ──poll 1s──►  nowplaying thread  ──mpsc──►  streaming loop
                             (dedupe on change)            (fan out to receivers)
```

### 1. `crates/capture/src/nowplaying.rs` (`#[cfg(windows)]`)

```rust
pub struct NowPlaying {
    pub title: String,
    pub artist: String,
    pub album: String,
    /// Cover art bytes with a sniffed MIME type; None if unavailable.
    pub art: Option<(Vec<u8>, &'static str)>,
}

pub struct MetadataWatcher { /* thread handle + stop flag */ }
impl MetadataWatcher {
    pub fn start() -> Result<(MetadataWatcher, Receiver<NowPlaying>), MetadataError>;
}
impl Drop for MetadataWatcher { /* stop and join */ }
```

- COM initialised **on the watcher thread** (apartment affinity), as in
  `handoff.rs`.
- Polls every `POLL_INTERVAL` (1 s). Tracks change on the order of minutes, so
  event-driven `MediaPropertiesChanged` callbacks buy nothing and cost COM
  complexity — the same trade-off decided in `--handoff` v2.
- **Emits only on change**, keyed on the `(title, artist, album)` triple.
  Artwork is fetched *only* when that triple changes, since decoding the
  thumbnail stream is the expensive part.
- WinRT async operations are resolved with a blocking `.get()`, which is
  correct here because the thread exists solely for this.

### 2. `crates/rtsp` — DMAP encoding + two new methods

Pure encoder (no I/O, fully unit-testable):

```rust
/// One DMAP item: 4-char tag, 4-byte big-endian length, payload.
fn dmap_item(tag: &[u8; 4], payload: &[u8]) -> Vec<u8>;

/// Encode a now-playing bundle: `mlit` container wrapping
/// `minm` (title), `asar` (artist), `asal` (album).
pub fn encode_dmap(title: &str, artist: &str, album: &str) -> Vec<u8>;
```

Session methods, following `set_volume`'s existing shape:

```rust
pub fn set_metadata(&mut self, dmap: &[u8], rtptime: u32) -> Result<(), SessionError>;
pub fn set_artwork(&mut self, image: &[u8], mime: &str, rtptime: u32) -> Result<(), SessionError>;
```

Both send `RTP-Info: rtptime=<current>` so the receiver associates the metadata
with a stream position, plus the usual `Session` / `DACP-ID` / `Active-Remote`
headers. Content types: `application/x-dmap-tagged` and `image/jpeg` /
`image/png`.

### 3. Client integration

`stream_audio_buffered_multi` gains `metadata_rx: Option<Receiver<NowPlaying>>`,
drained at the same point in the loop as mirrored volume updates (loop top, so
it runs on the paused/priming `continue` paths too). On a new value:

1. `set_metadata(...)` on every live receiver.
2. `set_artwork(...)` if art is present.
3. Stash it as `current_metadata`, so a receiver that **rejoins after a drop**
   is re-sent the current track rather than showing a stale or blank screen —
   the same treatment `current_volume_db` already gets in `finish_reconnect`.

The channel element is the capture crate's `NowPlaying`. Unlike the volume
channel (a plain `f32`, deliberately kept platform-neutral), this is a struct
that must cross the boundary; it lives in `capture` and is `#[cfg(windows)]`, so
the client parameter is `#[cfg(windows)]` too, with the non-Windows build
passing nothing. *(If that proves awkward at implementation time, the fallback
is a small platform-neutral struct in `core` — decide in the plan, not here.)*

### 4. CLI

- Default **on** for `capture` on Windows; `--no-metadata` disables.
- Silently inert on non-Windows and for `play` / `tone` (no error — the flag is
  simply meaningless there, unlike `--handoff` which promises silent speakers
  and so must fail loudly).
- Startup line when active: the detected player/track, so it is obvious the
  feature engaged.

## Error handling

Nothing in this feature may interrupt audio. Every failure path degrades to
"no metadata" with a `warn!`:

- No SMTC session / nothing playing → emit nothing, keep polling.
- Thumbnail missing or unreadable → send text without art.
- `SET_PARAMETER` rejected by a receiver → warn once per receiver, keep
  streaming (Shairport may not accept artwork; Apple TV is the target).
- COM init failure → warn, run the stream with metadata disabled.

**Artwork size cap:** images above a sane ceiling (~2 MB) are skipped rather
than sent, so a pathological thumbnail cannot stall the RTSP control channel
that also carries volume and `/feedback`.

## Testing

**Unit (pure, no COM):**
- `dmap_item` framing: tag bytes, big-endian length, payload round-trip.
- `encode_dmap`: container nesting, field order, UTF-8 payloads (accented and
  CJK titles), empty fields omitted rather than sent blank.
- Change detection: same triple → no emit; any field differing → emit.
- MIME sniffing: JPEG (`FF D8 FF`) and PNG (`89 50 4E 47`) magic bytes;
  unknown → skip art rather than mislabel it.

**Hardware:**
1. **Probe first** (see the uncertainty section) — minimal bundle renders on
   Apple TV.
2. Play a track in Spotify → title/artist/album and cover appear on the TV.
3. Skip to the next track → display updates within ~1 s.
4. Pause / resume → no spurious re-sends (dedupe holds).
5. Multi-room with a mid-stream receiver drop → the rejoining room shows the
   current track, not a blank or stale one.
6. `--no-metadata` → nothing is sent.
7. Shairport → text metadata accepted or cleanly ignored; audio unaffected
   either way.

## Rejected alternatives

- **Polling SMTC inline in the streaming loop.** No extra thread, but WinRT
  async calls block and stalling the pacing loop causes dropouts. The reason
  `--handoff` uses a thread.
- **Event-driven `MediaPropertiesChanged`.** Lower latency, but tracks change
  every few minutes so there is nothing to gain, and it costs COM callback
  plumbing. Available later if polling proves insufficient.
- **Scraping window titles.** Works without SMTC but is per-app, fragile, and
  gives no artwork.
