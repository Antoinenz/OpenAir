use anyhow::Result;
use std::net::SocketAddr;
use std::time::Duration;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env()
                .add_directive("openair=debug".parse()?)
                .add_directive("info".parse()?),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();

    // `openair play <ip:port> <file.wav>` — stream a WAV file.
    if args.len() >= 4 && args[1] == "play" {
        let addr: SocketAddr = args[2].parse()?;
        let path = std::path::Path::new(&args[3]);
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

        match openair_client::stream_audio(
            addr,
            "AA:BB:CC:DD:EE:FF",
            &mut source,
            Some(-8.0),
        ) {
            Ok(()) => println!("  ✓ playback finished successfully"),
            Err(e) => println!("  ✗ {}", e),
        }
        return Ok(());
    }

    // `openair tone <ip:port> [seconds]` — stream a 440 Hz test tone (Step 4).
    if args.len() >= 3 && args[1] == "tone" {
        let addr: SocketAddr = args[2].parse()?;
        let seconds: u32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(10);
        println!("OpenAir — streaming {}s test tone to {}\n", seconds, addr);
        match openair_client::stream_tone(addr, "AA:BB:CC:DD:EE:FF", seconds, 440.0, Some(-8.0)) {
            Ok(()) => println!("  ✓ tone streamed successfully"),
            Err(e) => println!("  ✗ {}", e),
        }
        return Ok(());
    }

    // Direct mode: `openair <ip:port>` skips discovery and pairs with the given address.
    if let Some(arg) = std::env::args().nth(1) {
        let addr: SocketAddr = arg.parse()?;
        println!("OpenAir — direct pairing with {}\n", addr);
        match openair_rtsp::pair_and_get_info(addr, "AA:BB:CC:DD:EE:FF") {
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
        let device_id = dev.txt.device_id.as_deref().unwrap_or("AA:BB:CC:DD:EE:FF");
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
