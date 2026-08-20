use anyhow::Result;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing_subscriber::EnvFilter;

use util::{extract_flag, extract_volume};

const DEFAULT_DEVICE_ID: &str = "AA:BB:CC:DD:EE:FF";
const DEFAULT_VOLUME_DB: f32 = -8.0;

/// Pure helpers factored out for unit testing (no network/audio access).
mod util {
    /// Extracts an optional `--volume <db>` flag from anywhere in `args`,
    /// returning the remaining positional args (flag and its value removed)
    /// and the parsed volume. Falls back to `default` if the flag is
    /// absent, or if present but its value fails to parse as `f32`.
    pub fn extract_volume(args: &[String], default: f32) -> (Vec<String>, f32) {
        let mut remaining = Vec::with_capacity(args.len());
        let mut volume = default;
        let mut skip_next = false;

        for (i, arg) in args.iter().enumerate() {
            if skip_next {
                skip_next = false;
                continue;
            }
            if arg == "--volume" {
                if let Some(v) = args.get(i + 1) {
                    volume = v.parse().unwrap_or(default);
                    skip_next = true;
                }
                continue;
            }
            remaining.push(arg.clone());
        }

        (remaining, volume)
    }

    /// Extracts an optional `<flag> <value>` string pair from anywhere in
    /// `args`, returning the remaining positional args and the value if the
    /// flag was present with one.
    pub fn extract_value(args: &[String], flag: &str) -> (Vec<String>, Option<String>) {
        let mut remaining = Vec::with_capacity(args.len());
        let mut value = None;
        let mut skip_next = false;
        for (i, arg) in args.iter().enumerate() {
            if skip_next {
                skip_next = false;
                continue;
            }
            if arg == flag {
                if let Some(v) = args.get(i + 1) {
                    value = Some(v.clone());
                    skip_next = true;
                }
                continue;
            }
            remaining.push(arg.clone());
        }
        (remaining, value)
    }

    /// Extracts a boolean flag (no value) from anywhere in `args`, returning
    /// the remaining positional args (flag removed) and whether it was
    /// present.
    pub fn extract_flag(args: &[String], flag: &str) -> (Vec<String>, bool) {
        let mut remaining = Vec::with_capacity(args.len());
        let mut present = false;
        for arg in args {
            if arg == flag {
                present = true;
                continue;
            }
            remaining.push(arg.clone());
        }
        (remaining, present)
    }

    /// Highest `--debug` level we define. Values above this are treated as a
    /// positional argument, not a level — so `tone x --debug 10` still means
    /// "10 seconds", which is what someone typing that almost certainly wants.
    pub const MAX_DEBUG_LEVEL: u8 = 2;

    /// Extracts `--debug [level]`. Bare `--debug` means level 1; an immediately
    /// following `0`–`2` sets the level explicitly. Absent → level 0.
    pub fn extract_debug_level(args: &[String]) -> (Vec<String>, u8) {
        let mut remaining = Vec::with_capacity(args.len());
        let mut level = 0u8;
        let mut skip_next = false;
        for (i, arg) in args.iter().enumerate() {
            if skip_next {
                skip_next = false;
                continue;
            }
            if arg == "--debug" {
                level = 1;
                if let Some(n) = args.get(i + 1).and_then(|v| v.parse::<u8>().ok()) {
                    if n <= MAX_DEBUG_LEVEL {
                        level = n;
                        skip_next = true;
                    }
                }
                continue;
            }
            remaining.push(arg.clone());
        }
        (remaining, level)
    }

