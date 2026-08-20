//! Drawing and driving the picker. The decisions all live in
//! [`crate::picker`]; this is the terminal half.

use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use crate::picker::{PickerRow, PickerState};

pub fn render(frame: &mut Frame, state: &PickerState) {
    let [list_area, status_area, keys_area] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    render_device_list(frame, list_area, state, " OpenAir ");

    let status = Paragraph::new(status_line(state)).style(Style::default().fg(Color::DarkGray));
    frame.render_widget(status, status_area);

    // A banner explains why the user is back here and outranks the keybind
    // line; a hint answers the key they just pressed and outranks both.
    let keys = match (state.hint(), state.banner()) {
        (Some(hint), _) => Paragraph::new(Line::from(Span::styled(
            format!("  {hint}"),
            Style::default().fg(Color::Yellow),
        ))),
        (None, Some(banner)) => Paragraph::new(Line::from(Span::styled(
            format!("  {banner}"),
            Style::default().fg(Color::Red),
        ))),
        (None, None) => Paragraph::new(Line::from(Span::styled(
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

/// One device row.
///
/// Deliberately carries no pairing marker. Pairing now happens after the user
/// confirms, so a mark that predicts it says nothing actionable — and nobody
/// reads a tick as "credentials on disk". `paired` still orders the list, which
/// is the useful half: your usual speakers stay at the top.
fn row_item(row: &PickerRow) -> ListItem<'static> {
    let mark = if row.selected { "[x]" } else { "[ ]" };
    let spans = vec![
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
    use crate::settings::Settings;
    use openair_discovery::{AirPlayDevice, AirPlayTxt};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::collections::HashMap;

    /// Bit 48 set: Transient pairing, so nothing is needed before streaming.
    const TRANSIENT: &str = "0x200,0x10000";
    /// Bit 9 only: audio, but Normal pairing required.
    const NEEDS_PAIRING: &str = "0x200,0x0";

    fn device(name: &str, addr: &str, id: &str, features: &str) -> AirPlayDevice {
        let mut raw: HashMap<String, String> = HashMap::new();
        raw.insert("features".into(), features.into());
        raw.insert("deviceid".into(), id.into());
        raw.insert("model".into(), "AppleTV6,2".into());
        AirPlayDevice::new(
            format!("{name}._airplay._tcp.local."),
            addr.parse().unwrap(),
            7000,
            AirPlayTxt::parse(&raw),
        )
    }

    fn draw(width: u16, height: u16, state: &PickerState) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(frame, state)).unwrap();
        terminal
    }

    #[test]
    fn no_pairing_marker_is_drawn() {
        // Both halves of the old marker: a paired device drew a ✓ and an
        // unpaired one drew "! needs pairing". Neither survives, and the flag
        // that drove them still exists for sorting and for the pairing screen.
        let mut state = PickerState::new(Settings::default(), vec!["2".into()], true);
        state.insert(device("Living Room", "192.168.1.106", "1", NEEDS_PAIRING));
        state.insert(device("Pool Room", "192.168.1.51", "2", TRANSIENT));

        let screen = draw(100, 20, &state).backend().to_string();
        assert!(!screen.contains('✓'), "no tick:\n{screen}");
        assert!(!screen.contains("needs pairing"), "no warning:\n{screen}");
        assert!(screen.contains("Living Room") && screen.contains("Pool Room"));
        assert!(
            state.rows().iter().any(|r| r.needs_pairing),
            "the flag itself must survive — the pairing screen reads it"
        );
    }

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
