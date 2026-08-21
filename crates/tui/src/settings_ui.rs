//! Drawing the settings overlay. Decisions live in
//! [`crate::settings_screen`]; this is the terminal half.

use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::settings::Settings;
use crate::settings_screen::{SettingsRow, SettingsState};

/// Overlay size, borders included. Wide enough for the longest label, its
/// value, and a short reason on the same line.
const PANEL: (u16, u16) = (58, 9);

pub fn render(frame: &mut Frame, state: &SettingsState) {
    let area = crate::rect::centred(frame.area(), PANEL.0, PANEL.1);
    // Drawn over a live frame — without this the dashboard shows through the
    // gaps between glyphs.
    frame.render_widget(Clear, area);

    let block = Block::default().borders(Borders::ALL).title(if state.streaming() {
        " settings — changes are live "
    } else {
        " settings "
    });
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // A terminal small enough to leave no interior is not an error worth
    // reporting — the border alone still says a panel is open.
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let [rows_area, footer_area] = if inner.height > 1 {
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(inner)
    } else {
        [inner, inner]
    };

    let lines: Vec<Line> = state
        .rows()
        .iter()
        .enumerate()
        .map(|(i, row)| row_line(*row, state, i == state.cursor()))
        .collect();
    frame.render_widget(Paragraph::new(lines), rows_area);

    if inner.height > 1 {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  ↑↓ move · ←→ adjust · space toggle · esc close",
                Style::default().fg(Color::DarkGray),
            ))),
            footer_area,
        );
    }
}

fn row_line(row: SettingsRow, state: &SettingsState, selected: bool) -> Line<'static> {
    let s: &Settings = &state.settings;
    let (label, value) = match row {
        SettingsRow::Handoff => ("handoff", on_off(s.handoff)),
        SettingsRow::Latency => ("latency", format!("{} ms", s.latency_ms)),
        SettingsRow::Volume => ("volume", format!("{:.0} dB", s.volume_db)),
        SettingsRow::Metadata => ("metadata", on_off(s.metadata)),
        SettingsRow::ShowControls => ("controls", on_off(s.show_controls)),
    };

    let mut spans = vec![
        Span::styled(
            if selected { " ▸ " } else { "   " },
            Style::default().fg(if selected {
                Color::Cyan
            } else {
                Color::DarkGray
            }),
        ),
        Span::styled(
            format!("{label:<10}"),
            if selected {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            },
        ),
        Span::raw(format!("{value:<8}")),
    ];

    // The reason goes on the row that caused it, not in a shared status line:
    // with five rows on screen, "which one failed" is the first question.
    if state.error_row() == Some(row) {
        if let Some(msg) = state.error() {
            spans.push(Span::styled(
                format!(" {msg}"),
                Style::default().fg(Color::Yellow),
            ));
        }
    }
    Line::from(spans)
}

fn on_off(v: bool) -> String {
    if v { "on" } else { "off" }.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings_screen::SettingsState;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn draw(width: u16, height: u16, state: &SettingsState) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(frame, state)).unwrap();
        terminal
    }

    #[test]
    fn every_row_and_its_value_is_drawn() {
        let state = SettingsState::new(Settings::default(), true, false);
        let screen = draw(100, 30, &state).backend().to_string();
        for expected in ["handoff", "latency", "volume", "metadata", "controls"] {
            assert!(screen.contains(expected), "missing {expected}:\n{screen}");
        }
        assert!(screen.contains("500 ms"), "the latency value:\n{screen}");
        assert!(screen.contains("-8 dB"), "the volume value:\n{screen}");
    }

    #[test]
    fn an_error_is_shown_against_its_row() {
        let mut state = SettingsState::new(Settings::default(), false, false);
        state.set_error(SettingsRow::Handoff, "no cable");
        let screen = draw(100, 30, &state).backend().to_string();
        assert!(screen.contains("no cable"), "{screen}");
    }

    #[test]
    fn the_streaming_overlay_says_changes_are_live() {
        let live = SettingsState::new(Settings::default(), true, true);
        assert!(draw(100, 30, &live).backend().to_string().contains("live"));

        let idle = SettingsState::new(Settings::default(), true, false);
        assert!(
            !draw(100, 30, &idle).backend().to_string().contains("live"),
            "before a stream there is nothing live to promise"
        );
    }

    #[test]
    fn rendering_survives_a_sweep_of_terminal_sizes() {
        // An overlay larger than its terminal panics ratatui on render, which
        // on the dashboard means taking a live stream down over a keystroke.
        let state = SettingsState::new(Settings::default(), true, true);
        for width in [1u16, 5, 10, 20, 40, 57, 58, 60, 200] {
            for height in [1u16, 2, 3, 5, 10, 30, 60] {
                draw(width, height, &state);
            }
        }
    }
}
