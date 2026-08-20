//! In-TUI HomeKit pairing: the PIN prompt that used to be a `stdin` read.
//!
//! [`PairingState`] is pure — a queue of devices, a PIN buffer and a phase —
//! so the whole flow is testable without a terminal or a network. The thread
//! that actually talks to the receiver lives in [`PairWorker`], which exists
//! because `pair_device` blocks: it calls a closure to obtain the PIN, and that
//! closure has to wait for a human.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};

use crossterm::event::KeyCode;

/// AirPlay PINs are four digits.
pub const PIN_LEN: usize = 4;

/// How many wrong PINs before we give up on a device.
///
/// The receiver generates a *new* PIN on every attempt, so a wrong entry is
/// nearly always a typo or a stale reading of the screen rather than a guess
/// at a secret — but an unbounded retry loop with a live socket is not
/// something to leave running either.
pub const MAX_ATTEMPTS: u32 = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPair {
    pub name: String,
    pub addr: SocketAddr,
    pub device_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairPhase {
    /// Waiting for the user to type a PIN.
    AwaitingPin,
    /// PIN sent; waiting on the receiver.
    Verifying,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairAction {
    None,
    /// Send this PIN to the receiver.
    Submit(String),
    /// Give up on the current device and move to the next.
    Skip,
    /// Abandon pairing entirely.
    Cancel,
}

/// What the queue produced once every device has been dealt with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingOutcome {
    /// Devices now paired, in the order they were handled.
    pub paired: Vec<PendingPair>,
    /// Devices skipped or exhausted, with why.
    pub failed: Vec<(PendingPair, String)>,
}

pub struct PairingState {
    queue: VecDeque<PendingPair>,
    current: Option<PendingPair>,
    pin: String,
    attempts_left: u32,
    phase: PairPhase,
    error: Option<String>,
    paired: Vec<PendingPair>,
    failed: Vec<(PendingPair, String)>,
}

impl PairingState {
    pub fn new(devices: Vec<PendingPair>) -> Self {
        let mut queue: VecDeque<PendingPair> = devices.into();
        let current = queue.pop_front();
        Self {
            queue,
            current,
            pin: String::new(),
            attempts_left: MAX_ATTEMPTS,
            phase: PairPhase::AwaitingPin,
            error: None,
            paired: Vec::new(),
            failed: Vec::new(),
        }
    }

    pub fn current(&self) -> Option<&PendingPair> {
        self.current.as_ref()
    }

    pub fn pin(&self) -> &str {
        &self.pin
    }

    pub fn phase(&self) -> PairPhase {
        self.phase
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn attempts_left(&self) -> u32 {
        self.attempts_left
    }

    /// How many devices are still queued behind the current one.
    pub fn remaining(&self) -> usize {
        self.queue.len()
    }

    /// `Some` once every device has been paired, skipped or exhausted.
    pub fn outcome(&self) -> Option<PairingOutcome> {
        if self.current.is_some() {
            return None;
        }
        Some(PairingOutcome {
            paired: self.paired.clone(),
            failed: self.failed.clone(),
        })
    }

    pub fn on_key(&mut self, key: KeyCode) -> PairAction {
        // While the receiver is deciding, the only useful key is escape.
        if self.phase == PairPhase::Verifying {
            return match key {
                KeyCode::Esc => PairAction::Cancel,
                _ => PairAction::None,
            };
        }

        match key {
            KeyCode::Char(c) if c.is_ascii_digit() => {
                if self.pin.len() < PIN_LEN {
                    self.pin.push(c);
                    self.error = None;
                }
                // Submitting on the fourth digit saves a keystroke, and there
                // is nothing else a complete PIN could be waiting for.
                if self.pin.len() == PIN_LEN {
                    self.phase = PairPhase::Verifying;
                    return PairAction::Submit(self.pin.clone());
                }
                PairAction::None
            }
            KeyCode::Backspace => {
                self.pin.pop();
                self.error = None;
                PairAction::None
            }
            KeyCode::Enter if self.pin.len() == PIN_LEN => {
                self.phase = PairPhase::Verifying;
                PairAction::Submit(self.pin.clone())
            }
            // Skip this device, not the whole flow: one un-pairable speaker
            // should not cost the user the rest of the group.
            KeyCode::Esc => PairAction::Skip,
            _ => PairAction::None,
        }
    }

    /// Record the receiver's verdict and advance if we are done with this one.
    pub fn on_result(&mut self, result: Result<(), String>) {
        let Some(current) = self.current.clone() else {
            return;
        };
        match result {
            Ok(()) => {
                self.paired.push(current);
                self.advance();
            }
            Err(why) => {
                self.attempts_left = self.attempts_left.saturating_sub(1);
                self.pin.clear();
                self.phase = PairPhase::AwaitingPin;
                if self.attempts_left == 0 {
                    self.failed.push((current, why));
                    self.advance();
                } else {
                    // The receiver shows a new PIN after a failed attempt, so
                    // say so — retyping the old one will fail identically.
                    self.error = Some(format!("{why} — check the screen for a new PIN"));
                }
            }
        }
    }

    /// Give up on the current device at the user's request.
    pub fn skip_current(&mut self) {
        if let Some(current) = self.current.clone() {
            self.failed.push((current, "skipped".to_string()));
            self.advance();
        }
    }

    fn advance(&mut self) {
        self.current = self.queue.pop_front();
        self.pin.clear();
        self.attempts_left = MAX_ATTEMPTS;
        self.phase = PairPhase::AwaitingPin;
        self.error = None;
    }
}

/// The thread doing the actual pairing for one device.
///
/// `pair_device` blocks waiting for a PIN, so it cannot run on the UI thread.
/// One worker handles one *attempt*: a rejected PIN ends the exchange, and the
/// receiver issues a fresh PIN, so a retry is a new worker rather than a
/// resumed one.
pub struct PairWorker {
    pin_tx: Sender<String>,
    result_rx: Receiver<Result<(), String>>,
}

impl PairWorker {
    pub fn spawn(addr: SocketAddr, device_id: String) -> Self {
        let (pin_tx, pin_rx) = mpsc::channel::<String>();
        let (result_tx, result_rx) = mpsc::channel();

        std::thread::spawn(move || {
            let mut provider = || pin_rx.recv().unwrap_or_default();
            let result = openair_client::pair_device(addr, &device_id, &mut provider)
                .map_err(|e| e.to_string());
            let _ = result_tx.send(result);
        });

        Self { pin_tx, result_rx }
    }

    /// Hand the typed PIN to the waiting thread.
    pub fn submit(&self, pin: String) {
        let _ = self.pin_tx.send(pin);
    }

    /// The verdict, once there is one.
    pub fn poll(&self) -> Option<Result<(), String>> {
        match self.result_rx.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            // The thread died without reporting; treat as a failure rather
            // than waiting forever for a message that will never come.
            Err(TryRecvError::Disconnected) => {
                Some(Err("pairing thread stopped unexpectedly".to_string()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(name: &str, n: u8) -> PendingPair {
        PendingPair {
            name: name.into(),
            addr: format!("192.168.1.{n}:7000").parse().unwrap(),
            device_id: format!("AA:BB:{n}"),
        }
    }

    fn one() -> PairingState {
        PairingState::new(vec![device("Living Room", 51)])
    }

    #[test]
    fn digits_accumulate() {
        let mut s = one();
        s.on_key(KeyCode::Char('1'));
        s.on_key(KeyCode::Char('2'));
        assert_eq!(s.pin(), "12");
    }

    #[test]
    fn non_digits_are_ignored() {
        let mut s = one();
        s.on_key(KeyCode::Char('a'));
        s.on_key(KeyCode::Char('-'));
        assert_eq!(s.pin(), "");
    }

    #[test]
    fn backspace_deletes() {
        let mut s = one();
        s.on_key(KeyCode::Char('1'));
        s.on_key(KeyCode::Char('2'));
        s.on_key(KeyCode::Backspace);
        assert_eq!(s.pin(), "1");
        // Backspacing an empty field must not panic.
        s.on_key(KeyCode::Backspace);
        s.on_key(KeyCode::Backspace);
        assert_eq!(s.pin(), "");
    }

    #[test]
    fn the_fourth_digit_submits() {
        let mut s = one();
        for c in ['1', '2', '3'] {
            assert_eq!(s.on_key(KeyCode::Char(c)), PairAction::None);
        }
        assert_eq!(
            s.on_key(KeyCode::Char('4')),
            PairAction::Submit("1234".into())
        );
        assert_eq!(s.phase(), PairPhase::Verifying);
    }

    #[test]
    fn a_fifth_digit_is_ignored() {
        let mut s = one();
        for c in ['1', '2', '3', '4'] {
            s.on_key(KeyCode::Char(c));
        }
        s.on_result(Err("nope".into()));
        for c in ['1', '2', '3', '4'] {
            s.on_key(KeyCode::Char(c));
        }
        // Back in Verifying after the fourth; further digits do nothing.
        s.on_key(KeyCode::Char('5'));
        assert_eq!(s.pin(), "1234");
    }

    #[test]
    fn keys_do_nothing_while_verifying() {
        let mut s = one();
        for c in ['1', '2', '3', '4'] {
            s.on_key(KeyCode::Char(c));
        }
        assert_eq!(s.phase(), PairPhase::Verifying);
        assert_eq!(s.on_key(KeyCode::Char('9')), PairAction::None);
        assert_eq!(s.pin(), "1234", "the field is frozen while we wait");
    }

    #[test]
    fn escape_while_verifying_cancels_the_whole_flow() {
        // There is a live socket mid-handshake; skipping to the next device
        // while this one is still talking would leave it dangling.
        let mut s = one();
        for c in ['1', '2', '3', '4'] {
            s.on_key(KeyCode::Char(c));
        }
        assert_eq!(s.on_key(KeyCode::Esc), PairAction::Cancel);
    }

    #[test]
    fn a_rejected_pin_clears_the_field_and_costs_an_attempt() {
        let mut s = one();
        for c in ['1', '2', '3', '4'] {
            s.on_key(KeyCode::Char(c));
        }
        s.on_result(Err("incorrect PIN".into()));

        assert_eq!(s.pin(), "");
        assert_eq!(s.attempts_left(), MAX_ATTEMPTS - 1);
        assert_eq!(s.phase(), PairPhase::AwaitingPin);
        // The receiver shows a new PIN each attempt; the message has to say so
        // or the user retypes the one they can still see on their notepad.
        assert!(s.error().unwrap().contains("new PIN"), "{:?}", s.error());
    }

    #[test]
    fn typing_clears_the_previous_error() {
        let mut s = one();
        for c in ['1', '2', '3', '4'] {
            s.on_key(KeyCode::Char(c));
        }
        s.on_result(Err("incorrect PIN".into()));
        assert!(s.error().is_some());
        s.on_key(KeyCode::Char('9'));
        assert!(s.error().is_none());
    }

    #[test]
    fn exhausting_attempts_moves_on() {
        let mut s = PairingState::new(vec![device("Living Room", 51), device("Pool Room", 52)]);
        for _ in 0..MAX_ATTEMPTS {
            for c in ['1', '2', '3', '4'] {
                s.on_key(KeyCode::Char(c));
            }
            s.on_result(Err("incorrect PIN".into()));
        }
        assert_eq!(s.current().unwrap().name, "Pool Room");
        assert_eq!(s.attempts_left(), MAX_ATTEMPTS, "a fresh device, fresh tries");
    }

    #[test]
    fn success_moves_to_the_next_device() {
        let mut s = PairingState::new(vec![device("Living Room", 51), device("Pool Room", 52)]);
        assert_eq!(s.current().unwrap().name, "Living Room");
        assert_eq!(s.remaining(), 1);

        for c in ['1', '2', '3', '4'] {
            s.on_key(KeyCode::Char(c));
        }
        s.on_result(Ok(()));
        assert_eq!(s.current().unwrap().name, "Pool Room");
        assert_eq!(s.pin(), "", "the next device starts clean");
    }

    #[test]
    fn skipping_advances_rather_than_aborting() {
        // One un-pairable speaker must not cost the user the rest of the group.
        let mut s = PairingState::new(vec![device("Living Room", 51), device("Pool Room", 52)]);
        assert_eq!(s.on_key(KeyCode::Esc), PairAction::Skip);
        s.skip_current();
        assert_eq!(s.current().unwrap().name, "Pool Room");
    }

    #[test]
    fn the_outcome_appears_only_once_everything_is_handled() {
        let mut s = PairingState::new(vec![device("Living Room", 51), device("Pool Room", 52)]);
        assert!(s.outcome().is_none());

        for c in ['1', '2', '3', '4'] {
            s.on_key(KeyCode::Char(c));
        }
        s.on_result(Ok(()));
        assert!(s.outcome().is_none(), "one device still queued");

        s.skip_current();
        let outcome = s.outcome().expect("everything handled");
        assert_eq!(outcome.paired.len(), 1);
        assert_eq!(outcome.paired[0].name, "Living Room");
        assert_eq!(outcome.failed.len(), 1);
        assert_eq!(outcome.failed[0].0.name, "Pool Room");
        assert_eq!(outcome.failed[0].1, "skipped");
    }

    #[test]
    fn an_empty_queue_is_immediately_done() {
        let s = PairingState::new(Vec::new());
        let outcome = s.outcome().expect("nothing to do");
        assert!(outcome.paired.is_empty());
        assert!(outcome.failed.is_empty());
    }

    #[test]
    fn a_result_with_no_current_device_is_ignored() {
        let mut s = PairingState::new(Vec::new());
        s.on_result(Ok(()));
        assert!(s.outcome().unwrap().paired.is_empty());
    }
}
