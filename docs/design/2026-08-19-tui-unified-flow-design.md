# OpenAir TUI — Unified Flow

**Status:** approved design, not yet implemented
**Date:** 2026-08-19
**Follows:** `2026-08-18-tui-design.md` (phases 1 and 2, both shipped)

## Problem

The TUI is currently two islands with the command line in between. Choosing
receivers happens in a full-screen picker; then the terminal is restored, the
CLI prints plain text while sessions are established, and a second full-screen
app starts. Anything that needs input during that gap — a pairing PIN — drops to
a `stdin` prompt from another era of the program.

The visible symptoms:

- The screen tears down and rebuilds between choosing and streaming.
- A receiver that fails to connect is dropped silently; the reason appears only
  in the log panel, after the dashboard has already opened.
- If *every* receiver fails, the stream returns an error and the process exits
  to a shell, with no way back to the picker short of running the command again.
- Pairing cannot happen from the picker at all. A device needing it is flagged
  and refused, and the user has to quit and run `openair pair`.

## Goal

One continuous terminal application from launch to exit. Every state the
program can be in — searching, pairing, connecting, streaming, failed — is a
screen, and the user is never returned to a bare shell except by quitting.

## Decisions

Settled during brainstorming:

1. **Pairing happens after the user confirms**, not before. The picker stays
   network-silent; pairing is part of the connect flow.
2. **A receiver that fails to connect stays visible in red**, with the reason,
   and is retried only when the user asks (`r`). No automatic retry — a device
   that is asleep or on another network fails identically ten seconds later, and
   silent repeated attempts are the unsolicited-connection behaviour the picker
   was built to avoid.
3. **Explicit CLI commands use the same flow.** `openair capture "Living Room"`
   skips the picker but still gets the connecting screen, in-TUI pairing and the
   dashboard. `--no-tui` remains the plain-text path.
4. **Partial success proceeds.** If some receivers connect, streaming starts and
   the failed ones are listed in red. Only if *none* connect do we return to the
   picker with a readable error.

## Architecture

### The threading model inverts

Phase 1 ran the dashboard on a worker thread and let the stream own the main
thread. That was deliberate: nothing in the audio path had to become `Send`.

A continuous TUI cannot work that way. The TUI owns the terminal across screens
that exist *before* any stream does, so it must own the main thread for the
program's whole life, and every blocking operation moves to a worker:

| Work | Thread | Reports back via |
|---|---|---|
| All screens, all input, all rendering | main | — |
| mDNS discovery | worker (existing `browse_live`) | channel |
| Pairing (`pair_setup` with PIN) | worker | channel |
| Streaming | worker | `Arc<StreamStats>` |

**The `Send` requirement this creates is the project's main risk**, and it is
checked by the compiler rather than discovered at runtime. Inspection suggests
it is already satisfied: `CaptureSource` holds only `Arc`s and primitives, and
`HandoffSession` already confines its COM objects to a worker of its own. The
handoff guard and the `SystemCapture` handle stay on the main thread regardless
— only the `AudioSource` moves.

### Connection progress rides the existing seam

`stream_audio_buffered_multi` establishes every receiver's RTSP session inside
itself, at the start. Three ways to surface that:

**A — Report progress through `StreamStats`** *(chosen)*. The setup loop
publishes each receiver's state as it goes (`Connecting` → `Connected` or
`Failed`). The TUI's connecting screen renders the same snapshot the dashboard
already reads. Retrying a failed receiver is the `Add` command that already
exists.

**B — Move connection out of the stream.** The TUI establishes sessions and
hands them over. Rejected: the stream re-establishes receivers on reconnect, so
it needs the connect path anyway — connection logic would live in two places, or
in a shared module both call, and that is a large refactor of the code that took
longest to get right for no behavioural gain.

**C — Pre-flight validation, then connect for real.** Rejected outright: it
doubles the connection work, and a receiver that validated can still fail at
stream time, so the failure UI is needed regardless. Worst of both.

A wins because the seam already exists and is proven. The change to the stream
is a handful of `set_receivers` calls in a loop that already iterates the
targets.

### Screens

```
        ┌──────────┐  confirm   ┌─────────┐  needs PIN  ┌─────────┐
        │  Picker  │───────────>│ Pairing │<───────────>│  (PIN)  │
        └──────────┘            └─────────┘             └─────────┘
             ^                       │ paired / not needed
             │ none connected        v
             │                  ┌────────────┐  ≥1 connected  ┌───────────┐
             └──────────────────│ Connecting │───────────────>│ Streaming │
                                └────────────┘                └───────────┘
```

```rust
enum Screen {
    Picker(PickerState),
    Pairing(PairingState),
    Connecting(ConnectingState),
    Streaming(DashboardState),
}
```

One `App` owns the current screen, the terminal, and the shared handles
(`LogBuffer`, settings). Each screen keeps the shape phases 1 and 2 established:
a state struct with `on_key` and pure logic, and a `render` function beside it.

