//! `openair --help`.
//!
//! Written by hand rather than derived from a parser, because OpenAir's
//! argument handling is hand-rolled: flags are extracted from anywhere in the
//! command line by [`crate::util`], so there is no parser object to ask.
//!
//! The risk that creates is drift — a flag added to `main` and never mentioned
//! here. The `every_flag_the_parser_knows_is_documented` test closes it by
//! reading `main.rs` at compile time and checking every flag literal appears
//! below, so an undocumented flag fails the build rather than going unnoticed.

/// What the user asked to be told about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Help {
    /// The whole thing.
    All,
    /// One command, named and known.
    Command(&'static str),
}

/// Commands, in the order they are listed and searched.
///
/// `capture` first: it is what bare `openair` runs, and the one most people
/// want.
const COMMANDS: &[&str] = &[
    "capture",
    "play",
    "tone",
    "pair",
    "discover",
    "devices",
    "restore-audio",
    "help",
];

/// Did the user ask for help, and about what?
///
/// Recognises `--help`, `-h` and a bare `help` command. A command name found
/// alongside any of them narrows the answer, so `openair capture --help` and
/// `openair help capture` mean the same thing — someone reaching for help
/// while typing a command should not be handed the whole manual.
pub fn requested(args: &[String]) -> Option<Help> {
    let asked = args
        .iter()
        .any(|a| a == "--help" || a == "-h" || a == "help");
    if !asked {
        return None;
    }
    let topic = args
        .iter()
        .filter(|a| a.as_str() != "help")
        .find_map(|a| COMMANDS.iter().find(|name| *name == a));
    Some(match topic {
        Some(name) => Help::Command(name),
        None => Help::All,
    })
}

/// The text to print.
pub fn render(help: &Help) -> String {
    match help {
        Help::All => OVERVIEW.to_string(),
        Help::Command(name) => detail(name).to_string(),
    }
}

const OVERVIEW: &str = "\
OpenAir — stream your PC's audio to AirPlay 2 receivers.

USAGE
  openair                            pick receivers on screen, then stream
  openair <command> <receiver>... [options]

  A <receiver> is a discovered name — matched case-insensitively on any part
  of it, so `pool` finds \"Pool Room\" — or an explicit ip:port such as
  192.168.1.106:7000. Streaming commands take several receivers: two or more
  plays the same audio, time-synchronized, in every room at once.

COMMANDS
  capture <receiver>... [seconds]    stream live system audio
  play <receiver>... <file.wav>      stream a WAV file
  tone <receiver>... [seconds]       stream a 440 Hz test tone
  pair <receiver>                    pair with a receiver that asks for a PIN
  discover [seconds]                 list receivers and what they advertise
  devices                            list audio output devices (Windows)
  restore-audio                      undo an interrupted --handoff (Windows)
  help [command]                     detail on one command

AUDIO
  --buffered            Use the AAC pipeline, whose latency you choose,
                        instead of realtime ALAC's fixed ~2 s. Implied by
                        --handoff and by naming more than one receiver.
  --latency <ms>        Starting buffered latency; default 500. An `ms`
                        suffix is fine. Raised automatically, in steps,
                        if the stream cuts out.
  --volume <dBFS>       Playback volume; 0 is full scale, default -8.
  --offset <name=ms>    Per-receiver play delay, e.g. --offset pool=+80ms.
                        Repeatable. Lines rooms up when one has a slow amp.

WINDOWS
  --handoff             Route system audio through a virtual cable, so the
                        PC speakers fall silent and the Windows volume
                        controls AirPlay. Needs VB-CABLE installed.
  --handoff-device <n>  Force the device --handoff routes through, by name.
  --no-metadata         Stop sending the current track to the receiver.
  --no-media-controls   Ignore play/pause/skip from a receiver's own remote.

OUTPUT
  --no-tui              Plain scrolling text: no picker, no dashboard. Used
                        automatically when stdout is not a terminal.
  --log                 Also write this run to logs/openair-<time>.log.
  --debug [0-2]         Console detail; bare --debug means 1.
  --help, -h            This text.

NETWORK
  --bind <ip>           Force the local address connections come from. Only
                        needed if OpenAir picks the wrong interface.