    /// Tracing filter directives for a verbosity level.
    ///
    /// - **0** (default): quiet. The CLI's own `println!` narration carries the
    ///   normal story, so only warnings and errors need to reach the console.
    ///   `mdns_sd` is silenced entirely — its sole output is a spurious
    ///   "failed to send response of shutdown" error on every clean exit.
    /// - **1**: our DEBUG plus third-party INFO — the historical default.
    /// - **2**: our TRACE, including the full decrypted body of everything the
    ///   receiver sends. Third-party crates stay at INFO deliberately: raising
    ///   them to DEBUG buries the interesting traffic under ~55k lines of mDNS
    ///   internals, which is the opposite of useful.
    pub fn debug_directives(level: u8) -> &'static [&'static str] {
        match level {
            0 => &["openair=warn", "warn", "mdns_sd=off"],
            1 => &["openair=debug", "info"],
            _ => &["openair=trace", "info"],
        }
    }

    /// Format a Unix timestamp as `YYYYMMDD-HHMMSS` (UTC) for a log filename.
    ///
    /// UTC deliberately: the log lines themselves are UTC, so a filename in the
    /// same zone makes it obvious which file covers which events.
    pub fn timestamp_name(unix_secs: u64) -> String {
        let (y, mo, d) = civil_from_days((unix_secs / 86_400) as i64);
        let s = unix_secs % 86_400;
        format!(
            "{y:04}{mo:02}{d:02}-{:02}{:02}{:02}",
            s / 3600,
            (s % 3600) / 60,
            s % 60
        )
    }

    /// Days since 1970-01-01 -> (year, month, day). Howard Hinnant's
    /// `civil_from_days`, the standard branch-free calendar conversion.
    fn civil_from_days(z: i64) -> (i64, u32, u32) {
        let z = z + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
        (if m <= 2 { y + 1 } else { y }, m, d)
    }

    /// Extracts an optional `--latency <ms>` flag (buffered pipeline anchor
    /// lead / end-to-end latency). Same semantics as `extract_volume`.
    pub fn extract_latency(args: &[String], default: u64) -> (Vec<String>, u64) {
        let mut remaining = Vec::with_capacity(args.len());
        let mut latency = default;
        let mut skip_next = false;
        for (i, arg) in args.iter().enumerate() {
            if skip_next {
                skip_next = false;
                continue;
            }
            if arg == "--latency" {
                if let Some(v) = args.get(i + 1) {
                    latency = v.parse().unwrap_or(default);
                    skip_next = true;
                }
                continue;
            }
            remaining.push(arg.clone());
        }
        (remaining, latency)
    }

    use std::collections::HashMap;

    /// Extracts any number of `--offset <name=ms>` flags (per-receiver anchor
    /// delay for multi-room), returning the remaining positional args and a
    /// map of lowercased receiver-name → offset in ms. The value may carry an
    /// optional `+`/`-` sign and an optional `ms` suffix, e.g.
    /// `--offset "Pool Room=+80ms"`.
    pub fn extract_offsets(args: &[String]) -> (Vec<String>, HashMap<String, i64>) {
        let mut remaining = Vec::with_capacity(args.len());
        let mut offsets = HashMap::new();
        let mut skip_next = false;
        for (i, arg) in args.iter().enumerate() {
            if skip_next {
                skip_next = false;
                continue;
            }
            if arg == "--offset" {
                if let Some(spec) = args.get(i + 1) {
                    if let Some((name, ms)) = parse_offset_spec(spec) {
                        offsets.insert(name, ms);
                    }
                    skip_next = true;
                }
                continue;
            }
            remaining.push(arg.clone());
        }
        (remaining, offsets)
    }

    /// Parses one `name=ms` offset spec into (lowercased name, ms). Accepts a
    /// trailing `ms` and a leading sign on the value.
    fn parse_offset_spec(spec: &str) -> Option<(String, i64)> {
        let (name, val) = spec.rsplit_once('=')?;
        let val = val.trim().trim_end_matches("ms").trim();
        let ms: i64 = val.parse().ok()?;
        Some((name.trim().to_lowercase(), ms))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn extract_offsets_parses_signed_and_ms_suffix() {
            let args = vec![
                "capture".to_string(),
                "pool".to_string(),
                "--offset".to_string(),
                "Pool Room=+80ms".to_string(),
                "--offset".to_string(),
                "test=-15".to_string(),
            ];
            let (rest, offs) = extract_offsets(&args);
            assert_eq!(rest, vec!["capture".to_string(), "pool".to_string()]);
            assert_eq!(offs.get("pool room"), Some(&80));
            assert_eq!(offs.get("test"), Some(&-15));
        }

        #[test]
        fn extract_volume_present() {
            let args = vec!["capture".to_string(), "--volume".to_string(), "-12.5".to_string()];
            let (rest, vol) = extract_volume(&args, -8.0);
            assert_eq!(rest, vec!["capture".to_string()]);
            assert_eq!(vol, -12.5);
        }

        #[test]
        fn extract_volume_absent_uses_default() {
            let args = vec!["capture".to_string(), "127.0.0.1:7000".to_string()];
            let (rest, vol) = extract_volume(&args, -8.0);
            assert_eq!(rest, args);
            assert_eq!(vol, -8.0);
        }

        #[test]
        fn extract_volume_malformed_uses_default() {
            let args = vec![
                "capture".to_string(),
                "--volume".to_string(),
                "not-a-number".to_string(),
            ];
            let (rest, vol) = extract_volume(&args, -8.0);
            assert_eq!(rest, vec!["capture".to_string()]);
            assert_eq!(vol, -8.0);
        }

        #[test]
        fn extract_volume_mid_args() {
            let args = vec![
                "capture".to_string(),
                "127.0.0.1:7000".to_string(),
                "--volume".to_string(),
                "-3".to_string(),
                "30".to_string(),
            ];
            let (rest, vol) = extract_volume(&args, -8.0);
            assert_eq!(
                rest,
                vec!["capture".to_string(), "127.0.0.1:7000".to_string(), "30".to_string()]
            );
            assert_eq!(vol, -3.0);
        }

        #[test]
        fn extract_value_present_and_absent() {
            let args = vec![
                "capture".to_string(),
                "pool".to_string(),
                "--handoff-device".to_string(),
                "CABLE Input".to_string(),
            ];
            let (rest, val) = extract_value(&args, "--handoff-device");
            assert_eq!(rest, vec!["capture".to_string(), "pool".to_string()]);
            assert_eq!(val, Some("CABLE Input".to_string()));

            let bare = vec!["capture".to_string(), "pool".to_string()];
            let (rest, val) = extract_value(&bare, "--handoff-device");
            assert_eq!(rest, bare);
            assert_eq!(val, None);
        }

        #[test]
        fn extract_flag_present() {
            let args = vec!["tone".to_string(), "127.0.0.1:7000".to_string(), "--buffered".to_string()];
            let (rest, present) = extract_flag(&args, "--buffered");
            assert_eq!(rest, vec!["tone".to_string(), "127.0.0.1:7000".to_string()]);
            assert!(present);
        }

        #[test]
        fn extract_flag_absent() {
            let args = vec!["tone".to_string(), "127.0.0.1:7000".to_string()];
            let (rest, present) = extract_flag(&args, "--buffered");
            assert_eq!(rest, args);
            assert!(!present);
        }

        #[test]
        fn extract_debug_level_bare_flag_is_level_one() {
            let args = vec!["capture".into(), "test".into(), "--debug".into()];
            let (rest, level) = extract_debug_level(&args);
            assert_eq!(rest, vec!["capture".to_string(), "test".to_string()]);
            assert_eq!(level, 1);
        }

        #[test]
        fn extract_debug_level_absent_is_zero() {
            let args = vec!["capture".to_string(), "test".to_string()];
            let (rest, level) = extract_debug_level(&args);
            assert_eq!(rest, args);
            assert_eq!(level, 0);
        }

        #[test]
        fn extract_debug_level_explicit_level_is_consumed() {
            let args = vec!["capture".into(), "test".into(), "--debug".into(), "2".into()];
            let (rest, level) = extract_debug_level(&args);
            assert_eq!(rest, vec!["capture".to_string(), "test".to_string()]);
            assert_eq!(level, 2);
        }

        /// `tone x --debug 10` must still mean "10 seconds": a number above the
        /// highest level we define is a positional, not a verbosity.
        #[test]
        fn extract_debug_level_leaves_out_of_range_numbers_as_positional() {
            let args = vec!["tone".into(), "test".into(), "--debug".into(), "10".into()];
            let (rest, level) = extract_debug_level(&args);
            assert_eq!(
                rest,
                vec!["tone".to_string(), "test".to_string(), "10".to_string()]
            );
            assert_eq!(level, 1, "bare --debug still enables level 1");
        }

        #[test]
        fn extract_debug_level_non_numeric_next_arg_is_untouched() {
            let args = vec!["capture".into(), "--debug".into(), "--handoff".into()];
            let (rest, level) = extract_debug_level(&args);
            assert_eq!(rest, vec!["capture".to_string(), "--handoff".to_string()]);
            assert_eq!(level, 1);
        }

        #[test]
        fn debug_directives_get_progressively_louder() {
            assert_eq!(debug_directives(0)[0], "openair=warn");
            assert_eq!(debug_directives(1)[0], "openair=debug");
            assert_eq!(debug_directives(2)[0], "openair=trace");
            // Anything above the max saturates rather than falling back to quiet.
            assert_eq!(debug_directives(9)[0], "openair=trace");
        }

        /// Raising third-party crates to DEBUG buries the traffic we care about
        /// under tens of thousands of mDNS lines, so no level may do it.
        #[test]
        fn debug_directives_never_raise_third_party_above_info() {
            for level in 0..=3 {
                for d in debug_directives(level) {
                    if !d.starts_with("openair") {
                        assert!(
                            !d.contains("debug") && !d.contains("trace"),
                            "level {level} directive {d:?} would flood the log"
                        );
                    }
                }
            }
        }

        #[test]
        fn quiet_level_silences_the_spurious_mdns_shutdown_error() {
            assert!(debug_directives(0).contains(&"mdns_sd=off"));
            // It should come back as soon as any debugging is asked for.
            assert!(!debug_directives(1).contains(&"mdns_sd=off"));
        }

        #[test]
        fn timestamp_name_formats_known_epochs() {
            assert_eq!(timestamp_name(0), "19700101-000000");
            // 2001-09-09T01:46:40Z — the classic 1e9 epoch second.
            assert_eq!(timestamp_name(1_000_000_000), "20010909-014640");
            // 2026-08-17T09:18:20Z, from a real capture session.
            assert_eq!(timestamp_name(1_786_958_300), "20260817-091820");
        }

        #[test]
        fn timestamp_name_handles_leap_day() {
            // 2024-02-29T12:00:00Z — a leap day, where naive date math breaks.
            assert_eq!(timestamp_name(1_709_208_000), "20240229-120000");
        }

        #[test]
        fn timestamp_names_sort_chronologically() {
            // Filenames are sorted by name when comparing runs, so lexical
            // order must match time order.
            let a = timestamp_name(1_786_958_300);
            let b = timestamp_name(1_786_958_400);
            assert!(a < b);
        }

        #[test]
        fn extract_flag_removes_no_tui() {
            let args = vec!["capture".into(), "--no-tui".into(), "Pool".into()];
            let (rest, found) = extract_flag(&args, "--no-tui");
            assert!(found);
            assert_eq!(rest, ["capture", "Pool"], "the flag must not reach dispatch");
        }
    }
}

