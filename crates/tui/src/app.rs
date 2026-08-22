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
use openair_client::{GroupTarget, StreamCommand, StreamStats};
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
use crate::settings_screen::{SettingsAction, SettingsRow, SettingsState};
use crate::settings_ui;
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
/// The `Settings` passed are the ones in force *now* — including anything the
/// user changed in the picker — so handoff, latency and volume take effect on
/// the run they were chosen for rather than the next one.
///
/// `FnMut` rather than `FnOnce`: returning to the picker after a failure and
/// starting again is a supported path.
pub type StreamLauncher<'a> = Box<
    dyn FnMut(Vec<GroupTarget>, Settings, Arc<StreamStats>, Arc<AtomicBool>) -> StreamHandle + 'a,
>;

/// Apply a settings change that needs work the TUI cannot do itself —
/// switching a Windows audio endpoint, above all.
///
/// Receives the settings in force before the change and after it, so the
/// applier acts only on what actually differs. Supplied by the CLI for the same
/// reason [`StreamLauncher`] is: `openair-tui` does not depend on
/// `openair-capture`, and that boundary is what keeps this crate testable on
/// every platform.
///
/// Called on the main thread. `cpal::Stream` is `!Send`, so `SystemCapture` —
/// and the capture rig holding it — never leave the thread that created them,
/// which is this one.
pub type SettingsApplier<'a> = Box<dyn FnMut(&Settings, &Settings) -> Result<(), String> + 'a>;

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
    /// The settings overlay, drawn *over* the screen it was opened from.
    Settings(Box<SettingsScreen>),
}

impl Screen {
    /// Short name, for tests and logs.
    pub fn name(&self) -> &'static str {
        match self {
            Screen::Picker(_) => "picker",
            Screen::Pairing(_) => "pairing",
            Screen::Connecting(_) => "connecting",
            Screen::Streaming(_) => "streaming",
            Screen::Settings(_) => "settings",
        }
    }

    /// The streaming screen, whether it is on top or underneath the settings
    /// overlay.
    ///
    /// Sampling, stopping and the closing summary all have to reach it either
    /// way. The overlay exists precisely so the user can watch the dashboard
    /// react while they adjust something — freezing it the moment settings
    /// opened would defeat the point.
    fn streaming(&self) -> Option<&StreamingScreen> {
        match self {
            Screen::Streaming(s) => Some(s),
            Screen::Settings(s) => s.origin.streaming(),
            _ => None,
        }
    }

    fn streaming_mut(&mut self) -> Option<&mut StreamingScreen> {
        match self {
            Screen::Streaming(s) => Some(s),
            Screen::Settings(s) => s.origin.streaming_mut(),
            _ => None,
        }
    }
}

/// The settings overlay, plus the screen it was opened from so closing returns
/// there rather than to a fixed destination.
pub struct SettingsScreen {
    pub state: SettingsState,
    origin: Box<Screen>,
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
    /// Applies settings changes that need work this crate cannot do — see
    /// [`SettingsApplier`]. `None` in tests and on platforms with nothing to
    /// apply, where a change is simply stored.
    applier: Option<SettingsApplier<'a>>,
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
            applier: None,
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
    /// Supply the closure that applies platform-side settings changes.
    pub fn with_applier(mut self, applier: SettingsApplier<'a>) -> Self {
        self.applier = Some(applier);
        self
    }

    /// A fresh, empty picker. Used only as a placeholder while another screen
    /// is moved out from under `&mut self`.
    fn placeholder(&self) -> Screen {
        Screen::Picker(Box::new(PickerScreen {
            state: PickerState::new(self.settings.clone(), Vec::new(), self.handoff_available),
            browse: None,
        }))
    }

    /// Open the settings overlay over whatever is on screen now.
    pub fn open_settings(&mut self) {
        let streaming = self.screen.streaming().is_some();
        let state = SettingsState::new(self.settings.clone(), self.handoff_available, streaming);
        let placeholder = self.placeholder();
        let origin = Box::new(std::mem::replace(&mut self.screen, placeholder));
        self.screen = Screen::Settings(Box::new(SettingsScreen { state, origin }));
    }

