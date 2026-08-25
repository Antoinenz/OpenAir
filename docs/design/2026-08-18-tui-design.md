# OpenAir TUI — Design

**Status:** approved design, phase 1 not yet implemented
**Date:** 2026-08-18

## Problem

Running bare `openair` today blocks for a fixed 5 s mDNS browse, then attempts
Transient pairing and `GET /info` against *every* device it found. On a home
network that is merely slow. On a shared network (the user tried it at school)
it is both slow and rude: we open pairing handshakes with strangers' devices
that we have no intention of streaming to.

Streaming itself is driven entirely by flags. Choosing receivers, setting
latency, enabling `--handoff` and watching the stream's health all happen
through a command line and a scrolling log. There is no way to see buffer
headroom, to know why the latency just stepped up, or to change anything
without killing the process and retyping.

## Goal

Make the terminal front end interactive: a fast device picker and a live
dashboard, with the plain CLI still available via `--no-tui`.

A full GUI is wanted eventually and is a much larger cross-platform job. The
TUI is not a stepping stone to it — it is a genuinely nice way to use the tool,
and worth keeping afterwards.

## Scope

**Phase 1 (this spec):** device picker + read-only dashboard. Phase 1 touches
no streaming internals; it only observes them.

**Phase 2 (designed for, not built):** per-receiver volume and latency
controls, add/remove receivers mid-stream. The seam that makes this cheap is
described under "Designed-for extension" below.

Out of scope entirely: mouse support, themes, config editing beyond the
picker's toggles, and any GUI work.

## Decisions

Settled during brainstorming:

1. **TUI is the default** for interactive runs. `--no-tui` restores today's
   plain-text behaviour, and the TUI auto-disables when stdout is not a
   terminal.
2. **The picker never touches the network beyond mDNS.** No probing, no
   pairing, until the user presses Enter.
3. **Global preferences persist; selection does not.** Receivers are chosen
   fresh each run.
4. **The graph plots buffer health** by default, with a key to switch it to
   bandwidth.
5. **Numbers travel by shared snapshot, events by tracing layer.**
6. **The TUI is a library crate**, not a second binary.

## Architecture

### Crate layout

`apps/tui` is currently a 12-line stub binary. Since `openair` itself must open
the TUI, the code belongs in a library:

- **Move** `apps/tui` → `crates/tui` (lib crate `openair-tui`), delete the stub
  `main.rs`.
- `apps/cli` gains a dependency on `openair-tui`.

This matches the existing convention: `crates/` holds libraries, `apps/` holds
binaries. `openair` stays the single binary.

`crates/tui` depends on `ratatui`, `crossterm`, `openair-client`,
`openair-discovery`, `openair-core`, `tracing`. It must **not** depend on
`openair-capture` — platform-specific concerns (handoff, SMTC) stay behind the
CLI's existing `#[cfg(windows)]` boundaries, and the TUI receives their results
as plain data.

### File structure

```
crates/tui/src/
  lib.rs        # public entry points: run_picker(), run_dashboard()
  picker.rs     # device list screen: state, input handling, layout
  dashboard.rs  # streaming screen: layout, sparkline, receiver table
  logs.rs       # tracing Layer -> bounded ring buffer, shared with the panel
  settings.rs   # settings.json load/save/merge with CLI flags
  term.rs       # raw mode / viewport setup, restore guard, panic hook
crates/client/src/stats.rs   # StreamStats — the shared snapshot
```

Each file has one responsibility and can be tested without the others.
`settings.rs` and `logs.rs` in particular are pure logic with no terminal
involvement, so they get ordinary unit tests.

### The seam: how the TUI sees inside the stream

`stream_audio_buffered_multi` is a blocking function that owns everything and
returns only when the stream ends. The TUI runs it on a worker thread and
renders on the main thread. Two independent channels carry information out:

**Numbers — shared snapshot.** A new `openair_client::StreamStats`, held as an
`Arc` by both sides. The stream loop overwrites current values as it goes; the
TUI samples them at render rate and builds its own history for the sparkline.
The stream keeps no history and has no opinion about display; the TUI keeps no
protocol state.

