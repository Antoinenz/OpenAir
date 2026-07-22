# Implementation Plan: `--handoff` (mute local + mirror Windows volume)

**Design doc:** `2026-07-22-handoff-local-mute-volume-mirror-design.md`
**Task:** #16
**Approach:** A (endpoint mute + ~50 ms polling bridge). B documented as future upgrade.

This plan is ordered so the **load-bearing risk is proven first** (Phase 0). Each
phase ends at a green `cargo test` / `cargo clippy` boundary and is committable.

---

## Phase 0 — Prove loopback survives endpoint mute (throwaway)

**Goal:** Confirm the core assumption before building anything permanent: WASAPI
loopback keeps delivering non-silent frames while the render endpoint is muted.

**Steps:**
1. Add `windows = { version = "0.54", features = [...] }` to
   `crates/capture/Cargo.toml` under a `[target.'cfg(windows)'.dependencies]`
   section. Features needed (verify exact paths against 0.54):
   `Win32_Media_Audio`, `Win32_System_Com`,
   `Win32_Foundation`, `Win32_Devices_FunctionDiscovery`.
   (0.54 already in `Cargo.lock` via cpal, so no new major version resolves.)
2. Write a **temporary** integration test or `examples/mute_probe.rs` in the
   capture crate that:
   - `CoInitializeEx(COINIT_MULTITHREADED)`
   - `IMMDeviceEnumerator` → `GetDefaultAudioEndpoint(eRender, eConsole)`
   - `Activate::<IAudioEndpointVolume>()`
   - read + stash original mute, `SetMute(TRUE, null)`
   - start `SystemCapture::start()`, play audio manually, sample the ring for
     ~2 s, compute peak
   - restore original mute
   - print peak.

**Manual verification (REQUIRED, hardware):** run with music playing, endpoint
muted by the probe. **Expected:** speakers silent, ring peak clearly non-zero.

**Decision gate:**
- Peak non-zero → assumption holds, proceed to Phase 1.
- Peak ~zero → **STOP.** Loopback is post-mute on this stack; the design is not
  viable as written. Report back before building further (candidates:
  per-app-exclusion, virtual device — both out of current scope).

**Do not commit the probe** — delete it after the gate (or keep behind
`#[ignore]` if genuinely useful, but the design says throwaway).

---

## Phase 1 — `VolumeBridge` core + pure mapping (unit-tested)

**Goal:** the Core Audio bridge, with all testable logic split into pure
functions.

**New file:** `crates/capture/src/handoff.rs`, gated `#![cfg(windows)]`.
`crates/capture/src/lib.rs` gains `#[cfg(windows)] pub mod handoff;`.

