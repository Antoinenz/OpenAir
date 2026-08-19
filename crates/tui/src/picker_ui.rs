//! Drawing and driving the picker. The decisions all live in
//! [`crate::picker`]; this is the terminal half.

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use crate::picker::{PickerAction, PickerRow, PickerState};
use crate::settings::Settings;
use crate::term;

/// How long to wait for a key before redrawing. Also the rate at which newly
/// discovered devices appear.
const TICK: Duration = Duration::from_millis(100);

pub struct PickerOutcome {
    pub receivers: Vec<PickerRow>,
    pub settings: Settings,
}

/// Run the interactive picker. `Ok(None)` means the user quit without
/// choosing.
///
/// Discovery starts before the first frame and runs for as long as the picker
/// is open, so the list keeps filling while the user reads it. Nothing is
/// contacted: no pairing, no `GET /info`, until this returns.
pub fn run_picker(
    settings: Settings,
    paired: Vec<String>,
    handoff_available: bool,
) -> io::Result<Option<PickerOutcome>> {
    let mut state = PickerState::new(settings, paired, handoff_available);
    let browse = openair_discovery::browse_live()
        .map_err(|e| io::Error::other(format!("mDNS discovery failed: {e}")))?;

    // Full screen, like the dashboard it leads into — the two screens are one
    // flow, and having the first be an inline prompt that then jumps to an
    // alternate screen made the transition jarring.
    let (mut terminal, _guard) = term::enter_alt()?;

    let outcome = loop {
        while let Ok(device) = browse.devices.try_recv() {
            state.insert(device);
        }

        terminal.draw(|frame| render(frame, &state))?;

        if !event::poll(TICK)? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        // Windows reports both press and release; acting on both would toggle
        // every selection twice.
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            break None;
        }

        match state.on_key(key.code) {
            PickerAction::Quit => break None,
            PickerAction::Start => {
                break Some(PickerOutcome {
                    receivers: state.chosen().into_iter().cloned().collect(),
                    settings: state.settings.clone(),
                })
            }
            PickerAction::None | PickerAction::Hint(_) => {}
        }
    };

    // Only the picker's own toggles write the file, and only on a clean exit.
    if let Some(outcome) = &outcome {
        if let Err(e) = outcome.settings.save() {
            tracing::warn!("could not save settings: {e}");
        }
    }

    Ok(outcome)
}

fn render(frame: &mut Frame, state: &PickerState) {
    let [list_area, status_area, keys_area] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    render_device_list(frame, list_area, state, " OpenAir ");

    let status = Paragraph::new(status_line(state)).style(Style::default().fg(Color::DarkGray));
    frame.render_widget(status, status_area);

    let keys = match state.hint() {
        Some(hint) => Paragraph::new(Line::from(Span::styled(
            format!("  {hint}"),
            Style::default().fg(Color::Yellow),
        ))),
        None => Paragraph::new(Line::from(Span::styled(
            "  ↑↓ move · space select · ⏎ start · h handoff · <> latency · q quit",
            Style::default().fg(Color::DarkGray),
        ))),
    };
    frame.render_widget(keys, keys_area);
}

/// Draw the device list. Shared with the dashboard's add-a-receiver overlay so
/// both screens agree on how a device is presented.
pub fn render_device_list(
    frame: &mut Frame,
    area: ratatui::layout::Rect,
    state: &PickerState,
    title: &str,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!("{title}({} found) ", state.rows().len()));

    if state.rows().is_empty() {
        let msg = Paragraph::new(Line::from(Span::styled(
            "  searching for AirPlay receivers…",
            Style::default().fg(Color::DarkGray),
        )))
        .block(block);
        frame.render_widget(msg, area);
        return;
    }

    let items: Vec<ListItem> = state.rows().iter().map(row_item).collect();
    // `List` owns the scrolling: with more receivers than rows, the selected
    // item is kept in view automatically.
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    let mut list_state = ListState::default().with_selected(Some(state.cursor()));
    frame.render_stateful_widget(list, area, &mut list_state);
}

fn row_item(row: &PickerRow) -> ListItem<'static> {
    let mark = if row.selected { "[x]" } else { "[ ]" };
    let mut spans = vec![
        Span::styled(
            format!(" {mark} "),
            Style::default().fg(if row.selected {
                Color::Green
            } else {
                Color::DarkGray
            }),
        ),
        Span::raw(format!("{:<20}", truncate(&row.name, 20))),
        Span::styled(
            format!("{:<16}", truncate(&row.model, 16)),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(row.addr.to_string(), Style::default().fg(Color::DarkGray)),
    ];
    if row.needs_pairing {
        spans.push(Span::styled(
            "  ! needs pairing",
            Style::default().fg(Color::Yellow),
        ));
    } else if row.paired {
        spans.push(Span::styled("  ✓", Style::default().fg(Color::Green)));
    }
    ListItem::new(Line::from(spans))
}

fn status_line(state: &PickerState) -> Line<'static> {
    let handoff = if !state.handoff_available() {
        "handoff unavailable".to_string()
    } else if state.settings.handoff {
        "handoff ON".to_string()
    } else {
        "handoff off".to_string()
    };
    Line::from(format!(
        "  {}   latency {} ms   volume {:.0} dB   selected {}",
        handoff,
        state.settings.latency_ms,
        state.settings.volume_db,
        state.chosen().len()
    ))
}

/// Trim to `max` display columns, marking the cut with an ellipsis.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_leaves_short_strings_alone() {
        assert_eq!(truncate("Pool Room", 20), "Pool Room");
    }

    #[test]
    fn truncate_marks_the_cut() {
        assert_eq!(truncate("A very long receiver name", 10), "A very lo…");
    }

    #[test]
    fn truncate_counts_characters_not_bytes() {
        // Slicing by byte would panic mid-character here.
        let name = "Café Café Café";
        let out = truncate(name, 6);
        assert_eq!(out.chars().count(), 6);
    }
}
