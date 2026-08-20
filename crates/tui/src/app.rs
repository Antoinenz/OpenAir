//! The application shell: one terminal session, one event loop, many screens.
//!
//! Before this existed the TUI was two islands with the command line in
//! between — a picker, then a restored terminal and plain text while sessions
//! were established, then a dashboard. Anything needing input in that gap (a
//! pairing PIN) fell back to a `stdin` prompt.
//!
//! Now every state the program can be in is a [`Screen`], and the terminal is
//! entered once and left once.
//!
//! ## Threading
//!
//! The TUI owns the **main** thread for the program's whole life, because it
//! owns the terminal across screens that exist before any stream does. The
//! stream therefore runs on a worker — the reverse of the phase-1 arrangement.
//!
//! This does *not* require `AudioSource: Send`. The [`StreamLauncher`] closure
//! stays on the main thread and builds its source **inside** the thread it
//! spawns, so only the ingredients (an `Arc` on the capture ring, a sample
//! rate, a stop flag) cross the boundary, and those are already `Send`. The
//! `HandoffSession` guard and the WASAPI capture handle never move at all.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use openair_client::{GroupTarget, StreamStats};
use openair_discovery::BrowseHandle;

use crate::dashboard::{DashAction, DashboardState};
use crate::dashboard_ui::{self, Summary};
use crate::logs::{self, LogBuffer};
use crate::picker::{PickerAction, PickerRow, PickerState};
use crate::picker_ui;
use crate::settings::Settings;
use crate::term;

/// Render/sample rate. Fast enough to feel live, slow enough to be invisible
/// next to the ~43 audio packets per second the stream is actually sending.
const TICK: Duration = Duration::from_millis(100);

/// Fallback device id for a receiver that advertises none.
const DEFAULT_DEVICE_ID: &str = "AA:BB:CC:DD:EE:FF";

/// Which screen the app opens on.
pub enum StartAt {
    /// Bare `openair`: choose receivers first.
    Picker,
    /// Receivers named on the command line: straight to streaming.
    Receivers(Vec<GroupTarget>),
}

/// Starts a stream on a worker thread and returns a handle to it.
///
/// Supplied by the caller (the CLI) rather than implemented here, so
/// platform-specific setup — `--handoff`, WASAPI capture, now-playing — stays
/// behind the `cfg` walls it already lives behind, and this crate never needs
/// to depend on `openair-capture`.
///
/// `FnMut` rather than `FnOnce`: returning to the picker after a failure and
/// starting again is a supported path.
pub type StreamLauncher<'a> =
    Box<dyn FnMut(Vec<GroupTarget>, Arc<StreamStats>, Arc<AtomicBool>) -> StreamHandle + 'a>;

/// A running stream.
pub struct StreamHandle {
    thread: JoinHandle<Result<(), String>>,
}

impl StreamHandle {
    pub fn new(thread: JoinHandle<Result<(), String>>) -> Self {
        Self { thread }
    }

    fn join(self) -> Result<(), String> {
        match self.thread.join() {
            Ok(result) => result,
            Err(_) => Err("stream thread panicked".to_string()),
        }
    }
}

/// The current screen, and whatever state belongs only to it.
pub enum Screen {
    Picker(Box<PickerScreen>),
    Streaming(Box<StreamingScreen>),
}

impl Screen {
    /// Short name, for tests and logs.
    pub fn name(&self) -> &'static str {
        match self {
            Screen::Picker(_) => "picker",
            Screen::Streaming(_) => "streaming",
        }
    }
}

pub struct PickerScreen {
    pub state: PickerState,
    /// Discovery runs for as long as this screen is up; dropping it stops the
    /// mDNS daemon.
    browse: Option<BrowseHandle>,
}

pub struct StreamingScreen {
    pub state: DashboardState,
    stats: Arc<StreamStats>,
    stop: Arc<AtomicBool>,
    handle: Option<StreamHandle>,
    last_sample: Instant,
}

pub struct App<'a> {
    screen: Screen,
    logs: LogBuffer,
    settings: Settings,
    launch: StreamLauncher<'a>,
    handoff_available: bool,
    /// Set when the user asks to leave entirely.
    quitting: bool,
}

