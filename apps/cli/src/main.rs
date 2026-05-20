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
