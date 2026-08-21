//! Drawing and driving the picker. The decisions all live in
//! [`crate::picker`]; this is the terminal half.

use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use crate::picker::{PickerRow, PickerState};

pub fn render(frame: &mut Frame, state: &PickerState) {
    let [list_area, footer_area] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(frame.area());

    render_device_list(frame, list_area, state, " OpenAir ");
    render_ready_button(frame, list_area, state);
    frame.render_widget(Paragraph::new(footer(state)), footer_area);
}

/// The single footer line.
///
/// A banner explains why the user is back here and outranks the ordinary
/// footer; a hint answers the key they just pressed and outranks both. Only one
/// of the three is ever on screen, which is why they share a row rather than
/// stacking.
fn footer(state: &PickerState) -> Line<'static> {
    if let Some(hint) = state.hint() {
        return Line::from(Span::styled(
            format!("  {hint}"),
            Style::default().fg(Color::Yellow),
        ));
    }
    if let Some(banner) = state.banner() {
        return Line::from(Span::styled(
            format!("  {banner}"),
            Style::default().fg(Color::Red),
        ));
    }

    let grey = Style::default().fg(Color::DarkGray);
    Line::from(vec![
        // State reads as values, not instructions: what handoff and latency
        // *are* is the question being answered here, and the keys that change
        // them are named on the right.
        Span::styled(format!("  {}", status_text(state)), grey),
        Span::styled("   ", grey),
        Span::styled(controls_text(state.settings.show_controls), grey),
    ])
}

/// The keys worth naming.
///
/// With `show_controls` off this is only what is non-obvious or easy to forget.
/// Arrow keys to move are neither — anyone will try them — and quitting answers
/// to `q`, `Esc` and `Ctrl+C`, so a line advertising one of the three earns
/// little.
///
/// `⏎ start` is omitted from **both** forms: the ready button already carries
/// that glyph where the eye is, and with `s settings` added the full line
/// overflowed 120 columns and silently lost `q quit` off the end. A keybind
/// line long enough to truncate is worse than a shorter one.
fn controls_text(show_all: bool) -> &'static str {
    if show_all {
        "↑↓ move · space select · h handoff · <> latency · s settings · q quit"
    } else {
        "space select · h handoff · <> latency · s settings"
    }
}

/// Width and height of the ready button, borders included.
const BUTTON: (u16, u16) = (12, 3);
/// Columns between the button and the list's right border, so the frame's
/// corner stays visible and the button reads as sitting *on* the frame.
const BUTTON_PAD: u16 = 3;
/// Below this width the button would cover the address column, so it is
/// dropped. Enter still starts the stream; only the affordance goes.
const BUTTON_MIN_WIDTH: u16 = 48;

/// The bottom-right ready button.
///
/// Green once a receiver is chosen, so "am I able to start?" is answerable at a
/// glance rather than by counting `[x]` marks. The `⏎` shows how to press it
/// without spending a footer line on a sentence.
///
/// Pressing Enter with nothing selected turns it yellow alongside the footer
/// hint. That colour rides the hint's lifetime — it clears on the next
/// keystroke — rather than a timer, because a timed flash on a screen that only
/// redraws on input or discovery would sometimes never be un-drawn.
fn render_ready_button(frame: &mut Frame, list_area: ratatui::layout::Rect, state: &PickerState) {
    if list_area.width < BUTTON_MIN_WIDTH || list_area.height < BUTTON.1 {
        return;
    }

    let ready = !state.chosen().is_empty();
    let colour = match (ready, state.hint().is_some()) {
        (true, _) => Color::Green,
        (false, true) => Color::Yellow,
        (false, false) => Color::DarkGray,
    };

    let area = crate::rect::bottom_right(list_area, BUTTON.0, BUTTON.1, BUTTON_PAD);
    // The list's border and rows run underneath; without clearing, they show
    // through the button's interior.
    frame.render_widget(ratatui::widgets::Clear, area);

    let style = Style::default().fg(colour);
    let label = Paragraph::new(Line::from(Span::styled(
        "  ⏎ READY ",
        if ready {
            style.add_modifier(Modifier::BOLD)
        } else {
            style
        },
    )))
    .block(Block::default().borders(Borders::ALL).style(style));
    frame.render_widget(label, area);
}

/// Draw the device list. Shared with the dashboard's add-a-receiver overlay so
/// both screens agree on how a device is presented.
pub fn render_device_list(
    frame: &mut Frame,
    area: ratatui::layout::Rect,
    state: &PickerState,
    title: &str,
) {
    // `total()`, not `rows().len()`: on a large network the list is windowed
    // for drawing, and a count that reported the window would say "50 found"
    // forever while devices kept arriving.
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!("{title}({} found) ", state.total()));

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