```rust
/// Live view of a running buffered stream. Written by the stream loop,
/// read by any observer (the TUI today). All fields are "current value",
/// never a history — observers sample at whatever rate they render.
pub struct StreamStats {
    /// Current anchor lead in ms, as raised by auto-latency.
    pub latency_ms: AtomicU64,
    /// Smallest headroom seen since the last sample, in ms; may be negative.
    /// Reset to i64::MAX by the reader after each read.
    pub min_lead_ms: AtomicI64,
    /// Total payload bytes sent, monotonic. Readers difference it over time
    /// to get a rate, so the stream never has to know the sampling interval.
    pub bytes_sent: AtomicU64,
    /// Wall-clock ns when the stream began sending.
    pub started_at_ns: AtomicU64,
    /// Per-receiver state, in the order given to the stream.
    pub receivers: Mutex<Vec<ReceiverStat>>,
    /// Most recent now-playing bundle, if metadata is enabled.
    pub now_playing: Mutex<Option<NowPlaying>>,
}

pub struct ReceiverStat {
    pub name: String,
    pub addr: SocketAddr,
    pub state: ReceiverState,
    pub offset_ms: i64,
}

pub enum ReceiverState { Connected, Reconnecting, Dead }
```

`min_lead_ms` deserves a note. The loop already computes exactly this number
every packet (`lib.rs`, the `play_deadline_ns(...) - ptp_now_ns()` line that
feeds auto-latency) and it is **group-wide, not per-receiver** — the anchor
line is shared across the group. So the sparkline is one series for the whole
stream, not one per receiver. The stream writes the running window minimum; the
reader resets it after sampling, which makes each sample "worst case since you
last looked" rather than an instantaneous value that could miss a dip between
frames.

The loop iterates once per AAC packet — 1024 frames at 44.1 kHz, about 43 times
per second. That is far too slow for the choice of mechanism to matter for
performance, so this is a clarity decision, not an efficiency one.

**Events — tracing layer.** The discrete things worth seeing (underrun,
receiver dropped, reconnect succeeded, metadata rejected) are *already* `warn!`
and `debug!` lines. Rather than duplicate them into the snapshot, the logs
panel reads them where they already are: a `tracing` Layer holding a bounded
`VecDeque<LogLine>` behind a `Mutex`, capacity ~500 lines, oldest dropped.

When the TUI is active this layer **replaces** the console `fmt` layer.
Otherwise stray log writes scribble over the rendered frame. The `--log` file
layer is untouched and keeps full detail, so `--debug 2 --log` still produces
the complete file while the panel shows a readable tail. `--debug` continues to
control the panel's verbosity exactly as it controls the console's today.

This split is the heart of the design: **numbers by snapshot, events by
tracing** — each on the mechanism already suited to it. The snapshot never
grows, and the log panel needed no new plumbing.

### Rejected alternatives

**Telemetry channel** (loop emits a `StreamEvent` enum, TUI folds them into
state). Faithful ordering, and a natural fit for discrete events. Rejected
because the TUI would have to reconstruct current state by replaying an event
log, and a stalled renderer either grows the queue unboundedly or drops events
silently — a poor failure mode for the panel whose entire job is telling you
what went wrong. The discrete events it is good at arrive via tracing anyway.

**Invert control** (refactor the loop into a `BufferedStream` with a `tick()`
the TUI drives). Most flexible, and phase-2 controls would fall out naturally.
Rejected because it restructures the single piece of code that took longest to
get working — the buffered loop, PTP anchoring and reconnect handling — for a
benefit phase 2 can obtain far more cheaply by adding one inbox field.

## Screens

### Picker

Shown by bare `openair`, and by any streaming command that names no receiver.

```
┌ OpenAir ─────────────────────────────────────────────────┐
│  [x] Living Room        Apple TV 4K      192.168.1.106   │
│  [ ] Pool Room          Shairport        192.168.1.51    │
│> [x] Kitchen            HomePod mini     192.168.1.88  ! │
│  [ ] Office Display     Apple TV HD      192.168.1.23    │
│                                          searching…  4   │
├──────────────────────────────────────────────────────────┤
│  handoff  ON (CABLE Input)   latency 500ms   vol -8dB    │
│  ↑↓ move  space select  h handoff  +/- latency  ⏎ start  │
└──────────────────────────────────────────────────────────┘
```

Behaviour:

- **Incremental.** Devices appear as they answer. There is no fixed wait — the
  first results land well under a second and the list keeps filling while you
  read it. Discovery runs until you press Enter.
- **mDNS only.** Name, address, model and capability flags all come from the
  TXT record we already parse (`AirPlayTxt`: `model`, `features`,
  `status_flags`). The paired indicator comes from the local `pairings.json`.
  Nothing is contacted.
- **`!` marker** flags a device that will need `openair pair` first — derived
  from `status_flags` (PIN required) with no pairing record on disk. Pressing
  Enter with such a device selected explains this rather than failing obscurely.