impl<'a> App<'a> {
    pub fn new(
        settings: Settings,
        logs: LogBuffer,
        handoff_available: bool,
        launch: StreamLauncher<'a>,
    ) -> Self {
        Self {
            screen: Screen::Picker(Box::new(PickerScreen {
                state: PickerState::new(settings.clone(), Vec::new(), handoff_available),
                browse: None,
            })),
            logs,
            settings,
            launch,
            handoff_available,
            quitting: false,
        }
    }

    pub fn screen(&self) -> &Screen {
        &self.screen
    }

    /// Run until the user quits or the stream ends.
    ///
    /// The terminal is entered once here and restored before returning, so the
    /// caller only ever sees a normal terminal.
    pub fn run(&mut self, start: StartAt) -> io::Result<Option<Summary>> {
        match start {
            StartAt::Picker => self.open_picker(),
            StartAt::Receivers(targets) => self.start_stream(targets),
        }

        logs::set_console_quiet(true);
        let (mut terminal, guard) = match term::enter_alt() {
            Ok(t) => t,
            Err(e) => {
                logs::set_console_quiet(false);
                return Err(e);
            }
        };

        let mut summary = None;
        let result = self.event_loop(&mut terminal, &mut summary);

        drop(guard);
        logs::set_console_quiet(false);
        result?;
        Ok(summary)
    }

    fn event_loop(
        &mut self,
        terminal: &mut term::Tui,
        summary: &mut Option<Summary>,
    ) -> io::Result<()> {
        while !self.quitting {
            self.tick();
            self.draw(terminal)?;

            if let Some(done) = self.finished_summary() {
                *summary = Some(done);
                break;
            }

            let timeout = self.poll_timeout();
            if event::poll(timeout)? {
                if let Event::Key(key) = event::read()? {
                    // Windows reports press *and* release; acting on both would
                    // toggle every selection twice.
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    self.on_key(key.code, key.modifiers, terminal)?;
                }
            }
        }
        Ok(())
    }

    /// Per-iteration work that isn't input: draining discovery, sampling stats.
    fn tick(&mut self) {
        match &mut self.screen {
            Screen::Picker(p) => {
                if let Some(browse) = &p.browse {
                    while let Ok(device) = browse.devices.try_recv() {
                        p.state.insert(device);
                    }
                }
            }
            Screen::Streaming(s) => {
                // Sampling runs on a clock, not on loop iterations: `event::poll`
                // returns early when a key arrives, so tying it to the loop made
                // a held key shrink every measurement window.
                let now = Instant::now();
                if now.duration_since(s.last_sample) >= TICK {
                    s.state.sample(&s.stats, now);
                    s.last_sample = now;
                }
            }
        }
    }

    fn draw(&mut self, terminal: &mut term::Tui) -> io::Result<()> {
        match &self.screen {
            Screen::Picker(p) => {
                terminal.draw(|frame| picker_ui::render(frame, &p.state))?;
            }
            Screen::Streaming(s) => {
                let logs = &self.logs;
                terminal.draw(|frame| dashboard_ui::render(frame, &s.state, logs))?;
            }
        }
        Ok(())
    }

    /// How long to wait for a key before the next iteration.
    fn poll_timeout(&self) -> Duration {
        match &self.screen {
            Screen::Streaming(s) => poll_timeout(s.last_sample.elapsed()),
            _ => TICK,
        }
    }

    /// `Some` once the stream has ended and the app should leave.
    fn finished_summary(&mut self) -> Option<Summary> {
        let Screen::Streaming(s) = &mut self.screen else {
            return None;
        };
        if !s.stats.ended() {
            return None;
        }
        let summary = Summary {
            elapsed: s.stats.elapsed(),
            receivers: s.state.receivers.len(),
            latency_ms: s.state.latency_ms,
            worst_lead_ms: s.state.worst_lead_ms,
            bytes_sent: s.stats.bytes_sent(),
            graph: s.state.graph,
        };
        if let Some(handle) = s.handle.take() {
            // The stream has already marked itself ended, so this returns
            // promptly; joining collects any error it wants to report.
            if let Err(e) = handle.join() {
                tracing::error!("stream ended with an error: {e}");
            }
        }
        Some(summary)
    }

