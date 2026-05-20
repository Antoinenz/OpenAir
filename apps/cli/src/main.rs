use anyhow::Result;
use std::time::Duration;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("openair=info".parse()?))
        .init();

    println!("OpenAir — scanning for AirPlay devices (5s)...\n");

    let mut devices = Vec::new();
    openair_discovery::browse(Duration::from_secs(5), |d| {
        println!("  {}", d);
        devices.push(d);
    })?;

    println!("\nFound {} device(s).", devices.len());
    Ok(())
}
