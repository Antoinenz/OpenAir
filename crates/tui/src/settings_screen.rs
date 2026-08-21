//! The settings overlay's state and key handling — no rendering, no platform
//! calls.
//!
//! Split from [`crate::settings_ui`] the same way [`crate::picker`] is split
//! from [`crate::picker_ui`]: decisions here, drawing there.
//!
//! What makes this testable is that *applying* a change is somebody else's job.
//! This module reports that a change was made and is told afterwards whether it
//! stuck — so "the handoff toggle failed and the row explains why" is a unit
//! test rather than an unplugging ritual.

use crossterm::event::KeyCode;

use crate::settings::{Settings, LATENCY_MAX_MS, LATENCY_MIN_MS, LATENCY_STEP_MS};

/// Volume adjustment bounds and step, in dB. Matches the range `Settings`
/// clamps to when loading a hand-edited file.
const VOLUME_MIN_DB: f32 = -60.0;
const VOLUME_MAX_DB: f32 = 0.0;
const VOLUME_STEP_DB: f32 = 1.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsRow {
    Handoff,
    Latency,
    Volume,
    Metadata,
    ShowControls,
}

const ROWS: [SettingsRow; 5] = [
    SettingsRow::Handoff,
    SettingsRow::Latency,
    SettingsRow::Volume,
    SettingsRow::Metadata,
    SettingsRow::ShowControls,
];

#[derive(Debug, Clone, PartialEq)]
pub enum SettingsAction {
    /// Redraw; nothing else.
    None,
    /// Close the overlay and return to the screen underneath.
    Close,
    /// The settings changed; the caller should apply and persist them.
    Apply(Settings),
}

pub struct SettingsState {
    pub settings: Settings,
    cursor: usize,
    /// Whether a virtual audio cable was detected. When false the handoff row
    /// cannot be switched on, exactly as in the picker.
    handoff_available: bool,
    /// Whether a stream is running, so the renderer can say whether changes
    /// take effect now.
    streaming: bool,
    error: Option<(SettingsRow, String)>,
}

impl SettingsState {
    pub fn new(settings: Settings, handoff_available: bool, streaming: bool) -> Self {
        let mut state = Self {
            settings,
            cursor: 0,
            handoff_available,
            streaming,
            error: None,
        };
        // A remembered preference cannot switch handoff on where there is no
        // cable to route through — the same rule the picker applies.
        if !handoff_available {
            state.settings.handoff = false;
        }
        state
    }