DIAGNOSTICS
  --random-sender-id    Announce a fresh sender identity for this run.
  --impersonate-iphone  Announce an iPhone's model and OS instead of ours.

Flags may appear anywhere in the command line.
Full documentation: https://github.com/Antoinenz/OpenAir/tree/main/docs
";

fn detail(name: &str) -> &'static str {
    match name {
        "capture" => CAPTURE,
        "play" => PLAY,
        "tone" => TONE,
        "pair" => PAIR,
        "discover" => DISCOVER,
        "devices" => DEVICES,
        "restore-audio" => RESTORE_AUDIO,
        _ => OVERVIEW,
    }
}

const CAPTURE: &str = "\
openair capture <receiver>... [seconds] — stream live system audio.

Captures whatever the machine is playing (WASAPI loopback of the default
output device) and streams it. Runs until Ctrl+C, or for `seconds` if you
give a number. Pausing the music pauses the stream, and resuming resumes it.

  openair capture \"Living Room\"
  openair capture \"Living Room\" \"Pool Room\" --latency 800
  openair capture pool 30 --volume -14
  openair capture 192.168.1.106:7000 --buffered

Naming two or more receivers plays the same audio in every room, on one
shared clock, mixing receiver types freely — an Apple TV and a Shairport
box stay in sync with each other.

Windows: --handoff silences the PC speakers and hands the Windows volume
control to AirPlay; the current track is sent to the receiver unless you
pass --no-metadata.

Options: --buffered --latency --volume --offset --handoff --handoff-device
         --no-metadata --no-media-controls --bind --no-tui --log --debug
";

const PLAY: &str = "\
openair play <receiver>... <file.wav> — stream a WAV file.

The file is the last argument. Any sample rate, 16-bit integer or 32-bit
float, mono or stereo — converted to what AirPlay carries automatically.

  openair play \"Pool Room\" song.wav --buffered
  openair play \"Living Room\" \"Pool Room\" album.wav

Options: --buffered --latency --volume --offset --bind --no-tui --log --debug
";

const TONE: &str = "\
openair tone <receiver>... [seconds] — stream a 440 Hz test tone.

The quickest way to prove a receiver works. Ten seconds unless you say
otherwise.

  openair tone \"Living Room\"
  openair tone \"Living Room\" 30 --volume -20

Options: --buffered --latency --volume --offset --bind --no-tui --log --debug
";

const PAIR: &str = "\
openair pair <receiver> — pair with a receiver that asks for a PIN.

The receiver shows four digits; type them in. Credentials are saved, so this
is needed once per device.

  openair pair \"Living Room\"

Rarely necessary now: the terminal UI pairs a receiver as part of choosing
it. Apple TV and HomePod need pairing; Shairport Sync does not.
";

const DISCOVER: &str = "\
openair discover — list AirPlay receivers and what they advertise.

Browses for five seconds unless you say otherwise, then prints each
receiver's address, model, AirTunes version, device id and feature bits.
Read-only: it connects to nothing.

  openair discover
  openair discover 15

The feature bits are what a receiver problem gets diagnosed against, so
include this output in a bug report. Two of them decide how a session is
set up: PTP-required receivers cannot fall back to NTP, and a receiver
asking for MFi auth-setup needs a step beyond ordinary pairing.
";

const DEVICES: &str = "\
openair devices — list audio output devices (Windows).

Shows every output device and marks the one --handoff would route through.
Read-only: it changes nothing.

  openair devices
";

const RESTORE_AUDIO: &str = "\
openair restore-audio — put the output device back (Windows).

A --handoff run restores your original output device when it exits, even on
Ctrl+C. If it was killed outright it never got the chance, and the PC stays
routed to a silent virtual cable. This puts it back.

  openair restore-audio
