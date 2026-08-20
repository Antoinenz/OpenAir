//! The device picker's state and input handling — no rendering, no network.
//!
//! Everything here is a pure function of the devices that have arrived so far
//! plus the keys pressed, which is what makes it testable. Discovery feeds it
//! through [`PickerState::insert`]; drawing reads [`PickerState::rows`].
//!
//! The picker deliberately never contacts a device. Names, addresses, models
//! and capabilities all come from the mDNS TXT record; "paired" comes from the
//! local pairings file. Nothing leaves the machine until the user presses
//! Enter — which is both faster and, on a shared network, the polite thing to
//! do.

use std::collections::HashSet;
use std::net::SocketAddr;

use crossterm::event::KeyCode;
use openair_discovery::{AirPlayDevice, DeviceSet};

use crate::settings::Settings;

/// One row as drawn.
#[derive(Debug, Clone, PartialEq)]
pub struct PickerRow {
    /// Stable identity, used to track selection across re-sorts.
    pub key: String,
    pub name: String,
    pub addr: SocketAddr,
    /// TXT `deviceid`, when advertised. The stream needs it; receivers that
    /// don't advertise one get the caller's default.
    pub device_id: Option<String>,
    pub model: String,
    pub selected: bool,
    /// We hold HomeKit credentials for this device.
    pub paired: bool,
    /// Needs `openair pair` before it can be streamed to.
    pub needs_pairing: bool,
}

/// What the caller should do after a key press.
#[derive(Debug, Clone, PartialEq)]
pub enum PickerAction {
    /// Redraw; nothing else.
    None,
    /// User asked to leave without streaming.
    Quit,
    /// User confirmed the selection.
    Start,
    /// Show a one-line explanation instead of acting.
    Hint(String),
}

pub struct PickerState {
    seen: DeviceSet,
    /// Rebuilt from `seen` whenever it changes; the order the user sees.
    rows: Vec<PickerRow>,
    selected: HashSet<String>,
    cursor: usize,
    pub settings: Settings,
    /// Device IDs we already hold credentials for.
    paired: HashSet<String>,
    /// Whether a virtual audio cable was detected (Windows). When false the
    /// handoff toggle cannot be turned on.
    handoff_available: bool,
    hint: Option<String>,
    /// Why the user was sent back here, if they were. Shown until the next
    /// keystroke, like a hint — an explanation that outlives the moment it
    /// explains just becomes furniture.
    banner: Option<String>,
}

impl PickerState {
    pub fn new(settings: Settings, paired: Vec<String>, handoff_available: bool) -> Self {
        let mut state = Self {
            seen: DeviceSet::new(),
            rows: Vec::new(),
            selected: HashSet::new(),
            cursor: 0,
            settings,
            paired: paired.into_iter().collect(),
            handoff_available,
            hint: None,
            banner: None,
        };
        // A remembered preference cannot switch handoff on where there is no
        // cable to route through: the preference is remembered, the outcome is
        // detected.
        if !handoff_available {
            state.settings.handoff = false;
        }
        state
    }

    /// Feed in a device from discovery. Returns whether the visible list
    /// changed, so the caller can skip a redraw on a repeat announcement.
    pub fn insert(&mut self, device: AirPlayDevice) -> bool {
        if !self.seen.insert(device) {
            return false;
        }
        self.rebuild_rows();
        true
    }

    fn rebuild_rows(&mut self) {
        // Remember what the cursor was pointing at: rows re-sort as devices
        // arrive, and a cursor pinned to an index would drift onto a different
        // receiver mid-keystroke.
        let anchored = self.rows.get(self.cursor).map(|r| r.key.clone());

        let mut rows: Vec<PickerRow> = self
            .seen
            .iter()
            .map(|d| {
                let key = DeviceSet::key_for(d);
                let paired = d
                    .txt
                    .device_id
                    .as_ref()
                    .map(|id| self.paired.contains(id))
                    .unwrap_or(false);
                PickerRow {
                    selected: self.selected.contains(&key),
                    needs_pairing: needs_pairing(d, paired),
                    paired,
                    key,
                    name: d.display_name().to_string(),
                    addr: SocketAddr::new(d.addr, d.port),
                    device_id: d.txt.device_id.clone(),
                    model: d.txt.model.clone().unwrap_or_else(|| "unknown".into()),
                }
            })
            .collect();

        // Paired first — your usual speakers stay put as strangers' devices
        // trickle in — then by name for a stable order.
        rows.sort_by(|a, b| {
            b.paired
                .cmp(&a.paired)
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| a.key.cmp(&b.key))
        });

