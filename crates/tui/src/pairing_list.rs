//! The stored-pairings list: state and key handling, no rendering and no
//! filesystem.
//!
//! Split from [`crate::pairing_list_ui`] the way every other screen here is.
//! The filesystem half is somebody else's job too: this reports that the user
//! asked to forget a receiver, and is told afterwards whether it happened. That
//! makes "the store was read-only and the row says why" a unit test rather than
//! an exercise in file permissions.
//!
//! Not to be confused with [`crate::pairing`], which is the PIN entry screen.
//! That one *creates* a pairing; this one manages the ones we already have.

use crossterm::event::KeyCode;
use openair_client::PairedPeer;

/// What the caller should do about a keypress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListAction {
    /// Redraw; nothing else.
    None,
    /// Go back to the settings rows.
    Close,
    /// Forget this device id, then report back with [`PairingList::forgotten`]
    /// or [`PairingList::set_error`].
    Forget(String),
}

pub struct PairingList {
    peers: Vec<PairedPeer>,
    cursor: usize,
    /// The row armed for deletion, if any. A second press on the same row
    /// confirms it.
    ///
    /// Held as a device id rather than an index so a list that changes
    /// underneath cannot leave the arming pointed at a different receiver —
    /// the failure mode there is forgetting the wrong one, silently.
    armed: Option<String>,
    status: Option<String>,
}

impl PairingList {
    pub fn new(peers: Vec<PairedPeer>) -> Self {
        Self {
            peers,
            cursor: 0,
            armed: None,
            status: None,
        }
    }

    pub fn peers(&self) -> &[PairedPeer] {
        &self.peers
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }

    /// The row waiting for a second press, if any.
    pub fn armed(&self) -> Option<&str> {
        self.armed.as_deref()
    }