    pub fn rows(&self) -> &[SettingsRow] {
        &ROWS
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn streaming(&self) -> bool {
        self.streaming
    }

    pub fn handoff_available(&self) -> bool {
        self.handoff_available
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_ref().map(|(_, msg)| msg.as_str())
    }

    /// Which row the current error belongs to, so the renderer can put it there
    /// rather than in a shared status line. With five rows on screen, "which
    /// one failed" is the first question.
    pub fn error_row(&self) -> Option<SettingsRow> {
        self.error.as_ref().map(|(row, _)| *row)
    }

    pub fn set_error(&mut self, row: SettingsRow, msg: impl Into<String>) {
        self.error = Some((row, msg.into()));
    }

    /// Put the settings back to `previous` after an apply failed.
    ///
    /// Deliberately does not clear the error: the whole point is that the user
    /// sees why the value bounced back.
    pub fn revert(&mut self, previous: Settings) {
        self.settings = previous;
    }

    pub fn on_key(&mut self, key: KeyCode) -> SettingsAction {
        match key {
            KeyCode::Up => {
                self.error = None;
                self.cursor = self.cursor.saturating_sub(1);
                SettingsAction::None
            }
            KeyCode::Down => {
                self.error = None;
                if self.cursor + 1 < ROWS.len() {
                    self.cursor += 1;
                }
                SettingsAction::None
            }
            KeyCode::Right | KeyCode::Char('>') | KeyCode::Char('.') => self.adjust(true),
            KeyCode::Left | KeyCode::Char('<') | KeyCode::Char(',') => self.adjust(false),
            KeyCode::Char(' ') | KeyCode::Enter => self.adjust(true),
            KeyCode::Char('s') | KeyCode::Esc => SettingsAction::Close,
            _ => SettingsAction::None,
        }
    }

    /// Adjust the highlighted row.
    ///
    /// `up` is ignored by boolean rows, which toggle either way — a checkbox
    /// has no direction, and making `←` mean "off" would be a rule nobody is
    /// told.
    fn adjust(&mut self, up: bool) -> SettingsAction {
        self.error = None;
        match ROWS[self.cursor] {
            SettingsRow::Handoff => {
                if !self.handoff_available {
                    self.set_error(
                        SettingsRow::Handoff,
                        "no virtual audio cable detected — install VB-CABLE to use handoff",
                    );
                    return SettingsAction::None;
                }
                self.settings.handoff = !self.settings.handoff;
            }
            SettingsRow::Latency => {
                let next = if up {
                    self.settings.latency_ms.saturating_add(LATENCY_STEP_MS)
                } else {
                    self.settings.latency_ms.saturating_sub(LATENCY_STEP_MS)
                };
                self.settings.latency_ms = next.clamp(LATENCY_MIN_MS, LATENCY_MAX_MS);
            }
            SettingsRow::Volume => {
                let delta = if up { VOLUME_STEP_DB } else { -VOLUME_STEP_DB };
                self.settings.volume_db =
                    (self.settings.volume_db + delta).clamp(VOLUME_MIN_DB, VOLUME_MAX_DB);
            }
            SettingsRow::Metadata => self.settings.metadata = !self.settings.metadata,
            SettingsRow::ShowControls => self.settings.show_controls = !self.settings.show_controls,
        }
        SettingsAction::Apply(self.settings.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> SettingsState {
        SettingsState::new(Settings::default(), true, false)
    }

    fn at(row: SettingsRow) -> SettingsState {
        let mut s = state();
        while s.rows()[s.cursor()] != row {
            s.on_key(KeyCode::Down);
        }
        s
    }

    #[test]
    fn the_cursor_stops_at_both_ends() {
        let mut s = state();
        s.on_key(KeyCode::Up);
        assert_eq!(s.cursor(), 0);
        for _ in 0..50 {
            s.on_key(KeyCode::Down);
        }
        assert_eq!(s.cursor(), s.rows().len() - 1);
    }

    #[test]
    fn latency_steps_and_clamps() {
        let mut s = at(SettingsRow::Latency);
        s.settings.latency_ms = LATENCY_MAX_MS;
        s.on_key(KeyCode::Right);
        assert_eq!(s.settings.latency_ms, LATENCY_MAX_MS, "clamped at the top");

        s.settings.latency_ms = LATENCY_MIN_MS;
        s.on_key(KeyCode::Left);
        assert_eq!(
            s.settings.latency_ms, LATENCY_MIN_MS,
            "clamped at the floor"
        );

        s.settings.latency_ms = 500;
        s.on_key(KeyCode::Right);
        assert_eq!(s.settings.latency_ms, 500 + LATENCY_STEP_MS);
        s.on_key(KeyCode::Left);
        assert_eq!(s.settings.latency_ms, 500);
    }

    #[test]
    fn angle_brackets_adjust_as_well_as_arrows() {
        // `<>` already means "adjust" on the picker and the dashboard; a
        // settings screen where it did nothing would be a trap.
        let mut s = at(SettingsRow::Latency);
        s.settings.latency_ms = 500;
        s.on_key(KeyCode::Char('>'));
        assert_eq!(s.settings.latency_ms, 500 + LATENCY_STEP_MS);
        s.on_key(KeyCode::Char('<'));
        assert_eq!(s.settings.latency_ms, 500);
    }

    #[test]
    fn volume_steps_and_clamps() {
        let mut s = at(SettingsRow::Volume);
        s.settings.volume_db = 0.0;
        s.on_key(KeyCode::Right);
        assert_eq!(s.settings.volume_db, 0.0, "0 dB is the ceiling");

        s.settings.volume_db = VOLUME_MIN_DB;
        s.on_key(KeyCode::Left);
        assert_eq!(s.settings.volume_db, VOLUME_MIN_DB, "-60 dB is the floor");

        s.settings.volume_db = -8.0;
        s.on_key(KeyCode::Left);
        assert_eq!(s.settings.volume_db, -9.0);
    }

    #[test]
    fn space_toggles_a_boolean_row_and_direction_is_ignored() {
        let mut s = at(SettingsRow::Metadata);
        let before = s.settings.metadata;
        s.on_key(KeyCode::Char(' '));
        assert_eq!(s.settings.metadata, !before);
        s.on_key(KeyCode::Enter);
        assert_eq!(s.settings.metadata, before, "enter toggles too");
        s.on_key(KeyCode::Left);
        assert_eq!(s.settings.metadata, !before, "a checkbox has no direction");
    }

    #[test]
    fn handoff_cannot_be_enabled_without_a_cable() {
        // Same rule the picker's `h` key enforces, and the same explanation.
        let mut s = SettingsState::new(Settings::default(), false, false);
        while s.rows()[s.cursor()] != SettingsRow::Handoff {
            s.on_key(KeyCode::Down);
        }
        assert!(!s.settings.handoff, "forced off when unavailable");
        assert_eq!(s.on_key(KeyCode::Char(' ')), SettingsAction::None);
        assert!(!s.settings.handoff);
        assert!(
            s.error().unwrap().contains("VB-CABLE"),
            "got: {:?}",
            s.error()
        );
    }

    #[test]
    fn a_change_asks_to_be_applied() {
        let mut s = at(SettingsRow::Metadata);
        match s.on_key(KeyCode::Char(' ')) {
            SettingsAction::Apply(next) => assert_eq!(next.metadata, s.settings.metadata),
            other => panic!("expected Apply, got {other:?}"),
        }
    }

    #[test]
    fn navigation_does_not_ask_to_be_applied() {
        let mut s = state();
        assert_eq!(s.on_key(KeyCode::Down), SettingsAction::None);
        assert_eq!(s.on_key(KeyCode::Up), SettingsAction::None);
    }

    #[test]
    fn s_and_esc_close() {
        let mut s = state();
        assert_eq!(s.on_key(KeyCode::Esc), SettingsAction::Close);
        assert_eq!(s.on_key(KeyCode::Char('s')), SettingsAction::Close);
    }

    #[test]
    fn revert_restores_the_value_and_keeps_the_reason_visible() {
        // The applier failed. The setting must go back to what is actually in
        // force, and the user must be told why rather than watching a value
        // silently bounce.
        let mut s = at(SettingsRow::Handoff);
        let before = s.settings.clone();
        s.on_key(KeyCode::Char(' '));
        assert_ne!(s.settings.handoff, before.handoff);

        s.set_error(SettingsRow::Handoff, "cable disappeared");
        s.revert(before.clone());
        assert_eq!(s.settings, before, "back to what is in force");
        assert_eq!(s.error(), Some("cable disappeared"));
        assert_eq!(s.error_row(), Some(SettingsRow::Handoff));
    }

    #[test]
    fn moving_clears_a_stale_error() {
        let mut s = state();
        s.set_error(SettingsRow::Handoff, "cable disappeared");
        s.on_key(KeyCode::Down);
        assert!(
            s.error().is_none(),
            "a stale explanation is worse than none"
        );
    }
}