**Public API:**
```rust
pub enum VolumeEvent { Level(f32) }   // dBFS; -144.0 == silent/muted
pub struct VolumeBridge { /* thread handle, stop flag, original state */ }
impl VolumeBridge {
    /// Mute local speakers, stash original state, start polling.
    pub fn start() -> Result<(VolumeBridge, Receiver<VolumeEvent>), HandoffError>;
}
impl Drop for VolumeBridge { /* restore original (scalar, muted) */ }
```
> Note: collapse the event model to a single `Level(f32)` carrying `-144.0` for
> mute (design's simplification option) — keeps client integration to one
> `set_volume` call, no separate mute path.

**Pure functions (no COM — these get the unit tests):**
```rust
/// Windows master scalar (0.0..=1.0) → AirPlay dBFS, clamped to [-30, 0];
/// 0.0 → -144.0 (silent).
fn scalar_to_dbfs(scalar: f32) -> f32;

/// Classify a poll sample vs the last-seen one into an optional emitted event
/// plus whether we must re-assert our speaker mute.
struct PollState { last_scalar: f32, last_muted: bool }
enum PollAction { Emit(VolumeEvent, /*reassert_mute*/ bool), ReassertOnly, Nothing }
fn classify(prev: &PollState, cur_scalar: f32, cur_muted: bool) -> (PollAction, PollState);
```

Classification rules (from design):
- `cur_scalar != prev.last_scalar` (beyond an epsilon) → `Emit(Level(scalar_to_dbfs(cur_scalar)), reassert = cur_muted == false)`. (Windows auto-unmutes on a volume key; if it did, re-assert.)
- scalar unchanged, `cur_muted != prev.last_muted`, and the change was **user-driven** (i.e. `cur_muted == false`, since we only ever set it `true`) → user pressed mute/unmute toggle → `Emit(Level(-144.0 if <toggled to muted intent>))`… but since our own writes keep it `true`, a user *unmute* shows as `cur_muted == false`. Treat that as an **AirPlay-mute toggle**: emit `Level(-144.0)` and `reassert = true`.
  - Rationale spelled out in code comments: because the speaker stays muted by us, the only mute-flag transition a user can produce is `true → false` (they hit unmute). We interpret that gesture as "toggle AirPlay mute" and immediately re-mute the speakers. A second press flips back.
  - **State to track the AirPlay-mute intent:** carry a bool in the poll thread (`airplay_muted`) toggled on each such gesture, so we emit the right level.
- else → `Nothing`.

**COM thread (in `start`):**
- `CoInitializeEx` on the spawned poll thread (COM apartment is per-thread; do
  it where the interface is used).
- Acquire endpoint + `IAudioEndpointVolume`, read original `(scalar, muted)`.
- `SetMute(TRUE)`.
- Loop every `POLL_INTERVAL` (50 ms) until stop flag:
  `GetMasterVolumeLevelScalar`, `GetMute`, `classify`, act (send event, re-assert
  mute), update `PollState`.
- On stop: restore original `(scalar, muted)`, `CoUninitialize`.

**Error type:** `HandoffError` (thiserror) — `ComInit`, `Enumerator`, `NoEndpoint`,
`Activate`, `Get/Set`. `start()` returns `Err` if any setup step fails so the
caller can degrade-off.

**Unit tests (`#[cfg(all(test, windows))]`, pure — no COM):**
- `scalar_to_dbfs`: `1.0 → 0.0`; `0.5 → ~-6.02`; `0.0316 → ~-30` (floor clamp
  holds below); `0.0 → -144.0`.
- `classify`:
  - volume raise with mute auto-cleared → `Emit(Level(...), reassert = true)`.
  - volume change with mute still set → `Emit(Level(...), reassert = false)`.
  - user unmute (scalar same, muted true→false), airplay currently audible →
    `Emit(Level(-144.0), reassert = true)` and intent flips to muted.
  - second user unmute gesture → emits last audible level, intent flips back.
  - no change → `Nothing`.

**Gate:** `cargo test -p openair-capture` green, `cargo clippy -p openair-capture
-- -D warnings` clean. Commit: "capture: VolumeBridge for --handoff (mute local +
mirror volume)".

---

## Phase 2 — Client plumbing: accept a volume-event receiver

**Goal:** let the buffered stream apply mirrored-volume updates live.

**`crates/client/src/lib.rs`:**
1. **Decision (settled):** `openair-client` already depends on `openair-capture`
   (verified: `crates/client/Cargo.toml` → `openair-capture = { path = ... }`,
   used by `CaptureSource`). BUT the channel element stays plain `f32` (dBFS) at
   the client boundary anyway — the client's job is only "apply this dBFS to
   every receiver", and it must not carry a `#[cfg(windows)]` type into its
   cross-platform signature. So: client accepts
   `Option<std::sync::mpsc::Receiver<f32>>`; the CLI adapts the capture bridge's
   `Receiver<VolumeEvent>` → `f32` (mapping `Level(db) → db`). This keeps the
   client signature identical on Linux/macOS.
2. Add param to `stream_audio_buffered_multi`:
   `volume_rx: Option<Receiver<f32>>`. Thread it through
   `stream_audio_buffered_with_latency` / `stream_audio_buffered` as `None` for
   the non-handoff callers (keeps their signatures via a new arg or a thin
   wrapper — prefer adding the arg and updating the two internal callers).
3. Introduce a mutable `current_volume_db: Option<f32>` initialized from
   `volume_db`. In the main loop (once per iteration, near `service_feedback`):
   drain `volume_rx.try_recv()` (while Ok) — last value wins; on a new value set
   `current_volume_db = Some(db)` and call `set_volume(db)` on every live
   receiver.
4. **Rejoiners:** `finish_reconnect` already takes `volume_db` — pass
   `current_volume_db` instead of the static seed so a reconnecting receiver gets
   the current mirrored level. (Design: first VolumeEvent hands control over;
   `current_volume_db` naturally captures "seed until first event, then mirror".)

**Tests:** the loop is integration-level (needs receivers); no new unit test here.
Keep the change minimal and rely on existing 71 tests + clippy. Add a small pure
helper if any logic is extractable (e.g. `drain_latest(rx) -> Option<f32>`) and
unit-test that.

**Gate:** `cargo build`, `cargo test` (all crates) green, `cargo clippy -- -D
warnings` clean. Commit: "client: mirror live volume updates into buffered
stream".

---

