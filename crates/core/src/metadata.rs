//! Now-playing metadata shared between the platform readers and the streamer.
//!
//! This lives in `core`, not in `capture`, deliberately. The *reader* is
//! platform-specific (Windows SMTC today, MPRIS on Linux later) but the
//! *shape* of what it produces is not, and the streaming API should not grow
//! `#[cfg(windows)]` in its signature — that would force every caller and
//! every call site to be conditionally compiled too.

/// What is currently playing on the sending machine.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NowPlaying {
    pub title: String,
    pub artist: String,
    pub album: String,
    /// Cover art bytes and its MIME type, if one was available and decodable.
    pub art: Option<(Vec<u8>, &'static str)>,
}

impl NowPlaying {
    /// The identity of a track, for change detection.
    ///
    /// Art is deliberately excluded: it is fetched only *because* this triple
    /// changed, so including it would be circular.
    pub fn key(&self) -> (String, String, String) {
        (
            self.title.clone(),
            self.artist.clone(),
            self.album.clone(),
        )
    }

    /// True when there is nothing worth sending to a receiver.
    pub fn is_empty(&self) -> bool {
        self.title.is_empty() && self.artist.is_empty() && self.album.is_empty()
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
        assert!(NowPlaying::default().is_empty());
    }
}
