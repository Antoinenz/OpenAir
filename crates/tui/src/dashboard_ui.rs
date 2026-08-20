//! Drawing the streaming dashboard, and the add-a-receiver overlay.
//!
//! The event loop lives in [`crate::app`]; this module only renders and runs
//! the modal overlay. Quitting is still expressed through the stream's `stop`
//! flag so shutdown takes the graceful path — play out the queued audio,
//! TEARDOWN each session, restore the Windows audio device.

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use openair_client::{ReceiverState, StreamCommand, StreamStats};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Sparkline};
use ratatui::Frame;

use crate::dashboard::{format_bitrate, format_bytes, format_elapsed, DashboardState};
use crate::logs::LogBuffer;
use crate::picker::{PickerAction, PickerState};
use crate::settings::Settings;
use crate::term;

/// Render rate. Fast enough to feel live, slow enough to be invisible next to
/// the ~43 audio packets per second the stream is actually sending.
const TICK: Duration = Duration::from_millis(100);

/// Below this the layout cannot be drawn honestly, so we say so instead of
/// rendering an unreadable jumble.
const MIN_WIDTH: u16 = 60;
const MIN_HEIGHT: u16 = 20;

/// What the run looked like, printed to normal scrollback after the terminal
/// is restored so the session leaves a trace.
pub struct Summary {
    pub elapsed: Duration,
    pub receivers: usize,
    pub latency_ms: u64,
    pub worst_lead_ms: Option<i64>,
    pub bytes_sent: u64,
}

impl std::fmt::Display for Summary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "  streamed {} to {} receiver(s) · {} · final latency {} ms",
            format_elapsed(self.elapsed),
            self.receivers,
            format_bytes(self.bytes_sent),
            self.latency_ms
        )?;
        if let Some(worst) = self.worst_lead_ms {
            write!(f, " · lowest buffer {worst} ms")?;
        }
        Ok(())
    }
}

/// Modal overlay: browse for receivers and add the chosen ones to the live
/// group.
///
/// Reuses [`PickerState`] wholesale rather than growing a second list widget —
/// the sort order, de-duplication and "needs pairing" rules are the ones the
/// startup picker already got right, and having two implementations of them
/// would mean fixing every such bug twice.
pub fn add_receiver(terminal: &mut term::Tui, stats: &StreamStats) -> io::Result<()> {
    let paired = openair_client::PairingStore::load()
        .map(|s| s.peer_ids())
        .unwrap_or_default();
    // Handoff is already decided for this run; the overlay only picks devices.
    let mut state = PickerState::new(Settings::default(), paired, false);

    let browse = openair_discovery::browse_live()
        .map_err(|e| io::Error::other(format!("mDNS discovery failed: {e}")))?;

    // Receivers already in the group must not be offered again.
    let existing: Vec<std::net::SocketAddr> = stats.receivers().iter().map(|r| r.addr).collect();

    loop {
        while let Ok(device) = browse.devices.try_recv() {
            let addr = std::net::SocketAddr::new(device.addr, device.port);
            if !existing.contains(&addr) {
                state.insert(device);
            }
        }

        terminal.draw(|frame| render_add_overlay(frame, &state))?;

        if !event::poll(TICK)? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Ok(());
        }

        match state.on_key(key.code) {
            PickerAction::Quit => return Ok(()),
            PickerAction::Start => {
                for row in state.chosen() {
                    let device_id = row
                        .device_id
                        .clone()
                        .unwrap_or_else(|| DEFAULT_DEVICE_ID.to_string());
                    stats.send(StreamCommand::Add {
                        addr: row.addr,
                        device_id,
                    });
                }
                return Ok(());
            }
            PickerAction::None | PickerAction::Hint(_) => {}
        }
    }
}

/// Fallback device id for a receiver that advertises none, matching the CLI.
const DEFAULT_DEVICE_ID: &str = "AA:BB:CC:DD:EE:FF";

fn render_add_overlay(frame: &mut Frame, state: &PickerState) {
    let area = centred(frame.area(), 70, 14);
    // Clear first: this is drawn over a live dashboard frame, and without it
    // the panel underneath shows through the gaps.
    frame.render_widget(ratatui::widgets::Clear, area);

    let [list_area, hint_area] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(area);

    crate::picker_ui::render_device_list(frame, list_area, state, " add a receiver ");

    let hint = match state.hint() {
        Some(h) => Span::styled(format!("  {h}"), Style::default().fg(Color::Yellow)),
        None => Span::styled(
            "  ↑↓ move · space select · ⏎ add · esc cancel",
            Style::default().fg(Color::DarkGray),
        ),
    };
    frame.render_widget(Paragraph::new(Line::from(hint)), hint_area);
}

