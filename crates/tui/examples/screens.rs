//! Render the TUI's screens to stdout as plain text.
//!
//! The README shows what OpenAir looks like. Hand-drawn box art would drift
//! from the real interface the first time a layout changed and nobody would
//! notice; a PNG would go stale just as quietly, only less legibly. This runs
//! the actual render functions against a ratatui `TestBackend`, so what it
//! prints is what the program draws.
//!
//!     cargo run -p openair-tui --example screens
//!
//! Widths are chosen to sit inside a README code block without wrapping.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::thread::sleep;
use std::time::{Duration, Instant};

use openair_client::{ReceiverStat, ReceiverState, StreamStats};
use openair_core::metadata::NowPlaying;
use openair_discovery::{AirPlayDevice, AirPlayTxt};
use openair_tui::dashboard::DashboardState;
use openair_tui::logs::LogLine;
use openair_tui::pairing::{PairingState, PendingPair};
use openair_tui::picker::PickerState;
use openair_tui::settings::Settings;
use openair_tui::{dashboard_ui, pairing_ui, picker_ui, LogBuffer};
use ratatui::backend::TestBackend;
use ratatui::Terminal;
use tracing::Level;

fn banner(title: &str) {
    println!("\n\n=== {title} ===\n");
}

/// `TestBackend`'s Display wraps each row in quote marks and pads to the full
/// width. Neither belongs in a README.
fn show(terminal: &Terminal<TestBackend>) {
    for line in terminal.backend().to_string().lines() {
        println!("{}", line.trim_matches('"').trim_end());
    }
}

fn device(name: &str, addr: &str, id: &str, model: &str, features: &str) -> AirPlayDevice {
    let mut raw: HashMap<String, String> = HashMap::new();
    raw.insert("features".into(), features.into());
    raw.insert("deviceid".into(), id.into());
    raw.insert("model".into(), model.into());
    AirPlayDevice::new(
        name.into(),
        addr.parse::<IpAddr>().unwrap(),
        7000,
        AirPlayTxt::parse(&raw),
    )
}

fn addr(s: &str) -> SocketAddr {
    s.parse().unwrap()
}

fn receiver(
    name: &str,
    at: &str,
    offset_ms: i64,
    trim_db: f32,
    lead_ms: Option<i64>,
    health: f32,
) -> ReceiverStat {
    ReceiverStat {
        name: name.into(),
        addr: addr(at),
        state: ReceiverState::Connected,
        offset_ms,
        trim_db,
        lead_ms,
        health,
        error: None,
        needs_pairing: false,
    }
}

fn picker() {
    // Transient-capable (bit 48) versus an Apple TV that wants a PIN, so the
    // list shows both pairing states rather than a uniform one.
    let mut state = PickerState::new(Settings::default(), vec!["3A:A1:CB:97:1A:87".into()], true);
    state.insert(device(
        "Living Room",
        "192.168.1.61",
        "3A:A1:CB:97:1A:87",
        "AppleTV14,1",
        "0x4A7FCA00,0xBC354BD0",
    ));
    state.insert(device(
        "Pool Room",
        "192.168.1.106",
        "C8:69:CD:67:92:16",
        "Shairport Sync",
        "0x1C340405F4A00,0x1C340405F4A00",
    ));
    state.insert(device(
        "Denon AVR-X1700H",
        "192.168.1.88",
        "00:05:CD:9A:1B:4C",
        "AVR-X1700H",
        "0x4A7FCA00,0xBC354BD0",
    ));

    let mut terminal = Terminal::new(TestBackend::new(104, 12)).unwrap();
    terminal.draw(|f| picker_ui::render(f, &state)).unwrap();
    banner("Picker");
    show(&terminal);
}

fn pin() {
    let state = PairingState::new(vec![PendingPair {
        name: "Living Room".into(),
        addr: addr("192.168.1.61:7000"),
        device_id: "3A:A1:CB:97:1A:87".into(),
    }]);

    let mut terminal = Terminal::new(TestBackend::new(104, 14)).unwrap();
    terminal.draw(|f| pairing_ui::render(f, &state)).unwrap();
    banner("PIN entry");
    show(&terminal);
}

fn dashboard() {
    let mut state = DashboardState::new(500);
    let stats = StreamStats::new(500);
    stats.set_receivers(vec![
        receiver("Living Room", "192.168.1.61:7000", 0, 0.0, Some(496), 1.0),
        receiver("Pool Room", "192.168.1.106:7000", 80, -3.0, Some(502), 1.0),
        receiver(
            "Denon AVR-X1700H",
            "192.168.1.88:7000",
            -40,
            0.0,
            Some(499),
            1.0,
        ),
    ]);

    stats.set_now_playing(NowPlaying {
        title: "Weightless".into(),
        artist: "Marconi Union".into(),
        album: "Ambient Transmissions Vol. 2".into(),
        art: None,
    });

    let buffer = LogBuffer::new(10);
    for (ts, level, msg) in [
        (
            "19:42:01",
            Level::INFO,
            "using stored HomeKit pairing (pair-verify) device_id=3A:A1:CB:97:1A:87",
        ),
        (
            "19:42:01",
            Level::INFO,
            "PTP: yielding to receiver clock (BMCA) grandmaster=Living Room",
        ),
        (
            "19:42:02",
            Level::INFO,
            "3 receivers on a shared clock, buffered AAC PT=103",
        ),
        (
            "19:42:02",
            Level::INFO,
            "now playing sent: cover art 41.2 kB",
        ),
    ] {
        buffer.push(LogLine {
            ts: ts.into(),
            level,
            msg: msg.into(),
        });
    }

    // The bandwidth graph is drawn from real elapsed time between samples, so
    // it has to be fed over a real interval rather than faked in one go.
    for i in 0i64..14 {
        stats.add_bytes((23_000 + (i % 4) * 1_400) as u64);
        stats.record_lead_ms(496 + (i % 5) * 3);
        state.sample(&stats, Instant::now());
        sleep(Duration::from_millis(45));
    }

    let mut terminal = Terminal::new(TestBackend::new(104, 26)).unwrap();
    terminal
        .draw(|f| dashboard_ui::render(f, &state, &buffer))
        .unwrap();
    banner("Streaming dashboard");
    show(&terminal);
}

fn main() {
    picker();
    pin();
    dashboard();
    println!();
}
