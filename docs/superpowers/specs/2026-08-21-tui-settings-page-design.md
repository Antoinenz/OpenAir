# D — Settings Page: Design

**Date:** 2026-08-21
**Status:** approved, awaiting implementation plan
**Depends on:** B (unified TUI flow) for the `Screen` state machine; C for
`show_controls`, which is currently editable only by hand-editing
`settings.json` and is the reason this project exists now rather than later.

## Goal

A settings surface inside the TUI, reachable both before and during a stream,
where every setting that *can* apply to a running stream *does*.

## Scope

Five rows, all of them existing `Settings` fields:

| Row | Field | Range |
|---|---|---|
| handoff | `handoff: bool` | on / off, forced off when no cable is present |
| latency | `latency_ms: u64` | 100–2000, 50 ms steps |
| volume | `volume_db: f32` | −60–0, 1 dB steps |
| metadata | `metadata: bool` | on / off |
| show controls | `show_controls: bool` | on / off |

No new persisted fields, so `CURRENT_VERSION` stays at 2.

**Out of scope:** `--bind`, `--handoff-device`, `--buffered`, `--log`,
`--debug`. These are either machine-specific setup (bind address, cable choice)
or diagnostics, and none benefit from a live toggle. They stay as flags.

Per-receiver trim and offset stay on the dashboard rows where they already are.
They are properties of a receiver, not of the program.

## 1. Presentation: an overlay, not a page

`s` opens the settings overlay from the picker and from the dashboard. `Esc` or
`s` closes it, returning to the screen it was opened from. A centred panel drawn
with `Clear` over the live frame, reusing `rect::centred` and matching the
add-receiver overlay the dashboard already has.

**Why an overlay rather than a full screen.** On the dashboard the buffer bars
stay visible underneath, so the user watches headroom react while dragging the
latency. That feedback loop is what makes a latency control comprehensible; a
full-screen page would hide the one thing being tuned against.

It also keeps one implementation for both entry points. A settings *page* from
the picker and a settings *overlay* from the dashboard would be two renderers
that must be kept in agreement forever.

### Keys

| Key | Does |
|---|---|
| `↑` / `↓` | move between rows |
| `←` / `→`, `<` / `>` | adjust the highlighted row |
| `space`, `⏎` | toggle a boolean row |
| `s`, `Esc` | close |

Adjustment keys are doubled (`←→` and `<>`) because `<>` already means "adjust"
on both the picker (latency) and the dashboard (offset), and arrows are what
anyone tries first in a list of values.

## 2. Applying changes: the applier closure

Some settings need platform work — switching a Windows audio endpoint — that
`openair-tui` deliberately cannot reach. `openair-tui` does not depend on
`openair-capture`, and that boundary is worth keeping: it is what lets the TUI
compile and be tested on any platform.

The CLI therefore supplies a closure, exactly as it already does for
`StreamLauncher`:

```rust
/// Apply a settings change that needs platform work. Receives the settings in
/// force before the change and after it, so the applier can act only on what
/// actually differs.
pub type SettingsApplier<'a> =
    Box<dyn FnMut(&Settings, &Settings) -> Result<(), String> + 'a>;
```

Called on the main thread, where `CaptureRig` already lives.

**Only while a stream is running.** Opened from the picker there is no capture
to rebuild and no group to re-anchor, so a change is simply stored. This follows
the rule established when handoff was moved to engage at stream start rather
than at launch: before a stream, `handoff` is a *preference*, and engaging it
early would silence the speakers while the user is still choosing receivers.
The picker's overlay therefore never calls the applier, and its own `h` and `<>`
keys keep working exactly as they do today.

**Why the main thread is available.** `cpal::Stream` is `!Send`, so
`SystemCapture` — and the `CaptureRig` holding it, and the `StreamLauncher`
closure holding *that* — never leave the thread that created them, which is the
TUI's own thread. Capture can therefore be rebuilt in place, without a worker
or a channel.

**Rejected alternatives.**

- *A command inbox on `StreamStats`, like `StreamCommand`.* Reuses existing
  machinery, but that inbox is drained by the **stream** thread while capture
  must be rebuilt on the **main** thread. It would need a second,
  differently-owned queue regardless; the reuse is only apparent.
- *Moving handoff into `openair-client`.* Removes the boundary problem by
  removing the boundary. Wrong layer: handoff is a Windows endpoint concern and
  `client` is protocol code, so this drags `windows-rs` into the streaming path
  for every platform.

## 3. Live handoff

### Ordering: start the new capture before stopping the old

Turning handoff **on** switches the Windows default output to the virtual cable;
turning it **off** restores the previous device. In both directions the outgoing
and incoming captures are loopbacks on **different endpoints**, so they may
briefly overlap. The order is therefore:

1. Engage the handoff session, or restore the previous default device.
2. Start the new capture, feeding the **existing** ring.
3. Only now drop the old capture.

This is chosen for its **failure** behaviour more than for the gap. If step 2
fails, the old capture is still running: the setting is left unchanged and the
reason is reported on the row. Tearing down first would strand the user with no
audio source and nothing to fall back to — a silent stream caused by a settings
keystroke.

### Feeding the existing ring

