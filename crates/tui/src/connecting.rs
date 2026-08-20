//! The connecting screen: what the user watches while sessions are
//! established.
//!
//! Everything here is derived from the same [`StreamStats`] snapshot the
//! dashboard reads — the stream publishes each receiver as `Connecting` before
//! its handshake and `Connected`/`Failed` after, so this screen needs no
//! machinery of its own.

use std::time::Instant;

use crossterm::event::KeyCode;
use openair_client::{ReceiverStat, ReceiverState, StreamStats};

/// Spinner frames. Braille dots rather than ASCII: they animate in place
/// without the width jitter of `|/-\`.
const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectOutcome {
    /// At least one receiver is still handshaking.
    Waiting,
    /// Everything has settled and at least one receiver connected.
    Ready,
    /// Everything has settled and none connected.
    AllFailed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectAction {
    None,
    /// Give up and go back to choosing receivers.
    Cancel,
}

pub struct ConnectingState {
    pub receivers: Vec<ReceiverStat>,
    spinner: usize,
    started_at: Instant,
}

impl Default for ConnectingState {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnectingState {
    pub fn new() -> Self {
        Self {
            receivers: Vec::new(),
            spinner: 0,
            started_at: Instant::now(),
        }
    }

    /// Take a reading and advance the spinner one frame.
    ///
    /// Called on the render clock, not per loop iteration — `event::poll`
    /// returns early on a keypress, so an iteration-driven spinner would race
    /// ahead whenever a key was held.
    pub fn sample(&mut self, stats: &StreamStats) {
        self.receivers = stats.receivers();
        self.spinner = (self.spinner + 1) % SPINNER.len();
    }

    pub fn spinner(&self) -> char {
        SPINNER[self.spinner]
    }

    pub fn elapsed(&self) -> std::time::Duration {
        self.started_at.elapsed()
    }

    pub fn on_key(&mut self, key: KeyCode) -> ConnectAction {
        match key {
            KeyCode::Esc | KeyCode::Char('q') => ConnectAction::Cancel,
            _ => ConnectAction::None,
        }
    }

    /// Whether the group has settled, and how it went.
    pub fn outcome(&self) -> ConnectOutcome {
        // Nothing published yet: the stream thread has not reached its setup
        // loop. Treat as waiting, never as failure — declaring "all failed"
        // before a single handshake started would be a lie.
        if self.receivers.is_empty() {
            return ConnectOutcome::Waiting;
        }
        if self.receivers.iter().any(|r| r.state.is_pending()) {
            return ConnectOutcome::Waiting;
        }
        if self
            .receivers
            .iter()
            .any(|r| r.state == ReceiverState::Connected)
        {
            ConnectOutcome::Ready
        } else {
            ConnectOutcome::AllFailed
        }
    }

    /// One-line explanation for the all-failed case, for the picker's banner.
    ///
    /// Prefers a specific reason when every receiver gave the same one, since
    /// that is usually the actionable case (wrong interface, whole subnet
    /// unreachable). Otherwise says how many failed and leaves the detail to
    /// the list.
    pub fn failure_summary(&self) -> String {
        let reasons: Vec<&str> = self
            .receivers
            .iter()
            .filter_map(|r| r.error.as_deref())
            .collect();
        let all_same = reasons
            .first()
            .is_some_and(|first| reasons.iter().all(|r| r == first));

        match (self.receivers.len(), all_same) {
            (1, _) => match self.receivers[0].error.as_deref() {
                Some(why) => format!("could not connect: {why}"),
                None => "could not connect to that receiver".to_string(),
            },
            (n, true) => format!("none of the {n} receivers connected: {}", reasons[0]),
            (n, false) => format!("none of the {n} receivers connected"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    fn addr(n: u8) -> SocketAddr {
        format!("192.168.1.{n}:7000").parse().unwrap()
    }

    fn stat(n: u8, state: ReceiverState, error: Option<&str>) -> ReceiverStat {
        ReceiverStat {
            name: format!("Receiver {n}"),
            addr: addr(n),
            state,
            offset_ms: 0,
            trim_db: 0.0,
            error: error.map(str::to_string),
        }
    }

    fn with(receivers: Vec<ReceiverStat>) -> ConnectingState {
        let stats = StreamStats::new(500);
        stats.set_receivers(receivers);
        let mut state = ConnectingState::new();
        state.sample(&stats);
        state
    }

    #[test]
    fn nothing_published_yet_is_waiting_not_failure() {
        // The stream thread has not reached its setup loop. Calling this
        // "all failed" would send the user back to the picker before a single
        // handshake had been attempted.
        assert_eq!(ConnectingState::new().outcome(), ConnectOutcome::Waiting);
    }

    #[test]
    fn any_pending_receiver_means_waiting() {
        let state = with(vec![
            stat(51, ReceiverState::Connected, None),
            stat(52, ReceiverState::Connecting, None),
        ]);
        assert_eq!(
            state.outcome(),
            ConnectOutcome::Waiting,
            "starting now would strand the second receiver"
        );
    }

    #[test]
    fn settled_with_one_connected_is_ready() {
        let state = with(vec![
            stat(51, ReceiverState::Connected, None),
            stat(52, ReceiverState::Failed, Some("connection refused")),
        ]);
        assert_eq!(
            state.outcome(),
            ConnectOutcome::Ready,
            "partial success still plays"
        );
    }

    #[test]
    fn settled_with_none_connected_is_all_failed() {
        let state = with(vec![
            stat(51, ReceiverState::Failed, Some("connection refused")),
            stat(52, ReceiverState::Failed, Some("no route to host")),
        ]);
        assert_eq!(state.outcome(), ConnectOutcome::AllFailed);
    }

    #[test]
    fn a_single_connected_receiver_is_ready() {
        let state = with(vec![stat(51, ReceiverState::Connected, None)]);
        assert_eq!(state.outcome(), ConnectOutcome::Ready);
    }

    #[test]
    fn the_spinner_advances_only_when_sampled() {
        let stats = StreamStats::new(500);
        let mut state = ConnectingState::new();
        let first = state.spinner();
        // Keys must not move it; only the render clock does.
        state.on_key(KeyCode::Char('x'));
        assert_eq!(state.spinner(), first);
        state.sample(&stats);
        assert_ne!(state.spinner(), first);
    }

    #[test]
    fn the_spinner_wraps() {
        let stats = StreamStats::new(500);
        let mut state = ConnectingState::new();
        let first = state.spinner();
        for _ in 0..SPINNER.len() {
            state.sample(&stats);
        }
        assert_eq!(state.spinner(), first, "a full cycle returns to the start");
    }

    #[test]
    fn esc_cancels() {
        let mut state = ConnectingState::new();
        assert_eq!(state.on_key(KeyCode::Esc), ConnectAction::Cancel);
        assert_eq!(state.on_key(KeyCode::Char('q')), ConnectAction::Cancel);
        assert_eq!(state.on_key(KeyCode::Enter), ConnectAction::None);
    }

    #[test]
    fn one_receiver_failing_names_its_reason() {
        let state = with(vec![stat(51, ReceiverState::Failed, Some("try --bind 10.0.0.2"))]);
        assert_eq!(
            state.failure_summary(),
            "could not connect: try --bind 10.0.0.2"
        );
    }

    #[test]
    fn a_shared_reason_is_reported_once() {
        // Every receiver failing the same way is the actionable case — usually
        // the wrong source interface, not three broken speakers.
        let state = with(vec![
            stat(51, ReceiverState::Failed, Some("try --bind 10.0.0.2")),
            stat(52, ReceiverState::Failed, Some("try --bind 10.0.0.2")),
        ]);
        assert_eq!(
            state.failure_summary(),
            "none of the 2 receivers connected: try --bind 10.0.0.2"
        );
    }

    #[test]
    fn differing_reasons_leave_the_detail_to_the_list() {
        let state = with(vec![
            stat(51, ReceiverState::Failed, Some("connection refused")),
            stat(52, ReceiverState::Failed, Some("no route to host")),
        ]);
        assert_eq!(state.failure_summary(), "none of the 2 receivers connected");
    }

    #[test]
    fn a_failure_without_a_reason_still_reads() {
        let state = with(vec![stat(51, ReceiverState::Failed, None)]);
        assert_eq!(state.failure_summary(), "could not connect to that receiver");
    }
}

// --- rendering ---------------------------------------------------------

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

pub fn render(frame: &mut Frame, state: &ConnectingState) {
    let area = centred_area(frame.area());
    let [header, list, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .areas(area);

    // No all-failed rendering here: the app leaves for the picker as soon as
    // the group settles that way, carrying the reason as a banner, so this
    // screen only ever shows work in progress. Individual failures still
    // appear in the list below while others are still connecting.
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                format!("  {} connecting…", state.spinner()),
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                format!("  {:.0}s elapsed", state.elapsed().as_secs_f32()),
                Style::default().fg(Color::DarkGray),
            )),
        ])
        .block(Block::default().borders(Borders::ALL).title(" OpenAir ")),
        header,
    );

    let rows: Vec<Line> = if state.receivers.is_empty() {
        vec![Line::from(Span::styled(
            "  preparing…",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        state
            .receivers
            .iter()
            .map(|r| {
                let colour = match r.state {
                    ReceiverState::Connected => Color::Green,
                    ReceiverState::Connecting | ReceiverState::Reconnecting => Color::Yellow,
                    ReceiverState::Failed | ReceiverState::Dead => Color::Red,
                };
                let mut spans = vec![
                    Span::raw(format!("  {:<24}", r.name)),
                    Span::styled(format!("{:<15}", r.state.label()), Style::default().fg(colour)),
                ];
                // The reason matters more than the status word, so give it the
                // rest of the row rather than truncating it away.
                if let Some(why) = &r.error {
                    spans.push(Span::styled(
                        why.clone(),
                        Style::default().fg(Color::DarkGray),
                    ));
                }
                Line::from(spans)
            })
            .collect()
    };
    frame.render_widget(
        Paragraph::new(rows).block(Block::default().borders(Borders::ALL).title(" receivers ")),
        list,
    );

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "  esc to cancel",
            Style::default().fg(Color::DarkGray),
        ))),
        footer,
    );
}

