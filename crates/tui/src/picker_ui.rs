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