        self.rows = rows;
        self.cursor = match anchored {
            Some(key) => self
                .rows
                .iter()
                .position(|r| r.key == key)
                .unwrap_or(self.cursor),
            None => self.cursor,
        };
        self.clamp_cursor();
    }

    fn clamp_cursor(&mut self) {
        if self.rows.is_empty() {
            self.cursor = 0;
        } else if self.cursor >= self.rows.len() {
            self.cursor = self.rows.len() - 1;
        }
    }

    pub fn rows(&self) -> &[PickerRow] {
        &self.rows
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn hint(&self) -> Option<&str> {
        self.hint.as_deref()
    }

    pub fn banner(&self) -> Option<&str> {
        self.banner.as_deref()
    }

    /// Explain why the user is back here, e.g. after nothing connected.
    pub fn set_banner(&mut self, msg: impl Into<String>) {
        self.banner = Some(msg.into());
    }

    pub fn handoff_available(&self) -> bool {
        self.handoff_available
    }

    /// The devices the user chose, in display order.
    pub fn chosen(&self) -> Vec<&PickerRow> {
        self.rows.iter().filter(|r| r.selected).collect()
    }

    pub fn on_key(&mut self, key: KeyCode) -> PickerAction {
        // Any keystroke clears the previous hint; a stale explanation is worse
        // than none.
        self.hint = None;
        self.banner = None;

        match key {
            KeyCode::Up => {
                self.cursor = self.cursor.saturating_sub(1);
                PickerAction::None
            }
            KeyCode::Down => {
                if !self.rows.is_empty() && self.cursor + 1 < self.rows.len() {
                    self.cursor += 1;
                }
                PickerAction::None
            }
            KeyCode::Char(' ') => {
                self.toggle_selection();
                PickerAction::None
            }
            KeyCode::Enter => self.confirm(),
            KeyCode::Char('h') => self.toggle_handoff(),
            // `<`/`>` rather than `+`/`-`: the dashboard uses `+`/`-` for
            // volume and `<`/`>` for timing, and these two screens sit in one
            // flow. One key meaning volume on one screen and latency on the
            // next is a trap.
            KeyCode::Char('>') | KeyCode::Char('.') => {
                self.settings.nudge_latency(true);
                PickerAction::None
            }
            KeyCode::Char('<') | KeyCode::Char(',') => {
                self.settings.nudge_latency(false);
                PickerAction::None
            }
            KeyCode::Char('q') | KeyCode::Esc => PickerAction::Quit,
            _ => PickerAction::None,
        }
    }

    fn toggle_selection(&mut self) {
        let Some(row) = self.rows.get_mut(self.cursor) else {
            return;
        };
        row.selected = !row.selected;
        if row.selected {
            self.selected.insert(row.key.clone());
        } else {
            self.selected.remove(&row.key);
        }
    }

    fn toggle_handoff(&mut self) -> PickerAction {
        if !self.handoff_available {
            return self.hint_action(
                "no virtual audio cable detected — install VB-CABLE to use handoff",
            );
        }
        self.settings.handoff = !self.settings.handoff;
        PickerAction::None
    }

    fn confirm(&mut self) -> PickerAction {
        let chosen = self.chosen();
        if chosen.is_empty() {
            return self.hint_action("select a receiver with space first");
        }
        // Catch this here rather than letting the handshake fail with
        // something opaque several seconds later.
        if let Some(row) = chosen.iter().find(|r| r.needs_pairing) {
            let name = row.name.clone();
            return self.hint_action(&format!(
                "{name} needs pairing first — run: openair pair \"{name}\""
            ));
        }
        PickerAction::Start
    }

    fn hint_action(&mut self, msg: &str) -> PickerAction {
        self.hint = Some(msg.to_string());
        PickerAction::Hint(msg.to_string())
    }
}

