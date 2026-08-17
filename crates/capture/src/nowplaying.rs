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

use windows::Media::Control::GlobalSystemMediaTransportControlsSessionManager;
use windows::Storage::Streams::{DataReader, IRandomAccessStreamReference};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

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
    fn key(&self) -> (String, String, String) {
        (
            self.title.clone(),
            self.artist.clone(),
            self.album.clone(),
        )
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

/// Read the current session's properties. `None` means nothing is playing,
/// which is a normal state, not an error.
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

    Some(NowPlaying {
        title,
        artist,
        album,
        art,
    })
}

/// Pull the thumbnail bytes out of a WinRT stream reference.
fn read_thumbnail(reference: &IRandomAccessStreamReference) -> Option<(Vec<u8>, &'static str)> {
    let stream = reference.OpenReadAsync().ok()?.get().ok()?;
    let size = stream.Size().ok()? as usize;
    if size == 0 {
        return None;
    }
    if size > MAX_ART_BYTES {
        warn!(size, "cover art too large — skipping");
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

        // Block until SMTC is reachable so a failure surfaces as a clean
        // error rather than a silently dead watcher.
        match ready_rx.recv() {
            Ok(Ok(())) => Ok((
                MetadataWatcher {
                    stop,
                    handle: Some(handle),
                },
                rx,
            )),
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
        // Cheap read first (no artwork), so we only pay for decoding the
        // thumbnail when the track has actually changed.
        if let Some(peek) = read_current(&manager, false) {
            let key = peek.key();
            if last_key.as_ref() != Some(&key) {
                if !peek.is_empty() {
                    if let Some(full) = read_current(&manager, true) {
                        debug!(
                            title = %full.title,
                            artist = %full.artist,
                            has_art = full.art.is_some(),
                            "now playing changed"
                        );
                        if tx.send(full).is_err() {
                            break; // receiver dropped — stream ended
                        }
                    }
                }
                last_key = Some(key);
            }
        }
        std::thread::sleep(POLL_INTERVAL);
    }

    unsafe { CoUninitialize() };
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
        assert_eq!(
            sniff_image_mime(&[0xFF, 0xD8, 0xFF, 0xE0, 0x00]),
            Some("image/jpeg")
        );
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

    /// Manual check against real SMTC — needs something actually playing, so
    /// it is ignored by default. Run with:
    ///   cargo test -p openair-capture -- --ignored --nocapture smtc_live
    #[test]
    #[ignore]
    fn smtc_live_read() {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok().unwrap();
        }
        let manager = GlobalSystemMediaTransportControlsSessionManager::RequestAsync()
            .and_then(|op| op.get())
            .expect("SMTC session manager");
        match read_current(&manager, true) {
            Some(np) => {
                println!("title:  {:?}", np.title);
                println!("artist: {:?}", np.artist);
                println!("album:  {:?}", np.album);
                match &np.art {
                    Some((bytes, mime)) => println!("art:    {} bytes, {mime}", bytes.len()),
                    None => println!("art:    none"),
                }
            }
            None => println!("no active media session (start playing something)"),
        }
        unsafe { CoUninitialize() };
    }
}
