# D — Settings Page Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A settings overlay in the TUI, reachable from the picker and the
dashboard, where every setting that can apply to a running stream does.

**Architecture:** The TUI owns a pure `SettingsScreen` state object and an
overlay renderer. Platform work it cannot reach (switching a Windows audio
endpoint) goes through a `SettingsApplier` closure supplied by the CLI, matching
the existing `StreamLauncher` idiom. Stream-side changes (latency, master
volume, metadata gating) ride the existing `StreamCommand` inbox. Live device
switching is made safe by publishing the capture sample rate through an
`Arc<AtomicU32>` the source re-reads, which also fixes a latent `--handoff` bug.

**Tech Stack:** Rust workspace, ratatui + crossterm (TUI), cpal (capture),
`std::sync::atomic`, serde_json (settings persistence).

**Spec:** `docs/superpowers/specs/2026-08-21-tui-settings-page-design.md`

## Global Constraints

- Canonical working directory is `C:\Users\antoi\OpenAir`. Not the Omnara
  worktree.
- Commit messages must not contain `Co-Authored-By: Claude` or any Claude
  attribution.
- Many small, focused commits — one logical change each. Commit as soon as a
  piece builds and passes tests.
- Never pass `--no-gpg-sign` or `--no-verify`. GPG signing needs the user
  present; if signing fails, stop and say so.
- `cargo clippy --workspace --all-targets` must report zero warnings before
  every commit.
- `openair-tui` must **not** gain a dependency on `openair-capture`. That
  boundary is what keeps the TUI compiling and testable on any platform.
- `Settings::CURRENT_VERSION` stays at **2**. This project adds no persisted
  fields.
- Latency bounds: `LATENCY_MIN_MS = 100`, `LATENCY_MAX_MS = 2000`,
  `LATENCY_STEP_MS = 50` (from `crates/tui/src/settings.rs`).
- Volume bounds: −60.0 to 0.0 dB, 1 dB steps.
- Pipeline sample rate is `openair_client::SAMPLE_RATE` = 44100.

---

### Task 1: Live capture sample rate

Fixes a latent bug on its own: today `--handoff` builds `CaptureSource` with
whatever rate the device had at start, and nothing verifies the virtual cable
runs at the same rate. A mismatch is a **pitch shift**, not a glitch. This task
is worth shipping even if the rest of the project is abandoned.