/// Current settings, phrased as values rather than as things to do.
fn status_text(state: &PickerState) -> String {
    let handoff = if !state.handoff_available() {
        "handoff unavailable"
    } else if state.settings.handoff {
        "handoff on"
    } else {
        "handoff off"
    };
    format!(
        "{} · {} ms · {:.0} dB · {} selected",
        handoff,
        state.settings.latency_ms,
        state.settings.volume_db,
        state.chosen().len()
    )
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
    fn the_ready_button_is_drawn_in_both_states() {
        let mut state = PickerState::new(Settings::default(), Vec::new(), true);
        state.insert(device("Pool Room", "192.168.1.51", "1", TRANSIENT));

        let idle = draw(100, 20, &state).backend().to_string();
        assert!(idle.contains("READY"), "drawn before anything is selected");

        state.on_key(crossterm::event::KeyCode::Char(' '));
        let armed = draw(100, 20, &state).backend().to_string();
        assert!(armed.contains("READY"));
    }

    #[test]
    fn the_ready_button_turns_green_only_when_a_receiver_is_chosen() {
        // The colour is the whole point of the button, so assert on the cell
        // style rather than on the text being present.
        let mut state = PickerState::new(Settings::default(), Vec::new(), true);
        state.insert(device("Pool Room", "192.168.1.51", "1", TRANSIENT));

        assert_eq!(button_colour(&state), Some(Color::DarkGray));
        state.on_key(crossterm::event::KeyCode::Char(' '));
        assert_eq!(button_colour(&state), Some(Color::Green));
    }

    #[test]
    fn the_ready_button_flags_a_refused_enter() {
        let mut state = PickerState::new(Settings::default(), Vec::new(), true);
        state.insert(device("Pool Room", "192.168.1.51", "1", TRANSIENT));
        state.on_key(crossterm::event::KeyCode::Enter); // nothing selected
        assert_eq!(button_colour(&state), Some(Color::Yellow));

        // And it goes quiet again with the hint, rather than staying lit.
        state.on_key(crossterm::event::KeyCode::Down);
        assert_eq!(button_colour(&state), Some(Color::DarkGray));
    }

    /// Colour of the button's top-left border cell, located the same way the
    /// renderer places it.
    fn button_colour(state: &PickerState) -> Option<Color> {
        let (w, h) = (100u16, 20u16);
        let terminal = draw(w, h, state);
        let list_area = ratatui::layout::Rect {
            x: 0,
            y: 0,
            width: w,
            height: h - 1, // the footer row
        };
        let r = crate::rect::bottom_right(list_area, BUTTON.0, BUTTON.1, BUTTON_PAD);
        Some(terminal.backend().buffer()[(r.x, r.y)].fg)
    }

    #[test]
    fn a_narrow_picker_drops_the_button_rather_than_covering_the_list() {
        let mut state = PickerState::new(Settings::default(), Vec::new(), true);
        state.insert(device("Pool Room", "192.168.1.51", "1", TRANSIENT));
        state.on_key(crossterm::event::KeyCode::Char(' '));

        let narrow = draw(40, 12, &state).backend().to_string();
        assert!(!narrow.contains("READY"), "dropped when it would not fit");
        assert!(narrow.contains("Pool Room"), "the list still renders");
    }

    #[test]
    fn rendering_survives_a_sweep_of_terminal_sizes() {
        // The button overlaps the list border, so it is exactly the kind of
        // widget that panics on a small terminal. Cheap insurance.
        let mut state = PickerState::new(Settings::default(), Vec::new(), true);
        state.insert(device("Pool Room", "192.168.1.51", "1", TRANSIENT));
        state.on_key(crossterm::event::KeyCode::Char(' '));
        for width in [20u16, 40, 47, 48, 60, 80, 200] {
            for height in [3u16, 5, 8, 20, 50] {
                draw(width, height, &state);
            }
        }
    }

    #[test]
    fn the_discreet_footer_omits_the_guessable_keys() {
        let mut state = PickerState::new(Settings::default(), Vec::new(), true);
        state.insert(device("Pool Room", "192.168.1.51", "1", TRANSIENT));
        assert!(!state.settings.show_controls, "off by default");

        let screen = draw(120, 20, &state).backend().to_string();
        assert!(screen.contains("space select"), "the non-obvious ones stay");
        assert!(screen.contains("h handoff"));
        assert!(screen.contains("<> latency"));
        assert!(
            !screen.contains("move"),
            "arrows are guessable:
{screen}"
        );
        assert!(!screen.contains("quit"), "q, esc and ctrl+c all work");
    }

    #[test]
    fn show_controls_brings_the_full_list_back() {
        let mut state = PickerState::new(
            Settings {
                show_controls: true,
                ..Settings::default()
            },
            Vec::new(),
            true,
        );
        state.insert(device("Pool Room", "192.168.1.51", "1", TRANSIENT));

        let screen = draw(120, 20, &state).backend().to_string();
        assert!(screen.contains("move") && screen.contains("quit"));
        assert!(screen.contains("space select"), "and still the rest");
    }

    #[test]
    fn state_reads_as_values_not_instructions() {
        // "500 ms" answers what the latency is; "<> latency" says how to
        // change it. Keeping those apart is the point of the merged line.
        let mut state = PickerState::new(Settings::default(), Vec::new(), true);
        state.insert(device("Pool Room", "192.168.1.51", "1", TRANSIENT));
        state.on_key(crossterm::event::KeyCode::Char(' '));

        let screen = draw(120, 20, &state).backend().to_string();
        for expected in ["handoff on", "500 ms", "-8 dB", "1 selected"] {
            assert!(
                screen.contains(expected),
                "missing {expected}:
{screen}"
            );
        }
    }

    #[test]
    fn the_footer_is_one_row_and_a_hint_takes_it_over() {
        // Two stacked rows became one, so a hint now displaces the status
        // line rather than sitting under it.
        let mut state = PickerState::new(Settings::default(), Vec::new(), true);
        state.insert(device("Pool Room", "192.168.1.51", "1", TRANSIENT));
        state.on_key(crossterm::event::KeyCode::Enter); // nothing selected

        let screen = draw(120, 20, &state).backend().to_string();
        assert!(screen.contains("select a receiver with space first"));
        assert!(!screen.contains("h handoff"), "the hint owns the row");
    }

    #[test]
    fn a_banner_outranks_the_status_line_but_not_a_hint() {
        let mut state = PickerState::new(Settings::default(), Vec::new(), true);
        state.insert(device("Pool Room", "192.168.1.51", "1", TRANSIENT));
        state.set_banner("nothing connected");
        assert!(draw(120, 20, &state)
            .backend()
            .to_string()
            .contains("nothing connected"));
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