- **Sorted** by paired-first, then name, so your usual speakers stay at the top
  as strangers' devices trickle in.
- Selecting two or more receivers implies the buffered pipeline, exactly as the
  CLI does today.

Keys: `↑`/`↓` move, `space` toggle selection, `h` toggle handoff, `+`/`-`
adjust latency in 50 ms steps, `Enter` start, `q`/`Esc` quit.

The handoff toggle defaults to ON on Windows when a virtual cable is detected
(reusing `capture::handoff::select_device`), OFF otherwise. Turning it off
persists — see Settings.

### Dashboard

```
┌ latency ────┐┌ bandwidth ──┐┌ now playing ─────────────────┐
│  500 ms     ││  128 kbit/s ││  Home  —  Bon Iver           │
│  auto +0    ││  1.2 MB     ││  22, A Million               │
└─────────────┘└─────────────┘└──────────────────────────────┘
┌ buffer health (ms ahead) ─────────────────────── [b] ─────┐
│      ▄▆█▇▆▅▆▇█▇▆▄▃▂▃▅▆▇█▇▆▅▆▇▆▅▄▅▆▇█▇▆▅▄▃▄▅▆▇█▇▆▅        │
│  min 180ms                                    now 340ms   │
└───────────────────────────────────────────────────────────┘
┌ receivers ────────────────────────────────────────────────┐
│  Living Room     192.168.1.106:7000   connected           │
│  Pool Room       192.168.1.51:7000    reconnecting…       │
└───────────────────────────────────────────────────────────┘
┌ logs ─────────────────────────────────────────────────────┐
│ 12:04:31 WARN  underrun risk — raising latency 500→750    │
│ 12:04:33 INFO  Pool Room reconnected                      │
└───────────────────────────────────────────────────────────┘
```

- Render at 10 Hz. Redraw only on tick or key, never per audio packet.
- **Bandwidth** is derived: difference `bytes_sent` between samples, divide by
  elapsed. Shown as a rate plus a session total.
- **Buffer health** sparkline holds ~120 samples (12 s at 10 Hz). `b` switches
  the same panel to bandwidth history. The title shows which is active.
- **Now playing** shows title/artist/album. Cover art is not rendered — pixel
  protocols vary too much across terminals to be worth it; the panel notes
  `[art]` when artwork was sent.
- **Logs** shows the tail of the ring buffer, newest at the bottom.
- Keys: `b` switch graph, `q`/`Ctrl+C` stop, `↑`/`↓` scroll logs.

## Terminal handling

- **Picker:** inline viewport (`ratatui::Viewport::Inline`), ~12 rows. It
  behaves like a prompt, leaves scrollback intact, and the chosen result stays
  printed afterwards.
- **Dashboard:** alternate screen. It is a mode you are in, and it needs the
  room. On exit the terminal is restored and a one-line summary is printed to
  normal scrollback (duration, receivers, final latency, underrun count) so the
  run leaves a trace.

Both go through `term.rs`, which owns raw-mode entry and provides a restore
guard plus a `std::panic::set_hook` wrapper. A panic mid-render must not leave
the user with a dead terminal — the hook restores cooked mode and leaves the
alternate screen *before* printing the panic message.

Ctrl+C is handled as a key event inside the TUI rather than as a signal, so
shutdown runs the existing graceful path: play out the queued audio, TEARDOWN
each session, and — critically — restore the Windows default audio device if
`--handoff` changed it. The existing `ctrlc` handler stays registered as a
backstop for `--no-tui` runs.

## Settings

`%APPDATA%\OpenAir\settings.json` on Windows, `$XDG_CONFIG_HOME/openair/` on
Linux — beside the existing `pairings.json`.

```json
{
  "version": 1,
  "handoff": true,
  "latency_ms": 500,
  "volume_db": -8.0,
  "metadata": true,
  "graph": "buffer"
}
```

Precedence, strictly: **CLI flag > settings file > built-in default.** A flag
overrides for that run only and never rewrites the file. Only the TUI's own
toggles write it, on exit.

Rules:
- Selected receivers are **not** persisted. Devices come and go; a stale
  pre-selection that silently streams somewhere unexpected is worse than
  picking each time.
- A missing file means all defaults — never an error.
- A corrupt or future-`version` file is logged at `warn` and ignored in favour
  of defaults. Settings are a convenience; they must never block a stream.
- `handoff: true` on a machine with no virtual cable still yields an off
  toggle, since detection fails. The preference is remembered, not the outcome.

