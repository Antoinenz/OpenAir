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
use openair_client::PairedPeer;

use crate::pairing_list::{ListAction, PairingList};
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
    AdaptiveResampling,
    /// Not a setting — opens the list of receivers we hold credentials for.
    Pairings,
}

const ROWS: [SettingsRow; 7] = [
    SettingsRow::Handoff,
    SettingsRow::Latency,
    SettingsRow::Volume,
    SettingsRow::Metadata,
    SettingsRow::ShowControls,
    SettingsRow::AdaptiveResampling,
    // Last, and after a visual gap in the renderer: it is the only row that
    // goes somewhere rather than changing something.
    SettingsRow::Pairings,
];

#[derive(Debug, Clone, PartialEq)]
pub enum SettingsAction {
    /// Redraw; nothing else.
    None,
    /// Close the overlay and return to the screen underneath.
    Close,
    /// The settings changed; the caller should apply and persist them.
    Apply(Settings),
    /// Forget the stored pairing for this device id, then report back with
    /// [`SettingsState::forgotten`] or [`SettingsState::pairing_error`].
    Forget(String),
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
    /// Receivers we hold HomeKit credentials for.
    ///
    /// Passed in rather than read here: `PairingStore::load` touches the real
    /// filesystem, and a settings screen that could not be unit-tested without
    /// one would stop being tested.
    peers: Vec<PairedPeer>,
    /// The pairings list, while it is open over the rows.
    list: Option<PairingList>,
}

impl SettingsState {
    pub fn new(settings: Settings, handoff_available: bool, streaming: bool) -> Self {
        Self::with_peers(settings, handoff_available, streaming, Vec::new())
    }

    pub fn with_peers(
        settings: Settings,
        handoff_available: bool,
        streaming: bool,
        peers: Vec<PairedPeer>,
    ) -> Self {
        let mut state = Self {
            settings,
            cursor: 0,
            handoff_available,
            streaming,
            error: None,
            peers,
            list: None,
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

    /// The pairings list, if it is open.
    pub fn list(&self) -> Option<&PairingList> {
        self.list.as_ref()
    }

    /// How many receivers we hold credentials for — shown on the row.
    pub fn pairing_count(&self) -> usize {
        match &self.list {
            Some(list) => list.peers().len(),
            None => self.peers.len(),
        }
    }

    /// A forget succeeded.
    pub fn forgotten(&mut self, device_id: &str) {
        self.peers.retain(|p| p.device_id != device_id);
        if let Some(list) = self.list.as_mut() {
            list.forgotten(device_id);
        }
    }

    /// A forget failed, and why.
    pub fn pairing_error(&mut self, msg: impl Into<String>) {
        if let Some(list) = self.list.as_mut() {
            list.set_error(msg);
        }
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
        // The list, while it is open, owns every key. Letting the rows keep
        // `←→` underneath would mean adjusting the latency you cannot see.
        if let Some(list) = self.list.as_mut() {
            return match list.on_key(key) {
                ListAction::None => SettingsAction::None,
                ListAction::Close => {
                    self.list = None;
                    SettingsAction::None
                }
                ListAction::Forget(id) => SettingsAction::Forget(id),
            };
        }

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
            SettingsRow::AdaptiveResampling => {
                self.settings.adaptive_resampling = !self.settings.adaptive_resampling
            }
            SettingsRow::Pairings => {
                // Nothing to apply: this row opens a screen rather than
                // holding a value. Only forwards -- `←` is what closes the
                // list again, so having it open one too would be a key that
                // undid itself.
                if up {
                    self.list = Some(PairingList::new(self.peers.clone()));
                }
                return SettingsAction::None;
            }
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

    fn peer(id: &str, name: &str) -> PairedPeer {
        PairedPeer {
            device_id: id.to_string(),
            name: Some(name.to_string()),
        }
    }

    /// Settings with two stored pairings, cursor parked on the pairings row.
    fn with_pairings() -> SettingsState {
        let mut s = SettingsState::with_peers(
            Settings::default(),
            true,
            false,
            vec![peer("AA:AA", "Living Room"), peer("BB:BB", "Pool Room")],
        );
        while s.rows()[s.cursor()] != SettingsRow::Pairings {
            s.on_key(KeyCode::Down);
        }
        s
    }

    #[test]
    fn the_pairings_row_counts_what_is_stored() {
        assert_eq!(with_pairings().pairing_count(), 2);
        assert_eq!(state().pairing_count(), 0);
    }

    #[test]
    fn the_pairings_row_opens_a_list_rather_than_changing_a_setting() {
        let mut s = with_pairings();
        let before = s.settings.clone();
        assert_eq!(s.on_key(KeyCode::Enter), SettingsAction::None);
        assert!(s.list().is_some());
        assert_eq!(s.settings, before, "it holds no value to change");
    }

    #[test]
    fn left_does_not_open_the_list() {
        // `←` is what closes it again; a key that undid itself would be a
        // trap.
        let mut s = with_pairings();
        s.on_key(KeyCode::Left);
        assert!(s.list().is_none());
    }

    #[test]
    fn the_open_list_takes_every_key() {
        // Otherwise `←→` would adjust a latency the user cannot see, on a
        // screen that is showing them something else entirely.
        let mut s = with_pairings();
        s.on_key(KeyCode::Enter);
        let latency = s.settings.latency_ms;
        for key in [KeyCode::Right, KeyCode::Char(' '), KeyCode::Char('>')] {
            assert_eq!(s.on_key(key), SettingsAction::None);
        }
        assert_eq!(s.settings.latency_ms, latency, "nothing underneath moved");
    }

    #[test]
    fn esc_closes_the_list_first_and_the_overlay_second() {
        // One Esc should not throw away both screens: the user who opened the
        // list to look at it expects to get back to settings.
        let mut s = with_pairings();
        s.on_key(KeyCode::Enter);
        assert_eq!(s.on_key(KeyCode::Esc), SettingsAction::None);
        assert!(s.list().is_none(), "back to the rows");
        assert_eq!(s.on_key(KeyCode::Esc), SettingsAction::Close);
    }

    #[test]
    fn forgetting_bubbles_up_and_the_count_follows() {
        let mut s = with_pairings();
        s.on_key(KeyCode::Enter);
        s.on_key(KeyCode::Char('d')); // arms
        assert_eq!(
            s.on_key(KeyCode::Char('d')),
            SettingsAction::Forget("AA:AA".into())
        );

        // The caller does the work, then reports back.
        s.forgotten("AA:AA");
        assert_eq!(s.pairing_count(), 1);
        s.on_key(KeyCode::Esc);
        assert_eq!(s.pairing_count(), 1, "and the row agrees once closed");
    }

    #[test]
    fn a_failed_forget_leaves_the_count_alone() {
        let mut s = with_pairings();
        s.on_key(KeyCode::Enter);
        s.pairing_error("pairings.json is read-only");
        assert_eq!(s.pairing_count(), 2);
        assert!(s.list().unwrap().status().unwrap().contains("read-only"));
    }

    #[test]
    fn reopening_the_list_shows_what_survived() {
        // The list is built from `peers` each time it opens. If forgetting only
        // updated the open list, closing and reopening would resurrect the row.
        let mut s = with_pairings();
        s.on_key(KeyCode::Enter);
        s.forgotten("AA:AA");
        s.on_key(KeyCode::Esc);
        s.on_key(KeyCode::Enter);
        assert_eq!(s.list().unwrap().peers().len(), 1);
        assert_eq!(s.list().unwrap().peers()[0].device_id, "BB:BB");
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
