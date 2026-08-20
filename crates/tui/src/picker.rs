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

/// How many rows are built at a time, and how far the window grows each time
/// the cursor approaches its end.
const ROW_LIMIT_STEP: usize = 50;

/// How close to the end of the window the cursor gets before it is extended.
/// Large enough that a held arrow key never catches up with the growth.
const ROW_LIMIT_MARGIN: usize = 10;

pub struct PickerState {
    seen: DeviceSet,
    /// Every known device, sorted — **not** the window.
    ///
    /// The window is applied in [`PickerState::rows`], so anything that has to
    /// be right regardless of what is on screen (selection, above all) reads
    /// this instead.
    all_rows: Vec<PickerRow>,
    /// How many of `all_rows` are offered for drawing.
    visible_limit: usize,
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
        Self::with_selection(settings, paired, handoff_available, Vec::new())
    }

    /// Build with `selection` (device keys) already chosen.
    ///
    /// Used when the user is sent back here after a failure: the devices have
    /// not been rediscovered yet, but a key that reappears will come back
    /// selected, so a retry is one keystroke rather than a re-pick.
    pub fn with_selection(
        settings: Settings,
        paired: Vec<String>,
        handoff_available: bool,
        selection: Vec<String>,
    ) -> Self {
        let mut state = Self {
            seen: DeviceSet::new(),
            all_rows: Vec::new(),
            visible_limit: ROW_LIMIT_STEP,
            selected: selection.into_iter().collect(),
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
        let anchored = self.all_rows.get(self.cursor).map(|r| r.key.clone());

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
                    model: d.pretty_model().to_string(),
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

        self.all_rows = rows;
        self.cursor = match anchored {
            Some(key) => self
                .all_rows
                .iter()
                .position(|r| r.key == key)
                .unwrap_or(self.cursor),
            None => self.cursor,
        };
        self.clamp_cursor();
    }

    fn clamp_cursor(&mut self) {
        if self.all_rows.is_empty() {
            self.cursor = 0;
        } else if self.cursor >= self.all_rows.len() {
            self.cursor = self.all_rows.len() - 1;
        }
        self.extend_window();
    }

    /// Grow the window when the cursor nears its end.
    ///
    /// Only ever grows. Shrinking it back would move rows out from under a
    /// cursor that is already there, and the memory saved is a few dozen
    /// structs.
    fn extend_window(&mut self) {
        while self.cursor + ROW_LIMIT_MARGIN >= self.visible_limit
            && self.visible_limit < self.all_rows.len()
        {
            self.visible_limit += ROW_LIMIT_STEP;
        }
    }

    /// The rows to draw: the first [`ROW_LIMIT_STEP`] devices, extended as the
    /// cursor travels down.
    ///
    /// **A rendering cap, not a discovery cap.** `DeviceSet` still keeps every
    /// device it hears about — limiting discovery would mean missing one that
    /// announces late, which on a busy network is exactly the receiver someone
    /// is waiting for.
    pub fn rows(&self) -> &[PickerRow] {
        &self.all_rows[..self.visible_limit.min(self.all_rows.len())]
    }

    /// How many devices are known, whether or not they are drawn.
    pub fn total(&self) -> usize {
        self.all_rows.len()
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
    ///
    /// Reads `all_rows`, not the window. Filtering to what happens to be on
    /// screen would silently drop a receiver the user picked before more
    /// devices arrived and pushed it out of view.
    pub fn chosen(&self) -> Vec<&PickerRow> {
        self.all_rows.iter().filter(|r| r.selected).collect()
    }

    /// The selected device keys, including any not currently on screen.
    ///
    /// Deliberately not `chosen()`: a device that has gone quiet is still
    /// selected, and losing that on the way back to the picker is the bug this
    /// exists to prevent.
    pub fn selection_keys(&self) -> Vec<String> {
        self.selected.iter().cloned().collect()
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
                if self.cursor + 1 < self.all_rows.len() {
                    self.cursor += 1;
                    self.extend_window();
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
        let Some(row) = self.all_rows.get_mut(self.cursor) else {
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
        // Devices needing a PIN used to be refused here. Pairing now happens
        // after this point, inside the flow, so there is nothing to refuse.
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

    /// `n` transient devices, named so their sort order is the insertion
    /// order — `dev-000` … `dev-199`.
    fn many_devices(p: &mut PickerState, n: usize) {
        for i in 0..n {
            p.insert(device(
                &format!("dev-{i:03}"),
                &format!("192.168.{}.{}", i / 250, i % 250 + 1),
                &format!("{i}"),
                TRANSIENT,
            ));
        }
    }

    #[test]
    fn a_large_network_renders_a_window_not_everything() {
        let mut p = picker();
        many_devices(&mut p, 200);
        assert_eq!(p.rows().len(), ROW_LIMIT_STEP, "only a window is drawn");
        assert_eq!(p.total(), 200, "but every device is still known");
    }

    #[test]
    fn the_window_extends_as_the_cursor_approaches_its_end() {
        let mut p = picker();
        many_devices(&mut p, 200);
        for _ in 0..45 {
            p.on_key(KeyCode::Down);
        }
        assert_eq!(p.cursor(), 45);
        assert_eq!(p.rows().len(), ROW_LIMIT_STEP * 2);
    }

    #[test]
    fn the_cursor_can_reach_the_last_device() {
        // The window must never become a floor the cursor cannot climb past.
        let mut p = picker();
        many_devices(&mut p, 200);
        for _ in 0..500 {
            p.on_key(KeyCode::Down);
        }
        assert_eq!(p.cursor(), 199);
        assert_eq!(p.rows().len(), 200, "fully extended, never over-extended");
        assert_eq!(p.rows()[p.cursor()].name, "dev-199");
    }

    #[test]
    fn a_selection_beyond_the_window_is_still_reported() {
        // The trap this whole task walks past: a chosen receiver ends up
        // outside the drawn window and must still be streamed to. Filtering
        // the selection to the visible rows would drop it silently, which is
        // the worst failure available here.
        //
        // Staged the way it actually happens: the user comes back from a
        // failed connect with the selection remembered as a key, then a busy
        // network fills the list in ahead of it. The cursor stays where it
        // is, so nothing extends the window to reveal it.
        let mut p = PickerState::with_selection(
            Settings::default(),
            Vec::new(),
            true,
            vec!["pool".to_string()],
        );
        many_devices(&mut p, 200);
        // Named to sort last, so it lands well outside the window.
        p.insert(device("zzz Pool Room", "192.168.9.9", "pool", TRANSIENT));

        assert_eq!(p.cursor(), 0, "the cursor never travelled");
        assert!(
            !p.rows().iter().any(|r| r.name == "zzz Pool Room"),
            "fixture check: it must actually be outside the window"
        );
        assert_eq!(p.chosen().len(), 1, "still chosen");
        assert_eq!(p.chosen()[0].name, "zzz Pool Room");
        assert_eq!(
            p.selection_keys(),
            ["pool"],
            "and survives another trip back here"
        );
    }

    #[test]
    fn sort_order_survives_an_extension() {
        // Paired devices sort first, and an extension must not reshuffle.
        let mut p = PickerState::new(Settings::default(), vec!["150".into()], true);
        many_devices(&mut p, 200);
        assert_eq!(p.rows()[0].name, "dev-150", "the paired one leads");

        for _ in 0..60 {
            p.on_key(KeyCode::Down);
        }
        assert_eq!(p.rows()[0].name, "dev-150", "and still leads after growing");
        let names: Vec<&str> = p.rows()[1..4].iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["dev-000", "dev-001", "dev-002"]);
    }

    #[test]
    fn a_device_arriving_below_the_cursor_does_not_shrink_the_window() {
        let mut p = picker();
        many_devices(&mut p, 200);
        for _ in 0..45 {
            p.on_key(KeyCode::Down);
        }
        let grown = p.rows().len();
        p.insert(device("aaa-first", "192.168.200.1", "aaa", TRANSIENT));
        assert!(p.rows().len() >= grown, "the window only ever grows");
    }

    #[test]
    fn a_small_network_is_unaffected() {
        let mut p = picker();
        many_devices(&mut p, 3);
        assert_eq!(p.rows().len(), 3);
        assert_eq!(p.total(), 3);
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
    fn an_unpaired_device_can_now_be_confirmed() {
        // Pairing happens inside the flow, after this point, so the picker no
        // longer refuses a device that needs a PIN.
        let mut p = picker();
        p.insert(device("Living Room", "192.168.1.106", "1", NEEDS_PAIRING));
        p.on_key(KeyCode::Char(' '));
        assert_eq!(p.on_key(KeyCode::Enter), PickerAction::Start);
        assert!(
            p.chosen()[0].needs_pairing,
            "still flagged, so the flow knows to pair it"
        );
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
    fn a_prior_selection_is_restored_as_devices_reappear() {
        // After a failed connect the user is sent back here. The devices have
        // not been rediscovered yet, so the selection has to survive as keys
        // and reattach when they announce again.
        let mut p = PickerState::with_selection(
            Settings::default(),
            Vec::new(),
            true,
            vec!["1".to_string()],
        );
        assert!(p.chosen().is_empty(), "nothing discovered yet");

        p.insert(device("Pool Room", "192.168.1.51", "1", TRANSIENT));
        assert_eq!(p.chosen().len(), 1);
        assert_eq!(p.chosen()[0].name, "Pool Room");
    }

    #[test]
    fn selection_keys_include_devices_not_currently_listed() {
        let p = PickerState::with_selection(
            Settings::default(),
            Vec::new(),
            true,
            vec!["1".to_string(), "2".to_string()],
        );
        let mut keys = p.selection_keys();
        keys.sort();
        assert_eq!(keys, ["1", "2"]);
    }

    #[test]
    fn the_banner_clears_on_the_next_keystroke() {
        let mut p = picker();
        p.set_banner("nothing connected");
        assert!(p.banner().is_some());
        p.on_key(KeyCode::Down);
        assert!(p.banner().is_none(), "a stale explanation becomes furniture");
    }

    #[test]
    fn quit_keys_report_quit() {
        let mut p = picker();
        assert_eq!(p.on_key(KeyCode::Char('q')), PickerAction::Quit);
        assert_eq!(p.on_key(KeyCode::Esc), PickerAction::Quit);
    }
}