    /// Close the overlay, returning to the screen it was opened from.
    fn close_settings(&mut self) {
        let placeholder = self.placeholder();
        if let Screen::Settings(s) = std::mem::replace(&mut self.screen, placeholder) {
            self.screen = *s.origin;
        }
    }

    /// Apply a settings change, reverting it if the applier refuses.
    ///
    /// Order matters: platform work first, then the stream commands, then
    /// persistence. A setting that could not be applied must never reach
    /// `settings.json`, or the file would claim a state the program is not in.
    fn apply_settings(&mut self, previous: Settings, next: Settings) {
        if let Some(applier) = self.applier.as_mut() {
            if let Err(why) = applier(&previous, &next) {
                if let Screen::Settings(s) = &mut self.screen {
                    s.state.set_error(row_for_change(&previous, &next), why);
                    s.state.revert(previous);
                }
                return;
            }
        }

        if let Some(stream) = self.screen.streaming_mut() {
            let stats = Arc::clone(&stream.running.stats);
            let queue = |cmd| {
                if !stats.send(cmd) {
                    tracing::warn!("could not queue command — stream mailbox unavailable");
                }
            };
            if next.latency_ms != previous.latency_ms {
                queue(StreamCommand::SetLatency {
                    ms: next.latency_ms,
                });
            }
            if next.volume_db != previous.volume_db {
                queue(StreamCommand::SetMasterVolume { db: next.volume_db });
            }
            if next.metadata != previous.metadata {
                queue(StreamCommand::SetMetadataEnabled { on: next.metadata });
            }
            stream.state.show_controls = next.show_controls;
        }

        // The screen underneath keeps its own copy: leaving it stale would show
        // old values in the picker's footer the moment the overlay closes, and
        // its `h` key would toggle from the wrong starting point.
        if let Screen::Settings(s) = &mut self.screen {
            if let Screen::Picker(p) = s.origin.as_mut() {
                p.state.settings = next.clone();
            }
        }

        self.settings = next;
        if let Err(e) = self.settings.save() {
            tracing::warn!("could not save settings: {e}");
        }
    }

    /// Key handling for the paths that need no terminal.
    ///
    /// `on_key` takes one for the add-receiver overlay, which a unit test
    /// cannot supply. The settings overlay never needs it, so this exposes the
    /// same dispatch for the screens that do not.
    #[cfg(test)]
    fn on_key_for_test(&mut self, code: KeyCode) {
        if code == KeyCode::Char('s')
            && matches!(self.screen, Screen::Picker(_) | Screen::Streaming(_))
        {
            self.open_settings();
            return;
        }
        if matches!(self.screen, Screen::Settings(_)) {
            let previous = self.settings.clone();
            let Screen::Settings(sc) = &mut self.screen else {
                unreachable!("just matched");
            };
            match sc.state.on_key(code) {
                SettingsAction::None => {}
                SettingsAction::Close => self.close_settings(),
                SettingsAction::Apply(next) => self.apply_settings(previous, next),
            }
        }
    }

    fn tick(&mut self) {
        tick_screen(&mut self.screen);
        self.adopt_stream_latency();
        self.advance_from_pairing();
        self.advance_from_connecting();
    }