/// A rectangle of at most `width`×`height`, centred in `area`.
fn centred(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

pub fn render(frame: &mut Frame, state: &DashboardState, buffer: &LogBuffer) {
    let area = frame.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        render_too_small(frame, area);
        return;
    }

    let [top, graph, receivers, log_area] = Layout::vertical([
        Constraint::Length(4),
        Constraint::Length(5),
        Constraint::Min(3),
        Constraint::Length(8),
    ])
    .areas(area);

    render_top(frame, top, state);
    render_graph(frame, graph, state);
    render_receivers(frame, receivers, state);
    render_logs(frame, log_area, buffer, state);
}

fn render_too_small(frame: &mut Frame, area: Rect) {
    let msg = Paragraph::new(vec![
        Line::from(""),
        Line::from("terminal too small"),
        Line::from(format!("need at least {MIN_WIDTH}×{MIN_HEIGHT}")),
    ])
    .alignment(Alignment::Center)
    .style(Style::default().fg(Color::Yellow));
    frame.render_widget(msg, area);
}

fn render_top(frame: &mut Frame, area: Rect, state: &DashboardState) {
    let [latency, bandwidth, playing] = Layout::horizontal([
        Constraint::Length(16),
        Constraint::Length(20),
        Constraint::Min(20),
    ])
    .areas(area);

    let lead = match state.latest_lead_ms() {
        Some(ms) => format!("{ms} ms ahead"),
        None => "—".to_string(),
    };
    frame.render_widget(
        boxed("latency", vec![
            Line::from(Span::styled(
                format!("{} ms", state.latency_ms),
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(lead, Style::default().fg(Color::DarkGray))),
        ]),
        latency,
    );

    frame.render_widget(
        boxed("bandwidth", vec![
            Line::from(Span::styled(
                format_bitrate(state.bandwidth_bps()),
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                format_bytes(state.bytes_total()),
                Style::default().fg(Color::DarkGray),
            )),
        ]),
        bandwidth,
    );

    let np = match &state.now_playing {
        Some(np) => {
            let mut second = np.album.clone();
            if np.art.is_some() {
                second.push_str("  [art]");
            }
            vec![
                Line::from(Span::styled(
                    format!("{} — {}", np.title, np.artist),
                    Style::default().add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(second, Style::default().fg(Color::DarkGray))),
            ]
        }
        None => vec![Line::from(Span::styled(
            "no track info",
            Style::default().fg(Color::DarkGray),
        ))],
    };
    frame.render_widget(boxed("now playing", np), playing);
}

fn boxed<'a>(title: &'a str, lines: Vec<Line<'a>>) -> Paragraph<'a> {
    Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" {title} ")),
    )
}

fn render_graph(frame: &mut Frame, area: Rect, state: &DashboardState) {
    let [chart, footer] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area);

    let block = Block::default().borders(Borders::ALL).title(" bandwidth ");
    let inner = block.inner(chart);
    frame.render_widget(block, chart);

    // ratatui draws a sparkline oldest-first from the left; show only as many
    // samples as there are columns so the newest is always at the right edge.
    let series = state.graph_series();
    let visible = series
        .iter()
        .rev()
        .take(inner.width as usize)
        .rev()
        .copied()
        .collect::<Vec<u64>>();
    frame.render_widget(
        Sparkline::default()
            .data(&visible)
            .style(Style::default().fg(Color::Cyan)),
        inner,
    );

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("  now {}", format_bitrate(state.bandwidth_bps())),
            Style::default().fg(Color::DarkGray),
        ))),
        footer,
    );
}

/// A ten-cell bar for a 0..1 health fraction.
///
/// No history, deliberately: this answers "is this room about to cut out right
/// now", and a trend line would need a row of its own per receiver to say the
/// same thing less directly.
fn health_bar(health: f32) -> (String, Color) {
    const CELLS: usize = 10;
    let filled = (health * CELLS as f32).round().clamp(0.0, CELLS as f32) as usize;
    let bar: String = "█".repeat(filled) + &"░".repeat(CELLS - filled);
    // Thresholds are about the shape of the failure: below a fifth of the
    // anchor lead there is very little left to absorb a hiccup.
    let colour = if health > 0.5 {
        Color::Green
    } else if health > 0.2 {
        Color::Yellow
    } else {
        Color::Red
    };
    (bar, colour)
}