/// Whether a device must be paired with `openair pair` before streaming.
///
/// Derived from Transient-pairing support rather than the `sf` status flags:
/// a device that supports Transient pairing (feature bit 43 or 48) negotiates
/// its own keys per session and needs nothing stored, while one that does not
/// requires Normal pairing and therefore a credential on disk. The `sf` bit
/// layout is not verified anywhere in this codebase, and guessing at it would
/// put a wrong warning in front of the user.
fn needs_pairing(device: &AirPlayDevice, paired: bool) -> bool {
    !device.uses_transient_pairing() && !paired
}

#[cfg(test)]
mod tests {
    use super::*;
    use openair_discovery::AirPlayTxt;
    use std::collections::HashMap;

    /// `features` value with bit 48 (Transient pairing) set, so the device
    /// needs no prior `openair pair`.
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

    fn picker() -> PickerState {
        PickerState::new(Settings::default(), Vec::new(), true)
    }

    #[test]
    fn transient_devices_need_no_pairing() {
        let d = device("Pool Room", "192.168.1.51", "1", TRANSIENT);
        assert!(d.uses_transient_pairing(), "test fixture sets bit 48");
        assert!(!needs_pairing(&d, false));
    }

    #[test]
    fn non_transient_devices_need_pairing_unless_already_paired() {
        let d = device("Living Room", "192.168.1.106", "1", NEEDS_PAIRING);
        assert!(needs_pairing(&d, false));
        assert!(!needs_pairing(&d, true), "a stored credential clears it");
    }

    #[test]
    fn repeat_announcement_does_not_duplicate_a_row() {
        let mut p = picker();
        assert!(p.insert(device("Pool Room", "192.168.1.51", "1", TRANSIENT)));
        assert!(!p.insert(device("Pool Room", "192.168.1.51", "1", TRANSIENT)));
        assert_eq!(p.rows().len(), 1);
    }

