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

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use openair_client::{GroupTarget, StreamStats};
use openair_discovery::BrowseHandle;

use crate::connecting::{self, ConnectAction, ConnectOutcome, ConnectingState};
use crate::dashboard::{DashAction, DashboardState};
use crate::dashboard_ui::{self, Summary};
use crate::logs::{self, LogBuffer};
use crate::pairing::{PairAction, PairWorker, PairingState, PendingPair};
use crate::pairing_ui;
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
    Pairing(Box<PairingScreen>),
    Connecting(Box<ConnectingScreen>),
    Streaming(Box<StreamingScreen>),
}

impl Screen {
    /// Short name, for tests and logs.
    pub fn name(&self) -> &'static str {
        match self {
            Screen::Picker(_) => "picker",
            Screen::Pairing(_) => "pairing",
            Screen::Connecting(_) => "connecting",
            Screen::Streaming(_) => "streaming",
        }
    }
}

/// Everything a running stream needs to be observed and stopped.
///
/// Kept as one struct because it moves intact from the connecting screen to
/// the streaming screen: the stream is the same one throughout, and splitting
/// these apart invited handing over two of the three.
struct Running {
    stats: Arc<StreamStats>,
    stop: Arc<AtomicBool>,
    handle: Option<StreamHandle>,
}

