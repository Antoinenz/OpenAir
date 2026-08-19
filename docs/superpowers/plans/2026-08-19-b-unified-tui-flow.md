# B — Unified TUI Flow: Implementation Plan

**Spec:** `docs/superpowers/specs/2026-08-19-tui-unified-flow-design.md` — read it
first; this plan argues from it and does not restate the design.

**Goal:** One continuous terminal application from launch to exit — picker,
pairing, connecting and streaming as screens, never dropping to a shell.

**Architecture:** The TUI takes the main thread for the program's whole life;
discovery, pairing and streaming all run on workers. Connection progress reaches
the UI through the existing `Arc<StreamStats>` snapshot rather than by moving
connection out of the stream.

**Tech stack:** ratatui + crossterm, existing `openair-client` seams.

## Global constraints

- No `Co-Authored-By` or Claude attribution in commit messages.
- Many small focused commits; commit as soon as each piece builds and tests pass.
- `cargo clippy --workspace --all-targets -- -D warnings` clean; `cargo test`
  green.
- Work in `C:\Users\antoi\OpenAir`.
- GPG signing needs the user present. If signing times out, stop and say so.
- A failure in the UI layer must never take the stream down.

---

## Task 1 — `Connecting` and `Failed` receiver states

**Files:** `crates/client/src/stats.rs`, `crates/client/src/lib.rs`

Add to `ReceiverState`: `Connecting`, `Failed`. Add `error: Option<String>` to
`ReceiverStat`. Update `label()`.

In `stream_audio_buffered_multi`'s setup loop, publish before and after each
receiver's `setup`:

- before: a `ReceiverStat` with `state: Connecting`
- on success: `Connected`
- on failure: `Failed` with `error` set to the `connection_hint` when there is
  one, else the error's `to_string()`

The loop currently builds `group` then calls `receiver_stats` later; it needs a
`Vec<ReceiverStat>` maintained alongside so a partially-set-up group is
publishable. `receiver_stats` must merge in any `Failed` entries so they survive
subsequent snapshots.

**Tests:** `label()` covers the new variants; a `ReceiverStat` with an error
reports it. Merge behaviour: a `Failed` entry is not lost when `receiver_stats`
rebuilds from a group that never contained it.

**Commit:** `feat(client): publish per-receiver connect progress`

---

## Task 2 — `App` shell owning one terminal session

**Files:** create `crates/tui/src/app.rs`; modify `crates/tui/src/lib.rs`,
`apps/cli/src/main.rs`

The structural task. Behaviour must be **unchanged** at the end of it — picker
then dashboard — but through one `App` and one alternate-screen session.

**Produces:**
```rust
pub enum Screen { Picker(PickerState), Connecting(ConnectingState),
                  Pairing(PairingState), Streaming(DashboardState) }

pub struct App { /* screen, terminal, logs, settings, stream handle */ }

impl App {
    pub fn new(settings: Settings, logs: LogBuffer, start: StartAt) -> io::Result<Self>;
    pub fn run(&mut self) -> io::Result<Option<Summary>>;
}

pub enum StartAt { Picker, Receivers(Vec<GroupTarget>) }
```

The run loop is the one from `run_dashboard`, generalised: sample on a clock,
draw every iteration, `poll_timeout` for the rest of the tick. Screens that need
no sampling simply skip it.

Threading inverts here: `main` calls `App::run` and the stream is spawned onto a
worker by the App. `AudioSource` must become `Send` — inspection says
`CaptureSource` and the others already are, so expect this to be a
`Box<dyn AudioSource + Send>` signature change and nothing more. The
`HandoffSession` guard and the `SystemCapture` handle stay on the main thread.

In this task `Connecting` and `Pairing` may be stubs that fall straight through;
they are filled in by Tasks 3 and 4.

**Tests:** a transition table — `Picker` + confirm → next screen; `Streaming` +
quit → exit. Assert on `Screen` discriminants, no terminal involved.

**Commit:** `refactor(tui): one App owning every screen` (+ a separate
`refactor(client): AudioSource is Send` if that turns out non-trivial)

---

## Task 3 — Connecting screen

**Files:** create `crates/tui/src/connecting.rs`; modify `app.rs`

**Produces:**
```rust
pub struct ConnectingState { /* receivers, spinner frame, started_at */ }
impl ConnectingState {
    pub fn sample(&mut self, stats: &StreamStats);
    pub fn on_key(&mut self, key: KeyCode) -> ConnectAction;  // Esc -> Cancel
    pub fn outcome(&self) -> ConnectOutcome;
}
pub enum ConnectOutcome { Waiting, Ready, AllFailed }
```

