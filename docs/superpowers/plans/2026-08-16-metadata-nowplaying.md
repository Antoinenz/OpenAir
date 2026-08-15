# Now-Playing Metadata Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Send track title, artist, album and cover art from Windows to the AirPlay receiver so an Apple TV shows what is playing.

**Architecture:** A background thread reads Windows SMTC (System Media Transport Controls) every second, emits a `NowPlaying` struct on an mpsc channel only when the track changes, and the existing buffered streaming loop drains that channel and pushes the data to every live receiver via RTSP `SET_PARAMETER` — the same mechanism `set_volume` already uses. Mirrors the proven `--handoff` volume-bridge pattern.

**Tech Stack:** Rust, `windows` crate 0.54 (WinRT: `Media_Control`, `Storage_Streams`, `Foundation`), existing `openair-rtsp` session layer.

**Spec:** `docs/superpowers/specs/2026-08-15-metadata-nowplaying-design.md`

## Global Constraints

- **Platform:** Windows only. All SMTC code is `#[cfg(windows)]`; the crate must still build on Linux/macOS.
- **windows crate version:** `0.54` exactly (already in the lockfile via cpal — do not introduce a second major version).
- **Never interrupt audio:** every failure path in this feature degrades to "no metadata" with a `warn!`. A failed `SET_PARAMETER`, missing SMTC session, or unreadable thumbnail must not end the stream.
- **Artwork size cap:** 2 MB. Larger images are skipped, never sent — the RTSP control channel also carries volume and `/feedback`.
- **Poll interval:** 1 second.
- **Commit style:** many small focused commits, one logical change each (per `.claude/CLAUDE.md`). No Claude attribution in commit messages.
- **Quality gate for every task:** `cargo test` and `cargo clippy --all-targets -- -D warnings` must both pass before commit.

---

## File Structure

| File | Responsibility |
|------|----------------|
| `crates/rtsp/src/dmap.rs` (create) | Pure DMAP encoding. No I/O, no platform code. |
| `crates/rtsp/src/stream.rs` (modify) | Two new `SET_PARAMETER` methods: `set_metadata`, `set_artwork`. |
| `crates/rtsp/src/lib.rs` (modify) | Export the `dmap` module. |
| `crates/capture/src/nowplaying.rs` (create) | SMTC reader + watcher thread. `#[cfg(windows)]`. |
| `crates/capture/src/lib.rs` (modify) | Register the `nowplaying` module. |
| `crates/capture/Cargo.toml` (modify) | Add WinRT features. |
| `crates/client/src/lib.rs` (modify) | Accept a metadata channel; fan out to receivers; re-send on rejoin. |
| `apps/cli/src/main.rs` (modify) | Start the watcher for `capture`; `--no-metadata` flag. |

**Task order rationale:** Task 1 (DMAP) and Task 2 (RTSP methods) are pure/testable and have no platform dependency, so they land first and enable the Task 3 hardware probe. The probe gates everything after it.

---

### Task 1: DMAP encoder

**Files:**
- Create: `crates/rtsp/src/dmap.rs`
- Modify: `crates/rtsp/src/lib.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `openair_rtsp::dmap::encode_now_playing(title: &str, artist: &str, album: &str) -> Vec<u8>`

DMAP wire format is: 4-byte ASCII tag, then a 4-byte big-endian length, then the payload. Containers nest the same structure. We emit an `mlit` ("listing item") container holding `minm` (title), `asar` (artist), `asal` (album).

- [ ] **Step 1: Write the failing tests**

Create `crates/rtsp/src/dmap.rs`:

```rust
//! DMAP (DAAP) encoding for now-playing metadata.
//!
//! Wire format is a flat TLV: 4-byte ASCII tag, 4-byte big-endian payload
//! length, then the payload. Containers use the same shape and nest their
//! children as the payload — so an `mlit` container holding two fields is
//! just the concatenation of those fields, prefixed by `mlit` and their
//! combined length.

/// One DMAP item: tag, big-endian length, payload.
fn item(tag: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + payload.len());
    out.extend_from_slice(tag);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// Encode a now-playing bundle as an `mlit` container.