## Error handling

- **No devices found.** The picker shows an empty list with a hint, and keeps
  searching. It never exits on its own.
- **Enter with nothing selected** is a no-op with a hint line.
- **Enter on an unpaired PIN-required device** shows `run: openair pair
  "<name>"` rather than starting a stream that will fail during handshake.
- **Stream thread fails to start** (pairing rejected, unreachable): the TUI
  leaves the dashboard, restores the terminal, and prints the error to normal
  output. A failure at startup should read like a CLI failure, not be buried in
  a panel that then vanishes.
- **Stream fails mid-run:** existing behaviour is unchanged — a dropped
  receiver goes to background reconnect and shows `reconnecting…`. If every
  receiver dies the stream returns, and the TUI exits with the summary.
- **Terminal too small** (< 60×20 for the dashboard): render a single centred
  message asking for a larger window; resume on resize.

## Testing

Terminal rendering is awkward to assert on, so the design pushes logic out of
the render path:

- `settings.rs` — load/save round-trip, missing file, corrupt file, unknown
  version, flag-overrides-file precedence. Pure, table-driven.
- `logs.rs` — ring buffer bounds (capacity enforced, oldest dropped), and that
  the layer records the fields the panel formats.
- `picker.rs` — sort order (paired first, then name), incremental insert
  without duplicates when mDNS re-announces a device, selection toggling,
  latency clamping at the `+`/`-` bounds. All against a plain state struct with
  no terminal.
- `dashboard.rs` — bandwidth derivation from two `bytes_sent` samples,
  including the first-sample case and a wrap; sparkline history bounds.
- `StreamStats` — read-and-reset of `min_lead_ms` returns the window minimum
  and rearms.
- ratatui's `TestBackend` renders one dashboard frame against a fixed
  `StreamStats` to catch layout panics and the too-small-terminal path. One
  smoke test, not a snapshot suite — golden-frame tests would break on every
  cosmetic tweak and teach us nothing.

Manual verification (needs hardware): picker on a busy network confirms no
pairing traffic leaves the machine until Enter; dashboard during a real
underrun confirms the sparkline dips before auto-latency steps up.

## Discovery change

`openair_discovery::browse` blocks for a fixed timeout and delivers devices
through a callback. The picker needs discovery that runs *while* the user
interacts, so `discovery` gains:

```rust
/// Browse until the returned handle is dropped. Devices arrive on the
/// channel as they answer; unlike `browse`, this never blocks the caller.
pub fn browse_live() -> Result<BrowseHandle, mdns_sd::Error>;

pub struct BrowseHandle {
    pub devices: std::sync::mpsc::Receiver<AirPlayDevice>,
    // shuts the daemon down on drop
}
```

`browse` stays as-is — it is what `--no-tui` and the scripted paths use — and
is reimplemented on top of `browse_live` so there is one browsing code path.

## Designed-for extension (phase 2)

Per-receiver volume and latency, and add/remove mid-stream, need commands to
travel *into* the loop. The precedent already exists: `volume_rx` and
`metadata_rx` are channels into this same loop, and `reap_dead` plus
auto-latency already mutate receiver state mid-stream.

Phase 2 adds one field to the struct that already crosses the boundary:

```rust
pub struct StreamStats {
    // ... phase 1 fields ...
    /// Commands awaiting the stream loop; drained once per packet.
    pub inbox: Mutex<Vec<StreamCommand>>,
}

pub enum StreamCommand {
    SetVolume { receiver: usize, db: f32 },
    SetOffset { receiver: usize, ms: i64 },
    Add(GroupTarget),
    Remove(usize),
}
```

The loop drains `inbox` where it already calls `drain_latest_volume`. `Add`
reuses the existing reconnect path, which already builds a session and anchors
it to the live group — adding a receiver mid-stream is the same operation as
recovering one. No phase-1 decision needs revisiting to get there.

## Success criteria

1. Bare `openair` shows a usable device list in under a second and sends no
   pairing traffic until Enter.
2. `--no-tui` reproduces today's behaviour exactly; non-TTY stdout selects it
   automatically.
3. A streaming run renders latency, bandwidth, now-playing, a buffer-health
   sparkline, per-receiver state and a log tail, at 10 Hz, without affecting
   audio.
4. Turning handoff off in the picker survives a restart.
5. Ctrl+C from the dashboard restores the terminal and the Windows default
   audio device, and plays out queued audio, exactly as the CLI does today.
6. A panic during render never leaves the terminal in raw mode.