/// Resolves a `<ip:port>` or receiver-name argument to a socket address and
/// device id. Direct `ip:port` input always uses the default device id.
/// A name is matched case-insensitively against discovered device names
/// (cleaned of the mDNS service suffix); zero or multiple matches print the
/// discovered names and return `None`.
fn resolve_receiver(arg: &str) -> Option<(SocketAddr, String)> {
    if let Ok(addr) = arg.parse::<SocketAddr>() {
        return Some((addr, DEFAULT_DEVICE_ID.to_string()));
    }

    println!("'{}' is not an ip:port — searching for a receiver named like it (5s)...", arg);
    let mut devices = Vec::new();
    if let Err(e) = openair_discovery::browse(Duration::from_secs(5), |d| devices.push(d)) {
        println!("  ✗ discovery failed: {}", e);
        return None;
    }

    let needle = arg.to_lowercase();
    let matches: Vec<_> = devices
        .iter()
        .filter(|d| d.display_name().to_lowercase().contains(&needle))
        .collect();

    match matches.len() {
        1 => {
            let dev = matches[0];
            let addr = SocketAddr::new(dev.addr, dev.port);
            let device_id = dev
                .txt
                .device_id
                .clone()
                .unwrap_or_else(|| DEFAULT_DEVICE_ID.to_string());
            Some((addr, device_id))
        }
        0 => {
            println!("No receiver matched '{}'. Discovered device(s):", arg);
            for d in &devices {
                println!("  - {}", d.display_name());
            }
            None
        }
        _ => {
            println!("Multiple receivers matched '{}':", arg);
            for d in &matches {
                println!("  - {}", d.display_name());
            }
            None
        }
    }
}