**Files:**
- Modify: `crates/client/src/source.rs` (`LinearResampler`, `CaptureSource`)
- Test: `crates/client/src/source.rs` (inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:
  - `LinearResampler::set_rate(&mut self, src_rate: u32)`
  - `CaptureSource::new_with_rate(ring: Arc<Mutex<VecDeque<i16>>>, rate: Arc<AtomicU32>, max_seconds: Option<u32>, stop: Option<Arc<AtomicBool>>) -> Self`
  - `CaptureSource::new` keeps its existing signature (`device_rate: u32`) and
    delegates, so no existing caller changes.

- [ ] **Step 1: Write the failing test**

Add to the tests module in `crates/client/src/source.rs`:

```rust
#[test]
fn a_rate_change_is_picked_up_mid_stream() {
    // The --handoff hazard: the producer is swapped to a device running at a
    // different rate. A source that keeps resampling at the old ratio does not
    // glitch — it shifts pitch, which is easy to misdiagnose as a receiver
    // fault. So the ratio must follow the atomic.
    use std::sync::atomic::AtomicU32;

    let ring = Arc::new(Mutex::new(VecDeque::new()));
    let rate = Arc::new(AtomicU32::new(44_100));
    let mut src = CaptureSource::new_with_rate(
        Arc::clone(&ring),
        Arc::clone(&rate),
        None,
        None,
    );

    // 44100 -> 44100 is 1:1, so N source frames yield N output frames.
    push_frames(&ring, 44_100);
    let mut buf = vec![0i16; 2000];
    let at_parity = src.fill(&mut buf);
    assert!(at_parity > 0, "produced nothing at 1:1");

    // Double the source rate: two source frames now collapse into one output
    // frame, so the same buffer consumes roughly twice as much ring.
    rate.store(88_200, Ordering::Relaxed);
    let before = ring.lock().unwrap().len();
    let _ = src.fill(&mut buf);
    let consumed_fast = before - ring.lock().unwrap().len();

    rate.store(44_100, Ordering::Relaxed);
    let before = ring.lock().unwrap().len();
    let _ = src.fill(&mut buf);
    let consumed_slow = before - ring.lock().unwrap().len();

    assert!(
        consumed_fast > consumed_slow * 3 / 2,
        "the resample ratio did not follow the rate: {consumed_fast} vs {consumed_slow}"
    );
}

#[test]
fn an_unchanged_rate_does_not_reset_the_resampler() {
    // Reading the atomic every fill() must not be mistaken for a rate change,
    // which would re-prime the interpolation bracket on every call and
    // introduce a discontinuity per buffer.
    use std::sync::atomic::AtomicU32;

    let ring = Arc::new(Mutex::new(VecDeque::new()));
    let rate = Arc::new(AtomicU32::new(48_000));
    let mut src = CaptureSource::new_with_rate(ring.clone(), rate, None, None);
    push_frames(&ring, 48_000);

    let mut buf = vec![0i16; 512];
    for _ in 0..10 {
        src.fill(&mut buf);
    }
    assert_eq!(src.rate_changes(), 0, "no change was made, none should be seen");
}
```

Add this helper to the same tests module if it is not already present:

```rust
/// Push `frames` stereo frames of a simple ramp into `ring`.
fn push_frames(ring: &Arc<Mutex<VecDeque<i16>>>, frames: usize) {
    let mut g = ring.lock().unwrap();
    for i in 0..frames {
        let v = (i % 1000) as i16;
        g.push_back(v);
        g.push_back(v);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p openair-client a_rate_change_is_picked_up_mid_stream`
Expected: FAIL — `no function or associated item named 'new_with_rate' found`.

- [ ] **Step 3: Add `set_rate` to the resampler**

In `crates/client/src/source.rs`, add to `impl LinearResampler`:

```rust
    /// Change the source rate mid-stream, keeping the interpolation bracket
    /// and fractional position.
    ///
    /// Deliberately does **not** re-prime: `prev`/`next` are real samples that
    /// are still valid, and discarding them would put a discontinuity at every
    /// device change. Only the rate at which `src_pos` advances changes.
    pub(crate) fn set_rate(&mut self, src_rate: u32) {
        self.resample_ratio = f64::from(src_rate) / f64::from(SAMPLE_RATE);
    }
```

- [ ] **Step 4: Make `CaptureSource` read the rate from an atomic**

Change the struct field block in `crates/client/src/source.rs`:

```rust
pub struct CaptureSource {
    ring: Arc<Mutex<VecDeque<i16>>>,
    /// The rate the producer is currently capturing at.
    ///
    /// Shared rather than owned because `--handoff` can swap the capture
    /// device mid-stream, and the two devices need not run at the same rate.
    /// Read once per `fill()` into `device_rate`.
    rate_source: Arc<AtomicU32>,
    /// Cached copy of `rate_source`, so the many derived sizes below
    /// (prebuffer, drift high-water, drain target) stay plain arithmetic.
    device_rate: u32,
    /// Count of observed rate changes, for tests and diagnostics.
    rate_changes: u64,
    resampler: LinearResampler,
    // ... remaining fields unchanged ...
}
```

Add `use std::sync::atomic::AtomicU32;` to the imports at the top of the file
(the existing import line is `use std::sync::atomic::{AtomicBool, Ordering};` —
extend it to `use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};`).

Replace `CaptureSource::new` and add the new constructor:

```rust
    /// `ring`/`device_rate` come from `openair_capture::SystemCapture`.
    /// See [`CaptureSource::new_with_rate`] for the mid-stream-change variant.
    pub fn new(
        ring: Arc<Mutex<VecDeque<i16>>>,
        device_rate: u32,
        max_seconds: Option<u32>,
        stop: Option<Arc<AtomicBool>>,
    ) -> Self {
        Self::new_with_rate(
            ring,
            Arc::new(AtomicU32::new(device_rate)),
            max_seconds,
            stop,
        )
    }

    /// As [`CaptureSource::new`], but the capture rate is shared and may change
    /// while the stream runs — which is what happens when `--handoff` switches
    /// the capture device to a virtual cable running at a different rate.
    pub fn new_with_rate(
        ring: Arc<Mutex<VecDeque<i16>>>,
        rate: Arc<AtomicU32>,
        max_seconds: Option<u32>,
        stop: Option<Arc<AtomicBool>>,
    ) -> Self {
        let frames_remaining = max_seconds.map(|s| u64::from(s) * u64::from(SAMPLE_RATE));
        let device_rate = rate.load(Ordering::Relaxed);
        // The resampler needs an initial two-frame bracket, but the ring
        // may not have any data yet (capture just started) — prime with
        // silence; fill() waits for the real prebuffer before producing
        // output, and by the time it does, pull_ring_frame will be reading
        // live data anyway.
        let resampler = LinearResampler::new(device_rate, || Some([0, 0]));
        CaptureSource {
            ring,
            rate_source: rate,
            device_rate,
            rate_changes: 0,
            resampler,
            frames_remaining,
            prebuffer_done: false,
            stop,
            fills: 0,
            silent_frames: 0,
            blocking: false,
        }
    }

    /// How many times the capture rate has changed under this source.
    pub fn rate_changes(&self) -> u64 {
        self.rate_changes
    }

    /// Adopt a new capture rate if the producer has changed device.
    ///
    /// Compared against the cached value rather than applied unconditionally:
    /// re-priming on every `fill()` would put a discontinuity in every buffer.
    fn sync_rate(&mut self) {
        let current = self.rate_source.load(Ordering::Relaxed);
        if current == self.device_rate || current == 0 {
            return;
        }
        tracing::info!(
            from_hz = self.device_rate,
            to_hz = current,
            "capture device rate changed — following it"
        );
        self.device_rate = current;
        self.rate_changes += 1;
        self.resampler.set_rate(current);
    }
```

- [ ] **Step 5: Call `sync_rate` at the top of `fill()`**

In `impl AudioSource for CaptureSource`, make `sync_rate` the first statement of
`fill()`, before the `stop` check:

```rust
    fn fill(&mut self, buf: &mut [i16]) -> usize {
        // Before anything reads `device_rate` — the prebuffer and drift
        // thresholds below are all derived from it.
        self.sync_rate();
        // ... existing body unchanged ...
```

- [ ] **Step 6: Run the tests**

Run: `cargo test -p openair-client`
Expected: PASS, including the two new tests and every pre-existing source test.

- [ ] **Step 7: Clippy**

Run: `cargo clippy --workspace --all-targets`
Expected: zero warnings.

- [ ] **Step 8: Commit**

```bash
git add crates/client/src/source.rs
git commit -m "fix(client): follow the capture device's sample rate"
```

---

### Task 2: Capture into an existing ring

**Files:**
- Modify: `crates/capture/src/lib.rs`
- Test: `crates/capture/src/lib.rs` (inline tests)

**Interfaces:**
- Consumes: nothing from Task 1 (independent; ordered second only because it
  completes the same hazard).
- Produces:
  - `SystemCapture::start_on_ring(name_filter: Option<&str>, ring: Arc<Mutex<VecDeque<i16>>>) -> Result<Self, CaptureError>`
  - `SystemCapture::start_on` unchanged in signature, now delegating.

- [ ] **Step 1: Write the failing test**

Add to `crates/capture/src/lib.rs`:

```rust
#[cfg(test)]
mod ring_tests {
    use super::*;

    #[test]
    fn a_supplied_ring_is_the_one_capture_uses() {
        // Device-independent: proves the plumbing, not the audio. A real
        // capture needs hardware, so what is asserted here is that the ring
        // handed in is the ring the SystemCapture reports back — the property
        // a live device swap depends on.
        let ring: Arc<Mutex<VecDeque<i16>>> = Arc::new(Mutex::new(VecDeque::new()));
        ring.lock().unwrap().push_back(42);

        match SystemCapture::start_on_ring(Some("\u{0}no such device\u{0}"), Arc::clone(&ring)) {
            Ok(cap) => {
                assert!(
                    Arc::ptr_eq(&cap.ring, &ring),
                    "capture must write into the ring it was given, not a fresh one"
                );
            }
            Err(CaptureError::NoDevice) => {
                // No matching device, which is the expected outcome of the
                // deliberately impossible filter on a machine with real
                // hardware. The signature is what this test pins down.
            }
            Err(e) => panic!("unexpected error: {e}"),
        }
    }

    #[test]
    fn the_ring_is_preserved_across_a_notional_swap() {
        // The invariant the applier relies on: clearing and refilling the same
        // Arc is visible to a holder that never re-read the Arc.
        let ring: Arc<Mutex<VecDeque<i16>>> = Arc::new(Mutex::new(VecDeque::new()));
        let consumer = Arc::clone(&ring);
        ring.lock().unwrap().extend([1i16, 2, 3, 4]);

        ring.lock().unwrap().clear();
        ring.lock().unwrap().extend([9i16, 9]);

        assert_eq!(
            consumer.lock().unwrap().iter().copied().collect::<Vec<_>>(),
            vec![9, 9],
            "the consumer's handle sees the swap"
        );
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p openair-capture a_supplied_ring_is_the_one_capture_uses`
Expected: FAIL — `no function or associated item named 'start_on_ring'`.

- [ ] **Step 3: Implement `start_on_ring`**

In `crates/capture/src/lib.rs`, change `start_on` to delegate and add the new
function. The existing body of `start_on` moves into `start_on_ring`, with the
single change that the ring is taken as a parameter instead of being allocated:

```rust
    /// Start loopback capture of a specific output device, selected by
    /// case-insensitive substring of its name; `None` uses the default output.
    ///
    /// Used by `--handoff`, which routes system audio to a virtual cable and
    /// then captures from that cable explicitly rather than assuming the
    /// default-device switch took effect.
    pub fn start_on(name_filter: Option<&str>) -> Result<Self, CaptureError> {
        Self::start_on_ring(name_filter, Arc::new(Mutex::new(VecDeque::new())))
    }

    /// As [`SystemCapture::start_on`], but writes into a ring that already
    /// exists.
    ///
    /// This is what lets the capture device change without rebuilding the
    /// consumer: the `CaptureSource` on the stream thread keeps the same `Arc`
    /// and never learns that its producer was replaced.
    ///
    /// The caller is responsible for clearing the ring at a swap — this
    /// function cannot know whether it is replacing a producer or starting the
    /// first one, and clearing on a first start would discard a prebuffer that
    /// was filled deliberately.
    pub fn start_on_ring(
        name_filter: Option<&str>,
        ring: Arc<Mutex<VecDeque<i16>>>,
    ) -> Result<Self, CaptureError> {
        // ... existing body of start_on, minus the `let ring = ...` allocation ...
    }
```

When moving the body: find the line in the current `start_on` that allocates the
ring (of the form `let ring = Arc::new(Mutex::new(VecDeque::with_capacity(...)))`)
and delete it. Everything downstream already refers to `ring`, so nothing else
changes. If the allocation used `with_capacity`, drop that detail — the ring's
capacity is a `VecDeque` growth hint, and `RING_CAPACITY_SECONDS` is enforced by
the callback's trim logic, not by the allocation.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p openair-capture`
Expected: PASS.

- [ ] **Step 5: Clippy, then commit**

```bash
cargo clippy --workspace --all-targets
git add crates/capture/src/lib.rs
git commit -m "feat(capture): start capture on an existing ring"
```

---

### Task 3: Live latency, master volume and metadata gating

**Files:**
- Modify: `crates/client/src/stats.rs` (the `StreamCommand` enum)
- Modify: `crates/client/src/lib.rs` (extract `re_anchor_group`, handle the new
  commands in the drain loop around line 1364, and the auto-latency block
  around line 1548)
- Test: `crates/client/src/stats.rs` (inline tests)

**Interfaces:**
- Consumes: nothing from Tasks 1–2.
- Produces:
  - `StreamCommand::SetLatency { ms: u64 }`
  - `StreamCommand::SetMasterVolume { db: f32 }`
  - `StreamCommand::SetMetadataEnabled { on: bool }`
  - `fn re_anchor_group(ptp: &PtpClock, group: &mut [Receiver], latency_ms: u64, rtptime: u32) -> u64`
    returning the new `anchor_t_local`.

- [ ] **Step 1: Write the failing test**

Add to the tests module in `crates/client/src/stats.rs`:

```rust
#[test]
fn the_global_commands_round_trip_in_order() {
    let stats = StreamStats::new(500);
    assert!(stats.send(StreamCommand::SetLatency { ms: 750 }));
    assert!(stats.send(StreamCommand::SetMasterVolume { db: -12.0 }));
    assert!(stats.send(StreamCommand::SetMetadataEnabled { on: false }));

    let drained = stats.drain_commands();
    assert_eq!(
        drained,
        vec![
            StreamCommand::SetLatency { ms: 750 },
            StreamCommand::SetMasterVolume { db: -12.0 },
            StreamCommand::SetMetadataEnabled { on: false },
        ],
        "order matters: two latency changes in one tick must not transpose"
    );
    assert!(stats.drain_commands().is_empty(), "draining consumes");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p openair-client the_global_commands_round_trip_in_order`
Expected: FAIL — `no variant named 'SetLatency' found for enum 'StreamCommand'`.

- [ ] **Step 3: Add the variants**

In `crates/client/src/stats.rs`, add to `enum StreamCommand`, after `Remove`:

```rust
    /// Set the group anchor latency in ms and re-anchor every live receiver.
    ///
    /// Both directions are allowed. Lowering re-anchors *shallower* and may
    /// underrun immediately, at which point auto-latency raises it back — a
    /// self-correcting failure that is visible in the log panel and the buffer
    /// bars. Refusing to lower would hide a capability to prevent a failure the
    /// system already handles.
    SetLatency { ms: u64 },
    /// Set the group master volume in dB. Per-receiver trims are relative and
    /// survive this untouched.
    SetMasterVolume { db: f32 },
    /// Whether now-playing metadata is transmitted. The watcher keeps running
    /// either way; this gates sending.
    SetMetadataEnabled { on: bool },
```

- [ ] **Step 4: Run the test**

Run: `cargo test -p openair-client the_global_commands_round_trip_in_order`
Expected: PASS.

- [ ] **Step 5: Extract `re_anchor_group` from the auto-latency block**

In `crates/client/src/lib.rs`, the auto-latency block (near line 1548) currently
contains this sequence inline:

```rust
                    let t_local = ptp_now_ns() + current_latency * 1_000_000;
                    for r in &mut group {
                        if r.alive {
                            if let Err(e) = anchor_receiver(&ptp, r, t_local, rtptime) {
                                warn!(receiver = %r.name, "auto-latency anchor failed — dropping: {e}");
                                r.alive = false;
                            }
                        }
                    }
```

Lift it into a free function beside `anchor_receiver`, so the manual and
automatic paths anchor identically rather than drifting apart:

```rust
/// Re-anchor every live receiver so the current head plays `latency_ms` from
/// now, returning the new `anchor_t_local`.
///
/// Shared by auto-latency and by the settings page's manual change: two code
/// paths computing an anchor slightly differently is the kind of divergence
/// that produces a bug reproducible only one way round.
///
/// A receiver that cannot be re-anchored is marked dead; the caller is expected
/// to `reap_dead` afterwards.
fn re_anchor_group(
    ptp: &PtpClock,
    group: &mut [Receiver],
    latency_ms: u64,
    rtptime: u32,
    why: &str,
) -> u64 {
    let t_local = ptp_now_ns() + latency_ms * 1_000_000;
    for r in group.iter_mut() {
        if r.alive {
            if let Err(e) = anchor_receiver(ptp, r, t_local, rtptime) {
                warn!(receiver = %r.name, "{why} anchor failed — dropping: {e}");
                r.alive = false;
            }
        }
    }
    t_local
}
```

Replace the inlined sequence in the auto-latency block with:

```rust
                    let t_local =
                        re_anchor_group(&ptp, &mut group, current_latency, rtptime, "auto-latency");
```

Everything after it in that block (`reap_dead`, `anchor_t_local = t_local`,
`anchor_rtptime = rtptime`, `last_bump = Instant::now()`, `s.set_latency_ms`)
stays exactly as it is.

- [ ] **Step 6: Handle the three new commands in the drain loop**

In `crates/client/src/lib.rs`, replace the drain loop at line 1364:

```rust
            for cmd in s.drain_commands() {
                match cmd {
                    StreamCommand::SetLatency { ms } => {
                        let ms = ms.clamp(LATENCY_FLOOR_MS, AUTO_LATENCY_MAX_MS);
                        if ms != current_latency {
                            info!(from_ms = current_latency, to_ms = ms, "latency changed");
                            current_latency = ms;
                            anchor_t_local = re_anchor_group(
                                &ptp,
                                &mut group,
                                current_latency,
                                rtptime,
                                "latency change",
                            );
                            anchor_rtptime = rtptime;
                            // Treat a manual change as a bump, so auto-latency
                            // does not immediately step on top of a value the
                            // user just chose.
                            last_bump = Instant::now();
                            reap_dead(&mut group, &mut handles, reconnect);
                            s.set_latency_ms(current_latency);
                        }
                    }
                    StreamCommand::SetMasterVolume { db } => {
                        current_volume_db = db;
                        for r in group.iter_mut() {
                            if r.alive {
                                let level = effective_volume_db(current_volume_db, r.trim_db);
                                if let Err(e) = r.session.set_volume(level) {
                                    warn!(receiver = %r.name, "set_volume failed (continuing): {e}");
                                }
                            }
                        }
                    }
                    StreamCommand::SetMetadataEnabled { on } => {
                        metadata_enabled = on;
                    }
                    other => apply_command(
                        other,
                        &mut group,
                        &mut handles,
                        &ptp,
                        current_volume_db,
                        anchor_t_local,
                        anchor_rtptime,
                        rtptime,
                    ),
                }
            }
```

Add near the other loop constants in `crates/client/src/lib.rs`:

```rust
/// Lowest latency a manual change may request. Below this the anchor is inside
/// one packet's worth of audio and the stream cannot stay ahead of itself.
const LATENCY_FLOOR_MS: u64 = 100;
```

Declare `metadata_enabled` beside `current_volume_db` where the loop's mutable
state is set up, initialised from whether a metadata receiver was supplied:

```rust
    let mut metadata_enabled = metadata_rx.is_some();
```

- [ ] **Step 7: Gate metadata sending on the flag**

In the metadata block at line 1380, change the outer condition from:

```rust
        if let Some(rx) = &metadata_rx {
```

to:

```rust
        // The watcher keeps running when metadata is switched off; only
        // transmission stops. Draining regardless keeps the channel from
        // backing up while it is off, so switching back on sends the *current*
        // track rather than replaying a queue.
        if let Some(rx) = &metadata_rx {
            let latest = drain_latest_metadata(rx);
            if !metadata_enabled {
                if let Some(np) = latest {
                    current_metadata = Some(np);
                }
            } else {
```

and close the extra brace at the end of that block. The existing body keeps
using `latest` where it previously called `drain_latest_metadata(rx)` inline.

- [ ] **Step 8: Run the tests**

Run: `cargo test -p openair-client`
Expected: PASS, all pre-existing tests included.

- [ ] **Step 9: Clippy, then commit**

```bash
cargo clippy --workspace --all-targets
git add crates/client/src/stats.rs crates/client/src/lib.rs
git commit -m "feat(client): live latency, master volume and metadata gating"
```

---

### Task 4: The settings screen's state

Pure state and key handling, no rendering and no platform calls — the same
split `picker.rs` / `picker_ui.rs` already uses.

**Files:**
- Create: `crates/tui/src/settings_screen.rs`
- Modify: `crates/tui/src/lib.rs` (add `pub mod settings_screen;` and re-export)
- Test: `crates/tui/src/settings_screen.rs` (inline tests)

**Interfaces:**
- Consumes: `crate::settings::{Settings, LATENCY_MIN_MS, LATENCY_MAX_MS, LATENCY_STEP_MS}`.
- Produces:
  - `pub enum SettingsRow { Handoff, Latency, Volume, Metadata, ShowControls }`
  - `pub enum SettingsAction { None, Close, Apply(Settings) }`
  - `pub struct SettingsState { pub settings: Settings, ... }`
  - `SettingsState::new(settings: Settings, handoff_available: bool, streaming: bool) -> Self`
  - `SettingsState::on_key(&mut self, key: KeyCode) -> SettingsAction`
  - `SettingsState::rows(&self) -> &[SettingsRow]`
  - `SettingsState::cursor(&self) -> usize`
  - `SettingsState::error(&self) -> Option<&str>`
  - `SettingsState::set_error(&mut self, row: SettingsRow, msg: impl Into<String>)`
  - `SettingsState::revert(&mut self, previous: Settings)`

- [ ] **Step 1: Write the failing tests**

Create `crates/tui/src/settings_screen.rs` containing only the test module for
now:

```rust
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

        s.settings.latency_ms = 500;
        s.on_key(KeyCode::Right);
        assert_eq!(s.settings.latency_ms, 500 + LATENCY_STEP_MS);
        s.on_key(KeyCode::Left);
        assert_eq!(s.settings.latency_ms, 500);
    }

    #[test]
    fn angle_brackets_adjust_as_well_as_arrows() {
        // <> already means "adjust" on the picker and the dashboard; a settings
        // screen where it did nothing would be a trap.
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

        s.settings.volume_db = -60.0;
        s.on_key(KeyCode::Left);
        assert_eq!(s.settings.volume_db, -60.0, "-60 dB is the floor");

        s.settings.volume_db = -8.0;
        s.on_key(KeyCode::Left);
        assert_eq!(s.settings.volume_db, -9.0);
    }

    #[test]
    fn space_toggles_a_boolean_row() {
        let mut s = at(SettingsRow::Metadata);
        let before = s.settings.metadata;
        s.on_key(KeyCode::Char(' '));
        assert_eq!(s.settings.metadata, !before);
        s.on_key(KeyCode::Enter);
        assert_eq!(s.settings.metadata, before, "enter toggles too");
    }

    #[test]
    fn handoff_cannot_be_enabled_without_a_cable() {
        // Same rule the picker's `h` key enforces, and the same explanation.
        let mut s = SettingsState::new(Settings::default(), false, false);
        while s.rows()[s.cursor()] != SettingsRow::Handoff {
            s.on_key(KeyCode::Down);
        }
        assert!(!s.settings.handoff, "forced off when unavailable");
        s.on_key(KeyCode::Char(' '));
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
    }

    #[test]
    fn moving_clears_a_stale_error() {
        let mut s = state();
        s.set_error(SettingsRow::Handoff, "cable disappeared");
        s.on_key(KeyCode::Down);
        assert!(s.error().is_none(), "a stale explanation is worse than none");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

First add `pub mod settings_screen;` to `crates/tui/src/lib.rs` beside the other
`pub mod` lines.

Run: `cargo test -p openair-tui settings_screen`
Expected: FAIL to compile — `cannot find type 'SettingsState' in this scope`.

- [ ] **Step 3: Implement the module**

Write the implementation above the test module in
`crates/tui/src/settings_screen.rs`:

```rust
//! The settings overlay's state and key handling — no rendering, no platform
//! calls.
//!
//! Split from `settings_ui.rs` the same way `picker.rs` is split from
//! `picker_ui.rs`: decisions here, drawing there. What makes this testable is
//! that applying a change is somebody else's job — this module reports that a
//! change was made and is told afterwards whether it stuck.

use crossterm::event::KeyCode;

use crate::settings::{Settings, LATENCY_MAX_MS, LATENCY_MIN_MS, LATENCY_STEP_MS};

/// Volume adjustment bounds and step, in dB.
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
    /// Whether a stream is running. Held so the renderer can say which rows
    /// take effect now and which wait for the next start.
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

    /// The current error, if any.
    pub fn error(&self) -> Option<&str> {
        self.error.as_ref().map(|(_, msg)| msg.as_str())
    }

    /// Which row the current error belongs to, so the renderer can put it
    /// there rather than in a general-purpose status line.
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

    /// Adjust the highlighted row. `up` is ignored by boolean rows, which
    /// toggle either way — a checkbox has no direction.
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
            SettingsRow::ShowControls => {
                self.settings.show_controls = !self.settings.show_controls
            }
        }
        SettingsAction::Apply(self.settings.clone())
    }
}
```

Add to `crates/tui/src/lib.rs`:

```rust
pub use settings_screen::{SettingsAction, SettingsRow, SettingsState};
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p openair-tui settings_screen`
Expected: PASS, all eleven tests.

- [ ] **Step 5: Clippy, then commit**

```bash
cargo clippy --workspace --all-targets
git add crates/tui/src/settings_screen.rs crates/tui/src/lib.rs
git commit -m "feat(tui): settings screen state"
```

---

### Task 5: The settings overlay renderer

**Files:**
- Create: `crates/tui/src/settings_ui.rs`
- Modify: `crates/tui/src/lib.rs`
- Test: `crates/tui/src/settings_ui.rs` (inline tests)

**Interfaces:**
- Consumes: `SettingsState`, `SettingsRow` from Task 4; `crate::rect::centred`.
- Produces: `pub fn render(frame: &mut Frame, state: &SettingsState)`.

- [ ] **Step 1: Write the failing tests**

Create `crates/tui/src/settings_ui.rs` with the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn draw(width: u16, height: u16, state: &SettingsState) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(frame, state)).unwrap();
        terminal
    }

    #[test]
    fn every_row_and_its_value_is_drawn() {
        let state = SettingsState::new(Settings::default(), true, false);
        let screen = draw(100, 30, &state).backend().to_string();
        for expected in ["handoff", "latency", "volume", "metadata", "controls"] {
            assert!(screen.contains(expected), "missing {expected}:\n{screen}");
        }
        assert!(screen.contains("500 ms"), "the latency value:\n{screen}");
        assert!(screen.contains("-8 dB"), "the volume value:\n{screen}");
    }

    #[test]
    fn an_error_is_shown_against_its_row() {
        let mut state = SettingsState::new(Settings::default(), false, false);
        state.set_error(SettingsRow::Handoff, "no cable");
        let screen = draw(100, 30, &state).backend().to_string();
        assert!(screen.contains("no cable"), "{screen}");
    }

    #[test]
    fn rendering_survives_a_sweep_of_terminal_sizes() {
        // An overlay larger than its terminal panics ratatui on render, which
        // on the dashboard means taking a live stream down over a keystroke.
        let state = SettingsState::new(Settings::default(), true, true);
        for width in [10u16, 20, 40, 60, 80, 200] {
            for height in [3u16, 5, 10, 30, 60] {
                draw(width, height, &state);
            }
        }
    }

    #[test]
    fn the_streaming_overlay_says_changes_are_live() {
        let live = SettingsState::new(Settings::default(), true, true);
        assert!(draw(100, 30, &live).backend().to_string().contains("live"));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Add `pub mod settings_ui;` to `crates/tui/src/lib.rs`.

Run: `cargo test -p openair-tui settings_ui`
Expected: FAIL to compile — `cannot find function 'render' in this scope`.

- [ ] **Step 3: Implement the renderer**

Write above the tests in `crates/tui/src/settings_ui.rs`:

```rust
//! Drawing the settings overlay. Decisions live in
//! [`crate::settings_screen`]; this is the terminal half.

