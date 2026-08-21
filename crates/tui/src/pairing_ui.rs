//! Drawing the pairing screen. Decisions live in [`crate::pairing`].

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::pairing::{PairPhase, PairingState, MAX_ATTEMPTS, PIN_LEN};

pub fn render(frame: &mut Frame, state: &PairingState) {
    let area = centred_area(frame.area());
    let Some(current) = state.current() else {
        return;
    };

    let [header, pin_area, status, footer] = Layout::vertical([
        Constraint::Length(4),
        Constraint::Length(3),
        Constraint::Length(2),
        Constraint::Length(1),
    ])
    .areas(area);

    let queued = state.remaining();
    let mut header_lines = vec![
        Line::from(Span::styled(
            format!("  pairing with {}", current.name),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "  enter the PIN shown on the device",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    if queued > 0 {
        header_lines.push(Line::from(Span::styled(
            format!("  {queued} more after this one"),
            Style::default().fg(Color::DarkGray),
        )));
    }
    frame.render_widget(
        Paragraph::new(header_lines)
            .block(Block::default().borders(Borders::ALL).title(" OpenAir ")),
        header,
    );

    // Fixed-width boxes rather than a growing string, so the field does not
    // shift under the cursor as digits are typed.
    let filled = state.pin().chars().count();
    let cells: String = (0..PIN_LEN)
        .map(|i| if i < filled { '●' } else { '○' })
        .map(|c| format!(" {c} "))
        .collect();
    let pin_style = if state.phase() == PairPhase::Verifying {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(format!("   {cells}"), pin_style))),
        pin_area,
    );

    let status_line = match (state.phase(), state.error()) {
        (PairPhase::Verifying, _) => Line::from(Span::styled(
            "  checking…",
            Style::default().fg(Color::Yellow),
        )),
        (_, Some(err)) => Line::from(Span::styled(
            format!("  {err}"),
            Style::default().fg(Color::Red),
        )),
        _ => Line::from(""),
    };
    let attempts = if state.attempts_left() < MAX_ATTEMPTS {
        Line::from(Span::styled(
            format!("  {} attempt(s) left", state.attempts_left()),
            Style::default().fg(Color::DarkGray),
        ))
    } else {
        Line::from("")
    };
    frame.render_widget(Paragraph::new(vec![status_line, attempts]), status);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "  digits to enter · backspace to correct · esc to skip this device",
            Style::default().fg(Color::DarkGray),
        ))),
        footer,
    );
}

fn centred_area(area: Rect) -> Rect {
    let width = area.width.min(64);
    let height = area.height.min(11);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pairing::PendingPair;
    use crossterm::event::KeyCode;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn state() -> PairingState {
        PairingState::new(vec![PendingPair {
            name: "Living Room".into(),
            addr: "192.168.1.51:7000".parse().unwrap(),
            device_id: "AA:BB".into(),
        }])
    }

    fn draw(width: u16, height: u16, state: &PairingState) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(frame, state)).unwrap();
        terminal.backend().to_string()
    }

    #[test]
    fn names_the_device_being_paired() {
        let out = draw(100, 30, &state());
        assert!(out.contains("Living Room"));
        assert!(out.contains("PIN"));
    }

    #[test]
    fn shows_one_cell_per_digit() {
        let mut s = state();
        s.on_key(KeyCode::Char('1'));
        s.on_key(KeyCode::Char('2'));
        let out = draw(100, 30, &s);
        assert_eq!(out.matches('●').count(), 2);
        assert_eq!(out.matches('○').count(), PIN_LEN - 2);
    }

    #[test]
    fn shows_the_rejection_and_the_attempts_left() {
        let mut s = state();
        for c in ['1', '2', '3', '4'] {
            s.on_key(KeyCode::Char(c));
        }
        s.on_result(Err("incorrect PIN".into()));
        let out = draw(100, 30, &s);
        assert!(out.contains("new PIN"), "{out}");
        assert!(out.contains("attempt(s) left"));
    }

    #[test]
    fn an_empty_queue_renders_nothing_rather_than_panicking() {
        let s = PairingState::new(Vec::new());
        draw(100, 30, &s);
    }

    #[test]
    fn survives_a_tiny_terminal() {
        draw(20, 6, &state());
    }
}
