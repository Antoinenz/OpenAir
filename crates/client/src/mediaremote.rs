//! Parsing the media commands a receiver sends us.
//!
//! An Apple TV's remote does not control the Apple TV when it is acting as an
//! AirPlay receiver — it asks the *sender* to do something. The request arrives
//! on the reverse event channel as `POST /command` carrying a binary plist:
//!
//! ```text
//! type  = "sendMediaRemoteCommand"
//! value = {
//!     modernMediaRemoteCommand = { params = { ... }, ... },
//!     kMRMediaRemoteOptionSenderID    = <uuid>,
//!     kMRMediaRemoteOptionCommandID   = <uuid>,
//!     kMRMediaRemoteOptionSendOptionsNumber = <int>,
//! }
//! ```
//!
//! Hardware-observed on AppleTV6,2 / AirTunes 960.13.1, 2026-08-21.
//!
//! ## What we do with it
//!
//! OpenAir streams *system* audio, so there is no OpenAir playback to pause.
//! "Play" from the television means "play whatever this machine is playing",
//! and the only sensible target is the platform's own media session — on
//! Windows, the same SMTC the now-playing metadata is read from. Pausing our
//! own stream instead would leave the music player running into a void.
//!
//! Acting on the command therefore has to happen outside this crate, so this
//! module only turns bytes into a [`MediaCommand`].

use std::sync::OnceLock;

use tracing::{debug, info, warn};

/// Where commands are delivered.
///
/// Process-wide rather than per-stream, because that matches the thing being
/// controlled: a machine has one media session, and a command from any
/// receiver in a group means the same thing and goes to the same place.
/// Threading it through every stream would imply a choice that does not exist.
///
/// Set by the CLI, which owns the platform half -- this crate must not reach
/// SMTC directly, for the same reason the TUI must not.
static HANDLER: OnceLock<Box<dyn Fn(MediaCommand) + Send + Sync>> = OnceLock::new();

/// Register the handler for media commands. First call wins.
pub fn set_media_handler_fn(f: impl Fn(MediaCommand) + Send + Sync + 'static) {
    if HANDLER.set(Box::new(f)).is_err() {
        warn!("media command handler already set — ignoring the later one");
    }
}

/// Deliver a command, if anyone is listening.
pub(crate) fn dispatch(cmd: MediaCommand) {
    match HANDLER.get() {
        Some(h) => {
            info!(command = cmd.as_str(), "media command from receiver");
            h(cmd);
        }
        // Not a failure: `--no-tui` runs and the tone/file paths never install
        // one, and a receiver whose remote does nothing is better than a crash.
        None => debug!(command = cmd.as_str(), "media command ignored (no handler)"),
    }
}

/// A transport command from a receiver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaCommand {
    Play,
    Pause,
    TogglePlayPause,
    NextTrack,
    PreviousTrack,
    Stop,
}

impl MediaCommand {
    /// The command names Apple uses on the wire.
    fn from_wire(name: &str) -> Option<Self> {
        match name {
            "play" => Some(MediaCommand::Play),
            "pause" => Some(MediaCommand::Pause),
            "togglePlayPause" => Some(MediaCommand::TogglePlayPause),
            "nextTrack" => Some(MediaCommand::NextTrack),
            "previousTrack" => Some(MediaCommand::PreviousTrack),
            "stop" => Some(MediaCommand::Stop),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            MediaCommand::Play => "play",
            MediaCommand::Pause => "pause",
            MediaCommand::TogglePlayPause => "togglePlayPause",
            MediaCommand::NextTrack => "nextTrack",
            MediaCommand::PreviousTrack => "previousTrack",
            MediaCommand::Stop => "stop",
        }
    }
}

/// Extract a media command from an event-channel request, if it carries one.
///
/// Returns `None` for anything else the receiver sends — `updateInfo` above
/// all, which is by far the most common message and is not an instruction.
pub fn parse(request: &[u8]) -> Option<MediaCommand> {
    let body = body_of(request)?;
    if !body.starts_with(b"bplist00") {
        return None;
    }
    let value: plist::Value = plist::from_bytes(body).ok()?;
    let dict = value.as_dictionary()?;

    // The envelope names the message. Anything that is not a media command is
    // not our business, and mistaking one for another would have us skipping
    // tracks because a receiver said hello.
    if dict.get("type")?.as_string()? != "sendMediaRemoteCommand" {
        return None;
    }

    let name = command_name(dict.get("value")?)?;
    let cmd = MediaCommand::from_wire(&name);
    if cmd.is_none() {
        // Worth a line: the set below came from one hardware capture, so an
        // unknown name is a gap in our table rather than a malformed message.
        debug!(command = %name, "unhandled media remote command");
    }
    cmd
}

/// Find the command name inside the `value` dictionary.
///
/// Searched rather than read from a fixed path: the observed message nests it
/// under `modernMediaRemoteCommand.params`, but that shape is one capture from
/// one tvOS build, and a receiver that nests it one level differently should
/// still be understood. The `type` check in [`parse`] has already established
/// that this whole message is a media command, so a string found in here is
/// not ambiguous.
fn command_name(value: &plist::Value) -> Option<String> {
    fn walk(v: &plist::Value, depth: usize) -> Option<String> {
        if depth > 6 {
            return None;
        }
        match v {
            plist::Value::String(s) if MediaCommand::from_wire(s).is_some() => Some(s.clone()),
            plist::Value::Dictionary(d) => {
                // `command` first where it exists, so a dictionary that also
                // contains an unrelated matching string cannot win.
                if let Some(plist::Value::String(s)) = d.get("command") {
                    if MediaCommand::from_wire(s).is_some() {
                        return Some(s.clone());
                    }
                }
                d.values().find_map(|v| walk(v, depth + 1))
            }
            plist::Value::Array(a) => a.iter().find_map(|v| walk(v, depth + 1)),
            _ => None,
        }
    }
    walk(value, 0)
}