fn render_receivers(frame: &mut Frame, area: Rect, state: &DashboardState) {
    let show_bar = area.width >= 72;
    let show_offset = area.width >= 60;

    let lines: Vec<Line> = if state.receivers.is_empty() {
        vec![Line::from(Span::styled(
            "  connecting…",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        let cursor = state.cursor();
        state
            .receivers
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let colour = match r.state {
                    ReceiverState::Connected => Color::Green,
                    ReceiverState::Connecting | ReceiverState::Reconnecting => Color::Yellow,
                    ReceiverState::Failed | ReceiverState::Dead => Color::Red,
                };
                let selected = cursor == Some(i);
                let row = if selected {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                };

                let mut spans = vec![
                    Span::styled(if selected { " ▸ " } else { "   " }, row),
                    Span::styled(format!("{:<22}", truncate(&r.name, 22)), row),
                    // Always shown, including ±0: a column that appears and
                    // disappears makes the row jump, and a zero is information
                    // — it says this receiver is at the group level.
                    Span::styled(format!("{:>+5.0} dB ", r.trim_db), row.fg(Color::Cyan)),
                ];
                if show_offset {
                    spans.push(Span::styled(
                        format!("{:>+5} ms ", r.offset_ms),
                        row.fg(Color::Magenta),
                    ));
                }
                if show_bar {
                    let (bar, bar_colour) = health_bar(r.health);
                    let style = if r.state == ReceiverState::Connected {
                        row.fg(bar_colour)
                    } else {
                        row.fg(Color::DarkGray)
                    };
                    spans.push(Span::styled(format!(" {bar} "), style));
                }
                spans.push(Span::styled(format!(" {}", r.state.label()), row.fg(colour)));
                if let Some(why) = &r.error {
                    spans.push(Span::styled(format!("  {why}"), row.fg(Color::Red)));
                }
                Line::from(spans)
            })
            .collect()
    };

    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(format!(
            " receivers ({})   [↑↓] select · [+/-] vol · [<>] offset · [a] add · [r] retry · [d] drop ",
            state.receivers.len()
        ))),
        area,
    );
}

/// Trim to `max` display columns, marking the cut with an ellipsis.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