use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::settings::Settings;
use crate::settings_screen::{SettingsRow, SettingsState};

/// Overlay size. Wide enough for the longest label, its value and a short
/// reason on one line.
const PANEL: (u16, u16) = (58, 12);

pub fn render(frame: &mut Frame, state: &SettingsState) {
    let area = crate::rect::centred(frame.area(), PANEL.0, PANEL.1);
    // Drawn over a live frame — without this the dashboard shows through.
    frame.render_widget(Clear, area);

    let title = if state.streaming() {
        " settings — changes are live "
    } else {
        " settings "
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let [rows_area, footer_area] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(inner);

    let lines: Vec<Line> = state
        .rows()
        .iter()
        .enumerate()
        .map(|(i, row)| row_line(*row, state, i == state.cursor()))
        .collect();
    frame.render_widget(Paragraph::new(lines), rows_area);

    let footer = Span::styled(
        "  ↑↓ move · ←→ adjust · space toggle · esc close",
        Style::default().fg(Color::DarkGray),
    );
    frame.render_widget(Paragraph::new(Line::from(footer)), footer_area);
}

fn row_line(row: SettingsRow, state: &SettingsState, selected: bool) -> Line<'static> {
    let s: &Settings = &state.settings;
    let (label, value) = match row {
        SettingsRow::Handoff => ("handoff", on_off(s.handoff)),
        SettingsRow::Latency => ("latency", format!("{} ms", s.latency_ms)),
        SettingsRow::Volume => ("volume", format!("{:.0} dB", s.volume_db)),
        SettingsRow::Metadata => ("metadata", on_off(s.metadata)),
        SettingsRow::ShowControls => ("controls", on_off(s.show_controls)),
    };

    let marker = if selected { " ▸ " } else { "   " };
    let mut spans = vec![
        Span::styled(
            marker,
            Style::default().fg(if selected {
                Color::Cyan
            } else {
                Color::DarkGray
            }),
        ),
        Span::styled(
            format!("{label:<12}"),
            if selected {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            },
        ),
        Span::raw(format!("{value:<10}")),
    ];

    // The reason goes on the row that caused it, not in a shared status line:
    // with five rows on screen, "which one failed" is the first question.
    if state.error_row() == Some(row) {
        if let Some(msg) = state.error() {
            spans.push(Span::styled(
                format!(" {msg}"),
                Style::default().fg(Color::Yellow),
            ));
        }
    }
    Line::from(spans)
}