/// A comfortable box in the middle of the terminal: this screen has little to
/// say, and saying it full-width reads as an error report rather than progress.
fn centred_area(area: Rect) -> Rect {
    let width = area.width.min(76);
    let height = area.height.min(14);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod render_tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn draw(width: u16, height: u16, state: &ConnectingState) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(frame, state)).unwrap();
        terminal.backend().to_string()
    }

    #[test]
    fn renders_progress_without_panicking() {
        let stats = StreamStats::new(500);
        stats.set_receivers(vec![ReceiverStat {
            name: "Pool Room".into(),
            addr: "192.168.1.51:7000".parse().unwrap(),
            state: ReceiverState::Connecting,
            offset_ms: 0,
            trim_db: 0.0,
            error: None,
        }]);
        let mut state = ConnectingState::new();
        state.sample(&stats);

        let out = draw(100, 30, &state);
        assert!(out.contains("Pool Room"));
        assert!(out.contains("connecting"));
        assert!(out.contains("esc to cancel"));
    }

    #[test]
    fn a_failure_shows_its_reason_while_others_connect() {
        // A partial failure stays on screen: the user can see which receiver
        // dropped out and why, without losing the ones still connecting.
        let stats = StreamStats::new(500);
        stats.set_receivers(vec![
            ReceiverStat {
                name: "Pool Room".into(),
                addr: "192.168.1.51:7000".parse().unwrap(),
                state: ReceiverState::Failed,
                offset_ms: 0,
                trim_db: 0.0,
                error: Some("connection refused".into()),
            },
            ReceiverStat {
                name: "Living Room".into(),
                addr: "192.168.1.52:7000".parse().unwrap(),
                state: ReceiverState::Connecting,
                offset_ms: 0,
                trim_db: 0.0,
                error: None,
            },
        ]);
        let mut state = ConnectingState::new();
        state.sample(&stats);

        let out = draw(100, 30, &state);
        assert!(out.contains("refused"), "the reason must be on screen: {out}");
        assert!(out.contains("Living Room"));
    }

    #[test]
    fn survives_a_tiny_terminal() {
        // centred_area clamps to the frame; without that this panics.
        let state = ConnectingState::new();
        draw(20, 6, &state);
    }
}