    /// Persist a latency the *stream* chose.
    ///
    /// Auto-latency raises the anchor depth when a receiver is running dry, and
    /// that value is the one the network actually needed. Leaving it unsaved
    /// meant every run started back at a setting already known to be too low
    /// for this house, and re-learned it the hard way — with a dropout each
    /// time.
    ///
    /// Only ever adopts what the stream reports, so a user's own change is
    /// still theirs: the stream is told about that one first, and reports the
    /// same number straight back.
    fn adopt_stream_latency(&mut self) {
        let Some(stream) = self.screen.streaming() else {
            return;
        };
        let live = stream.state.latency_ms;
        if live == 0 || live == self.settings.latency_ms {
            return;
        }
        tracing::info!(
            from_ms = self.settings.latency_ms,
            to_ms = live,
            "adopting the latency the stream settled on"
        );
        self.settings.latency_ms = live;
        if let Err(e) = self.settings.save() {
            tracing::warn!("could not save settings: {e}");
        }
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
        let mut state = DashboardState::new(self.settings.latency_ms);
        state.show_controls = self.settings.show_controls;
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
}

/// Draw one screen. Recursive so the settings overlay draws the screen it was
/// opened from underneath itself.
fn render_screen(frame: &mut ratatui::Frame, screen: &Screen, logs: &LogBuffer) {
    match screen {
        Screen::Picker(p) => picker_ui::render(frame, &p.state),
        Screen::Pairing(p) => pairing_ui::render(frame, &p.state),
        Screen::Connecting(c) => connecting::render(frame, &c.state),
        Screen::Streaming(s) => dashboard_ui::render(frame, &s.state, logs),
        Screen::Settings(s) => {
            render_screen(frame, &s.origin, logs);
            settings_ui::render(frame, &s.state);
        }
    }
}

impl<'a> App<'a> {
    fn draw(&mut self, terminal: &mut term::Tui) -> io::Result<()> {
        let screen = &self.screen;
        let logs = &self.logs;
        terminal.draw(|frame| render_screen(frame, screen, logs))?;
        Ok(())
    }

    /// How long to wait for a key before the next iteration.
    fn poll_timeout(&self) -> Duration {
        // Through the overlay too: the dashboard underneath still needs its
        // sampling cadence while settings are open.
        if let Some(s) = self.screen.streaming() {
            return poll_timeout(s.last_sample.elapsed());
        }
        match &self.screen {
            Screen::Connecting(c) => poll_timeout(c.last_sample.elapsed()),
            _ => TICK,
        }
    }