**Picker** — unchanged except that `needs_pairing` no longer blocks confirming.
Selecting a device that needs pairing is now allowed; confirming routes to
Pairing rather than refusing.

**Pairing** — one device at a time, in the order selected. Shows the device
name, a four-character PIN field, and the state of the exchange. On success the
credential is persisted (existing `PairingStore`) and the next device needing
pairing is offered. `Esc` skips *that device* — it is then treated as a failed
receiver rather than aborting everything, so one un-pairable speaker doesn't
cost the user the rest of the group.

**Connecting** — a header naming what is being waited on, a spinner, and one
line per receiver showing `connecting… / connected / failed: <reason>`. `Esc`
cancels: sets the stream's `stop` flag and returns to Picker. Transitions to
Streaming as soon as at least one receiver is connected *and* none are still
pending; returns to Picker with an error banner if all failed.

**Streaming** — today's dashboard, plus failed receivers rendered in red with
their reason, `r` to retry one and `d` to dismiss it.

### Failure representation

`ReceiverState` gains two variants:

```rust
pub enum ReceiverState {
    Connecting,
    Connected,
    Reconnecting,
    /// Never connected, or gave up. Carries why, for the list.
    Failed,
    Dead,
}
```

`ReceiverStat` gains `error: Option<String>` — the reason, phrased for a person.
Where the existing code already turns an OS error into a hint (the
`connection_hint` path that suggests `--bind`), that hint is what belongs here;
the raw `10054` belongs in the log panel.

## CLI integration

`main()` stops branching between "picker" and "named receivers" and instead
chooses the TUI's *entry screen*:

- no receivers named → start at `Picker`
- receivers named → resolve them, start at `Connecting` (or `Pairing` first)
- `--no-tui` or non-terminal stdout → today's plain-text path, unchanged

The stream is started by the App, not by `main`, since the App decides when a
retry or an add warrants one.

## Error handling

- **Discovery fails to start** — Picker renders the error in place and offers
  retry; it does not exit.
- **Pairing rejected (wrong PIN)** — stay on the Pairing screen, clear the
  field, show the failure and the attempts remaining. The receiver's PIN
  changes on its screen, so the message says to re-read it.
- **Pairing fails for another reason** — treated as that receiver failing;
  continue to the next.
- **All receivers fail** — back to Picker with a one-line summary, selections
  preserved so the user can retry without re-picking.
- **Stream ends by itself** (source exhausted, every receiver lost) — leave the
  TUI cleanly and print the existing summary.
- **Terminal too small** — as today, on every screen rather than just the
  dashboard.

The rule inherited from phase 2 holds throughout: a failure in the UI layer must
never take the stream down.

## Testing

The pattern from phases 1 and 2 — logic in a state struct, rendering separate —
keeps this testable without a terminal:

- **Screen transitions** — a table-driven test over
  `(screen, event) → next screen`: confirm with a device needing pairing goes to
  Pairing; all-failed goes back to Picker; one-of-two-failed goes to Streaming.
  This is the heart of the project and deserves the most tests.
- **`PairingState`** — PIN accumulates digits, backspace, rejects non-digits,
  submits at four; a rejected PIN clears the field and decrements attempts;
  `Esc` marks that device skipped rather than aborting.
- **`ConnectingState`** — derives "still waiting" / "ready to stream" / "all
  failed" from a set of `ReceiverStat`s. Pure function of the snapshot.
- **Failure text** — a receiver whose connect failed shows the hint, not the
  raw OS error.
- One `TestBackend` render per new screen, for layout panics only.

Manual, needs hardware: pairing an Apple TV entirely inside the TUI; pulling a
receiver's power mid-connect to see the red state and `r` retry.

## Out of scope

Deliberately not in this project, each its own piece of work:

- **Device list presentation** — pretty model names, the ready button, the
  discreet footer, lazy loading past ~40 receivers, dropping the tick marks.
- **Settings page** — tabs, view preferences, "always show controls".
- **Dashboard layout rework** — full-width stats, per-receiver buffer bars,
  overall-only graph, responsive drop order.
- **Audio quality** — the linear-interpolation resampler. Unrelated to the TUI
  and higher value than any of it.

The screen state machine here is what those first three build on, which is why
this project comes first.

## Success criteria

1. From launch to exit the terminal is never handed back to a shell except by
   quitting.
2. An Apple TV needing a PIN can be paired without leaving the TUI, and the
   credential persists.
3. A receiver that fails to connect is visible, in red, with a readable reason,
   and `r` retries just that one.
4. Some-fail-some-succeed starts streaming to the ones that worked.
5. All-fail returns to the picker with the selection intact.
6. `openair capture "Living Room"` and bare `openair` differ only in which
   screen they start on.
7. `--no-tui` behaviour is unchanged.