    fn on_key(
        &mut self,
        code: KeyCode,
        mods: KeyModifiers,
        terminal: &mut term::Tui,
    ) -> io::Result<()> {
        // Ctrl+C means the same thing everywhere: leave, but let whatever is
        // running shut down gracefully.
        if code == KeyCode::Char('c') && mods.contains(KeyModifiers::CONTROL) {
            self.request_exit();
            return Ok(());
        }

        match &mut self.screen {
            Screen::Picker(p) => match p.state.on_key(code) {
                PickerAction::Quit => self.quitting = true,
                PickerAction::Start => {
                    let chosen: Vec<PickerRow> = p.state.chosen().into_iter().cloned().collect();
                    self.settings = p.state.settings.clone();
                    if let Err(e) = self.settings.save() {
                        tracing::warn!("could not save settings: {e}");
                    }
                    self.start_stream(targets_from(&chosen));
                }
                PickerAction::None | PickerAction::Hint(_) => {}
            },
            Screen::Streaming(s) => match s.state.on_key(code) {
                DashAction::Quit => self.request_exit(),
                DashAction::Command(cmd) => {
                    if !s.stats.send(cmd) {
                        tracing::warn!("could not queue command — stream mailbox unavailable");
                    }
                }
                DashAction::OpenPicker => {
                    dashboard_ui::add_receiver(terminal, &s.stats)?;
                }
                DashAction::None => {}
            },
        }
        Ok(())
    }

    /// Ask the current activity to stop. While streaming this hands shutdown to
    /// the stream — it still has queued audio to play out and sessions to tear
    /// down — and the loop leaves once it reports `ended`.
    fn request_exit(&mut self) {
        match &self.screen {
            Screen::Streaming(s) => s.stop.store(true, Ordering::SeqCst),
            _ => self.quitting = true,
        }
    }

    fn open_picker(&mut self) {
        let paired = openair_client::PairingStore::load()
            .map(|s| s.peer_ids())
            .unwrap_or_default();
        let browse = match openair_discovery::browse_live() {
            Ok(b) => Some(b),
            Err(e) => {
                tracing::error!("mDNS discovery failed to start: {e}");
                None
            }
        };
        self.screen = Screen::Picker(Box::new(PickerScreen {
            state: PickerState::new(self.settings.clone(), paired, self.handoff_available),
            browse,
        }));
    }

    fn start_stream(&mut self, targets: Vec<GroupTarget>) {
        let stats = StreamStats::new(self.settings.latency_ms);
        let stop = Arc::new(AtomicBool::new(false));
        let handle = (self.launch)(targets, Arc::clone(&stats), Arc::clone(&stop));
        self.screen = Screen::Streaming(Box::new(StreamingScreen {
            state: DashboardState::new(self.settings.graph, self.settings.latency_ms),
            stats,
            stop,
            handle: Some(handle),
            last_sample: Instant::now(),
        }));
    }
}

/// How long to wait for a key, given how long ago the last sample was taken.
///
/// Never longer than what remains of the tick (or keys would delay sampling,
/// which shrinks every measurement window while a key is held), and never zero
/// (which would spin the CPU when rendering overruns a tick).
fn poll_timeout(since_last_sample: Duration) -> Duration {
    TICK.saturating_sub(since_last_sample)
        .max(Duration::from_millis(1))
}