    /// `Some` once the stream has ended and the app should leave.
    fn finished_summary(&mut self) -> Option<Summary> {
        // Reached through the overlay as well: a stream that ends while the
        // user has settings open must still finish and print its summary.
        let s = self.screen.streaming_mut()?;
        if !s.running.stats.ended() {
            return None;
        }
        let summary = Summary {
            elapsed: s.running.stats.elapsed(),
            receivers: s.state.receivers.len(),
            latency_ms: s.state.latency_ms,
            worst_lead_ms: s.state.worst_lead_ms,
            bytes_sent: s.running.stats.bytes_sent(),
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

        // `s` means the same thing on the two screens that offer it, so it is
        // answered here rather than threaded through two key handlers that
        // would each have to return a new action for it.
        if code == KeyCode::Char('s')
            && matches!(self.screen, Screen::Picker(_) | Screen::Streaming(_))
        {
            self.open_settings();
            return Ok(());
        }

        match &mut self.screen {
            Screen::Settings(_) => {
                let previous = self.settings.clone();
                let Screen::Settings(sc) = &mut self.screen else {
                    unreachable!("just matched");
                };
                match sc.state.on_key(code) {
                    SettingsAction::None => {}
                    SettingsAction::Close => self.close_settings(),
                    SettingsAction::Apply(next) => self.apply_settings(previous, next),
                }
            }
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
            // Ctrl+C with the overlay open means the same as without it: act
            // on what is actually running underneath.
            Screen::Settings(s) => match s.origin.as_ref() {
                Screen::Streaming(st) => st.running.stop(),
                Screen::Connecting(c) => {
                    c.running.stop();
                    self.quitting = true;
                }
                _ => self.quitting = true,
            },
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
        let handle = (self.launch)(
            targets,
            self.settings.clone(),
            Arc::clone(&stats),
            Arc::clone(&stop),
        );
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

/// One tick of whatever the screen needs: draining discovery, polling a pair
/// worker, sampling a stream.
///
/// A free function so the settings overlay can recurse into the screen beneath
/// it. Without that, opening settings over the dashboard would freeze the
/// buffer bars — and watching them react is the whole reason the settings
/// panel is an overlay rather than a page.
fn tick_screen(screen: &mut Screen) {
    match screen {
        Screen::Settings(s) => tick_screen(&mut s.origin),
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
}

/// Which row a change belongs to, so a failure lands on the row that caused it.
///
/// Falls back to handoff, the only row whose apply can fail today — and the one
/// a caller with no detectable difference most likely came from.
fn row_for_change(previous: &Settings, next: &Settings) -> SettingsRow {
    if previous.latency_ms != next.latency_ms {
        SettingsRow::Latency
    } else if previous.volume_db != next.volume_db {
        SettingsRow::Volume
    } else if previous.metadata != next.metadata {
        SettingsRow::Metadata
    } else if previous.show_controls != next.show_controls {
        SettingsRow::ShowControls
    } else {
        SettingsRow::Handoff
    }
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
        let launch: StreamLauncher<'_> = Box::new(move |targets, _settings, stats, _stop| {
            started.lock().unwrap().push(targets);
            StreamHandle::new(std::thread::spawn(move || {
                stats.mark_ended();
                Ok(())
            }))
        });
        App::new(Settings::default(), LogBuffer::new(10), false, launch)
    }

    #[test]
    fn s_opens_settings_and_esc_returns_to_the_picker() {
        let started = std::sync::Mutex::new(Vec::new());
        let mut app = test_app(&started);
        app.open_settings();
        assert_eq!(app.screen().name(), "settings");

        app.close_settings();
        assert_eq!(
            app.screen().name(),
            "picker",
            "closing returns to where it was opened from, not to a fixed screen"
        );
    }

    #[test]
    fn a_failed_apply_reverts_the_setting_and_explains() {
        // The property the applier closure exists to make testable: no
        // hardware, no endpoint switch, just "the applier said no".
        let started = std::sync::Mutex::new(Vec::new());
        let calls = std::cell::RefCell::new(Vec::new());
        let applier: SettingsApplier<'_> = Box::new(|old: &Settings, new: &Settings| {
            calls.borrow_mut().push((old.latency_ms, new.latency_ms));
            Err("cable disappeared".to_string())
        });
        let mut app = test_app(&started).with_applier(applier);

        let before = app.settings.clone();
        app.open_settings();
        // Move to latency and step it up.
        app.on_key_for_test(KeyCode::Down);
        app.on_key_for_test(KeyCode::Right);

        let Screen::Settings(sc) = &app.screen else {
            panic!("still on the settings overlay");
        };
        assert_eq!(
            sc.state.settings.latency_ms, before.latency_ms,
            "reverted to what is actually in force"
        );
        assert_eq!(sc.state.error(), Some("cable disappeared"));
        assert_eq!(sc.state.error_row(), Some(SettingsRow::Latency));
        assert_eq!(
            app.settings.latency_ms, before.latency_ms,
            "a setting that could not be applied must not become the app's"
        );
        assert_eq!(calls.borrow().len(), 1, "the applier was consulted once");
    }

    #[test]
    fn a_successful_apply_reaches_the_screen_underneath() {
        // The picker keeps its own copy of the settings. Leaving it stale
        // would show old values in its footer the moment the overlay closes,
        // and its `h` key would toggle from the wrong starting point.
        let started = std::sync::Mutex::new(Vec::new());
        let mut app = test_app(&started);

        app.open_settings();
        app.on_key_for_test(KeyCode::Down);
        app.on_key_for_test(KeyCode::Right);
        let raised = app.settings.latency_ms;
        app.close_settings();

        let Screen::Picker(p) = &app.screen else {
            panic!("back on the picker");
        };
        assert_eq!(p.state.settings.latency_ms, raised);
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
        assert_eq!(
            p.state.chosen().len(),
            1,
            "still selected when it reappears"
        );
    }

    /// Publish one receiver in `state` and let the app react.
    fn publish(app: &mut App<'_>, state: ReceiverState, error: Option<&str>) {
        let Screen::Connecting(c) = &mut app.screen else {
            panic!("expected connecting");
        };
        c.running
            .stats
            .set_receivers(vec![openair_client::ReceiverStat {
                name: "Pool Room".into(),
                addr: "192.168.1.51:7000".parse().unwrap(),
                state,
                offset_ms: 0,
                trim_db: 0.0,
                lead_ms: None,
                health: 0.0,
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