fn render_logs(frame: &mut Frame, area: Rect, buffer: &LogBuffer, state: &DashboardState) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" logs   [PgUp/PgDn] scroll · [b] graph · [q] stop ");
    let rows = block.inner(area).height as usize;

    // `log_scroll` counts lines back from the newest.
    let tail = buffer.tail(rows + state.log_scroll);
    let end = tail.len().saturating_sub(state.log_scroll);
    let start = end.saturating_sub(rows);

    let lines: Vec<Line> = tail[start..end]
        .iter()
        .map(|l| {
            let colour = match l.level {
                tracing::Level::ERROR => Color::Red,
                tracing::Level::WARN => Color::Yellow,
                tracing::Level::INFO => Color::Reset,
                _ => Color::DarkGray,
            };
            Line::from(vec![
                Span::styled(format!(" {} ", l.ts), Style::default().fg(Color::DarkGray)),
                Span::styled(format!("{:<5} ", l.level.as_str()), Style::default().fg(colour)),
                Span::raw(l.msg.clone()),
            ])
        })
        .collect();

    frame.render_widget(Paragraph::new(lines).block(block), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn draw(width: u16, height: u16, state: &DashboardState) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let buffer = LogBuffer::new(10);
        terminal
            .draw(|frame| render(frame, state, &buffer))
            .unwrap();
        terminal
    }

    #[test]
    fn renders_a_frame_without_panicking() {
        // A layout panic would take the whole stream down, so this is the one
        // rendering test worth having. Deliberately not a golden-frame
        // snapshot: those break on every cosmetic tweak and teach nothing.
        let mut state = DashboardState::new(500);
        let stats = StreamStats::new(500);
        stats.record_lead_ms(320);
        stats.add_bytes(4096);
        stats.set_receivers(vec![openair_client::ReceiverStat {
            name: "Pool Room".into(),
            addr: "192.168.1.51:7000".parse().unwrap(),
            state: ReceiverState::Connected,
            offset_ms: 80,
            trim_db: 0.0,
            lead_ms: None,
            health: 0.0,
            error: None,
        }]);
        state.sample(&stats, Instant::now());

        let terminal = draw(100, 32, &state);
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("Pool Room"));
        assert!(rendered.contains("500 ms"));
    }

    #[test]
    fn renders_the_too_small_message_instead_of_a_jumble() {
        let state = DashboardState::new(500);
        let terminal = draw(40, 10, &state);
        assert!(terminal.backend().to_string().contains("too small"));
    }

    #[test]
    fn renders_at_exactly_the_minimum_size() {
        // The boundary the layout constraints have to survive.
        let state = DashboardState::new(500);
        let terminal = draw(MIN_WIDTH, MIN_HEIGHT, &state);
        assert!(!terminal.backend().to_string().contains("too small"));
    }

    #[test]
    fn renders_trim_and_offset_for_a_receiver_that_has_them() {
        let mut state = DashboardState::new(500);
        let stats = StreamStats::new(500);
        stats.set_receivers(vec![openair_client::ReceiverStat {
            name: "Pool Room".into(),
            addr: "192.168.1.51:7000".parse().unwrap(),
            state: ReceiverState::Connected,
            offset_ms: 80,
            trim_db: -6.0,
            lead_ms: None,
            health: 0.0,
            error: None,
        }]);
        state.sample(&stats, Instant::now());

        let rendered = draw(120, 32, &state).backend().to_string();
        assert!(rendered.contains("-6"), "trim shown: {rendered}");
        assert!(rendered.contains("+80 ms"), "offset shown");
    }

    #[test]
    fn the_add_overlay_renders_over_the_dashboard() {
        let state = PickerState::new(Settings::default(), Vec::new(), false);
        let mut terminal = Terminal::new(TestBackend::new(100, 32)).unwrap();
        terminal
            .draw(|frame| render_add_overlay(frame, &state))
            .unwrap();
        assert!(terminal.backend().to_string().contains("add a receiver"));
    }

    #[test]
    fn centred_never_exceeds_its_container() {
        // A popup wider than the terminal would panic ratatui on render.
        let small = Rect {
            x: 0,
            y: 0,
            width: 20,
            height: 6,
        };
        let r = centred(small, 70, 14);
        assert!(r.width <= small.width && r.height <= small.height);
        assert!(r.x + r.width <= small.x + small.width);
        assert!(r.y + r.height <= small.y + small.height);
    }

    #[test]
    fn the_graph_shows_bandwidth() {
        let state = DashboardState::new(500);
        let terminal = draw(100, 32, &state);
        assert!(terminal.backend().to_string().contains("bandwidth"));
    }

    #[test]
    fn renders_zero_trim_and_offset_rather_than_blank_columns() {
        // A column that appears only when non-zero makes the row jump as
        // values change, and a zero says "this receiver is at the group
        // level", which is worth stating.
        let mut state = DashboardState::new(500);
        let stats = StreamStats::new(500);
        stats.set_receivers(vec![openair_client::ReceiverStat {
            name: "Pool Room".into(),
            addr: "192.168.1.51:7000".parse().unwrap(),
            state: ReceiverState::Connected,
            offset_ms: 0,
            trim_db: 0.0,
            lead_ms: Some(500),
            health: 1.0,
            error: None,
        }]);
        state.sample(&stats, Instant::now());

        let out = draw(120, 32, &state).backend().to_string();
        assert!(out.contains("+0 dB"), "{out}");
        assert!(out.contains("+0 ms"), "{out}");
    }

    #[test]
    fn a_healthy_receiver_gets_a_full_bar_and_a_starved_one_an_empty_bar() {
        let (full, _) = health_bar(1.0);
        let (empty, _) = health_bar(0.0);
        assert_eq!(full.matches('█').count(), 10);
        assert_eq!(empty.matches('░').count(), 10);
    }

    #[test]
    fn health_bar_colour_warns_before_it_empties() {
        // Red must arrive with headroom left, not at the moment audio cuts.
        assert_eq!(health_bar(1.0).1, Color::Green);
        assert_eq!(health_bar(0.3).1, Color::Yellow);
        assert_eq!(health_bar(0.1).1, Color::Red);
    }

    #[test]
    fn health_bar_never_overflows_its_width() {
        for h in [-1.0, 0.0, 0.5, 1.0, 2.0, f32::NAN] {
            let (bar, _) = health_bar(h);
            assert_eq!(bar.chars().count(), 10, "health {h} produced {bar:?}");
        }
    }

    #[test]
    fn summary_reads_as_one_line() {
        let s = Summary {
            elapsed: Duration::from_secs(125),
            receivers: 2,
            latency_ms: 750,
            worst_lead_ms: Some(180),
            bytes_sent: 3 * 1024 * 1024,
        };
        let text = s.to_string();
        assert!(text.contains("2:05"));
        assert!(text.contains("2 receiver(s)"));
        assert!(text.contains("750 ms"));
        assert!(text.contains("lowest buffer 180 ms"));
    }
}