/// Turn picker rows into stream targets.
pub fn targets_from(rows: &[PickerRow]) -> Vec<GroupTarget> {
    rows.iter()
        .map(|r| GroupTarget {
            addr: r.addr,
            device_id: r
                .device_id
                .clone()
                .unwrap_or_else(|| DEFAULT_DEVICE_ID.to_string()),
            offset_ms: 0,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    fn row(addr: &str, device_id: Option<&str>) -> PickerRow {
        PickerRow {
            key: addr.into(),
            name: "Test".into(),
            addr: addr.parse().unwrap(),
            device_id: device_id.map(str::to_string),
            model: "AppleTV6,2".into(),
            selected: true,
            paired: false,
            needs_pairing: false,
        }
    }

    fn test_device(ip: &str) -> openair_discovery::AirPlayDevice {
        use std::collections::HashMap;
        let mut raw: HashMap<String, String> = HashMap::new();
        // bit 9 (audio) + bit 48 (transient pairing)
        raw.insert("features".into(), "0x200,0x10000".into());
        raw.insert("deviceid".into(), "AA:BB".into());
        openair_discovery::AirPlayDevice::new(
            "Test._airplay._tcp.local.".into(),
            ip.parse().unwrap(),
            7000,
            openair_discovery::AirPlayTxt::parse(&raw),
        )
    }

    /// An App whose launcher records what it was asked to stream and returns a
    /// thread that ends immediately.
    fn test_app(started: &std::sync::Mutex<Vec<Vec<GroupTarget>>>) -> App<'_> {
        let launch: StreamLauncher<'_> = Box::new(move |targets, stats, _stop| {
            started.lock().unwrap().push(targets);
            StreamHandle::new(std::thread::spawn(move || {
                stats.mark_ended();
                Ok(())
            }))
        });
        App::new(Settings::default(), LogBuffer::new(10), false, launch)
    }

    #[test]
    fn starts_on_the_picker() {
        let started = std::sync::Mutex::new(Vec::new());
        let app = test_app(&started);
        assert_eq!(app.screen().name(), "picker");
    }

    #[test]
    fn confirming_the_picker_starts_a_stream() {
        let started = std::sync::Mutex::new(Vec::new());
        let mut app = test_app(&started);

        let Screen::Picker(p) = &mut app.screen else {
            panic!("expected the picker");
        };
        p.state.insert(test_device("192.168.1.51"));
        p.state.on_key(KeyCode::Char(' ')); // select
        assert_eq!(p.state.on_key(KeyCode::Enter), PickerAction::Start);

        let chosen: Vec<PickerRow> = p.state.chosen().into_iter().cloned().collect();
        app.start_stream(targets_from(&chosen));

        assert_eq!(app.screen().name(), "streaming");
        let calls = started.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].len(), 1);
        assert_eq!(
            calls[0][0].addr,
            "192.168.1.51:7000".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn quitting_while_streaming_stops_the_stream_rather_than_the_app() {
        // The distinction that matters: the stream still has queued audio to
        // play out and sessions to tear down, so `q` must not just exit.
        let started = std::sync::Mutex::new(Vec::new());
        let mut app = test_app(&started);
        app.start_stream(targets_from(&[row("192.168.1.51:7000", None)]));

        app.request_exit();
        assert!(!app.quitting, "the app waits for the stream to finish");
        let Screen::Streaming(s) = &app.screen else {
            panic!("expected streaming");
        };
        assert!(s.stop.load(Ordering::SeqCst), "the stream was asked to stop");
    }

    #[test]
    fn quitting_the_picker_ends_the_app_immediately() {
        // No stream to drain, so there is nothing to wait for.
        let started = std::sync::Mutex::new(Vec::new());
        let mut app = test_app(&started);
        app.request_exit();
        assert!(app.quitting);
    }

    #[test]
    fn poll_timeout_waits_only_the_rest_of_the_tick() {
        // The bug this guards: keys arriving mid-tick used to restart a full
        // wait each time, so holding a key sampled far more often than 10 Hz
        // and the graph and bandwidth readings jumped around.
        assert_eq!(poll_timeout(Duration::ZERO), TICK);
        assert_eq!(poll_timeout(TICK / 4), TICK - TICK / 4);
    }

    #[test]
    fn poll_timeout_is_never_zero() {
        // A zero timeout spins the CPU when a render overruns the tick.
        assert!(poll_timeout(TICK * 2) > Duration::ZERO);
    }

    #[test]
    fn a_receiver_without_a_device_id_gets_the_default() {
        let targets = targets_from(&[row("192.168.1.51:7000", None)]);
        assert_eq!(targets[0].device_id, DEFAULT_DEVICE_ID);
    }

    #[test]
    fn an_advertised_device_id_is_used() {
        let targets = targets_from(&[row("192.168.1.51:7000", Some("AA:BB"))]);
        assert_eq!(targets[0].device_id, "AA:BB");
    }

    #[test]
    fn starting_with_named_receivers_skips_the_picker() {
        let started = std::sync::Mutex::new(Vec::new());
        let mut app = test_app(&started);
        app.start_stream(vec![GroupTarget {
            addr: "192.168.1.51:7000".parse().unwrap(),
            device_id: "AA:BB".into(),
            offset_ms: 0,
        }]);
        assert_eq!(app.screen().name(), "streaming");
    }

    #[test]
    fn the_summary_is_taken_once_the_stream_ends() {
        let started = std::sync::Mutex::new(Vec::new());
        let mut app = test_app(&started);
        app.start_stream(targets_from(&[row("192.168.1.51:7000", None)]));

        // The test launcher marks the stream ended on its own thread; wait for
        // it rather than racing it.
        let deadline = Instant::now() + Duration::from_secs(2);
        while app.finished_summary().is_none() {
            assert!(Instant::now() < deadline, "stream never reported ending");
            std::thread::yield_now();
        }
    }
}