/// The body of an RTSP request: everything past the blank line.
fn body_of(request: &[u8]) -> Option<&[u8]> {
    request
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| &request[i + 4..])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a request shaped like the ones the receiver sends.
    fn request(body: plist::Value) -> Vec<u8> {
        let mut bytes = Vec::new();
        plist::to_writer_binary(&mut bytes, &body).unwrap();
        let mut req = format!(
            "POST /command RTSP/1.0\r\nCSeq: 3\r\nContent-Length: {}\r\n\
             Content-Type: application/x-apple-binary-plist\r\n\r\n",
            bytes.len()
        )
        .into_bytes();
        req.extend_from_slice(&bytes);
        req
    }

    /// The observed shape: command nested under modernMediaRemoteCommand.
    fn command_message(name: &str) -> Vec<u8> {
        let mut params = plist::Dictionary::new();
        params.insert("command".into(), name.into());

        let mut modern = plist::Dictionary::new();
        modern.insert("params".into(), plist::Value::Dictionary(params));

        let mut value = plist::Dictionary::new();
        value.insert(
            "modernMediaRemoteCommand".into(),
            plist::Value::Dictionary(modern),
        );
        value.insert(
            "kMRMediaRemoteOptionCommandID".into(),
            "892C86AE-A6C9-40C8-9DF1-392000CC4446".into(),
        );
        value.insert("kMRMediaRemoteOptionSendOptionsNumber".into(), 0i64.into());

        let mut root = plist::Dictionary::new();
        root.insert("type".into(), "sendMediaRemoteCommand".into());
        root.insert("value".into(), plist::Value::Dictionary(value));
        request(plist::Value::Dictionary(root))
    }

    #[test]
    fn the_observed_play_command_is_understood() {
        // The exact message captured from AppleTV6,2 on 2026-08-21.
        assert_eq!(parse(&command_message("play")), Some(MediaCommand::Play));
    }

    #[test]
    fn every_command_in_the_table_round_trips() {
        for cmd in [
            MediaCommand::Play,
            MediaCommand::Pause,
            MediaCommand::TogglePlayPause,
            MediaCommand::NextTrack,
            MediaCommand::PreviousTrack,
            MediaCommand::Stop,
        ] {
            assert_eq!(
                parse(&command_message(cmd.as_str())),
                Some(cmd),
                "{} did not round trip",
                cmd.as_str()
            );
        }
    }

    #[test]
    fn update_info_is_not_a_command() {
        // By far the most common message on this channel. Acting on it would
        // mean skipping a track every time a receiver introduced itself.
        let mut value = plist::Dictionary::new();
        value.insert("name".into(), "test".into());
        value.insert("model".into(), "AppleTV6,2".into());
        let mut root = plist::Dictionary::new();
        root.insert("type".into(), "updateInfo".into());
        root.insert("value".into(), plist::Value::Dictionary(value));
        assert_eq!(parse(&request(plist::Value::Dictionary(root))), None);
    }

    #[test]
    fn an_update_info_containing_the_word_play_is_still_not_a_command() {
        // The real updateInfo advertises playbackCapabilities, so words like
        // this genuinely appear in it. The envelope type is what decides.
        let mut caps = plist::Dictionary::new();
        caps.insert("supportsPlay".into(), "play".into());
        let mut value = plist::Dictionary::new();
        value.insert(
            "playbackCapabilities".into(),
            plist::Value::Dictionary(caps),
        );
        let mut root = plist::Dictionary::new();
        root.insert("type".into(), "updateInfo".into());
        root.insert("value".into(), plist::Value::Dictionary(value));
        assert_eq!(parse(&request(plist::Value::Dictionary(root))), None);
    }

    #[test]
    fn an_unknown_command_name_is_ignored_rather_than_guessed() {
        assert_eq!(parse(&command_message("beamMeUpScotty")), None);
    }

    #[test]
    fn a_command_at_the_top_of_value_is_found() {
        // Same envelope, flatter nesting -- a plausible variation across tvOS
        // builds, and one we should survive.
        let mut value = plist::Dictionary::new();
        value.insert("command".into(), "pause".into());
        let mut root = plist::Dictionary::new();
        root.insert("type".into(), "sendMediaRemoteCommand".into());
        root.insert("value".into(), plist::Value::Dictionary(value));
        assert_eq!(
            parse(&request(plist::Value::Dictionary(root))),
            Some(MediaCommand::Pause)
        );
    }

    #[test]
    fn malformed_input_never_panics() {
        // This runs on bytes from the network, on the thread that keeps the
        // session alive. A panic here would drop the audio.
        assert_eq!(parse(b""), None);
        assert_eq!(parse(b"POST /command RTSP/1.0\r\n\r\n"), None);
        assert_eq!(parse(b"POST /command RTSP/1.0\r\n\r\nnot a plist"), None);
        assert_eq!(parse(b"\r\n\r\nbplist00"), None);
        assert_eq!(parse(b"\r\n\r\nbplist00\xff\xfe\x00garbage"), None);
        assert_eq!(parse(b"no header block at all"), None);
    }
}