## Phase 3 — CLI wiring: `--handoff` flag

**Goal:** `openair capture <recv>... --handoff` mutes local + mirrors volume.

**`apps/cli/src/main.rs`:**
1. `let (raw_args, handoff) = extract_flag(&raw_args, "--handoff");` (reuse the
   existing helper).
2. Validity checks (before starting capture):
   - `handoff && args[0] != "capture"` → error, usage note, return.
   - `#[cfg(not(windows))]` + `handoff` → friendly "only supported on Windows"
     error, return. (Compile the bridge start only under `#[cfg(windows)]`.)
3. In the `capture` branch, when `handoff` is set (Windows only):
   - `openair_capture::handoff::VolumeBridge::start()`:
     - `Ok((bridge, rx))` → keep `bridge` alive for the stream's lifetime
       (bind to a variable that drops after `stream_fn`, restoring state); pass
       `Some(rx_as_f32)` into the stream. Map `VolumeEvent::Level(db)` → `db`
       (or make client take `Receiver<VolumeEvent>` directly if we accept the
       capture-type dep — decide in Phase 2). Print
       "🔇 local speakers muted — Windows volume now controls AirPlay".
     - `Err(e)` → `warn!` + print a notice, **continue streaming normally**
       (degrade-off), `rx = None`.
   - The `--volume` seed still flows as the initial `Some(volume_db)`; the first
     mirrored event overrides it (Phase 2 `current_volume_db`).
4. `stream_fn` needs to forward the receiver. Extend the closure to take
   `Option<Receiver<f32>>` (or set a captured var). Since `stream_fn` also serves
   `play`/`tone` (which pass `None`), give it the extra arg.
5. Ctrl+C: existing handler sets the stop flag; the stream returns; `bridge`
   drops → speakers/volume restored. Verify `bridge` is dropped on **all** exit
   paths of the capture branch (including early returns after it's created —
   there are none between start and stream, good).

**Gate:** `cargo build` green, `cargo clippy -- -D warnings` clean. Commit:
"cli: --handoff flag (Windows capture) wires VolumeBridge".

---

## Phase 4 — Docs + manual hardware verification

1. **README:** add `--handoff` to the Flags table — "Windows + capture only.
   Silences local speakers while streaming and mirrors the Windows master volume
   (slider / keys / mute) onto AirPlay. `--volume` sets the initial level until
   you first touch the Windows volume." Add a capture example.
2. **DEVLOG:** Session 11 entry — feature, approach A, Phase-0 result, B noted as
   future path. Mark "Code complete; Phase 0 verified; end-to-end pending final
   hardware pass" honestly.
3. **STATUS:** add `--handoff` to CLI row and to "Awaiting hardware verification"
   with the test checklist below.

**Manual hardware checklist:**
1. Phase 0 already proven (loopback survives mute).
2. `openair capture "<room>" --handoff` → speakers silent, AirPlay plays.
3. Drag Windows slider / press volume keys → AirPlay volume follows (~50 ms lag
   ok); speakers stay silent.
4. Press Windows mute key → AirPlay goes silent; press again → returns; speakers
   stay silent throughout.
5. Ctrl+C → original Windows volume + mute restored.
6. Multi-room (`--handoff` + two rooms) → both track the volume; a reconnecting
   room comes back at the current level.
7. Non-Windows build (`cargo build` on Linux CI if available) → `--handoff`
   compiles to the friendly-error path, no COM code compiled.

**Gate:** docs committed. Final commit: "docs: --handoff usage + Session 11".

---

## Risks / notes
- **COM apartment:** init COM on the *poll thread*, not the caller's. Never share
  the `IAudioEndpointVolume` across threads.
- **Restore reliability:** `Drop` must not panic; swallow COM errors on restore
  with a `warn!`. If the process is killed hard (not Ctrl+C), Windows restores
  endpoint state on session end anyway.
- **Epsilon on scalar compare:** floats from Windows are quantized; use a small
  epsilon (~1e-4) so we don't emit spurious events or miss real 1-step changes.
- **Dep direction:** confirmed `openair-client` already depends on
  `openair-capture` (CaptureSource). We still use a plain `f32` (dBFS) channel at
  the client boundary — NOT `Receiver<VolumeEvent>` — so the client's public
  signature stays free of any `#[cfg(windows)]` type and compiles unchanged on
  Linux/macOS. The CLI does the `VolumeEvent → f32` adaptation.
- **Future B:** the `VolumeBridge` surface (start → Receiver → Drop) is unchanged
  by an event-driven rewrite, so B stays internal.