/// Resolve several receiver arguments (`ip:port` or names) with at most ONE
/// mDNS browse shared by all names, applying any per-receiver `--offset`
/// (keyed case-insensitively by the argument the user typed). Returns `None`
/// (after printing why) if any argument doesn't resolve to exactly one
/// receiver.
fn resolve_receivers(
    args: &[String],
    offsets: &std::collections::HashMap<String, i64>,
) -> Option<Vec<openair_client::GroupTarget>> {
    let mut out: Vec<openair_client::GroupTarget> = Vec::new();
    let names: Vec<&String> = args
        .iter()
        .filter(|a| a.parse::<SocketAddr>().is_err())
        .collect();

    let mut devices = Vec::new();
    if !names.is_empty() {
        println!(
            "Searching for receiver(s) {} (5s)...",
            names
                .iter()
                .map(|n| format!("'{n}'"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        if let Err(e) = openair_discovery::browse(Duration::from_secs(5), |d| devices.push(d)) {
            println!("  ✗ discovery failed: {}", e);
            return None;
        }
    }

    for arg in args {
        let offset_ms = offsets.get(&arg.to_lowercase()).copied().unwrap_or(0);
        if let Ok(addr) = arg.parse::<SocketAddr>() {
            out.push(openair_client::GroupTarget {
                addr,
                device_id: DEFAULT_DEVICE_ID.to_string(),
                offset_ms,
            });
            continue;
        }
        let needle = arg.to_lowercase();
        let matches: Vec<_> = devices
            .iter()
            .filter(|d| d.display_name().to_lowercase().contains(&needle))
            .collect();
        match matches.len() {
            1 => {
                let dev = matches[0];
                let device_id = dev
                    .txt
                    .device_id
                    .clone()
                    .unwrap_or_else(|| DEFAULT_DEVICE_ID.to_string());
                out.push(openair_client::GroupTarget {
                    addr: SocketAddr::new(dev.addr, dev.port),
                    device_id,
                    offset_ms,
                });
            }
            0 => {
                println!("No receiver matched '{}'. Discovered device(s):", arg);
                for d in &devices {
                    println!("  - {}", d.display_name());
                }
                return None;
            }
            _ => {
                println!("Multiple receivers matched '{}':", arg);
                for d in &matches {
                    println!("  - {}", d.display_name());
                }
                return None;
            }
        }
    }
    Some(out)
}

/// Placeholder receiver argument used when the picker chose the receivers.
///
/// The capture branch is reached by argument shape, and the picker's result is
/// a list of resolved targets rather than a list of names — so it stands in for
/// them and is never parsed.
const PICKER_SELECTION: &str = "<picker>";

/// Set up logging: always to the console, and additionally to a timestamped
/// file under `logs/` when `--log` is given.
///
/// A file copy matters because the interesting bugs here only show up in a
/// full run's log — the 30 s Apple TV teardown was found by comparing event
/// ordering across runs, which is painful to do from a scrollback buffer.
fn init_logging(
    debug_level: u8,
    to_file: bool,
    panel: openair_tui::LogBuffer,
) -> Result<Option<std::path::PathBuf>> {
    use tracing_subscriber::prelude::*;

    let build_filter = |level: u8| -> Result<EnvFilter> {
        let mut filter = EnvFilter::from_default_env();
        for directive in util::debug_directives(level) {
            filter = filter.add_directive(directive.parse()?);
        }
        Ok(filter)
    };

    let console_filter = build_filter(debug_level)?;
    // The log file is for reading a run after the fact, so it stays detailed
    // even when the console is quiet — a clean terminal and a complete log at
    // the same time. `--debug 2` raises the file too.
    let file_filter = build_filter(debug_level.max(1))?;

    let path = if to_file {
        let dir = std::path::PathBuf::from("logs");
        std::fs::create_dir_all(&dir)?;
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Some(dir.join(format!("openair-{}.log", util::timestamp_name(secs))))
    } else {
        None
    };

    // `Option<Layer>` is itself a Layer, so the file sink can be absent without
    // the two branches having different subscriber types. Each layer carries
    // its own filter so console and file verbosity are independent.
    let file_layer = match &path {
        Some(p) => {
            let file = std::fs::File::create(p)?;
            Some(
                tracing_subscriber::fmt::layer()
                    // No colour codes: this is read by humans and by grep.
                    .with_ansi(false)
                    .with_writer(move || file.try_clone().expect("clone log file handle"))
                    .with_filter(file_filter),
            )
        }
        None => None,
    };

    // While the dashboard owns the screen, console writes would scribble over
    // the frame. Rather than rebuilding the subscriber when it starts, the
    // console layer carries a second, dynamic filter that consults a flag the
    // dashboard sets — so the same process narrates normally before and after,
    // and stays silent during.
    let console_layer = tracing_subscriber::fmt::layer()
        .with_filter(console_filter)
        .with_filter(tracing_subscriber::filter::filter_fn(|_| {
            !openair_tui::logs::console_quiet()
        }));

    tracing_subscriber::registry()
        .with(console_layer)
        // The panel's own filter matches the console's, so `--debug` controls
        // both the same way.
        .with(openair_tui::LogLayer::new(panel).with_filter(build_filter(debug_level)?))
        .with(file_layer)
        .init();

    Ok(path)
}

/// Start a `--handoff` session (Windows): route system audio to a virtual
/// output device so the speakers go quiet, and mirror the Windows volume onto
/// AirPlay. Returns the session guard (keep it alive for the stream's lifetime
/// — dropping it restores the original output device) and a receiver of
/// mirrored dBFS updates.
///
/// `Err` is fatal by design: `--handoff` promises silent speakers, and silently
/// streaming with them still playing would be worse than refusing.
#[cfg(windows)]
fn start_handoff(
    device_override: Option<String>,
) -> Result<
    (
        openair_capture::handoff::HandoffSession,
        std::sync::mpsc::Receiver<f32>,
    ),
    openair_capture::handoff::HandoffError,
> {
    let (session, event_rx) = openair_capture::handoff::HandoffSession::start(device_override)?;
    // Adapt the capture crate's VolumeEvent → plain f32 (dBFS) so the client's
    // stream signature stays platform-independent.
    let (fwd_tx, fwd_rx) = std::sync::mpsc::channel::<f32>();
    std::thread::spawn(move || {
        for ev in event_rx {
            let openair_capture::handoff::VolumeEvent::Level(db) = ev;
            if fwd_tx.send(db).is_err() {
                break; // stream ended
            }
        }
    });
    Ok((session, fwd_rx))
}

#[tokio::main]
async fn main() -> Result<()> {
    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    // Parsed before logging starts so the flag itself never reaches the
    // subcommand matching below.
    let (raw_args, want_log) = extract_flag(&raw_args, "--log");
    let (raw_args, no_tui) = extract_flag(&raw_args, "--no-tui");
    let (raw_args, debug_level) = util::extract_debug_level(&raw_args);
    // Shared with the dashboard's log panel. Always installed — it is 500
    // bounded lines, and having it ready means the panel shows what happened
    // *before* it opened, including setup and pairing.
    let log_panel = openair_tui::LogBuffer::default();
    let log_path = init_logging(debug_level, want_log, log_panel.clone())?;
    if let Some(p) = &log_path {
        println!("📝 logging this run to {}", p.display());
    }

    let (raw_args, volume_db) = extract_volume(&raw_args, DEFAULT_VOLUME_DB);
    let (raw_args, latency_ms) = util::extract_latency(&raw_args, 500);
    let (raw_args, offsets) = util::extract_offsets(&raw_args);
    // --bind <ip> forces the local source address for receiver connections,
    // for setups where our interface selection guesses wrong.
    let (raw_args, bind_ip) = util::extract_value(&raw_args, "--bind");
    if let Some(spec) = &bind_ip {
        match spec.parse::<std::net::IpAddr>() {
            Ok(ip) => {
                openair_core::net::set_source_override(ip);
                println!("Binding receiver connections to {}", ip);
            }
            Err(_) => {
                println!("--bind expects an IP address, got '{}'", spec);
                return Ok(());
            }
        }
    }

    let (raw_args, no_metadata) = extract_flag(&raw_args, "--no-metadata");
    let (raw_args, handoff) = extract_flag(&raw_args, "--handoff");
    let (raw_args, handoff_device) = util::extract_value(&raw_args, "--handoff-device");
    let (args, buffered) = extract_flag(&raw_args, "--buffered");

    // --- Interactive picker -------------------------------------------------
    //
    // Bare `openair` on a terminal opens the TUI picker instead of scanning and
    // then trying to pair with every device it found — which was slow, and on a
    // shared network meant opening handshakes with strangers' receivers.
    // `--no-tui` (or a non-terminal stdout, e.g. a pipe) keeps the old
    // behaviour.
    let use_tui = !no_tui && openair_tui::is_interactive();

    // Bare `openair` under the TUI means "capture, but let me choose the
    // receivers on screen". The App owns that transition now, so all this does
    // is route into the capture branch with an empty receiver list; it does not
    // touch the network.
    let start_at_picker = args.is_empty() && use_tui;
    let mut args = args;
    let mut handoff = handoff;
    let mut latency_ms = latency_ms;
    let mut volume_db = volume_db;

    if start_at_picker {
        // The picker's saved toggles stand in for the flags on this path.
        let settings = openair_tui::Settings::load();
        handoff = settings.handoff;
        latency_ms = settings.latency_ms;
        volume_db = settings.volume_db;
        args = vec!["capture".to_string(), PICKER_SELECTION.to_string()];
    }

    // --handoff mirrors live volume, which only the buffered pipeline applies,
    // so enabling it implies --buffered. Picked receivers always stream
    // buffered — it is the pipeline with selectable latency.
    let buffered = buffered || handoff || start_at_picker;

    // --handoff (mute local speakers + mirror Windows volume) is capture-only
    // and Windows-only. Reject early with a clear message otherwise.
    if handoff && args.first().map(String::as_str) != Some("capture") {
        println!("--handoff is only valid with `capture` (it routes system audio");
        println!("through a virtual device and mirrors the Windows volume to AirPlay).");
        return Ok(());
    }
    #[cfg(not(windows))]
    if handoff {
        println!("--handoff is only supported on Windows.");
        return Ok(());
    }

    // Dispatches to the realtime ALAC pipeline (fixed ~2s protocol latency)
    // or the buffered AAC pipeline (sender-chosen latency, `--latency <ms>`,
    // default 500) depending on the `--buffered` flag. Multiple receivers
    // always use the buffered pipeline — that's the multi-room mode.
    let stream_fn = move |targets: &[openair_client::GroupTarget],
                          source: &mut dyn openair_client::AudioSource,
                          volume: Option<f32>,
                          volume_rx: Option<std::sync::mpsc::Receiver<f32>>,
                          metadata_rx: Option<
        std::sync::mpsc::Receiver<openair_core::metadata::NowPlaying>,
    >,
                          stats: Option<Arc<openair_client::StreamStats>>| {
        if targets.len() > 1 && !buffered {
            println!("  (multi-room uses the buffered pipeline — enabling --buffered)");
        }
        if buffered || targets.len() > 1 {
            openair_client::stream_audio_buffered_multi(
                targets, source, volume, latency_ms, volume_rx, metadata_rx, stats,
            )
        } else {
            // The realtime ALAC path has neither a metadata channel nor stats.
            let _ = (metadata_rx, stats);
            openair_client::stream_audio(targets[0].addr, &targets[0].device_id, source, volume)
        }
    };

    // `openair devices` — list output devices and show which one --handoff
    // would route through. Read-only; changes nothing.
    #[cfg(windows)]
    if args.first().map(String::as_str) == Some("devices") {
        match openair_capture::handoff::list_output_devices() {
            Ok(listing) => {
                println!("Audio output devices ({}):\n", listing.devices.len());
                for dev in &listing.devices {
                    let mark = if listing.selected.as_deref() == Some(dev.id.as_str()) {
                        " ← --handoff would use this"
                    } else {
                        ""
                    };
                    println!("  {}{}", dev.name, mark);
                }
                if listing.selected.is_none() {
                    println!("\nNo virtual audio device detected.");
                    println!("--handoff needs one — install VB-CABLE: https://vb-audio.com/Cable/");
                    println!("(Or pass --handoff-device \"<name>\" to force a specific device.)");
                }
            }
            Err(e) => println!("  ✗ could not list audio devices: {}", e),
        }
        return Ok(());
    }

    // `openair restore-audio` — safety hatch: if a --handoff run was killed
    // before it could restore the default output device, the user is left
    // routed to a silent virtual cable with no obvious cause. This puts it back.
    #[cfg(windows)]
    if args.first().map(String::as_str) == Some("restore-audio") {
        match openair_capture::handoff::pending_restore() {
            None => println!("Nothing to restore — no interrupted --handoff session found."),
            Some(_) => match openair_capture::handoff::restore_now() {
                Ok(name) => println!("  ✓ default output device restored to \"{}\"", name),
                Err(e) => println!("  ✗ could not restore the default output device: {}", e),
            },
        }
        return Ok(());
    }

    // Surface an unrestored --handoff switch rather than leaving the user
    // hunting for why their speakers are silent. (Also true while a handoff
    // stream is running in another window, hence the hedge.)
    #[cfg(windows)]
    if openair_capture::handoff::pending_restore().is_some() {
        println!("⚠ A --handoff session may not have restored your audio output device.");
        println!("  If OpenAir isn't streaming elsewhere, run: openair restore-audio\n");
    }

    // `openair pair <ip:port|name>` — one-time Normal HomeKit pairing with the
    // PIN shown on the device's screen (Apple TV / HomePod). Credentials are
    // persisted; later `play`/`capture`/`tone` connect via pair-verify
    // automatically.
    if args.len() >= 2 && args[0] == "pair" {
        let Some((addr, device_id)) = resolve_receiver(&args[1]) else {
            return Ok(());
        };
        println!("OpenAir — HomeKit pairing with {} ({})\n", addr, device_id);
        println!("A PIN should appear on the device's screen...");
        let mut pin_prompt = || {
            use std::io::Write as _;
            print!("Enter PIN: ");
            std::io::stdout().flush().ok();
            let mut line = String::new();
            std::io::stdin().read_line(&mut line).ok();
            line.trim().to_string()
        };
        match openair_client::pair_device(addr, &device_id, &mut pin_prompt) {
            Ok(()) => println!("  ✓ paired — this device will now connect automatically"),
            Err(e) => println!("  ✗ pairing failed: {}", e),
        }
        return Ok(());
    }

    // `openair capture <ip:port|name>... [seconds] [--volume <db>] [--buffered]` — stream
    // live system audio (WASAPI loopback of the default output device) for
    // `seconds`, or indefinitely (until Ctrl+C) if omitted. Multiple
    // receivers = synchronized multi-room (buffered pipeline).
    if args.len() >= 2 && args[0] == "capture" {
        let mut recv_args: Vec<String> = args[1..].to_vec();
        let seconds: Option<u32> = recv_args.last().and_then(|s| s.parse().ok());
        if seconds.is_some() {
            recv_args.pop();
        }
        if recv_args.is_empty() {
            println!("usage: openair capture <receiver>... [seconds]");
            return Ok(());
        }
        // Starting at the picker means the user has not chosen yet, so there is
        // nothing to resolve and no reason to browse.
        let receivers = if start_at_picker {
            Vec::new()
        } else {
            match resolve_receivers(&recv_args, &offsets) {
                Some(r) => r,
                None => return Ok(()),
            }
        };

        let stop = Arc::new(AtomicBool::new(false));
        {
            let stop = stop.clone();
            ctrlc::set_handler(move || {
                stop.store(true, Ordering::SeqCst);
            })
            .ok();
        }

        let dest = receivers
            .iter()
            .map(|t| t.addr.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        match seconds {
            Some(s) => println!("OpenAir — capturing {}s of system audio to {}\n", s, dest),
            None => println!(
                "OpenAir — capturing until Ctrl+C… (streaming system audio to {})\n",
                dest
            ),
        }

        // --handoff: route system audio to a virtual output device BEFORE
        // starting capture, so we capture the cable rather than the speakers.
        // `_handoff_session` must outlive the stream call — dropping it puts
        // the user's default output device back.
        #[cfg(windows)]
        let (_handoff_session, volume_rx, capture_device) = if handoff {
            match start_handoff(handoff_device.clone()) {
                Ok((session, rx)) => {
                    let name = session.device_name().to_string();
                    println!("  🔀 system audio routed to \"{}\"", name);
                    println!("     speakers are silent; the Windows volume now controls AirPlay");
                    (Some(session), Some(rx), Some(name))
                }
                Err(e) => {
                    println!("  ✗ --handoff failed: {}", e);
                    return Ok(());
                }
            }
        } else {
            (None, None, None)
        };
        #[cfg(not(windows))]
        let (volume_rx, capture_device): (Option<std::sync::mpsc::Receiver<f32>>, Option<String>) =
            (None, None);

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
                    println!("  ⚠ now-playing metadata unavailable: {}", e);
                    (None, None)
                }
            }
        };
        #[cfg(not(windows))]
        let metadata_rx: Option<std::sync::mpsc::Receiver<openair_core::metadata::NowPlaying>> = {
            let _ = no_metadata;
            None
        };

        let cap = match openair_capture::SystemCapture::start_on(capture_device.as_deref()) {
            Ok(c) => c,
            Err(e) => {
                println!("  ✗ failed to start system audio capture: {}", e);
                println!("    (no default output device, or WASAPI loopback unavailable)");
                return Ok(());
            }
        };
        println!("  capturing: {} @ {} Hz", cap.device_name, cap.device_rate);

        // The TUI owns the main thread for the whole run, so the stream goes to
        // a worker. Building the source *inside* that worker is what keeps
        // `AudioSource` free of a `Send` bound: only the capture ring (an
        // `Arc`), the sample rate and the stop flag cross the boundary.
        if use_tui {
            let ring = cap.ring.clone();
            let device_rate = cap.device_rate;
            let blocking = buffered;
            // `FnMut` may be called again (retry after a failure), but these
            // receivers have a single consumer — hand each to the first stream
            // that asks and `None` thereafter.
            let mut volume_rx = volume_rx;
            let mut metadata_rx = metadata_rx;

            let launcher: openair_tui::StreamLauncher = Box::new(
                move |targets, stats: Arc<openair_client::StreamStats>, stop| {
                    let ring = ring.clone();
                    let vrx = volume_rx.take();
                    let mrx = metadata_rx.take();
                    openair_tui::StreamHandle::new(std::thread::spawn(move || {
                        let mut source = openair_client::CaptureSource::new(
                            ring,
                            device_rate,
                            seconds,
                            Some(stop),
                        );
                        // Buffered pipelines send ahead of realtime; a live
                        // source must rate-limit them by blocking for data
                        // rather than padding silence, which sounds like
                        // chopped audio for the first seconds.
                        if blocking || targets.len() > 1 {
                            source = source.with_blocking();
                        }
                        let result = openair_client::stream_audio_buffered_multi(
                            &targets,
                            &mut source,
                            Some(volume_db),
                            latency_ms,
                            vrx,
                            mrx,
                            Some(Arc::clone(&stats)),
                        )
                        .map_err(|e| e.to_string());
                        // Mark ended even on failure, or the UI would sit on a
                        // frozen frame forever waiting for a stream that has
                        // already given up.
                        stats.mark_ended();
                        result
                    }))
                },
            );

            let settings = openair_tui::Settings::load();
            #[cfg(windows)]
            let handoff_available = openair_capture::handoff::list_output_devices()
                .map(|l| l.selected.is_some())
                .unwrap_or(false);
            #[cfg(not(windows))]
            let handoff_available = false;

            let mut app = openair_tui::App::new(
                settings,
                log_panel.clone(),
                handoff_available,
                launcher,
            );
            let start = if receivers.is_empty() {
                openair_tui::StartAt::Picker
            } else {
                openair_tui::StartAt::Receivers(receivers)
            };
            match app.run(start) {
                Ok(Some(summary)) => println!("{summary}"),
                Ok(None) => {}
                Err(e) => println!("  ⚠ interface error: {e}"),
            }
            return Ok(());
        }

        let mut source = openair_client::CaptureSource::new(
            cap.ring.clone(),
            cap.device_rate,
            seconds,
            Some(stop.clone()),
        );
        if buffered || receivers.len() > 1 {
            source = source.with_blocking();
        }

        match stream_fn(
            &receivers,
            &mut source,
            Some(volume_db),
            volume_rx,
            metadata_rx,
            None,
        ) {
            Ok(()) => println!("  ✓ capture streamed successfully"),
            Err(e) => println!("  ✗ {}", e),
        }
        // `cap` stays alive (and capturing) until here, keeping the loopback
        // stream running for the whole duration of the call above.
        return Ok(());
    }

    // `openair play <ip:port|name>... <file.wav> [--volume <db>] [--buffered]` — stream a
    // WAV file; the file is the LAST argument. Multiple receivers = multi-room.
    if args.len() >= 3 && args[0] == "play" {
        let path = std::path::Path::new(&args[args.len() - 1]);
        let recv_args: Vec<String> = args[1..args.len() - 1].to_vec();
        let Some(receivers) = resolve_receivers(&recv_args, &offsets) else {
            return Ok(());
        };
        let dest = receivers
            .iter()
            .map(|t| t.addr.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        println!("OpenAir — playing {} to {}\n", path.display(), dest);

        if !path.exists() {
            println!("  ✗ file not found: {}", path.display());
            return Ok(());
        }

        let mut source = match openair_client::WavSource::open(path) {
            Ok(s) => s,
            Err(e) => {
                println!("  ✗ unsupported or invalid WAV file: {}", e);
                return Ok(());
            }
        };

        match stream_fn(&receivers, &mut source, Some(volume_db), None, None, None) {
            Ok(()) => println!("  ✓ playback finished successfully"),
            Err(e) => println!("  ✗ {}", e),
        }
        return Ok(());
    }

    // `openair tone <ip:port|name>... [seconds] [--volume <db>] [--buffered]` — stream a
    // 440 Hz test tone. Multiple receivers = multi-room.
    if args.len() >= 2 && args[0] == "tone" {
        let mut recv_args: Vec<String> = args[1..].to_vec();
        let seconds: u32 = match recv_args.last().and_then(|s| s.parse().ok()) {
            Some(s) => {
                recv_args.pop();
                s
            }
            None => 10,
        };
        if recv_args.is_empty() {
            println!("usage: openair tone <receiver>... [seconds]");
            return Ok(());
        }
        let Some(receivers) = resolve_receivers(&recv_args, &offsets) else {
            return Ok(());
        };
        let dest = receivers
            .iter()
            .map(|t| t.addr.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        println!("OpenAir — streaming {}s test tone to {}\n", seconds, dest);
        let mut source = openair_client::SineSource::new(440.0, seconds);
        match stream_fn(&receivers, &mut source, Some(volume_db), None, None, None) {
            Ok(()) => println!("  ✓ tone streamed successfully"),
            Err(e) => println!("  ✗ {}", e),
        }
        return Ok(());
    }

    // Direct mode: `openair <ip:port>` skips discovery and pairs with the given address.
    if let Some(arg) = args.first() {
        let addr: SocketAddr = arg.parse()?;
        println!("OpenAir — direct pairing with {}\n", addr);
        match openair_rtsp::pair_and_get_info(addr, DEFAULT_DEVICE_ID) {
            Ok(info) => {
                println!("  ✓ GET /info succeeded ({} bytes)\n", info.len());
                if let Ok(s) = std::str::from_utf8(&info) {
                    println!("{}", &s[..s.len().min(512)]);
                }
            }
            Err(e) => println!("  ✗ {}", e),
        }
        return Ok(());
    }

    println!("OpenAir — scanning for AirPlay devices (5s)...\n");

    let mut devices = Vec::new();
    openair_discovery::browse(Duration::from_secs(5), |d| {
        println!("  [{}] {} @ {}:{}", devices.len(), d.name, d.addr, d.port);
        devices.push(d);
    })?;

    if devices.is_empty() {
        println!("\nNo devices found.");
        return Ok(());
    }

    println!("\nFound {} device(s). Attempting pairing...\n", devices.len());

    for dev in &devices {
        let addr = SocketAddr::new(dev.addr, dev.port);
        let device_id = dev.txt.device_id.as_deref().unwrap_or(DEFAULT_DEVICE_ID);
        println!("→ Trying {} @ {} ...", dev.name, addr);

        match openair_rtsp::pair_and_get_info(addr, device_id) {
            Ok(info) => {
                println!("  ✓ GET /info succeeded ({} bytes)\n", info.len());
                if let Ok(s) = std::str::from_utf8(&info) {
                    println!("{}", &s[..s.len().min(512)]);
                }
                return Ok(());
            }
            Err(e) => {
                println!("  ✗ {}\n", e);
            }
        }
    }

    println!("No devices paired successfully.");
    Ok(())
}