///
/// Empty fields are omitted rather than sent blank, so a receiver shows a
/// missing album as absent instead of an empty line.
pub fn encode_now_playing(title: &str, artist: &str, album: &str) -> Vec<u8> {
    let mut body = Vec::new();
    for (tag, value) in [
        (b"minm", title),
        (b"asar", artist),
        (b"asal", album),
    ] {
        if !value.is_empty() {
            body.extend_from_slice(&item(tag, value.as_bytes()));
        }
    }
    item(b"mlit", &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read one item at `pos`, returning (tag, payload, next_pos).
    fn read_item(buf: &[u8], pos: usize) -> ([u8; 4], &[u8], usize) {
        let tag: [u8; 4] = buf[pos..pos + 4].try_into().unwrap();
        let len = u32::from_be_bytes(buf[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let start = pos + 8;
        (tag, &buf[start..start + len], start + len)
    }

    #[test]
    fn item_frames_tag_length_and_payload() {
        let out = item(b"minm", b"hi");
        assert_eq!(&out[0..4], b"minm");
        assert_eq!(&out[4..8], &2u32.to_be_bytes());
        assert_eq!(&out[8..], b"hi");
    }

    #[test]
    fn length_is_big_endian() {
        // 256 bytes must encode as 00 00 01 00, not little-endian 00 01 00 00.
        let out = item(b"minm", &vec![b'x'; 256]);
        assert_eq!(&out[4..8], &[0x00, 0x00, 0x01, 0x00]);
    }

    #[test]
    fn encodes_all_three_fields_in_order() {
        let buf = encode_now_playing("Song", "Artist", "Album");
        let (tag, body, end) = read_item(&buf, 0);
        assert_eq!(&tag, b"mlit");
        assert_eq!(end, buf.len(), "container length must cover the whole body");

        let (t1, v1, p) = read_item(body, 0);
        assert_eq!((&t1, v1), (b"minm", b"Song".as_slice()));
        let (t2, v2, p) = read_item(body, p);
        assert_eq!((&t2, v2), (b"asar", b"Artist".as_slice()));
        let (t3, v3, p) = read_item(body, p);
        assert_eq!((&t3, v3), (b"asal", b"Album".as_slice()));
        assert_eq!(p, body.len());
    }

    #[test]
    fn omits_empty_fields() {
        let buf = encode_now_playing("Song", "", "");
        let (_, body, _) = read_item(&buf, 0);
        let (tag, value, next) = read_item(body, 0);
        assert_eq!(&tag, b"minm");
        assert_eq!(value, b"Song");
        assert_eq!(next, body.len(), "no empty artist/album items");
    }

    #[test]
    fn utf8_payloads_use_byte_length_not_char_count() {
        // "é" is 2 bytes, "日本" is 6 — a char-count length would corrupt the stream.
        let buf = encode_now_playing("café", "日本", "");
        let (_, body, _) = read_item(&buf, 0);
        let (_, title, p) = read_item(body, 0);
        assert_eq!(title, "café".as_bytes());
        assert_eq!(title.len(), 5);
        let (_, artist, _) = read_item(body, p);
        assert_eq!(artist, "日本".as_bytes());
        assert_eq!(artist.len(), 6);
    }

    #[test]
    fn all_empty_produces_an_empty_container() {
        let buf = encode_now_playing("", "", "");
        let (tag, body, end) = read_item(&buf, 0);
        assert_eq!(&tag, b"mlit");
        assert!(body.is_empty());
        assert_eq!(end, buf.len());
    }
}
```

Add to `crates/rtsp/src/lib.rs` alongside the existing module declarations:

```rust
pub mod dmap;
```

- [ ] **Step 2: Run tests to verify they pass**

Run: `cargo test -p openair-rtsp dmap`
Expected: 6 tests pass. (Implementation is written alongside the tests here because the encoder is a single pure function whose behaviour the tests fully pin; splitting it into a separate red step would add a cycle without adding information.)

- [ ] **Step 3: Clippy**

Run: `cargo clippy -p openair-rtsp --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 4: Commit**

```bash
git add crates/rtsp/src/dmap.rs crates/rtsp/src/lib.rs
git commit -m "rtsp: DMAP encoder for now-playing metadata"
```

---

### Task 2: RTSP SET_PARAMETER methods

**Files:**
- Modify: `crates/rtsp/src/stream.rs` (add after `set_volume`, around line 346)

**Interfaces:**
- Consumes: `openair_rtsp::dmap::encode_now_playing` (Task 1).
- Produces:
  - `StreamSession::set_metadata(&mut self, dmap: &[u8], rtptime: u32) -> Result<(), SessionError>`
  - `StreamSession::set_artwork(&mut self, image: &[u8], mime: &str, rtptime: u32) -> Result<(), SessionError>`

These follow the exact shape of the existing `set_volume` (same headers, same `check_ok`), differing only in content type and the added `RTP-Info` header that ties the metadata to a stream position.

- [ ] **Step 1: Add the two methods**

In `crates/rtsp/src/stream.rs`, directly after the `set_volume` method:

```rust
    /// SET_PARAMETER now-playing metadata (DMAP/DAAP-tagged).
    ///
    /// `rtptime` is the current stream position, which the receiver uses to
    /// associate the metadata with the audio it is about to play.
    pub fn set_metadata(&mut self, dmap: &[u8], rtptime: u32) -> Result<(), SessionError> {
        let rtp_info = format!("rtptime={}", rtptime);
        let raw = self.conn.request(
            "SET_PARAMETER",
            &self.uri.clone(),
            &[
                ("Session", "1"),
                ("RTP-Info", &rtp_info),
                ("DACP-ID", &self.dacp_id.clone()),
                ("Active-Remote", &self.active_remote.to_string()),
            ],
            dmap,
            Some("application/x-dmap-tagged"),
        )?;
        check_ok(&raw)
    }

    /// SET_PARAMETER cover art. `mime` is "image/jpeg" or "image/png".
    pub fn set_artwork(
        &mut self,
        image: &[u8],
        mime: &str,
        rtptime: u32,
    ) -> Result<(), SessionError> {
        let rtp_info = format!("rtptime={}", rtptime);
        let raw = self.conn.request(
            "SET_PARAMETER",
            &self.uri.clone(),
            &[
                ("Session", "1"),
                ("RTP-Info", &rtp_info),
                ("DACP-ID", &self.dacp_id.clone()),
                ("Active-Remote", &self.active_remote.to_string()),
            ],
            image,
            Some(mime),
        )?;
        check_ok(&raw)
    }
```

- [ ] **Step 2: Build and clippy**

Run: `cargo clippy -p openair-rtsp --all-targets -- -D warnings`
Expected: clean. (No unit test here — these methods are pure I/O wrappers over `conn.request`, with no logic to assert. Their correctness is established by the Task 3 hardware probe.)

- [ ] **Step 3: Commit**

```bash
git add crates/rtsp/src/stream.rs
git commit -m "rtsp: SET_PARAMETER methods for metadata and artwork"
```

---

### Task 3: Hardware probe — confirm the wire format ⚠️ GATE

**Files:**
- Create: `crates/client/examples/metadata_probe.rs` (throwaway — deleted in Step 5)

**Interfaces:**
- Consumes: Tasks 1 and 2.
- Produces: an *answer*, not code. Everything after this task depends on it.

**Why this task exists:** the DMAP tag set and `mlit` container framing come from reverse-engineering notes, not a checkable specification. Building the SMTC reader, client plumbing and CLI on an unverified format risks three layers of work resting on a wrong guess. This costs ten minutes and removes that risk.

- [ ] **Step 1: Write the probe**

Create `crates/client/examples/metadata_probe.rs`:

```rust
//! THROWAWAY probe: does the receiver render DMAP metadata as we encode it?
//!
//! Usage: cargo run -p openair-client --example metadata_probe -- <ip:port>
//!
//! Streams a tone (so there is a live session) and sends one metadata bundle
//! a few seconds in. Watch the receiver's screen.
use std::net::SocketAddr;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().with_max_level(tracing::Level::DEBUG).init();
    let addr: SocketAddr = std::env::args()
        .nth(1)
        .expect("usage: metadata_probe <ip:port>")
        .parse()?;

    let dmap = openair_rtsp::dmap::encode_now_playing(
        "OpenAir Probe Title",
        "OpenAir Probe Artist",
        "OpenAir Probe Album",
    );
    println!("DMAP bundle ({} bytes): {:02x?}", dmap.len(), &dmap);
    println!("Now run a stream to {addr} and watch the screen.");
    println!("If nothing appears, try: bare items (no mlit wrapper).");
    Ok(())
}
```

> **Note for the implementer:** the probe above only prints the bundle. To
> actually send it you need a live session, so the practical approach is to
> temporarily add a `set_metadata` call inside
> `stream_audio_buffered_multi` right after the initial anchor (near
> `crates/client/src/lib.rs:841`), run `openair capture "<apple tv>" --buffered`,
> and watch the TV. Revert that temporary call before Step 5.

- [ ] **Step 2: Run against the Apple TV**

Run: `cargo run -p openair-client --example metadata_probe -- <appletv-ip>:7000`
then the temporary-call variant described above.

Expected: "OpenAir Probe Title / Artist / Album" appears on the Apple TV's
now-playing screen.

- [ ] **Step 3: If it does NOT display, iterate on framing**

In order, trying one change at a time and re-running:
1. Send the fields **bare** (no `mlit` wrapper) — change `encode_now_playing`
   to return `body` directly instead of `item(b"mlit", &body)`.
2. Wrap in `mlit` but prepend an `mikd` (item kind) item with a single byte
   `0x02`: `item(b"mikd", &[2])` as the first child.
3. Check whether the receiver returned a non-200 status — `check_ok` will
   surface it as a `SessionError`; log the actual response.

- [ ] **Step 4: Record the finding**

Append the confirmed format to the design doc
(`docs/superpowers/specs/2026-08-15-metadata-nowplaying-design.md`) under a new
"Confirmed wire format" heading, stating exactly which framing displayed and
which did not. If `encode_now_playing` needed changing, update Task 1's tests to
match the confirmed format.

- [ ] **Step 5: Delete the probe and commit the finding**

```bash
rm crates/client/examples/metadata_probe.rs
git add docs/superpowers/specs/2026-08-15-metadata-nowplaying-design.md
# plus crates/rtsp/src/dmap.rs if the format needed correcting
git commit -m "docs: confirm DMAP wire format against Apple TV"
```

**GATE:** do not start Task 4 until metadata renders on the device.

---

### Task 4: SMTC now-playing reader

**Files:**
- Create: `crates/capture/src/nowplaying.rs`
- Modify: `crates/capture/src/lib.rs`
- Modify: `crates/capture/Cargo.toml`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:
  - `openair_capture::nowplaying::NowPlaying { title: String, artist: String, album: String, art: Option<(Vec<u8>, &'static str)> }` (derives `Debug, Clone, PartialEq`)
  - `openair_capture::nowplaying::MetadataWatcher::start() -> Result<(MetadataWatcher, std::sync::mpsc::Receiver<NowPlaying>), MetadataError>`
  - `MetadataWatcher` implements `Drop` (stops and joins the thread).
  - `openair_capture::nowplaying::MetadataError` (implements `std::error::Error` via thiserror)

- [ ] **Step 1: Add the WinRT features**

In `crates/capture/Cargo.toml`, extend the existing
`[target.'cfg(windows)'.dependencies]` `windows` feature list with:

```toml
    "Foundation",
    "Media_Control",
    "Storage_Streams",
```

- [ ] **Step 2: Write the failing tests for the pure logic**

Create `crates/capture/src/nowplaying.rs` containing the pure helpers and their
tests first (the COM parts come in Step 4):

```rust
//! Reading Windows "now playing" state (SMTC) for the metadata feature.
//!
//! Every mainstream player — Spotify, browsers, Apple Music, foobar2000 —
//! reports to System Media Transport Controls, so reading SMTC works
//! regardless of which app is producing sound.
//!
//! Polls once a second on a dedicated thread rather than subscribing to
//! `MediaPropertiesChanged`: tracks change every few minutes, so callback
//! latency buys nothing and would cost COM callback plumbing. Same trade-off
//! as `handoff.rs`.

use std::sync::mpsc::{Receiver, Sender};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread::JoinHandle;
use std::time::Duration;

use thiserror::Error;
use tracing::{debug, warn};

/// How often we sample SMTC.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Cover art larger than this is skipped. The RTSP control channel also
/// carries volume and /feedback, so a pathological image must not stall it.
const MAX_ART_BYTES: usize = 2 * 1024 * 1024;

/// What is currently playing on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NowPlaying {
    pub title: String,
    pub artist: String,
    pub album: String,
    /// Cover art bytes and its MIME type, if one was available and decodable.
    pub art: Option<(Vec<u8>, &'static str)>,
}

impl NowPlaying {
    /// The identity of a track for change detection. Art is excluded: it is
    /// fetched only when this triple changes, so it can't participate.
    fn key(&self) -> (&str, &str, &str) {
        (&self.title, &self.artist, &self.album)
    }

    /// True when there is nothing worth sending.
    fn is_empty(&self) -> bool {
        self.title.is_empty() && self.artist.is_empty() && self.album.is_empty()
    }
}

#[derive(Debug, Error)]
pub enum MetadataError {
    #[error("COM initialization failed: {0}")]
    ComInit(String),
    #[error("no media session manager available: {0}")]
    SessionManager(String),
}

/// Identify an image by magic bytes. Returns the MIME type, or `None` for
/// anything we don't recognise — we skip unknown formats rather than
/// mislabel them, since a wrong Content-Type is worse than no artwork.
fn sniff_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some("image/png")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn np(title: &str, artist: &str, album: &str) -> NowPlaying {
        NowPlaying {
            title: title.into(),
            artist: artist.into(),
            album: album.into(),
            art: None,
        }
    }

    #[test]
    fn sniffs_jpeg_and_png() {
        assert_eq!(sniff_image_mime(&[0xFF, 0xD8, 0xFF, 0xE0, 0x00]), Some("image/jpeg"));
        assert_eq!(
            sniff_image_mime(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00]),
            Some("image/png")
        );
    }

    #[test]
    fn rejects_unknown_and_truncated_images() {
        assert_eq!(sniff_image_mime(b"not an image"), None);
        assert_eq!(sniff_image_mime(&[0xFF, 0xD8]), None, "truncated JPEG magic");
        assert_eq!(sniff_image_mime(&[]), None);
    }

    #[test]
    fn key_ignores_artwork() {
        let mut a = np("T", "A", "Al");
        let b = np("T", "A", "Al");
        a.art = Some((vec![1, 2, 3], "image/jpeg"));
        assert_eq!(a.key(), b.key(), "art must not affect track identity");
    }

    #[test]
    fn key_distinguishes_each_field() {
        let base = np("T", "A", "Al");
        assert_ne!(base.key(), np("T2", "A", "Al").key());
        assert_ne!(base.key(), np("T", "A2", "Al").key());
        assert_ne!(base.key(), np("T", "A", "Al2").key());
    }

    #[test]
    fn is_empty_only_when_all_text_fields_are_blank() {
        assert!(np("", "", "").is_empty());
        assert!(!np("T", "", "").is_empty());
        assert!(!np("", "A", "").is_empty());
    }
}
```

Register the module in `crates/capture/src/lib.rs`, next to the existing
`handoff` declaration:

```rust
/// Windows "now playing" metadata (SMTC) for the metadata feature.
#[cfg(windows)]
pub mod nowplaying;
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p openair-capture nowplaying`
Expected: 5 tests pass.

- [ ] **Step 4: Add the SMTC reader and watcher thread**

Append to `crates/capture/src/nowplaying.rs`:

```rust
use windows::Media::Control::GlobalSystemMediaTransportControlsSessionManager;
use windows::Storage::Streams::{DataReader, IRandomAccessStreamReference};
use windows::Win32::System::Com::{
    CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED,
};

/// Read the current session's properties. `Ok(None)` means nothing is
/// playing, which is a normal state, not an error.
fn read_current(
    manager: &GlobalSystemMediaTransportControlsSessionManager,
    want_art: bool,
) -> Option<NowPlaying> {
    let session = manager.GetCurrentSession().ok()?;
    let props = session.TryGetMediaPropertiesAsync().ok()?.get().ok()?;

    let title = props.Title().map(|s| s.to_string()).unwrap_or_default();
    let artist = props.Artist().map(|s| s.to_string()).unwrap_or_default();
    let album = props.AlbumTitle().map(|s| s.to_string()).unwrap_or_default();

    let art = if want_art {
        props.Thumbnail().ok().and_then(|t| read_thumbnail(&t))
    } else {
        None
    };

    Some(NowPlaying { title, artist, album, art })
}

/// Pull the thumbnail bytes out of a WinRT stream reference.
fn read_thumbnail(reference: &IRandomAccessStreamReference) -> Option<(Vec<u8>, &'static str)> {
    let stream = reference.OpenReadAsync().ok()?.get().ok()?;
    let size = stream.Size().ok()? as usize;
    if size == 0 || size > MAX_ART_BYTES {
        if size > MAX_ART_BYTES {
            warn!(size, "cover art too large — skipping");
        }
        return None;
    }
    let reader = DataReader::CreateDataReader(&stream).ok()?;
    reader.LoadAsync(size as u32).ok()?.get().ok()?;
    let mut buf = vec![0u8; size];
    reader.ReadBytes(&mut buf).ok()?;
    let mime = sniff_image_mime(&buf)?;
    Some((buf, mime))
}

/// A running now-playing watcher. Dropping it stops the thread.
pub struct MetadataWatcher {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl MetadataWatcher {
    /// Start watching. Returns the watcher (keep it alive for the stream's
    /// lifetime) and a channel of updates, one per track change.
    pub fn start() -> Result<(MetadataWatcher, Receiver<NowPlaying>), MetadataError> {
        let (tx, rx) = std::sync::mpsc::channel::<NowPlaying>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), MetadataError>>();
        let stop = Arc::new(AtomicBool::new(false));

        let thread_stop = stop.clone();
        let handle = std::thread::Builder::new()
            .name("nowplaying".into())
            .spawn(move || run(thread_stop, ready_tx, tx))
            .expect("spawn nowplaying thread");

        match ready_rx.recv() {
            Ok(Ok(())) => Ok((MetadataWatcher { stop, handle: Some(handle) }, rx)),
            Ok(Err(e)) => {
                let _ = handle.join();
                Err(e)
            }
            Err(_) => {
                let _ = handle.join();
                Err(MetadataError::ComInit("watcher thread exited early".into()))
            }
        }
    }
}

impl Drop for MetadataWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Watcher thread body: COM init, then poll until stopped.
fn run(
    stop: Arc<AtomicBool>,
    ready_tx: Sender<Result<(), MetadataError>>,
    tx: Sender<NowPlaying>,
) {
    unsafe {
        if let Err(e) = CoInitializeEx(None, COINIT_MULTITHREADED).ok() {
            let _ = ready_tx.send(Err(MetadataError::ComInit(e.to_string())));
            return;
        }
    }

    let manager = match GlobalSystemMediaTransportControlsSessionManager::RequestAsync()
        .and_then(|op| op.get())
    {
        Ok(m) => m,
        Err(e) => {
            let _ = ready_tx.send(Err(MetadataError::SessionManager(e.to_string())));
            unsafe { CoUninitialize() };
            return;
        }
    };
    let _ = ready_tx.send(Ok(()));

    let mut last_key: Option<(String, String, String)> = None;
    while !stop.load(Ordering::SeqCst) {
        // Cheap read first (no artwork), so we only pay for the thumbnail
        // when the track has actually changed.
        if let Some(peek) = read_current(&manager, false) {
            let key = (peek.title.clone(), peek.artist.clone(), peek.album.clone());
            let changed = last_key.as_ref() != Some(&key);
            if changed && !peek.is_empty() {
                if let Some(full) = read_current(&manager, true) {
                    debug!(title = %full.title, artist = %full.artist, "now playing changed");
                    if tx.send(full).is_err() {
                        break; // receiver dropped — stream ended
                    }
                }
                last_key = Some(key);
            } else if changed {
                last_key = Some(key);
            }
        }
        std::thread::sleep(POLL_INTERVAL);
    }

    unsafe { CoUninitialize() };
}
```

- [ ] **Step 5: Build, test, clippy**

Run: `cargo test -p openair-capture` then
`cargo clippy -p openair-capture --all-targets -- -D warnings`
Expected: all tests pass, clippy clean.

- [ ] **Step 6: Commit**

```bash
git add crates/capture/src/nowplaying.rs crates/capture/src/lib.rs crates/capture/Cargo.toml
git commit -m "capture: read now-playing metadata from Windows SMTC"
```

---

### Task 5: Client fan-out

**Files:**
- Modify: `crates/client/src/lib.rs`

**Interfaces:**
- Consumes: `openair_capture::nowplaying::NowPlaying` (Task 4),
  `StreamSession::set_metadata` / `set_artwork` (Task 2),
  `openair_rtsp::dmap::encode_now_playing` (Task 1).
- Produces: `stream_audio_buffered_multi(..., metadata_rx: Option<Receiver<NowPlaying>>)` — a **seventh** parameter, appended after the existing `volume_rx`.

The existing signature (line ~703) is:

```rust
pub fn stream_audio_buffered_multi(
    targets: &[GroupTarget],
    source: &mut dyn AudioSource,
    volume_db: Option<f32>,
    latency_ms: u64,
    volume_rx: Option<std::sync::mpsc::Receiver<f32>>,
) -> Result<(), Box<dyn std::error::Error>>
```

- [ ] **Step 1: Add the parameter and a send helper**

Change the signature to append:

```rust
    #[cfg(windows)] metadata_rx: Option<std::sync::mpsc::Receiver<openair_capture::nowplaying::NowPlaying>>,
```

Add this free function next to `drain_latest_volume` (around line 661):

```rust
/// Drain all pending now-playing updates, returning only the most recent.
/// Track changes are rare, but coalescing keeps a burst from queueing several
/// round-trips on the RTSP control channel.
#[cfg(windows)]
fn drain_latest_metadata(
    rx: &std::sync::mpsc::Receiver<openair_capture::nowplaying::NowPlaying>,
) -> Option<openair_capture::nowplaying::NowPlaying> {
    let mut latest = None;
    while let Ok(v) = rx.try_recv() {
        latest = Some(v);
    }
    latest
}

/// Push one now-playing update to a receiver. Failures are logged and
/// swallowed: a receiver that rejects metadata (Shairport may not accept
/// artwork) must keep playing audio.
#[cfg(windows)]
fn send_metadata(
    r: &mut BufferedReceiver,
    np: &openair_capture::nowplaying::NowPlaying,
    rtptime: u32,
) {
    let dmap = openair_rtsp::dmap::encode_now_playing(&np.title, &np.artist, &np.album);
    if let Err(e) = r.session.set_metadata(&dmap, rtptime) {
        warn!(receiver = %r.name, "set_metadata failed (continuing): {e}");
    }
    if let Some((bytes, mime)) = &np.art {
        if let Err(e) = r.session.set_artwork(bytes, mime, rtptime) {
            warn!(receiver = %r.name, "set_artwork failed (continuing): {e}");
        }
    }
}
```

- [ ] **Step 2: Track current metadata and drain it in the loop**

Next to `let mut current_volume_db = volume_db;` (around line 841) add:

```rust
    // Latest now-playing info, re-sent to receivers that rejoin after a drop
    // so a reconnecting room doesn't show a blank or stale screen.
    #[cfg(windows)]
    let mut current_metadata: Option<openair_capture::nowplaying::NowPlaying> = None;
```

Directly after the existing volume-drain block (around line 890-900, the
`if let Some(rx) = &volume_rx { ... }` block), add:

```rust
        // Now-playing metadata: same loop position as volume, so it still
        // runs on the paused/priming `continue` paths.
        #[cfg(windows)]
        if let Some(rx) = &metadata_rx {
            if let Some(np) = drain_latest_metadata(rx) {
                for r in group.iter_mut() {
                    if r.alive {
                        send_metadata(r, &np, rtptime);
                    }
                }
                current_metadata = Some(np);
            }
        }
```

- [ ] **Step 3: Re-send to rejoining receivers**

In the reconnect-handling block (around line 860-870), after the
`finish_reconnect(...)` call succeeds and before `group.push(br)`, add:

```rust
                            #[cfg(windows)]
                            if let Some(np) = &current_metadata {
                                send_metadata(&mut br, np, rtptime);
                            }
```

Note `br` must be declared `mut` for this — change `if let Some(br) =` to
`if let Some(mut br) =`.

- [ ] **Step 4: Update the two internal call sites**

`stream_audio_buffered_with_latency` (around line 357) calls
`stream_audio_buffered_multi`. Append `#[cfg(windows)] None,` as the final
argument.

- [ ] **Step 5: Add tests for the drain helper**

In the existing `mod tests` at the bottom of `crates/client/src/lib.rs`:

```rust
    #[cfg(windows)]
    #[test]
    fn drain_latest_metadata_coalesces_to_newest() {
        use openair_capture::nowplaying::NowPlaying;
        let (tx, rx) = std::sync::mpsc::channel::<NowPlaying>();
        let mk = |t: &str| NowPlaying {
            title: t.into(),
            artist: "A".into(),
            album: "Al".into(),
            art: None,
        };
        tx.send(mk("first")).unwrap();
        tx.send(mk("second")).unwrap();
        assert_eq!(drain_latest_metadata(&rx).unwrap().title, "second");
        assert!(drain_latest_metadata(&rx).is_none());
    }
```

- [ ] **Step 6: Build, test, clippy**

Run: `cargo test` then `cargo clippy --all-targets -- -D warnings`
Expected: all pass. (The CLI will not compile yet — it still calls the old
signature. Fix that in Task 6; if you need a green build here, add
`#[cfg(windows)] None,` to the CLI call site now.)

- [ ] **Step 7: Commit**

```bash
git add crates/client/src/lib.rs
git commit -m "client: fan out now-playing metadata to receivers"
```

---

### Task 6: CLI wiring

**Files:**
- Modify: `apps/cli/src/main.rs`

**Interfaces:**
- Consumes: `MetadataWatcher::start()` (Task 4), the new
  `stream_audio_buffered_multi` signature (Task 5).
- Produces: `--no-metadata` flag; metadata on by default for `capture`.

- [ ] **Step 1: Parse the flag**

Next to the existing `--handoff` extraction:

```rust
    let (raw_args, no_metadata) = extract_flag(&raw_args, "--no-metadata");
```

- [ ] **Step 2: Extend the stream closure**

`stream_fn` currently takes `(targets, source, volume, volume_rx)`. Add a
fifth parameter for the metadata receiver and forward it:

```rust
    let stream_fn = move |targets: &[openair_client::GroupTarget],
                          source: &mut dyn openair_client::AudioSource,
                          volume: Option<f32>,
                          volume_rx: Option<std::sync::mpsc::Receiver<f32>>,
                          metadata_rx: Option<std::sync::mpsc::Receiver<openair_capture::nowplaying::NowPlaying>>| {
        if targets.len() > 1 && !buffered {
            println!("  (multi-room uses the buffered pipeline — enabling --buffered)");
        }
        if buffered || targets.len() > 1 {
            openair_client::stream_audio_buffered_multi(
                targets, source, volume, latency_ms, volume_rx, metadata_rx,
            )
        } else {
            let _ = metadata_rx; // realtime ALAC path carries no metadata
            openair_client::stream_audio(targets[0].addr, &targets[0].device_id, source, volume)
        }
    };
```

> Non-Windows note: the `metadata_rx` parameter and the
> `openair_capture::nowplaying` path are Windows-only. Guard the closure
> parameter with `#[cfg(windows)]` in the same way as the client signature, or
> if that proves unwieldy in a closure, hoist the metadata handling out of the
> closure into the `capture` branch only.

- [ ] **Step 3: Start the watcher in the capture branch**

In the `capture` branch, after the `--handoff` block and before `stream_fn` is
called:

```rust
        // Now-playing metadata (Windows): on by default, --no-metadata opts out.
        // Failure here is never fatal — the stream matters, the screen doesn't.
        #[cfg(windows)]
        let (_metadata_watcher, metadata_rx) = if no_metadata {
            (None, None)
        } else {
            match openair_capture::nowplaying::MetadataWatcher::start() {
                Ok((w, rx)) => {
                    println!("  ♪ sending now-playing metadata (--no-metadata to disable)");
                    (Some(w), Some(rx))
                }
                Err(e) => {
                    println!("  ⚠ now-playing metadata unavailable: {e}");
                    (None, None)
                }
            }
        };
```

Then pass `metadata_rx` to `stream_fn`.

- [ ] **Step 4: Update the `play` and `tone` call sites**

Both pass `None` for the new parameter (a WAV file or test tone has no
now-playing state).

- [ ] **Step 5: Build, test, clippy**

Run: `cargo build`, `cargo test`, `cargo clippy --all-targets -- -D warnings`
Expected: all clean.

- [ ] **Step 6: Verify the flag parses**

Run: `./target/debug/openair.exe --no-metadata` (with no other args)
Expected: normal usage/scan output, no "unknown receiver" error — proving the
flag is consumed rather than treated as a receiver name.

- [ ] **Step 7: Commit**

```bash
git add apps/cli/src/main.rs
git commit -m "cli: send now-playing metadata by default, --no-metadata to disable"
```

---

### Task 7: Documentation

**Files:**
- Modify: `README.md`, `DEVLOG.md`, `STATUS.md`

- [ ] **Step 1: README**

Add to the Flags table:

```markdown
| `--no-metadata` | capture (**Windows**) | off | Stop sending now-playing info. By default `capture` reads the current track from Windows (title, artist, album, cover art) and shows it on the receiver — an Apple TV displays it on its now-playing screen. |
```

Add a note under the existing Notes list:

```markdown
- Now-playing metadata is read from Windows' System Media Transport Controls,
  so it works with any player that reports there (Spotify, browsers, Apple
  Music, foobar2000, …) — no per-app integration.
```

- [ ] **Step 2: DEVLOG**

Add a Session entry at the top covering: what shipped, the SMTC polling
choice and why (tracks change slowly; same reasoning as `--handoff`), the
confirmed DMAP wire format from Task 3, and anything the probe disproved.

- [ ] **Step 3: STATUS**

Add `--no-metadata` to the `apps/cli` row, and add a hardware-verification
block:

```markdown
### Now-playing metadata (Windows, Session 14)

- **Text** — play a track in Spotify; title/artist/album appear on the Apple TV.
- **Cover art** — the album image appears alongside.
- **Track change** — skip; the display updates within ~1 s.
- **No spam** — pausing/resuming does not re-send (dedupe holds); check the
  DEBUG log for a single "now playing changed" per track.
- **Rejoin** — drop and restore a receiver mid-stream; it shows the current
  track, not a blank screen.
- **`--no-metadata`** — nothing is sent.
- **Shairport** — accepts or cleanly ignores; audio unaffected either way.
```

- [ ] **Step 4: Commit**

```bash
git add README.md DEVLOG.md STATUS.md
git commit -m "docs: now-playing metadata"
```

---

## Self-Review

**Spec coverage:**
- SMTC reader → Task 4 ✅
- DMAP encoding → Task 1 ✅
- `set_metadata` / `set_artwork` → Task 2 ✅
- Client fan-out + re-send on rejoin → Task 5 ✅
- CLI default-on + `--no-metadata` → Task 6 ✅
- Probe-before-building requirement → Task 3 ✅ (explicit gate)
- Artwork 2 MB cap → Task 4, `MAX_ART_BYTES` ✅
- MIME sniffing, unknown → skip → Task 4, `sniff_image_mime` ✅
- Degrade-never-fail error handling → Task 4 (`Ok(None)` for no session), Task 5 (`send_metadata` swallows errors), Task 6 (watcher failure is non-fatal) ✅
- Non-goals (scrub bar, DACP, non-Windows) → not implemented anywhere ✅

**Placeholder scan:** none. Every code step contains complete code. Task 3
intentionally has an open outcome — that is the point of a probe, and its
iteration options are enumerated concretely.

**Type consistency:** `NowPlaying` fields (`title`, `artist`, `album`, `art`)
are used identically in Tasks 4, 5 and 6. `art` is
`Option<(Vec<u8>, &'static str)>` throughout, matching `sniff_image_mime`'s
`Option<&'static str>` return. `encode_now_playing(title, artist, album)`
argument order is consistent between Tasks 1, 3 and 5. `set_metadata(dmap,
rtptime)` and `set_artwork(image, mime, rtptime)` match between Tasks 2 and 5.

**Known soft spot:** the `#[cfg(windows)]` parameter threading through the
client signature and the CLI closure (Tasks 5 and 6) is the fiddliest part.
The spec's stated fallback — a platform-neutral struct in `core` — is the
escape hatch if it becomes unwieldy; take it rather than fighting cfg
attributes across a closure boundary.
