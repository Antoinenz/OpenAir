//! Drawing the stored-pairings list. Decisions live in
//! [`crate::pairing_list`]; this is the terminal half.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::pairing_list::PairingList;

/// Width given to the name before the device id starts.
const NAME_WIDTH: usize = 22;

/// Render into an area the caller has already cleared and framed.
///
/// Takes an area rather than the whole frame because it is drawn *inside* the
/// settings panel: same border, same position, so opening it reads as going
/// deeper rather than as a second window appearing somewhere else.
pub fn render(frame: &mut Frame, area: Rect, list: &PairingList) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    let [rows_area, status_area, footer_area] = if area.height >= 3 {
        Layout::vertical([
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(area)
    } else {
        [area, area, area]
    };

    if list.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  nothing paired yet",
                Style::default().fg(Color::DarkGray),
            ))),
            rows_area,
        );
    } else {
        let lines: Vec<Line> = list
            .peers()
            .iter()
            .enumerate()
            .map(|(i, peer)| {
                let armed = list.armed() == Some(peer.device_id.as_str());
                row_line(peer.label(), &peer.device_id, i == list.cursor(), armed)
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), rows_area);
    }

    if area.height >= 3 {
        if let Some(status) = list.status() {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!("  {status}"),
                    Style::default().fg(Color::Yellow),
                ))),
                status_area,
            );
        }
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  ↑↓ move · d forget · esc back",
                Style::default().fg(Color::DarkGray),
            ))),
            footer_area,
        );
    }
}

fn row_line(label: &str, device_id: &str, selected: bool, armed: bool) -> Line<'static> {
    let marker_colour = if armed {
        Color::Yellow
    } else if selected {
        Color::Cyan
    } else {
        Color::DarkGray
    };

    let mut name_style = Style::default();
    if selected {
        name_style = name_style.add_modifier(Modifier::BOLD);
    }
    if armed {
        name_style = name_style.fg(Color::Yellow);
    }

    Line::from(vec![
        Span::styled(
            if selected { " ▸ " } else { "   " },
            Style::default().fg(marker_colour),
        ),
        Span::styled(truncate(label, NAME_WIDTH), name_style),
        // The device id is what makes two speakers called "HomePod"
        // distinguishable, so it earns its place -- but dimmed, because it is
        // not what anyone is looking for.
        Span::styled(
            device_id.to_string(),
            Style::default().fg(Color::DarkGray),
        ),
    ])
}

/// Pad to `width`, or cut with an ellipsis if it will not fit.
fn truncate(s: &str, width: usize) -> String {
    let len = s.chars().count();
    if len <= width {
        return format!("{s:<width$}");
    }
    let kept: String = s.chars().take(width.saturating_sub(1)).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use openair_client::PairedPeer;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn peer(id: &str, name: Option<&str>) -> PairedPeer {
        PairedPeer {
            device_id: id.to_string(),
            name: name.map(str::to_string),
        }
    }

    fn draw(width: u16, height: u16, list: &PairingList) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), list))
            .unwrap();
        terminal.backend().to_string()
    }

    fn list() -> PairingList {
        PairingList::new(vec![
            peer("AA:AA", Some("Living Room")),
            peer("BB:BB", None),
        ])
    }

    #[test]
    fn names_and_device_ids_are_both_drawn() {
        let screen = draw(70, 10, &list());
        assert!(screen.contains("Living Room"), "{screen}");
        // Two speakers with the same name are told apart by this alone.
        assert!(screen.contains("AA:AA"), "{screen}");
    }

    #[test]
    fn a_pairing_with_no_name_still_shows_something() {
        let screen = draw(70, 10, &list());
        assert!(screen.contains("BB:BB"), "{screen}");
    }

    #[test]
    fn an_empty_list_says_so() {
        // A blank panel looks like a bug. This looks like an answer.
        let screen = draw(70, 10, &PairingList::new(Vec::new()));
        assert!(screen.contains("nothing paired"), "{screen}");
    }

    #[test]
    fn the_confirmation_prompt_is_shown() {
        let mut l = list();
        l.on_key(crossterm::event::KeyCode::Char('d'));
        let screen = draw(70, 10, &l);
        assert!(screen.contains("press d again"), "{screen}");
    }

    #[test]
    fn the_keybinds_are_shown() {
        let screen = draw(70, 10, &list());
        assert!(screen.contains("d forget"), "{screen}");
        assert!(screen.contains("esc back"), "{screen}");
    }

    #[test]
    fn a_long_name_is_cut_rather_than_pushing_the_device_id_off() {
        let long = PairingList::new(vec![peer(
            "AA:AA",
            Some("Antoine's Extremely Long Speaker Name In The Hall"),
        )]);
        let screen = draw(70, 10, &long);
        assert!(screen.contains('…'), "{screen}");
        assert!(screen.contains("AA:AA"), "the id survived:\n{screen}");
    }

    #[test]
    fn rendering_survives_a_sweep_of_terminal_sizes() {
        // This is drawn over a live dashboard; a panic here takes the stream
        // down over a keystroke.
        let mut l = list();
        l.on_key(crossterm::event::KeyCode::Char('d'));
        for width in [1u16, 5, 20, 40, 70, 200] {
            for height in [1u16, 2, 3, 5, 20] {
                draw(width, height, &l);
            }
        }
        draw(1, 1, &PairingList::new(Vec::new()));
    }
}
