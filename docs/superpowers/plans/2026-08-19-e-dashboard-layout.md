# E — Dashboard Layout Rework: Implementation Plan

**Depends on:** B (`2026-08-19-b-unified-tui-flow.md`). E changes what the
streaming screen draws; B changes how screens are owned. Doing E first would
mean building it twice.

**Goal:** Make the streaming screen read as one coherent view: overall stats
across the top, and per-receiver information — including buffer health — on the
receiver's own row.

**Design note:** this project has no separate spec. The decisions are recorded
here, from the user's brief of 2026-08-19.

## Global constraints

Same as B: small focused commits, clippy clean, tests green, no Claude
attribution, work in `C:\Users\antoi\OpenAir`.

---

## The reasoning that shapes it

Phase 1 put buffer health in the big graph. That was wrong in a way worth
stating, because it drives every task here: **buffer health is per-receiver, and
the graph is a single series**. Today `min_lead_ms` is group-wide, taken off the
shared anchor line, so the graph shows one number for the whole group and cannot
tell you *which* room is about to drop out.

So the split becomes:

- **Graph** — overall only: bandwidth. History is meaningful here.
- **Buffer health** — a per-receiver bar on the receiver's row. No history; a
  full bar is healthy, an empty bar means the audio is about to cut. Instantaneous
  is the right shape: you want to see which room is in trouble *now*.

This requires per-receiver lead, which the stream does not currently compute.

---

## Task 1 — Per-receiver buffer headroom

**Files:** `crates/client/src/lib.rs`, `crates/client/src/stats.rs`

`ReceiverStat` gains `lead_ms: Option<i64>` and `health: f32` (0.0–1.0).

The group anchor line is shared, but each receiver has its own `offset_ns`, so
its play deadline differs. Compute per receiver in the send loop, where the
group lead is already computed:

```rust
let lead_ns = play_deadline_ns(anchor_t_local, anchor_rtptime, rtptime) as i64
            + r.offset_ns
            - ptp_now_ns() as i64;
```

`health` normalises against the current anchor latency: `lead_ms / latency_ms`,
clamped to 0.0–1.0. A receiver at the full anchor lead is fully buffered; at
zero it is about to underrun. Normalising against `latency_ms` rather than a
constant matters because auto-latency moves the target — a fixed denominator
would make every bar jump when the latency steps up.

Keep the group-wide `min_lead_ms` as it is; auto-latency still uses it.

**Tests (write first):** `health` is 1.0 at full lead, 0.0 at zero lead, clamps
below zero and above one; a receiver with a positive offset reports more
headroom than one without, given the same anchor.

**Commit:** `feat(client): per-receiver buffer headroom`

---

## Task 2 — Graph becomes overall-only

**Files:** `crates/tui/src/dashboard.rs`, `crates/tui/src/dashboard_ui.rs`,
`crates/tui/src/settings.rs`

Drop `GraphKind` and the `b` key; the graph shows bandwidth history and nothing
else. Remove `buffer_history` and `graph_series`; keep `worst_lead_ms` for the
summary line, which is still worth reporting after a run.

`settings.json` keeps the `graph` field but ignores it, or drops it — dropping
is cleaner, and `Settings::load` already tolerates unknown fields, so an existing
file needs no migration.

**Tests:** update those that assert on `GraphKind`; assert the summary still
reports `worst_lead_ms`.

**Commit:** `refactor(tui): the graph shows overall stats only`

---

## Task 3 — Receiver row layout

**Files:** `crates/tui/src/dashboard_ui.rs`

One row per receiver, laid out as fixed-width columns:

```
 ▸ Living Room        -3 dB   +80 ms   ████████░░   connected
```

- **Name** — the pretty name, not `ip:port` (C supplies the name; until then
  `ReceiverStat::name` is the address, so this task shows whatever it holds).
- **Volume and offset** — always shown, including `±0 dB` and `±0 ms`. A column
  that appears and disappears makes the row jump; a zero is information.
- **Buffer bar** — ten cells, `█` filled against `░` empty, from `health`.
  Green above ~0.5, yellow above ~0.2, red below.
- **Status** — floats right: `connected`, `connecting…`, `reconnecting…`,
  `failed`, `dead`, coloured as today.

**Tests:** a `TestBackend` render asserting `±0 dB` appears for an untrimmed
receiver; the bar renders full at `health = 1.0` and empty at `0.0`.

**Commit:** `feat(tui): receiver rows with buffer bars and constant columns`

---

## Task 4 — Full-width stats and responsive drop order

**Files:** `crates/tui/src/dashboard_ui.rs`

Top stats span the full width rather than three fixed-width boxes. As the
terminal narrows, drop in this order — first to go, first listed:

1. the buffer bar column
2. the graph
3. the offset column

Below the current `MIN_WIDTH`/`MIN_HEIGHT` the too-small message still stands.

Implement as a `Layout` chosen from the available width, not by clamping
individual widgets, so a narrow terminal produces a deliberate layout rather
than a squeezed one.

**Tests:** render at 60, 90 and 140 columns; assert the bar is absent at 60 and
present at 140, and that none of the three panics.

**Commit:** `feat(tui): responsive dashboard layout`

---

## Self-review notes

- The one genuinely new mechanism is per-receiver `health` (Task 1); everything
  else is presentation.
- Task 2 deletes a feature shipped four days ago (the `b` toggle). That is
  intended: it existed because buffer health was in the wrong place, and this
  project moves it.
- Risk: `health` normalised against a moving `latency_ms` means the bars all
  shift when auto-latency steps up. That is correct — the target moved — but it
  will look like a glitch, so the auto-latency log line should stay visible in
  the panel to explain it.