impl Running {
    fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

pub struct PairingScreen {
    pub state: PairingState,
    /// The thread handling the current attempt, if one is in flight.
    worker: Option<PairWorker>,
    /// Receivers to stream to once pairing finishes — carried through so
    /// the user does not have to choose again.
    targets: Vec<GroupTarget>,
}

pub struct ConnectingScreen {
    pub state: ConnectingState,
    running: Running,
    last_sample: Instant,
    /// Device id per receiver address, carried through so the dashboard can
    /// retry one later.
    device_ids: HashMap<SocketAddr, String>,
}

pub struct PickerScreen {
    pub state: PickerState,
    /// Discovery runs for as long as this screen is up; dropping it stops the
    /// mDNS daemon.
    browse: Option<BrowseHandle>,
}

pub struct StreamingScreen {
    pub state: DashboardState,
    running: Running,
    last_sample: Instant,
}

pub struct App<'a> {
    screen: Screen,
    logs: LogBuffer,
    settings: Settings,
    launch: StreamLauncher<'a>,
    handoff_available: bool,
    /// Device keys the user last chose, so a return to the picker does not
    /// make them pick again.
    last_selection: Vec<String>,
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
            last_selection: Vec::new(),
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
            Screen::Pairing(p) => {
                if let Some(result) = p.worker.as_ref().and_then(|w| w.poll()) {
                    p.worker = None;
                    p.state.on_result(result);
                }
            }
            Screen::Connecting(c) => {
                let now = Instant::now();
                if now.duration_since(c.last_sample) >= TICK {
                    c.state.sample(&c.running.stats);
                    c.last_sample = now;
                }
            }
            Screen::Streaming(s) => {
                // Sampling runs on a clock, not on loop iterations: `event::poll`
                // returns early when a key arrives, so tying it to the loop made
                // a held key shrink every measurement window.
                let now = Instant::now();
                if now.duration_since(s.last_sample) >= TICK {
                    s.state.sample(&s.running.stats, now);
                    s.last_sample = now;
                }
            }
        }
        self.advance_from_pairing();
        self.advance_from_connecting();
    }

    /// Once every queued device has been dealt with, connect.
    ///
    /// A device that could not be paired is dropped from the target list
    /// rather than blocking the rest: streaming to the speakers that did
    /// pair is better than streaming to none.
    fn advance_from_pairing(&mut self) {
        let Screen::Pairing(p) = &self.screen else {
            return;
        };
        let Some(outcome) = p.state.outcome() else {
            return;
        };
        for (device, why) in &outcome.failed {
            tracing::warn!(receiver = %device.name, "not paired: {why}");
        }
        let unpaired: Vec<SocketAddr> = outcome.failed.iter().map(|(d, _)| d.addr).collect();
        let targets: Vec<GroupTarget> = p
            .targets
            .iter()
            .filter(|t| !unpaired.contains(&t.addr))
            .cloned()
            .collect();

        if targets.is_empty() {
            // Nothing left to stream to; back to the picker rather than
            // connecting to an empty group.
            self.open_picker();
            if let Screen::Picker(picker) = &mut self.screen {
                picker.state.set_banner("no receivers were paired");
            }
            return;
        }
        self.start_stream(targets);
    }

    /// Move on once the group has settled.
    ///
    /// `Ready` goes to the dashboard; `AllFailed` stays put so the user can
    /// read why before pressing esc. Waiting does nothing — deliberately
    /// including the case where nothing has been published yet, since the
    /// stream thread may not have reached its setup loop.
    fn advance_from_connecting(&mut self) {
        let Screen::Connecting(c) = &self.screen else {
            return;
        };
        match c.state.outcome() {
            ConnectOutcome::Waiting => return,
            ConnectOutcome::AllFailed => {
                // Nothing to stream to, so waiting for the user to press
                // esc would just be a dead screen. Go back and say why.
                let banner = c.state.failure_summary();
                c.running.stop();
                self.open_picker();
                if let Screen::Picker(p) = &mut self.screen {
                    p.state.set_banner(banner);
                }
                return;
            }
            ConnectOutcome::Ready => {}
        }
        let placeholder = Screen::Picker(Box::new(PickerScreen {
            state: PickerState::new(self.settings.clone(), Vec::new(), self.handoff_available),
            browse: None,
        }));
        let Screen::Connecting(c) = std::mem::replace(&mut self.screen, placeholder) else {
            unreachable!("just matched");
        };
        let mut state = DashboardState::new(self.settings.graph, self.settings.latency_ms);
        // A retry needs the device id, which `ReceiverStat` does not carry --
        // it is what pairing keys off, so it has to come from the targets.
        state.set_device_ids(c.device_ids.clone());
        // Seed from the reading the connecting screen already took, so the
        // first dashboard frame shows the group rather than an empty list.
        state.sample(&c.running.stats, Instant::now());
        self.screen = Screen::Streaming(Box::new(StreamingScreen {
            state,
            running: c.running,
            last_sample: Instant::now(),
        }));
    }

    fn draw(&mut self, terminal: &mut term::Tui) -> io::Result<()> {
        match &self.screen {
            Screen::Picker(p) => {
                terminal.draw(|frame| picker_ui::render(frame, &p.state))?;
            }
            Screen::Pairing(p) => {
                terminal.draw(|frame| pairing_ui::render(frame, &p.state))?;
            }
            Screen::Connecting(c) => {
                terminal.draw(|frame| connecting::render(frame, &c.state))?;
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
            Screen::Connecting(c) => poll_timeout(c.last_sample.elapsed()),
            Screen::Streaming(s) => poll_timeout(s.last_sample.elapsed()),
            _ => TICK,
        }
    }

    /// `Some` once the stream has ended and the app should leave.
    fn finished_summary(&mut self) -> Option<Summary> {
        let Screen::Streaming(s) = &mut self.screen else {
            return None;
        };
        if !s.running.stats.ended() {
            return None;
        }
        let summary = Summary {
            elapsed: s.running.stats.elapsed(),
            receivers: s.state.receivers.len(),
            latency_ms: s.state.latency_ms,
            worst_lead_ms: s.state.worst_lead_ms,
            bytes_sent: s.running.stats.bytes_sent(),
            graph: s.state.graph,
        };
        if let Some(handle) = s.running.handle.take() {
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
                    self.last_selection = p.state.selection_keys();
                    self.settings = p.state.settings.clone();
                    if let Err(e) = self.settings.save() {
                        tracing::warn!("could not save settings: {e}");
                    }
                    self.begin(targets_from(&chosen), pending_pairs(&chosen));
                }
                PickerAction::None | PickerAction::Hint(_) => {}
            },
            Screen::Pairing(p) => match p.state.on_key(code) {
                PairAction::Submit(pin) => {
                    // The worker is created on first submit rather than on
                    // entering the screen: it opens a socket and waits, and
                    // holding one open while the user finds their remote is
                    // how sessions get dropped.
                    let worker = p.worker.get_or_insert_with(|| {
                        let device = p.state.current().expect("submitting implies a device");
                        PairWorker::spawn(device.addr, device.device_id.clone())
                    });
                    worker.submit(pin);
                }
                PairAction::Skip => {
                    p.worker = None;
                    p.state.skip_current();
                }
                PairAction::Cancel => {
                    p.worker = None;
                    self.open_picker();
                }
                PairAction::None => {}
            },
            Screen::Connecting(c) => {
                if c.state.on_key(code) == ConnectAction::Cancel {
                    // Tear the half-built group down before going back, or
                    // its sessions would linger on the receivers.
                    c.running.stop();
                    let banner = match c.state.outcome() {
                        ConnectOutcome::AllFailed => Some(c.state.failure_summary()),
                        _ => None,
                    };
                    self.open_picker();
                    if let (Screen::Picker(p), Some(banner)) = (&mut self.screen, banner) {
                        p.state.set_banner(banner);
                    }
                }
            }
            Screen::Streaming(s) => match s.state.on_key(code) {
                DashAction::Quit => self.request_exit(),
                DashAction::Command(cmd) => {
                    if !s.running.stats.send(cmd) {
                        tracing::warn!("could not queue command — stream mailbox unavailable");
                    }
                }
                DashAction::OpenPicker => {
                    dashboard_ui::add_receiver(terminal, &s.running.stats)?;
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
            Screen::Streaming(s) => s.running.stop(),
            // Nothing is playing yet, so there is nothing to drain: stop
            // the half-built group and leave.
            Screen::Connecting(c) => {
                c.running.stop();
                self.quitting = true;
            }
            Screen::Picker(_) | Screen::Pairing(_) => self.quitting = true,
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
            state: PickerState::with_selection(
                self.settings.clone(),
                paired,
                self.handoff_available,
                self.last_selection.clone(),
            ),
            browse,
        }));
    }

    /// Start the run: pair anything that needs it first, then connect.
    fn begin(&mut self, targets: Vec<GroupTarget>, needs_pairing: Vec<PendingPair>) {
        if needs_pairing.is_empty() {
            self.start_stream(targets);
            return;
        }
        self.screen = Screen::Pairing(Box::new(PairingScreen {
            state: PairingState::new(needs_pairing),
            worker: None,
            targets,
        }));
    }

    fn start_stream(&mut self, targets: Vec<GroupTarget>) {
        let stats = StreamStats::new(self.settings.latency_ms);
        let stop = Arc::new(AtomicBool::new(false));
        let device_ids: HashMap<SocketAddr, String> = targets
            .iter()
            .map(|t| (t.addr, t.device_id.clone()))
            .collect();
        let handle = (self.launch)(targets, Arc::clone(&stats), Arc::clone(&stop));
        self.screen = Screen::Connecting(Box::new(ConnectingScreen {
            state: ConnectingState::new(),
            running: Running {
                stats,
                stop,
                handle: Some(handle),
            },
            last_sample: Instant::now(),
            device_ids,
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

/// The chosen rows that must be paired before they can be streamed to.
pub fn pending_pairs(rows: &[PickerRow]) -> Vec<PendingPair> {
    rows.iter()
        .filter(|r| r.needs_pairing)
        .map(|r| PendingPair {
            name: r.name.clone(),
            addr: r.addr,
            device_id: r
                .device_id
                .clone()
                .unwrap_or_else(|| DEFAULT_DEVICE_ID.to_string()),
        })
        .collect()
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
    use openair_client::ReceiverState;
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

        assert_eq!(app.screen().name(), "connecting");
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
        publish(&mut app, ReceiverState::Connected, None);

        app.request_exit();
        assert!(!app.quitting, "the app waits for the stream to finish");
        let Screen::Streaming(s) = &app.screen else {
            panic!("expected streaming");
        };
        assert!(
            s.running.stop.load(Ordering::SeqCst),
            "the stream was asked to stop"
        );
    }

    #[test]
    fn quitting_while_connecting_leaves_immediately() {
        // Nothing is playing yet, so there is nothing to drain — but the
        // half-built group still has to be told to stop.
        let started = std::sync::Mutex::new(Vec::new());
        let mut app = test_app(&started);
        app.start_stream(targets_from(&[row("192.168.1.51:7000", None)]));

        app.request_exit();
        assert!(app.quitting);
        let Screen::Connecting(c) = &app.screen else {
            panic!("expected connecting");
        };
        assert!(c.running.stop.load(Ordering::SeqCst));
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
    fn starting_a_stream_opens_the_connecting_screen_first() {
        // Not the dashboard: sessions take seconds to establish, and the
        // dashboard has nothing truthful to show until they have.
        let started = std::sync::Mutex::new(Vec::new());
        let mut app = test_app(&started);
        app.start_stream(targets_from(&[row("192.168.1.51:7000", None)]));
        assert_eq!(app.screen().name(), "connecting");
    }

    #[test]
    fn the_app_advances_to_streaming_once_a_receiver_connects() {
        let started = std::sync::Mutex::new(Vec::new());
        let mut app = test_app(&started);
        app.start_stream(targets_from(&[row("192.168.1.51:7000", None)]));

        publish(&mut app, ReceiverState::Connected, None);
        assert_eq!(app.screen().name(), "streaming");
    }

    #[test]
    fn the_app_stays_on_connecting_while_a_receiver_is_pending() {
        let started = std::sync::Mutex::new(Vec::new());
        let mut app = test_app(&started);
        app.start_stream(targets_from(&[row("192.168.1.51:7000", None)]));

        publish(&mut app, ReceiverState::Connecting, None);
        assert_eq!(app.screen().name(), "connecting");
    }

    #[test]
    fn everything_failing_returns_to_the_picker_with_the_reason() {
        // Sitting on a connecting screen with nothing left to connect to would
        // be a dead end, so the app goes back on its own and says why.
        let started = std::sync::Mutex::new(Vec::new());
        let mut app = test_app(&started);
        app.start_stream(targets_from(&[row("192.168.1.51:7000", None)]));

        publish(&mut app, ReceiverState::Failed, Some("connection refused"));

        assert_eq!(app.screen().name(), "picker");
        let Screen::Picker(p) = &app.screen else {
            panic!("expected the picker");
        };
        assert!(
            p.state.banner().unwrap().contains("refused"),
            "got: {:?}",
            p.state.banner()
        );
    }

    #[test]
    fn a_failed_connect_keeps_the_selection_for_a_retry() {
        // Making the user re-pick the same receivers after a failure is the
        // kind of small insult that makes a tool tiring.
        let started = std::sync::Mutex::new(Vec::new());
        let mut app = test_app(&started);
        app.last_selection = vec!["AA:BB".to_string()];
        app.start_stream(targets_from(&[row("192.168.1.51:7000", Some("AA:BB"))]));

        publish(&mut app, ReceiverState::Failed, Some("connection refused"));

        let Screen::Picker(p) = &mut app.screen else {
            panic!("expected the picker");
        };
        p.state.insert(test_device("192.168.1.51"));
        assert_eq!(p.state.chosen().len(), 1, "still selected when it reappears");
    }

    /// Publish one receiver in `state` and let the app react.
    fn publish(app: &mut App<'_>, state: ReceiverState, error: Option<&str>) {
        let Screen::Connecting(c) = &mut app.screen else {
            panic!("expected connecting");
        };
        c.running.stats.set_receivers(vec![openair_client::ReceiverStat {
            name: "Pool Room".into(),
            addr: "192.168.1.51:7000".parse().unwrap(),
            state,
            offset_ms: 0,
            trim_db: 0.0,
            error: error.map(str::to_string),
        }]);
        c.state.sample(&c.running.stats);
        app.advance_from_connecting();
    }

    #[test]
    fn a_device_needing_a_pin_routes_through_pairing_first() {
        let started = std::sync::Mutex::new(Vec::new());
        let mut app = test_app(&started);
        let mut needs = row("192.168.1.51:7000", Some("AA:BB"));
        needs.needs_pairing = true;

        app.begin(
            targets_from(std::slice::from_ref(&needs)),
            pending_pairs(&[needs]),
        );
        assert_eq!(app.screen().name(), "pairing");
        assert!(
            started.lock().unwrap().is_empty(),
            "nothing is streamed until pairing finishes"
        );
    }

    #[test]
    fn devices_already_paired_skip_straight_to_connecting() {
        let started = std::sync::Mutex::new(Vec::new());
        let mut app = test_app(&started);
        let ready = row("192.168.1.51:7000", Some("AA:BB"));
        app.begin(
            targets_from(std::slice::from_ref(&ready)),
            pending_pairs(&[ready]),
        );
        assert_eq!(app.screen().name(), "connecting");
    }

    #[test]
    fn only_the_devices_that_need_it_are_queued_for_pairing() {
        let mut needs = row("192.168.1.51:7000", Some("AA:BB"));
        needs.needs_pairing = true;
        let ready = row("192.168.1.52:7000", Some("CC:DD"));

        let queued = pending_pairs(&[needs, ready]);
        assert_eq!(queued.len(), 1);
        assert_eq!(
            queued[0].addr,
            "192.168.1.51:7000".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn pairing_success_carries_the_selection_into_the_stream() {
        // The user chose two receivers; one needed a PIN. Both must end up in
        // the group -- making them re-pick the other would be absurd.
        let started = std::sync::Mutex::new(Vec::new());
        let mut app = test_app(&started);
        let mut needs = row("192.168.1.51:7000", Some("AA:BB"));
        needs.needs_pairing = true;
        let ready = row("192.168.1.52:7000", Some("CC:DD"));
        let rows = [needs, ready];

        app.begin(targets_from(&rows), pending_pairs(&rows));
        let Screen::Pairing(p) = &mut app.screen else {
            panic!("expected pairing");
        };
        p.state.on_result(Ok(()));
        app.advance_from_pairing();

        assert_eq!(app.screen().name(), "connecting");
        let calls = started.lock().unwrap();
        assert_eq!(calls[0].len(), 2, "both receivers made it through");
    }

    #[test]
    fn an_unpairable_device_is_dropped_but_the_others_still_play() {
        let started = std::sync::Mutex::new(Vec::new());
        let mut app = test_app(&started);
        let mut needs = row("192.168.1.51:7000", Some("AA:BB"));
        needs.needs_pairing = true;
        let ready = row("192.168.1.52:7000", Some("CC:DD"));
        let rows = [needs, ready];

        app.begin(targets_from(&rows), pending_pairs(&rows));
        let Screen::Pairing(p) = &mut app.screen else {
            panic!("expected pairing");
        };
        p.state.skip_current();
        app.advance_from_pairing();

        assert_eq!(app.screen().name(), "connecting");
        let calls = started.lock().unwrap();
        assert_eq!(calls[0].len(), 1, "only the paired receiver");
        assert_eq!(
            calls[0][0].addr,
            "192.168.1.52:7000".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn skipping_the_only_device_returns_to_the_picker() {
        let started = std::sync::Mutex::new(Vec::new());
        let mut app = test_app(&started);
        let mut needs = row("192.168.1.51:7000", Some("AA:BB"));
        needs.needs_pairing = true;

        app.begin(
            targets_from(std::slice::from_ref(&needs)),
            pending_pairs(&[needs]),
        );
        let Screen::Pairing(p) = &mut app.screen else {
            panic!("expected pairing");
        };
        p.state.skip_current();
        app.advance_from_pairing();

        assert_eq!(app.screen().name(), "picker");
        assert!(started.lock().unwrap().is_empty(), "nothing to stream to");
        let Screen::Picker(p) = &app.screen else {
            panic!("expected the picker");
        };
        assert!(p.state.banner().unwrap().contains("no receivers"));
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
        assert_eq!(app.screen().name(), "connecting");
    }

    #[test]
    fn the_summary_is_taken_once_the_stream_ends() {
        let started = std::sync::Mutex::new(Vec::new());
        let mut app = test_app(&started);
        app.start_stream(targets_from(&[row("192.168.1.51:7000", None)]));
        publish(&mut app, ReceiverState::Connected, None);

        // The test launcher marks the stream ended on its own thread; wait for
        // it rather than racing it.
        let deadline = Instant::now() + Duration::from_secs(2);
        while app.finished_summary().is_none() {
            assert!(Instant::now() < deadline, "stream never reported ending");
            std::thread::yield_now();
        }
    }
}