`outcome` is a pure function of the receiver list: `Waiting` while any is
`Connecting`; `AllFailed` if none reached `Connected`; else `Ready`. Spinner
advances on the render tick, not per iteration, or it spins at keyboard speed —
the same bug fixed in `poll_timeout`.

`Esc` sets the stream's `stop` flag and returns to `Picker`.

**Tests (write first):** all three `outcome` cases, including the boundary where
one receiver is connected and another still connecting (must be `Waiting`, not
`Ready` — starting early would strand the second one). Spinner advances only on
tick.

**Commit:** `feat(tui): connecting screen`

---

## Task 4 — Pairing screen

**Files:** create `crates/tui/src/pairing.rs`; modify `app.rs`

`openair_client::pair_device(addr, device_id, &mut pin_prompt)` takes a closure
that returns the PIN. On a worker, that closure blocks on a channel receive; the
TUI sends the PIN when the user submits. No change to the pairing crate.

**Produces:**
```rust
pub struct PairingState { /* queue of devices, current, pin buffer, attempts */ }
impl PairingState {
    pub fn on_key(&mut self, key: KeyCode) -> PairAction;
    pub fn on_result(&mut self, result: Result<(), String>);
    pub fn current(&self) -> Option<&PendingPair>;
}
pub enum PairAction { None, Submit(String), Skip, Cancel }
```

PIN field: digits accumulate, backspace deletes, non-digits ignored, submits at
four. A rejected PIN clears the field and decrements attempts — and the message
must say to re-read the device's screen, because the receiver generates a new
PIN on each attempt.

`Esc` skips **this device only**, marking it failed; the queue advances.

**Tests (write first):** digits accumulate and cap at four; backspace; non-digit
ignored; submit emits `Submit` with the typed PIN; rejection clears the field
and decrements attempts; `Esc` advances the queue rather than emptying it.

**Commit:** `feat(tui): in-TUI pairing with PIN entry`

---

## Task 5 — Failed receivers in the dashboard

**Files:** `crates/tui/src/dashboard.rs`, `crates/tui/src/dashboard_ui.rs`

Render `Failed` rows in red with their `error`. Add `r` — retry the selected
receiver, which is the existing `StreamCommand::Add`. `d` already removes.

Ensure a `Failed` row is selectable: the cursor currently walks whatever is in
`receivers`, so this should follow for free — assert it.

**Tests:** `r` on a failed receiver emits `Add` with its address and device id;
`r` on a connected one is a no-op; a failed row is reachable with the arrows.

**Commit:** `feat(tui): retry failed receivers from the dashboard`

---

## Task 6 — All-failed returns to the picker

**Files:** `crates/tui/src/app.rs`, `crates/tui/src/picker.rs`

On `ConnectOutcome::AllFailed`, return to `Picker` carrying a one-line banner
and **the previous selection intact**, so the user retries without re-picking.
`PickerState` gains `set_banner(String)` and the ability to be constructed with
a pre-selected set of keys.

**Tests:** a picker rebuilt with a prior selection reports those rows selected
once the devices reappear from discovery; the banner clears on the next
keystroke, like `hint`.

**Commit:** `feat(tui): return to the picker when nothing connects`

---

## Task 7 — CLI entry-screen selection

**Files:** `apps/cli/src/main.rs`

Replace the picker/named-receivers branch with a choice of `StartAt`. Named
receivers resolve as they do today and start at `Connecting`; bare `openair`
starts at `Picker`. `--no-tui` and non-TTY keep the existing plain path
untouched.

Delete the now-dead `PICKER_SELECTION` sentinel and the `picked.take()` hook —
the App owns this now.

**Tests:** existing CLI arg tests stay green; add one asserting `--no-tui` still
routes away from the App.

**Commit:** `feat(cli): choose the TUI entry screen instead of branching`

---

## Task 8 — Docs

**Files:** `README.md`, `STATUS.md`, `DEVLOG.md`

README: pairing no longer needs a separate command for the common case; describe
the flow. STATUS: `tui` crate row, and resolve the "dashboard is capture-only"
note if Task 7 covered it. DEVLOG: why progress rides `StreamStats` rather than
moving connection out, and the threading inversion and what forced it.

**Commit:** `docs: unified TUI flow`

---

## Self-review notes

- Spec coverage: states (1), shell (2), connecting (3), pairing (4), failures
  (5), all-failed (6), CLI (7), docs (8).
- Ordering: Task 1 is independent and testable alone. Task 2 is the risky one
  and comes early so `Send` problems surface before anything is built on top.
  Tasks 3–6 each add one screen behaviour and can be reviewed separately.
- Watch for: `receiver_stats` rebuilding from `group` and silently dropping
  `Failed` entries (Task 1), and the spinner advancing per loop iteration rather
  than per tick (Task 3).