```rust
impl SystemCapture {
    /// Start capture writing into a ring that already exists, so a device
    /// change does not require the consumer to be rebuilt.
    pub fn start_on_ring(
        name_filter: Option<&str>,
        ring: Arc<Mutex<VecDeque<i16>>>,
    ) -> Result<Self, CaptureError>;
}
```

`SystemCapture::start_on` becomes a thin wrapper that allocates a ring and
delegates.

### The sample-rate hazard

`CaptureSource::new(ring, rate, …)` currently takes `rate: u32` **by value**,
captured once at stream start. Speakers at 48 kHz and a virtual cable at 44.1 kHz
are different rates. Swapping the producer underneath a source that still
believes the old rate does not glitch — it resamples at the wrong ratio, which
is a **pitch shift**, and it would be easy to misdiagnose as a receiver problem.

Fix: `rate` becomes `Arc<AtomicU32>`, re-read per block by `CaptureSource`.

The ring is **cleared at the swap, by the applier**, between starting the new
capture and dropping the old — not inside `start_on_ring`, which has no way to
know whether it is replacing a producer or starting the first one. Without the
clear, the ring briefly holds samples captured at two different rates and the
tail is resampled at the new ratio: a short pitch artifact, where clearing gives
a clean short gap instead.

Sequenced against §3's ordering, the full swap is: engage or restore the
endpoint → start the new capture on the shared ring → clear the ring → publish
the new rate to the atomic → drop the old capture. The rate is published *after*
the clear so no block is ever resampled with a ratio that does not match the
samples in front of it.

This is worth doing independently of this feature: today `--handoff` assumes the
cable's rate matches whatever the source was built with, and nothing verifies
it.

## 4. Live latency, volume and metadata

Three new variants on the existing `StreamCommand` inbox, drained by the stream
loop about every 23 ms — well inside one frame of a keystroke:

```rust
SetLatency { ms: u64 },
SetMasterVolume { db: f32 },
SetMetadataEnabled { on: bool },
```

**Latency.** Reuses the re-anchor block auto-latency already performs, lifted
into a shared function so both paths anchor identically. Both directions are
allowed. Lowering re-anchors *shallower* and may underrun immediately, at which
point auto-latency raises it back — self-correcting, and visible in the log
panel and the buffer bars. Restricting the control to raising would hide a
capability to prevent a failure the system already handles.

**Volume.** The stream loop already computes each receiver's level as
`effective_volume_db(master, trim)`. Only the master needs to become mutable.
Per-receiver trims are relative and so survive a master change untouched, which
is the property `--handoff`'s volume mirroring already relies on.

**Metadata.** The watcher keeps running; the stream gates *sending* on the flag.
Swapping the `mpsc::Receiver` that the running stream owns would be
substantially harder for no user-visible gain.

**Accepted behaviour change:** with metadata switched off, the SMTC watcher
stays hooked rather than being torn down. Nothing is transmitted. This differs
from `--no-metadata`, which still never starts the watcher at all.

## 5. Error handling

Every failure surfaces **on the row that caused it** and leaves the setting at
its previous value. The rules:

- The applier returns `Err(String)`; the settings screen shows it against the
  row and reverts the field.
- A reverted setting is never written to `settings.json`. The file records what
  is actually in force.
- `StreamStats::send` returning `false` (poisoned mailbox) is reported the same
  way, rather than the key silently doing nothing.
- Handoff stays force-disabled when no virtual cable is present, exactly as the
  picker already does. The row shows the same explanation the picker's `h` key
  gives.

## 6. Persistence

Unchanged in mechanism: the TUI's own toggles write `settings.json`; CLI flags
override for one run and never write back.

One clarification this project forces. A flag such as `--latency 300` overrides
the file for the run. If the user then edits latency on the settings page, the
new value **is** written — the user has expressed a preference more recently and
more explicitly than the command line did. The override applies at startup, not
as a permanent lock.

## 7. Testing

| Area | Tests |
|---|---|
| Screen state | Row navigation, clamping at both ends of each range, booleans toggling, close returns to the originating screen |
| Applier | A fake applier records calls and can be told to fail: asserts only-what-changed is passed, a failure reverts the field and shows the reason, and a reverted field is not saved |
| Render | Overlay draws at a range of terminal sizes without panicking; the dashboard is still visible underneath |
| `StreamCommand` | The three new variants round-trip through the inbox in order |
| `CaptureSource` | Rate read from the atomic per block: changing it mid-read changes the resample ratio |
| `SystemCapture` | `start_on_ring` writes into the ring it was handed |

The closure is what makes the interesting half testable without hardware —
"handoff failed, the row explains why, the setting stayed put" becomes a unit
test rather than an unplugging ritual.

**Hardware, by hand:** the actual endpoint switch mid-stream. Toggle handoff
while music plays and confirm audio resumes on the other path, that per-receiver
trims are preserved, and that pitch is correct when the two devices run at
different sample rates (the case §3 exists for).

## Self-review notes

- §3's ordering and §5's revert rule are the same decision seen from two sides:
  never destroy the working configuration before the replacement is proven.
- The rate hazard (§3) is the one part of this that is a **bug fix wearing a
  feature's clothes**. If this project were cancelled, that change should still
  be made.
- §6's precedence clarification is a real decision, not documentation of an
  existing one. It has no test above it because it is a statement about what
  writing means; the persistence tests already cover the mechanism.
