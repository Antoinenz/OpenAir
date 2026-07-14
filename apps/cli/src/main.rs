use anyhow::Result;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing_subscriber::EnvFilter;

use util::{clean_device_name, extract_flag, extract_volume};

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

    /// Cleans an mDNS-advertised AirPlay service name for display/matching,
    /// e.g. "Pool Room._airplay._tcp.local." -> "Pool Room".
    pub fn clean_device_name(raw: &str) -> String {
        raw.split("._airplay").next().unwrap_or(raw).to_string()
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

    #[cfg(test)]
    mod tests {
        use super::*;

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
        fn clean_device_name_strips_service_suffix() {
            assert_eq!(clean_device_name("Pool Room._airplay._tcp.local."), "Pool Room");
        }

        #[test]
        fn clean_device_name_passthrough_when_no_suffix() {
            assert_eq!(clean_device_name("Pool Room"), "Pool Room");
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
        .filter(|d| clean_device_name(&d.name).to_lowercase().contains(&needle))
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
                println!("  - {}", clean_device_name(&d.name));
            }
            None
        }
        _ => {
            println!("Multiple receivers matched '{}':", arg);
            for d in &matches {
                println!("  - {}", clean_device_name(&d.name));
            }
            None
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env()
                .add_directive("openair=debug".parse()?)
                .add_directive("info".parse()?),
        )
        .init();

    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    let (raw_args, volume_db) = extract_volume(&raw_args, DEFAULT_VOLUME_DB);
    let (raw_args, latency_ms) = util::extract_latency(&raw_args, 500);
    let (args, buffered) = extract_flag(&raw_args, "--buffered");

    // Dispatches to the realtime ALAC pipeline (fixed ~2s protocol latency)
    // or the buffered AAC pipeline (sender-chosen latency, `--latency <ms>`,
    // default 500) depending on the `--buffered` flag.
    let stream_fn = move |addr: SocketAddr,
                          device_id: &str,
                          source: &mut dyn openair_client::AudioSource,
                          volume: Option<f32>| {
        if buffered {
            openair_client::stream_audio_buffered_with_latency(
                addr, device_id, source, volume, latency_ms,
            )
        } else {
            openair_client::stream_audio(addr, device_id, source, volume)
        }
    };

    // `openair capture <ip:port|name> [seconds] [--volume <db>] [--buffered]` — stream
    // live system audio (WASAPI loopback of the default output device) for
    // `seconds`, or indefinitely (until Ctrl+C) if omitted.
    if args.len() >= 2 && args[0] == "capture" {
        let Some((addr, device_id)) = resolve_receiver(&args[1]) else {
            return Ok(());
        };
        let seconds: Option<u32> = args.get(2).and_then(|s| s.parse().ok());

        let stop = Arc::new(AtomicBool::new(false));
        {
            let stop = stop.clone();
            ctrlc::set_handler(move || {
                stop.store(true, Ordering::SeqCst);
            })
            .ok();
        }

        match seconds {
            Some(s) => println!("OpenAir — capturing {}s of system audio to {}\n", s, addr),
            None => println!(
                "OpenAir — capturing until Ctrl+C… (streaming system audio to {})\n",
                addr
            ),
        }

        let cap = match openair_capture::SystemCapture::start() {
            Ok(c) => c,
            Err(e) => {
                println!("  ✗ failed to start system audio capture: {}", e);
                println!("    (no default output device, or WASAPI loopback unavailable)");
                return Ok(());
            }
        };
        println!("  device rate: {} Hz", cap.device_rate);

        let mut source = openair_client::CaptureSource::new(
            cap.ring.clone(),
            cap.device_rate,
            seconds,
            Some(stop),
        );
        // Buffered pipelines send ahead of realtime; a live source must
        // rate-limit them by blocking for data instead of padding silence
        // (which sounds like glitchy, chopped audio for the first seconds).
        if buffered {
            source = source.with_blocking();
        }

        match stream_fn(addr, &device_id, &mut source, Some(volume_db)) {
            Ok(()) => println!("  ✓ capture streamed successfully"),
            Err(e) => println!("  ✗ {}", e),
        }
        // `cap` stays alive (and capturing) until here, keeping the loopback
        // stream running for the whole duration of the call above.
        return Ok(());
    }

    // `openair play <ip:port|name> <file.wav> [--volume <db>] [--buffered]` — stream a WAV file.
    if args.len() >= 3 && args[0] == "play" {
        let Some((addr, device_id)) = resolve_receiver(&args[1]) else {
            return Ok(());
        };
        let path = std::path::Path::new(&args[2]);
        println!("OpenAir — playing {} to {}\n", path.display(), addr);

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

        match stream_fn(addr, &device_id, &mut source, Some(volume_db)) {
            Ok(()) => println!("  ✓ playback finished successfully"),
            Err(e) => println!("  ✗ {}", e),
        }
        return Ok(());
    }

    // `openair tone <ip:port|name> [seconds] [--volume <db>] [--buffered]` — stream a 440 Hz test tone (Step 4).
    if args.len() >= 2 && args[0] == "tone" {
        let Some((addr, device_id)) = resolve_receiver(&args[1]) else {
            return Ok(());
        };
        let seconds: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(10);
        println!("OpenAir — streaming {}s test tone to {}\n", seconds, addr);
        let mut source = openair_client::SineSource::new(440.0, seconds);
        match stream_fn(addr, &device_id, &mut source, Some(volume_db)) {
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