fn on_off(v: bool) -> String {
    if v { "on" } else { "off" }.to_string()
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p openair-tui settings_ui`
Expected: PASS.

- [ ] **Step 5: Clippy, then commit**

```bash
cargo clippy --workspace --all-targets
git add crates/tui/src/settings_ui.rs crates/tui/src/lib.rs
git commit -m "feat(tui): settings overlay renderer"
```

---

### Task 6: Wire the overlay into the App

**Files:**
- Modify: `crates/tui/src/app.rs`
- Modify: `crates/tui/src/lib.rs`
- Modify: `crates/tui/src/picker_ui.rs` (add `s settings` to the footer)
- Modify: `crates/tui/src/dashboard_ui.rs` (add `[s] settings` to a panel title)
- Test: `crates/tui/src/app.rs` (inline tests)

**Interfaces:**
- Consumes: `SettingsState`, `SettingsAction` (Task 4); `settings_ui::render`
  (Task 5); `StreamCommand::{SetLatency, SetMasterVolume, SetMetadataEnabled}`
  (Task 3).
- Produces:
  - `pub type SettingsApplier<'a> = Box<dyn FnMut(&Settings, &Settings) -> Result<(), String> + 'a>;`
  - `App::with_applier(self, applier: SettingsApplier<'a>) -> Self`

- [ ] **Step 1: Write the failing tests**

Add to the tests module in `crates/tui/src/app.rs`:

```rust
#[test]
fn a_failed_apply_reverts_the_setting_and_explains() {
    // The property the closure exists to make testable: no hardware, no
    // endpoint switch, just "the applier said no".
    use std::cell::RefCell;
    use std::rc::Rc;

    let calls: Rc<RefCell<Vec<(bool, bool)>>> = Rc::new(RefCell::new(Vec::new()));
    let seen = Rc::clone(&calls);
    let applier: crate::SettingsApplier = Box::new(move |old, new| {
        seen.borrow_mut().push((old.metadata, new.metadata));
        Err("cable disappeared".to_string())
    });

    let mut screen = SettingsState::new(Settings::default(), true, true);
    let before = screen.settings.clone();
    let action = screen.on_key(KeyCode::Down); // to latency
    assert_eq!(action, SettingsAction::None);

    // Drive the same path App uses.
    let mut applier = applier;
    if let SettingsAction::Apply(next) = screen.on_key(KeyCode::Right) {
        if let Err(why) = applier(&before, &next) {
            screen.set_error(SettingsRow::Latency, why);
            screen.revert(before.clone());
        }
    }

    assert_eq!(screen.settings, before, "reverted to what is in force");
    assert_eq!(screen.error(), Some("cable disappeared"));
    assert_eq!(calls.borrow().len(), 1, "the applier was consulted once");
}

#[test]
fn settings_open_and_close_back_to_the_picker() {
    let mut app = test_app();
    app.open_settings();
    assert!(matches!(app.screen, Screen::Settings(_)));
    app.on_key(KeyCode::Esc);
    assert!(
        matches!(app.screen, Screen::Picker(_)),
        "closing returns to where it was opened from"
    );
}
```

Use whatever `test_app()` helper already exists in that module; if there is
none, build one mirroring the existing screen-transition tests.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p openair-tui settings_open_and_close`
Expected: FAIL — `no variant named 'Settings' found for enum 'Screen'`.

- [ ] **Step 3: Add the applier type and the Screen variant**

In `crates/tui/src/app.rs`, beside `StreamLauncher`:

```rust
/// Apply a settings change that needs work the TUI cannot do itself —
/// switching a Windows audio endpoint, above all.
///
/// Receives the settings in force before the change and after it, so the
/// applier acts only on what actually differs. Supplied by the CLI for the same
/// reason `StreamLauncher` is: `openair-tui` does not depend on
/// `openair-capture`, and that boundary is what keeps this crate testable on
/// every platform.
///
/// Called on the main thread. `cpal::Stream` is `!Send`, so `SystemCapture` —
/// and the `CaptureRig` holding it — never leave the thread that created them,
/// which is this one.
pub type SettingsApplier<'a> =
    Box<dyn FnMut(&Settings, &Settings) -> Result<(), String> + 'a>;
```

Add to `enum Screen`:

```rust
    Settings(Box<SettingsScreen>),
```

and define:

```rust
/// The settings overlay, plus the screen it was opened from so closing returns
/// there rather than to a fixed destination.
pub struct SettingsScreen {
    pub state: SettingsState,
    pub origin: Box<Screen>,
}
```

Add an `applier: Option<SettingsApplier<'a>>` field to `App`, defaulting to
`None`, with:

```rust
    /// Supply the closure that applies platform-side settings changes.
    pub fn with_applier(mut self, applier: SettingsApplier<'a>) -> Self {
        self.applier = Some(applier);
        self
    }
```

- [ ] **Step 4: Open, close and apply**

Add to `impl App`:

```rust
    /// Open the settings overlay over whatever is on screen now.
    pub fn open_settings(&mut self) {
        let streaming = matches!(self.screen, Screen::Streaming(_));
        let state = SettingsState::new(
            self.settings.clone(),
            self.handoff_available,
            streaming,
        );
        let placeholder = Screen::Picker(Box::new(PickerState::new(
            self.settings.clone(),
            Vec::new(),
            self.handoff_available,
        )));
        let origin = Box::new(std::mem::replace(&mut self.screen, placeholder));
        self.screen = Screen::Settings(Box::new(SettingsScreen { state, origin }));
    }

    /// Apply a settings change, reverting it if the applier refuses.
    ///
    /// The order matters: platform work first, then the stream commands, then
    /// persistence. A setting that could not be applied must not reach
    /// `settings.json`, or the file would claim a state the program is not in.
    fn apply_settings(&mut self, previous: Settings, next: Settings) {
        if let Some(applier) = self.applier.as_mut() {
            if let Err(why) = applier(&previous, &next) {
                if let Screen::Settings(s) = &mut self.screen {
                    s.state.set_error(row_for_change(&previous, &next), why);
                    s.state.revert(previous);
                }
                return;
            }
        }

        if let Some(running) = self.running.as_ref() {
            if next.latency_ms != previous.latency_ms {
                running.stats.send(StreamCommand::SetLatency {
                    ms: next.latency_ms,
                });
            }
            if next.volume_db != previous.volume_db {
                running.stats.send(StreamCommand::SetMasterVolume {
                    db: next.volume_db,
                });
            }
            if next.metadata != previous.metadata {
                running.stats.send(StreamCommand::SetMetadataEnabled {
                    on: next.metadata,
                });
            }
        }

        self.settings = next;
        if let Err(e) = self.settings.save() {
            tracing::warn!("could not save settings: {e}");
        }
    }
```

And the helper, beside `apply_settings`:

```rust
/// Which row a change belongs to, so a failure lands on the row that caused it.
/// Falls back to handoff, the only row whose apply can fail today.
fn row_for_change(previous: &Settings, next: &Settings) -> SettingsRow {
    if previous.latency_ms != next.latency_ms {
        SettingsRow::Latency
    } else if previous.volume_db != next.volume_db {
        SettingsRow::Volume
    } else if previous.metadata != next.metadata {
        SettingsRow::Metadata
    } else if previous.show_controls != next.show_controls {
        SettingsRow::ShowControls
    } else {
        SettingsRow::Handoff
    }
}
```

- [ ] **Step 5: Route keys and rendering**

In the key-dispatch match in `App::on_key`, add a `Screen::Settings` arm before
the others:

```rust
            Screen::Settings(_) => {
                let Screen::Settings(s) = &mut self.screen else {
                    unreachable!("just matched");
                };
                let previous = self.settings.clone();
                match s.state.on_key(key) {
                    SettingsAction::None => {}
                    SettingsAction::Close => {
                        let Screen::Settings(s) = std::mem::replace(
                            &mut self.screen,
                            Screen::Picker(Box::new(PickerState::new(
                                self.settings.clone(),
                                Vec::new(),
                                self.handoff_available,
                            ))),
                        ) else {
                            unreachable!("just matched");
                        };
                        self.screen = *s.origin;
                    }
                    SettingsAction::Apply(next) => self.apply_settings(previous, next),
                }
            }
```

In the picker and dashboard arms, add `s` as the key that opens settings, before
their existing handling:

```rust
                if key == KeyCode::Char('s') {
                    self.open_settings();
                    return;
                }
```

In `App::render` (or wherever the screen match for drawing lives), add:

```rust
            Screen::Settings(s) => {
                // Draw the origin underneath, then the overlay over it: on the
                // dashboard this is what lets the user watch the buffer bars
                // react while they drag the latency.
                render_screen(frame, &s.origin, buffer);
                crate::settings_ui::render(frame, &s.state);
            }
```

Factor the existing per-screen drawing into `render_screen(frame, screen,
buffer)` so the overlay can call it for its origin.

- [ ] **Step 6: Advertise the key**

In `crates/tui/src/picker_ui.rs`, `controls_text`:

```rust
fn controls_text(show_all: bool) -> &'static str {
    if show_all {
        "↑↓ move · space select · ⏎ start · h handoff · <> latency · s settings · q quit"
    } else {
        "space select · h handoff · <> latency · s settings"
    }
}
```

In `crates/tui/src/dashboard_ui.rs`, `receiver_controls`:

```rust
fn receiver_controls(show_all: bool) -> &'static str {
    if show_all {
        "   [↑↓] select · [+/-] vol · [<>] offset · [a] add · [r] retry · [d] drop · [s] settings"
    } else {
        "   [+/-] vol · [<>] offset · [a] add · [r] retry · [d] drop · [s] settings"
    }
}
```

The picker_ui tests from project C assert the *absence* of "move" and "quit" in
the short form; adding `s settings` does not affect them. Re-run them to confirm.

- [ ] **Step 7: Run all TUI tests**

Run: `cargo test -p openair-tui`
Expected: PASS.

- [ ] **Step 8: Clippy, then commit**

```bash
cargo clippy --workspace --all-targets
git add crates/tui/src/app.rs crates/tui/src/lib.rs crates/tui/src/picker_ui.rs crates/tui/src/dashboard_ui.rs
git commit -m "feat(tui): open settings from the picker and the dashboard"
```

---

### Task 7: The CLI applier — the real device swap

**Files:**
- Modify: `apps/cli/src/main.rs` (`CaptureRig`, the `StreamLauncher`
  construction near line 1135)
- Test: manual, on hardware — this task's logic is the platform half the
  closure exists to isolate.

**Interfaces:**
- Consumes: `SystemCapture::start_on_ring` (Task 2);
  `CaptureSource::new_with_rate` (Task 1); `SettingsApplier` (Task 6).
- Produces: the applier passed to `App::with_applier`.

- [ ] **Step 1: Give `CaptureRig` a shared ring and rate**

Add fields to `struct CaptureRig`:

```rust
    /// The ring the running stream's source is reading from, and the rate it
    /// believes the producer is capturing at. Shared so the capture device can
    /// be swapped underneath a live stream.
    ring: Option<std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<i16>>>>,
    rate: Option<std::sync::Arc<std::sync::atomic::AtomicU32>>,
```

Initialise both to `None` where `CaptureRig` is constructed near line 1135.

In `prepare`, after the capture starts, store them and return the `Arc<AtomicU32>`
instead of a bare `u32`:

```rust
        let (ring, rate) = (
            cap.ring.clone(),
            std::sync::Arc::new(std::sync::atomic::AtomicU32::new(cap.device_rate)),
        );
        self.ring = Some(std::sync::Arc::clone(&ring));
        self.rate = Some(std::sync::Arc::clone(&rate));
        self.capture = Some(cap);
```

Change `prepare`'s return type's second element from `u32` to
`std::sync::Arc<std::sync::atomic::AtomicU32>`, and in `launch` change the
source construction to:

```rust
                    let mut source = openair_client::CaptureSource::new_with_rate(
                        ring,
                        rate,
                        seconds,
                        Some(stop),
                    );
```

- [ ] **Step 2: Implement the swap**

Add to `impl CaptureRig`:

```rust
    /// Apply a settings change that needs platform work.
    ///
    /// Only handoff qualifies today: latency, volume and metadata reach the
    /// stream through `StreamCommand`, and `show_controls` is pure rendering.
    fn apply(
        &mut self,
        previous: &openair_tui::Settings,
        next: &openair_tui::Settings,
    ) -> Result<(), String> {
        if previous.handoff == next.handoff {
            return Ok(());
        }
        // No stream running: handoff is a preference until one starts, exactly
        // as it is when set from the command line. Engaging it here would
        // silence the speakers while the user is still in the picker.
        if self.capture.is_none() {
            return Ok(());
        }
        self.swap_capture(next.handoff)
    }

    /// Switch the capture device without rebuilding the consumer.
    ///
    /// Order is deliberate and is about failure, not latency: the new capture
    /// is proven before the old one is dropped, so a failure leaves a working
    /// stream and an unchanged setting.
    #[cfg(windows)]
    fn swap_capture(&mut self, want_handoff: bool) -> Result<(), String> {
        let Some(ring) = self.ring.clone() else {
            return Err("no capture is running".to_string());
        };
        let Some(rate) = self.rate.clone() else {
            return Err("no capture is running".to_string());
        };

        // 1. Move the endpoint. Keep the old handoff session alive until the
        //    new capture works, so a failure can be undone.
        let previous_handoff = self.handoff.take();
        let (new_handoff, capture_device) = if want_handoff {
            match start_handoff(self.handoff_device.clone()) {
                Ok((session, _volume_rx)) => {
                    let name = session.device_name().to_string();
                    (Some(session), Some(name))
                }
                Err(e) => {
                    // Nothing has changed yet; put the old session back.
                    self.handoff = previous_handoff;
                    return Err(e.to_string());
                }
            }
        } else {
            // Dropping the old session restores the previous default device.
            drop(previous_handoff);
            (None, None)
        };

        // 2. Start the new capture on the shared ring.
        let cap = match openair_capture::SystemCapture::start_on_ring(
            capture_device.as_deref(),
            std::sync::Arc::clone(&ring),
        ) {
            Ok(cap) => cap,
            Err(e) => {
                // The old capture is still running and still feeding the ring.
                // Undo the endpoint move and report; the setting stays put.
                self.handoff = if want_handoff { None } else { previous_handoff_restore(self) };
                drop(new_handoff);
                return Err(format!("could not start capture: {e}"));
            }
        };

        // 3. Clear the ring, then publish the new rate, then drop the old
        //    capture. The rate goes last so no block is ever resampled with a
        //    ratio that does not match the samples in front of it.
        if let Ok(mut g) = ring.lock() {
            g.clear();
        }
        rate.store(cap.device_rate, std::sync::atomic::Ordering::Relaxed);
        tracing::info!(
            device = %cap.device_name,
            rate = cap.device_rate,
            "capture device swapped"
        );
        self.handoff = new_handoff;
        self.capture = Some(cap); // dropping the old one stops it
        Ok(())
    }

    #[cfg(not(windows))]
    fn swap_capture(&mut self, _want_handoff: bool) -> Result<(), String> {
        Err("handoff is only available on Windows".to_string())
    }
```

Note on the error path in step 2 of `swap_capture`: `previous_handoff` has
already been consumed by the `want_handoff == false` branch, so restoring it is
only meaningful when `want_handoff` was true and the *new* session started but
capture failed. Simplify by replacing the marked line with:

```rust
                self.handoff = None;
                drop(new_handoff);
                return Err(format!("could not start capture: {e}"));
```

and accept that a capture failure during a handoff-on attempt leaves the
endpoint restored and handoff off — which matches what the user sees on the row.
Delete the `previous_handoff_restore` reference; it is not a real function.

- [ ] **Step 3: Pass the applier to the App**

Where the `StreamLauncher` is built near line 1147, the same `rig` cannot be
moved into two closures. Wrap it:

```rust
            let rig = std::rc::Rc::new(std::cell::RefCell::new(rig));
            let launch_rig = std::rc::Rc::clone(&rig);
            let launcher: openair_tui::StreamLauncher = Box::new(
                move |targets, settings, stats, stop| {
                    launch_rig.borrow_mut().launch(targets, settings, stats, stop)
                },
            );
            let apply_rig = std::rc::Rc::clone(&rig);
            let applier: openair_tui::SettingsApplier = Box::new(move |old, new| {
                apply_rig.borrow_mut().apply(old, new)
            });
```

`Rc`/`RefCell` rather than `Arc`/`Mutex` because both closures stay on the main
thread — the same `!Send` fact that makes this design work at all. Then add
`.with_applier(applier)` to the `App` construction.

- [ ] **Step 4: Build and smoke-test**

```bash
cargo build --release
./target/release/openair.exe capture
./target/release/openair.exe devices
```

Expected: the usage line and the device list, unchanged.

- [ ] **Step 5: Clippy, then commit**

```bash
cargo clippy --workspace --all-targets
git add apps/cli/src/main.rs
git commit -m "feat(cli): swap the capture device on a live handoff toggle"
```

- [ ] **Step 6: Hardware test (by hand)**

With music playing to at least one receiver:

1. Press `s`, move to handoff, toggle it **on**. Audio should continue after a
   short gap; the Windows default device becomes the virtual cable.
2. Check pitch is correct — this is the case Task 1 exists for. If the two
   devices run at different rates and pitch is wrong, Task 1 is not working.
3. Toggle handoff **off**. The original device is restored, audio continues.
4. Confirm per-receiver trims set before the swap are still in effect.
5. Change latency by several steps and watch the buffer bars respond.
6. Lower the latency hard (to 100 ms) and confirm auto-latency raises it back
   rather than the stream collapsing.

---

### Task 8: Documentation

**Files:**
- Modify: `README.md` (the terminal UI section)
- Modify: `STATUS.md` (the `tui` and `client` crate rows, Next Steps)
- Modify: `DEVLOG.md` (a session entry at the top)

- [ ] **Step 1: README**

In the dashboard key table, add:

```markdown
| `s` | settings |
```

After the preferences paragraph, add:

```markdown
Press `s` from either the picker or the dashboard for the settings overlay.
From the dashboard the panel sits over the live frame rather than replacing it,
so you can watch the buffer bars react while you adjust the latency — which is
the only way a latency control is comprehensible.

Everything on it applies to a running stream. Toggling handoff mid-stream
switches the Windows default device and moves capture to it without rebuilding
the audio pipeline: the new capture is started and proven *before* the old one
is dropped, so if it fails you keep the stream you had and the setting stays
where it was.
```

- [ ] **Step 2: STATUS**

Update the `tui` row's test count to the actual number from
`cargo test -p openair-tui`, and add "settings overlay (live handoff, latency,
volume, metadata)" to its description. Update the `client` row to mention
`SetLatency`/`SetMasterVolume`/`SetMetadataEnabled` and the shared capture rate.
Remove project D from Next Steps.

- [ ] **Step 3: DEVLOG**

Add an entry at the top dated the day of implementation, covering: why the
applier closure rather than a second inbox; why the new capture starts before
the old one stops; the sample-rate hazard and why it was a latent bug rather
than a new one; and the ordering rule that the rate is published after the ring
is cleared.

- [ ] **Step 4: Commit**

```bash
git add README.md STATUS.md DEVLOG.md
git commit -m "docs: settings overlay"
```

---

## Self-Review

**Spec coverage.** Every section maps to a task: §1 presentation → Tasks 4–5;
§2 applier → Task 6 (type) and Task 7 (implementation); §3 live handoff →
Tasks 1, 2, 7; §4 latency/volume/metadata → Task 3; §5 error handling → Task 4
(`revert`, `set_error`) and Task 6 (`apply_settings` order); §6 persistence →
Task 6 step 4, where saving happens only after a successful apply; §7 testing →
distributed across each task's test steps plus Task 7 step 6 for hardware.

**Known rough edge.** Task 7 step 2 contains a correction to its own code
(`previous_handoff_restore` is not a real function). It is written that way
deliberately rather than silently cleaned up, because the ownership problem it
runs into — `previous_handoff` being consumed on one branch and needed on
another — is exactly the thing an implementer will hit, and seeing the wrong
version next to the right one is more useful than seeing only the right one.

**Sequencing.** Tasks 1 and 2 are independently valuable and land first for that
reason: they fix a real `--handoff` bug that exists today. If the rest of this
project were abandoned, they should still ship.

**Type consistency check.** `CaptureSource::new_with_rate` (Task 1) is called in
Task 7 step 1 with the same parameter order. `SystemCapture::start_on_ring`
(Task 2) is called in Task 7 step 2 with `(Option<&str>, Arc<Mutex<VecDeque<i16>>>)`
as defined. `StreamCommand` variants (Task 3) are constructed in Task 6 step 4
with matching field names (`ms`, `db`, `on`). `SettingsRow` and `SettingsAction`
(Task 4) are used in Tasks 5 and 6 as defined. `SettingsApplier` (Task 6) is
satisfied by `CaptureRig::apply` (Task 7), whose signature
`(&Settings, &Settings) -> Result<(), String>` matches.
