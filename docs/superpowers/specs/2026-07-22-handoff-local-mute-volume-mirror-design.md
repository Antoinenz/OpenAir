# Design: `--handoff` — mute local speakers + mirror Windows volume to AirPlay

**Date:** 2026-07-22
**Status:** Approved (design), pending implementation plan
**Scope:** Windows only, `capture` mode only
**Task:** #16

## Summary

Add an opt-in mode, enabled by the `--handoff` flag, for `openair capture` on
Windows. When enabled, OpenAir:

1. **Silences the local speakers** while streaming, so audio comes out of the
   AirPlay receiver(s) only — no double audio from the PC.
2. **Mirrors the Windows master volume onto AirPlay.** When the user changes the
   Windows volume (slider, volume keys, or the mute button), OpenAir catches the
   change and forwards it to every receiver via the existing RTSP
   `SET_PARAMETER volume`. The familiar Windows volume UI becomes the AirPlay
   volume control even though the PC itself stays silent.

The name evokes "hand the audio *and* its volume control off to AirPlay."

## Motivation

Today `openair capture` loops back system audio to AirPlay while the PC speakers
keep playing the same audio — annoying double audio. Users want the local output
silenced but still want to reach for the normal Windows volume to control loudness.
Because the speakers are silent, that volume gesture should instead drive the
remote AirPlay volume.

## Key technical facts

- **Capture is WASAPI loopback via `cpal`** (`crates/capture/src/lib.rs`), which
  abstracts the raw Core Audio APIs. To touch the endpoint mute/volume we add the
  `windows` crate directly, in a new `#[cfg(windows)]` module in the `capture`
  crate.
- **AirPlay volume** is set via `session.set_volume(db)` →
  RTSP `SET_PARAMETER volume` in dBFS (`0` = full scale, `-30` ≈ quiet,
  `-144` = mute). See `crates/rtsp/src/stream.rs::set_volume`.
- **WASAPI loopback taps the render mix *before* the endpoint mute/volume is
  applied.** That is why loopback recordings don't change level when you drag the
  Windows slider. This mode relies on that property: we mute the endpoint
  (speakers silent) while loopback keeps delivering full-scale audio to AirPlay.

### ⚠️ Load-bearing assumption — verify first

The entire approach depends on **loopback still delivering non-silent audio while
the endpoint is muted**. This is believed true (loopback is pre-endpoint-mute) but
**must be verified empirically before building the rest**:

> **Phase 0 (throwaway):** mute the default render endpoint via
> `IAudioEndpointVolume::SetMute(TRUE)`, run loopback capture, and confirm the
> captured frames are non-silent.

If Phase 0 fails, **stop** — there is no clean driver-free fallback (see
Rejected approach C), and the feature must be rethought rather than built on a
false assumption.

## Chosen approach: A — endpoint mute + polling bridge

A new `#[cfg(windows)]` module in the `capture` crate exposes a `VolumeBridge`:

- `VolumeBridge::start()`:
  - `CoInitializeEx` on the bridge thread.
  - Get the default **render** endpoint (`eRender`, `eConsole`) and activate
    `IAudioEndpointVolume`.
  - Read and stash the **original** `(scalar, muted)` so it can be restored.
  - Set `mute = TRUE` (speakers silent).
  - Spawn a poll thread and return a `Receiver<VolumeEvent>`.
- **Poll thread (~50 ms cadence):** read current `(scalar, muted)` and diff
  against last-seen to classify the user's action:
  - **scalar changed** → volume adjustment → emit the new level. If Windows
    auto-cleared our mute (it does this on a volume-key press), re-assert
    `mute = TRUE`.
  - **scalar unchanged but mute toggled by the user** → treat as an
    **AirPlay-mute toggle** → emit mute/unmute; re-assert speaker `mute = TRUE`.
  - The persistent "muted" speaker icon is *honest* here: it means "your PC
    speakers are off."
- **Mapping** `scalar → dBFS`: `20·log10(scalar)`, clamped to `[-30, 0]`;
  `scalar == 0` or an AirPlay-mute → `-144`. (Curve is tunable; refine on
  hardware.)
- `VolumeBridge::drop()`: restore the original `(scalar, muted)`.

### Event model

```
enum VolumeEvent {
    Level(f32), // dBFS to forward to receivers
    Muted,      // forward -144
    Unmuted,    // forward the last Level (or seed)
}
```