";

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    /// Every `"--flag"` literal `main.rs` matches on.
    ///
    /// Only literals that are *exactly* a flag count: the scan requires a
    /// closing quote straight after the name, so the error message
    /// `"--bind expects an IP address"` is not mistaken for a flag.
    fn flags_in(src: &str) -> Vec<String> {
        let bytes = src.as_bytes();
        let mut found = Vec::new();
        for (i, _) in src.match_indices("\"--") {
            let start = i + 1;
            let mut end = start + 2;
            while end < bytes.len() && (bytes[end].is_ascii_lowercase() || bytes[end] == b'-') {
                end += 1;
            }
            if bytes.get(end) == Some(&b'"') && end > start + 2 {
                let flag = src[start..end].to_string();
                if !found.contains(&flag) {
                    found.push(flag);
                }
            }
        }
        found
    }

    #[test]
    fn every_flag_the_parser_knows_is_documented() {
        // Read at compile time, so this cannot quietly pass by failing to
        // find the file.
        let src = include_str!("main.rs");
        let text = render(&Help::All);
        let flags = flags_in(src);
        assert!(flags.len() > 10, "the scan found almost nothing: {flags:?}");

        let missing: Vec<&String> = flags.iter().filter(|f| !text.contains(*f)).collect();
        assert!(
            missing.is_empty(),
            "flags accepted by main.rs but absent from --help: {missing:?}\n\
             Add them to OVERVIEW in help.rs — a flag nobody can discover is a\n\
             flag nobody uses."
        );
    }

    #[test]
    fn the_scan_ignores_prose_that_merely_starts_with_a_flag() {
        let found = flags_in("println!(\"--bind expects an IP address\"); x == \"--log\"");
        assert_eq!(found, vec!["--log".to_string()]);
    }

    #[test]
    fn every_command_is_listed_in_the_overview() {
        for name in COMMANDS {
            assert!(
                OVERVIEW.contains(name),
                "`{name}` is a command but is not in the command list"
            );
        }
    }

    #[test]
    fn every_command_with_a_page_of_its_own_is_reachable() {
        // `help` has none deliberately -- its page is the overview.
        for name in COMMANDS.iter().filter(|n| **n != "help") {
            let text = detail(name);
            assert_ne!(text, OVERVIEW, "`{name}` falls through to the overview");
            assert!(text.contains(name), "`{name}`'s page does not name it");
        }
    }

    #[test]
    fn the_help_flags_are_recognised() {
        assert_eq!(requested(&args("--help")), Some(Help::All));
        assert_eq!(requested(&args("-h")), Some(Help::All));
        assert_eq!(requested(&args("help")), Some(Help::All));
    }

    #[test]
    fn a_command_narrows_the_answer() {
        // Both spellings, because someone reaching for help mid-command
        // should not be handed the whole manual.
        assert_eq!(
            requested(&args("help capture")),
            Some(Help::Command("capture"))
        );
        assert_eq!(
            requested(&args("capture --help")),
            Some(Help::Command("capture"))
        );
        assert_eq!(
            requested(&args("capture pool -h")),
            Some(Help::Command("capture"))
        );
        assert_eq!(
            requested(&args("help restore-audio")),
            Some(Help::Command("restore-audio"))
        );
    }

    #[test]
    fn an_unknown_topic_falls_back_to_the_overview() {
        // Better than an error: they asked for help, so give them some.
        assert_eq!(requested(&args("help wibble")), Some(Help::All));
        assert_eq!(requested(&args("--help wibble")), Some(Help::All));
    }

    #[test]
    fn ordinary_commands_do_not_ask_for_help() {
        assert_eq!(requested(&args("capture pool")), None);
        assert_eq!(requested(&args("tone 10")), None);
        assert_eq!(requested(&[]), None);
    }

    #[test]
    fn a_receiver_called_help_is_still_help() {
        // Ambiguous, and resolved towards help deliberately: someone with a
        // speaker named "help" can still reach it by ip:port, but someone who
        // wants help has nothing else to try.
        assert!(requested(&args("capture help")).is_some());
    }

    #[test]
    fn the_overview_fits_a_standard_terminal() {
        for line in OVERVIEW.lines() {
            assert!(
                line.chars().count() <= 79,
                "{} chars, wraps on an 80-column terminal: {line}",
                line.chars().count()
            );
        }
    }

    #[test]
    fn every_page_fits_a_standard_terminal() {
        for name in COMMANDS {
            for line in detail(name).lines() {
                assert!(
                    line.chars().count() <= 79,
                    "`{name}`: {} chars, wraps on an 80-column terminal: {line}",
                    line.chars().count()
                );
            }
        }
    }
}