    /// A line to show under the list: what just happened, or what went wrong.
    pub fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }

    pub fn set_error(&mut self, msg: impl Into<String>) {
        self.armed = None;
        self.status = Some(msg.into());
    }

    /// The forget succeeded: drop the row and say so.
    pub fn forgotten(&mut self, device_id: &str) {
        let label = self
            .peers
            .iter()
            .find(|p| p.device_id == device_id)
            .map(|p| p.label().to_string());
        self.peers.retain(|p| p.device_id != device_id);
        self.armed = None;
        self.cursor = self.cursor.min(self.peers.len().saturating_sub(1));
        if let Some(label) = label {
            self.status = Some(format!("forgot {label} — it will ask for a PIN again"));
        }
    }

    pub fn on_key(&mut self, key: KeyCode) -> ListAction {
        match key {
            KeyCode::Esc | KeyCode::Left => ListAction::Close,
            KeyCode::Up => {
                self.disarm();
                self.cursor = self.cursor.saturating_sub(1);
                ListAction::None
            }
            KeyCode::Down => {
                self.disarm();
                if self.cursor + 1 < self.peers.len() {
                    self.cursor += 1;
                }
                ListAction::None
            }
            KeyCode::Char('d') | KeyCode::Delete => self.forget_selected(),
            _ => {
                self.disarm();
                ListAction::None
            }
        }
    }

    /// Clear both the arming and the message it belongs to.
    ///
    /// They go together: leaving "press d again" on screen after the arming has
    /// lapsed is an invitation to press `d` and be surprised.
    fn disarm(&mut self) {
        self.armed = None;
        self.status = None;
    }

    /// First press arms, second press asks for it to be done.
    ///
    /// A confirmation step rather than a dialog: forgetting is destructive but
    /// recoverable — the cost of getting it wrong is pairing again — so it
    /// wants a moment's friction, not a modal.
    fn forget_selected(&mut self) -> ListAction {
        let Some(peer) = self.peers.get(self.cursor) else {
            return ListAction::None;
        };
        let id = peer.device_id.clone();
        if self.armed.as_deref() == Some(id.as_str()) {
            return ListAction::Forget(id);
        }
        self.status = Some(format!("press d again to forget {}", peer.label()));
        self.armed = Some(id);
        ListAction::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(id: &str, name: Option<&str>) -> PairedPeer {
        PairedPeer {
            device_id: id.to_string(),
            name: name.map(str::to_string),
        }
    }

    fn list() -> PairingList {
        PairingList::new(vec![
            peer("AA:AA", Some("Living Room")),
            peer("BB:BB", Some("Pool Room")),
            peer("CC:CC", None),
        ])
    }

    #[test]
    fn the_cursor_stops_at_both_ends() {
        let mut l = list();
        l.on_key(KeyCode::Up);
        assert_eq!(l.cursor(), 0);
        for _ in 0..50 {
            l.on_key(KeyCode::Down);
        }
        assert_eq!(l.cursor(), 2);
    }

    #[test]
    fn one_press_arms_and_the_second_forgets() {
        let mut l = list();
        assert_eq!(l.on_key(KeyCode::Char('d')), ListAction::None);
        assert_eq!(l.armed(), Some("AA:AA"));
        assert!(
            l.status().unwrap().contains("Living Room"),
            "{:?}",
            l.status()
        );

        assert_eq!(
            l.on_key(KeyCode::Char('d')),
            ListAction::Forget("AA:AA".into())
        );
    }

    #[test]
    fn moving_away_disarms() {
        // Otherwise `d`, arrow, `d` would forget a receiver the user never
        // confirmed -- and the second `d` looks like a first press to them.
        let mut l = list();
        l.on_key(KeyCode::Char('d'));
        l.on_key(KeyCode::Down);
        assert_eq!(l.armed(), None);
        assert_eq!(l.status(), None, "the prompt goes with the arming");
        assert_eq!(l.on_key(KeyCode::Char('d')), ListAction::None);
        assert_eq!(l.armed(), Some("BB:BB"), "armed the row we moved to");
    }

    #[test]
    fn an_unrelated_keypress_disarms() {
        let mut l = list();
        l.on_key(KeyCode::Char('d'));
        l.on_key(KeyCode::Char('x'));
        assert_eq!(l.armed(), None);
    }

    #[test]
    fn delete_works_as_well_as_d() {
        let mut l = list();
        l.on_key(KeyCode::Delete);
        assert_eq!(
            l.on_key(KeyCode::Delete),
            ListAction::Forget("AA:AA".into())
        );
    }

    #[test]
    fn forgetting_removes_the_row_and_says_which() {
        let mut l = list();
        l.forgotten("BB:BB");
        assert_eq!(l.peers().len(), 2);
        assert!(l.peers().iter().all(|p| p.device_id != "BB:BB"));
        assert!(
            l.status().unwrap().contains("Pool Room"),
            "{:?}",
            l.status()
        );
        assert_eq!(l.armed(), None);
    }

    #[test]
    fn forgetting_the_last_row_pulls_the_cursor_back() {
        // The cursor indexes the list, so leaving it past the end would panic
        // the renderer or, worse, arm nothing while looking armed.
        let mut l = list();
        for _ in 0..2 {
            l.on_key(KeyCode::Down);
        }
        assert_eq!(l.cursor(), 2);
        l.forgotten("CC:CC");
        assert_eq!(l.cursor(), 1);
    }

    #[test]
    fn forgetting_everything_leaves_a_usable_screen() {
        let mut l = list();
        for id in ["AA:AA", "BB:BB", "CC:CC"] {
            l.forgotten(id);
        }
        assert!(l.is_empty());
        assert_eq!(l.cursor(), 0);
        // Nothing to forget, and no panic for trying.
        assert_eq!(l.on_key(KeyCode::Char('d')), ListAction::None);
        assert_eq!(l.on_key(KeyCode::Esc), ListAction::Close);
    }

    #[test]
    fn an_empty_list_is_not_a_special_case() {
        let mut l = PairingList::new(Vec::new());
        assert!(l.is_empty());
        assert_eq!(l.on_key(KeyCode::Down), ListAction::None);
        assert_eq!(l.on_key(KeyCode::Char('d')), ListAction::None);
    }

    #[test]
    fn a_failed_forget_leaves_the_row_and_explains() {
        let mut l = list();
        l.on_key(KeyCode::Char('d'));
        l.on_key(KeyCode::Char('d'));
        l.set_error("pairings.json is read-only");
        assert_eq!(l.peers().len(), 3, "nothing was removed");
        assert_eq!(l.armed(), None, "and it is not still armed");
        assert!(l.status().unwrap().contains("read-only"));
    }

    #[test]
    fn esc_and_left_go_back() {
        let mut l = list();
        assert_eq!(l.on_key(KeyCode::Esc), ListAction::Close);
        assert_eq!(l.on_key(KeyCode::Left), ListAction::Close);
    }

    #[test]
    fn the_arming_follows_the_receiver_not_the_row_number() {
        // If the list shifts under us -- a forget from anywhere -- an armed
        // *index* would end up pointing at whichever receiver moved into that
        // slot, and the next `d` would silently forget the wrong one.
        let mut l = list();
        l.on_key(KeyCode::Down);
        l.on_key(KeyCode::Char('d'));
        assert_eq!(l.armed(), Some("BB:BB"));

        l.forgotten("AA:AA"); // everything shifts up one
        assert_eq!(l.armed(), None, "a shifted list cannot stay armed");
    }
}