(Exact shape finalized in the plan; may collapse to a single `Level(f32)` with
`-144` for mute to keep the client integration trivial.)

### Trade-offs

- *Pros:* no COM callback interface to implement; simple, robust; all COM stays
  in one small module.
- *Cons:* up to ~50 ms of audible "blip" if a volume key auto-unmutes before the
  next poll; ~50 ms volume latency (imperceptible for a volume knob).

### Future upgrade path — approach B (documented, not built now)

If the poll-based blip ever becomes annoying, upgrade to an **event-driven**
bridge implementing `IAudioEndpointVolumeCallback` (`#[implement]` from the
`windows` crate). Windows then pushes changes with an **event-context GUID**,
letting us cleanly ignore our *own* re-mute writes and shrink the blip window to
near zero. It is more COM/lifetime complexity for a volume knob, so it is
deferred. The `VolumeBridge` public interface (start → `Receiver<VolumeEvent>` →
drop-restore) is designed so this swap is internal and does not change callers.

## Client integration

- `stream_audio_buffered_multi` gains an optional `Receiver<VolumeEvent>`
  parameter (the realtime `capture` path too, if used).
- The main streaming loop already polls per iteration (reconnect handles,
  `/feedback` keepalive). It also **drains volume events** and calls
  `set_volume(db)` on every live receiver.
- **Rejoiners:** `finish_reconnect` applies the current mirrored volume so a
  reconnecting receiver matches the others (extend the existing `volume_db`
  plumbing to carry the live mirrored value, not just the CLI `--volume`).
- **`--volume` interaction (per earlier decision):** `--volume` seeds the
  **initial** AirPlay volume; the **first** `VolumeEvent` hands control to
  Windows from then on. (Initial-only default → first-touch handover.)

## CLI

- New flag `--handoff`, valid **only** with `capture` on **Windows**.
- Hard error (clear message, non-zero exit) if combined with `play`/`tone`, or on
  non-Windows targets. `#[cfg(not(windows))]` compiles the flag to a friendly
  "not supported on this platform" error.
- Composes with existing flags (`--buffered`, `--latency`, `--offset`,
  multi-room). With `--handoff`, `--volume` is the initial seed only.

## Restore & failure handling

- Original `(scalar, muted)` is restored on **normal exit and Ctrl+C**, via the
  `VolumeBridge`'s `Drop` (the CLI already has a Ctrl+C path that unwinds the
  stream; the bridge is dropped there).
- If Core Audio init/activation fails, **warn and stream normally** — the feature
  degrades *off* and must never kill an otherwise-working stream.
- If Phase 0 (loopback-survives-mute) fails, the feature is not viable as
  designed; do not ship the mute step.

## Testing

**Unit (pure, no COM):**
- `scalar → dBFS` mapping: `1.0 → 0`, `0.5 → ~-6`, `0.0316 → ~-30` (clamp floor),
  `0.0 → -144`.
- The diff/handover state machine: given a sequence of `(scalar, muted)` samples,
  assert the emitted `VolumeEvent`s and the re-assert-mute decisions
  (scalar-changed vs mute-toggled-by-user classification).

**Hardware:**
1. **Phase 0** — loopback delivers non-silent frames while endpoint is muted.
2. Speakers stay silent while AirPlay plays (with `--handoff`).
3. Slider / volume keys move the AirPlay volume; ~50 ms lag acceptable.
4. Mute key toggles AirPlay mute; speakers stay silent.
5. Original volume + mute restored after Ctrl+C.
6. Multi-room: all receivers track the mirrored volume together, including a
   reconnecting receiver.

## Rejected approaches

- **B now (event-driven callback):** correct long-term but more COM complexity
  than warranted for v1. Kept as a documented upgrade path.
- **C (virtual audio device):** cleanest decoupling and an honest mute indicator,
  but requires bundling/installing a **signed kernel audio driver** — out of scope
  for v1.
- **Per-session mute** (mute every render session via `ISimpleAudioVolume`):
  rejected — per-session mute happens *before* the mix, so it would also silence
  the loopback capture, killing the stream.
- **Silence via endpoint volume `scalar = 0`** instead of mute: rejected — the
  scalar is the very knob the user drags, so we'd be fighting the user and the
  slider would snap back to 0 on every adjustment.

## Out of scope (v1)

- Non-Windows platforms (Linux/macOS handoff).
- `play` / `tone` handoff (no local speaker output to silence there).
- Per-receiver independent volume mirroring (one mirrored volume for the group).