    #[test]
    fn paired_devices_sort_before_unpaired_then_by_name() {
        let mut p = PickerState::new(Settings::default(), vec!["2".into()], true);
        p.insert(device("Alpha", "192.168.1.10", "1", TRANSIENT));
        p.insert(device("Zulu", "192.168.1.11", "2", TRANSIENT)); // paired
        p.insert(device("Bravo", "192.168.1.12", "3", TRANSIENT));

        let names: Vec<&str> = p.rows().iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["Zulu", "Alpha", "Bravo"]);
    }

    #[test]
    fn selection_follows_the_device_when_rows_resort() {
        // The bug this prevents: select row 0, a paired device arrives and
        // takes the top slot, and the selection now points at the wrong
        // receiver.
        let mut p = PickerState::new(Settings::default(), vec!["2".into()], true);
        p.insert(device("Alpha", "192.168.1.10", "1", TRANSIENT));
        p.on_key(KeyCode::Char(' '));
        assert_eq!(p.chosen()[0].name, "Alpha");

        p.insert(device("Zulu", "192.168.1.11", "2", TRANSIENT)); // paired, sorts first
        assert_eq!(p.rows()[0].name, "Zulu");
        assert_eq!(p.chosen().len(), 1);
        assert_eq!(p.chosen()[0].name, "Alpha", "selection stayed with the device");
    }

    #[test]
    fn cursor_follows_the_device_when_rows_resort() {
        let mut p = PickerState::new(Settings::default(), vec!["2".into()], true);
        p.insert(device("Alpha", "192.168.1.10", "1", TRANSIENT));
        assert_eq!(p.rows()[p.cursor()].name, "Alpha");
        p.insert(device("Zulu", "192.168.1.11", "2", TRANSIENT));
        assert_eq!(
            p.rows()[p.cursor()].name,
            "Alpha",
            "the highlight must not jump to another device"
        );
    }

    #[test]
    fn cursor_stays_in_bounds_at_both_ends() {
        let mut p = picker();
        p.insert(device("Alpha", "192.168.1.10", "1", TRANSIENT));
        p.insert(device("Bravo", "192.168.1.11", "2", TRANSIENT));

        p.on_key(KeyCode::Up);
        assert_eq!(p.cursor(), 0, "already at the top");
        p.on_key(KeyCode::Down);
        p.on_key(KeyCode::Down);
        p.on_key(KeyCode::Down);
        assert_eq!(p.cursor(), 1, "already at the bottom");
    }

    #[test]
    fn keys_on_an_empty_list_do_not_panic() {
        let mut p = picker();
        assert_eq!(p.on_key(KeyCode::Up), PickerAction::None);
        assert_eq!(p.on_key(KeyCode::Down), PickerAction::None);
        assert_eq!(p.on_key(KeyCode::Char(' ')), PickerAction::None);
        assert!(matches!(p.on_key(KeyCode::Enter), PickerAction::Hint(_)));
    }

    #[test]
    fn space_toggles_selection_off_again() {
        let mut p = picker();
        p.insert(device("Alpha", "192.168.1.10", "1", TRANSIENT));
        p.on_key(KeyCode::Char(' '));
        assert_eq!(p.chosen().len(), 1);
        p.on_key(KeyCode::Char(' '));
        assert!(p.chosen().is_empty());
    }

    #[test]
    fn enter_with_nothing_selected_hints_instead_of_starting() {
        let mut p = picker();
        p.insert(device("Alpha", "192.168.1.10", "1", TRANSIENT));
        match p.on_key(KeyCode::Enter) {
            PickerAction::Hint(msg) => assert!(msg.contains("space"), "got: {msg}"),
            other => panic!("expected a hint, got {other:?}"),
        }
    }

    #[test]
    fn enter_on_an_unpaired_device_names_the_pair_command() {
        let mut p = picker();
        p.insert(device("Living Room", "192.168.1.106", "1", NEEDS_PAIRING));
        p.on_key(KeyCode::Char(' '));
        match p.on_key(KeyCode::Enter) {
            PickerAction::Hint(msg) => {
                assert!(msg.contains(r#"openair pair "Living Room""#), "got: {msg}");
            }
            other => panic!("expected a hint, got {other:?}"),
        }
    }

    #[test]
    fn enter_with_a_valid_selection_starts() {
        let mut p = picker();
        p.insert(device("Pool Room", "192.168.1.51", "1", TRANSIENT));
        p.on_key(KeyCode::Char(' '));
        assert_eq!(p.on_key(KeyCode::Enter), PickerAction::Start);
    }

    #[test]
    fn a_keystroke_clears_the_previous_hint() {
        let mut p = picker();
        p.insert(device("Alpha", "192.168.1.10", "1", TRANSIENT));
        p.on_key(KeyCode::Enter);
        assert!(p.hint().is_some());
        p.on_key(KeyCode::Down);
        assert!(p.hint().is_none(), "a stale explanation is worse than none");
    }

    #[test]
    fn latency_keys_clamp_at_the_bounds() {
        let mut p = picker();
        p.settings.latency_ms = crate::settings::LATENCY_MAX_MS;
        p.on_key(KeyCode::Char('>'));
        assert_eq!(p.settings.latency_ms, crate::settings::LATENCY_MAX_MS);

        p.settings.latency_ms = crate::settings::LATENCY_MIN_MS;
        p.on_key(KeyCode::Char('<'));
        assert_eq!(p.settings.latency_ms, crate::settings::LATENCY_MIN_MS);
    }

    #[test]
    fn handoff_toggles_when_a_cable_is_present() {
        let mut p = picker();
        assert!(p.settings.handoff, "default on when available");
        p.on_key(KeyCode::Char('h'));
        assert!(!p.settings.handoff);
        p.on_key(KeyCode::Char('h'));
        assert!(p.settings.handoff);
    }

    #[test]
    fn handoff_cannot_be_enabled_without_a_cable() {
        // The remembered preference is `true`, but there is nothing to route
        // through — so it starts off and the key explains why.
        let mut p = PickerState::new(Settings::default(), Vec::new(), false);
        assert!(!p.settings.handoff);
        match p.on_key(KeyCode::Char('h')) {
            PickerAction::Hint(msg) => assert!(msg.contains("VB-CABLE"), "got: {msg}"),
            other => panic!("expected a hint, got {other:?}"),
        }
        assert!(!p.settings.handoff);
    }

    #[test]
    fn quit_keys_report_quit() {
        let mut p = picker();
        assert_eq!(p.on_key(KeyCode::Char('q')), PickerAction::Quit);
        assert_eq!(p.on_key(KeyCode::Esc), PickerAction::Quit);
    }
}
