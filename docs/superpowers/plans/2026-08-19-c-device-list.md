# C — Device List Presentation: Implementation Plan

**Depends on:** B (`2026-08-19-b-unified-tui-flow.md`) for the ready button and
the removal of the pairing marker, both of which assume pairing happens after
confirming. The rest is independent.

**Goal:** Make the device list read like a finished product: readable model
names, a clear ready state, and a quieter frame.

**Design note:** no separate spec; decisions recorded here from the user's brief
of 2026-08-19.

## Global constraints

Same as B.

---

## Task 1 — Readable model names

**Files:** create `crates/discovery/src/model.rs`; modify
`crates/discovery/src/device.rs`, `lib.rs`

```rust
/// Marketing name for an mDNS `model` identifier, or the identifier itself.
pub fn pretty_model(model: &str) -> &str;
```

Table for identifiers we can name with confidence, falling back to the raw
string. **The fallback is the important part**: this table is assembled from
public sources and cannot be exhaustive, and showing an unknown device as its
raw identifier is right — inventing a wrong name is worse than showing none.

Known mappings to start with:

| Identifier | Name |
|---|---|
| `AppleTV5,3` | Apple TV HD |
| `AppleTV6,2` | Apple TV 4K |
| `AppleTV11,1` | Apple TV 4K (2nd gen) |
| `AppleTV14,1` | Apple TV 4K (3rd gen) |
| `AppleTV3,1`, `AppleTV3,2` | Apple TV (3rd gen) |
| `AudioAccessory1,1`, `AudioAccessory1,2` | HomePod |
| `AudioAccessory5,1` | HomePod mini |
| `AudioAccessory6,1` | HomePod (2nd gen) |
| `AirPort10,115` | AirPort Express |
| `ShairportSync` | Shairport Sync |

Mac identifiers (`MacBookPro18,3`, `Macmini9,1`, …) are too numerous to table.
Handle them by prefix instead — `MacBookPro` → "MacBook Pro", `MacBookAir` →
"MacBook Air", `Macmini` → "Mac mini", `iMac` → "iMac", `MacStudio` →
"Mac Studio" — dropping the version suffix. A generation number nobody can
decode is not worth the row width.

Expose as `AirPlayDevice::pretty_model()` and use it wherever `txt.model` is
shown, including the dashboard once B carries names through.

**Tests (write first):** each table entry; a prefix case; an unknown identifier
returns itself unchanged; an empty model returns something sane.

**Commit:** `feat(discovery): readable model names`

---

## Task 2 — Drop the pairing markers

**Files:** `crates/tui/src/picker_ui.rs`, `crates/tui/src/picker.rs`

Remove the `✓` and `! needs pairing` marks. After B, pairing happens when the
user confirms, so a marker that predicts it is both unclear (nobody reads a tick
as "credentials on disk") and no longer load-bearing.

Keep `PickerRow::paired` — the **sort order** still uses it, and your usual
speakers staying at the top is the useful half of that information. It stops
being drawn, not stops existing.

**Tests:** the sort test stays green; a render test asserts no tick is drawn.

**Commit:** `refactor(tui): stop drawing pairing markers`

---

## Task 3 — The ready button

**Files:** `crates/tui/src/picker_ui.rs`

A block in the bottom-right corner, overlapping the list border:

```
        ┌──────────┐
        │  ⏎ READY │   green when ready
        └──────────┘
```

Green when at least one receiver is selected; grey otherwise. The `⏎` says how
to press it without a sentence telling the user to. On `Enter` while not ready,
the button flashes and the existing hint explains why — the hint mechanism from
phase 1 already carries exactly this text.

Implement as a `Clear` + block drawn over the list's bottom-right, reusing the
`centred` helper's arithmetic (extract it to a shared `rect` helper rather than
copying it — it already exists in `dashboard_ui.rs` for the add overlay).

**Tests:** the rect stays inside its container at small sizes; a render test at
60 columns does not panic and the button is present in both states.

**Commit:** `feat(tui): ready button`

---

## Task 4 — Discreet footer and the `show_controls` setting

**Files:** `crates/tui/src/settings.rs`, `crates/tui/src/picker_ui.rs`,
`crates/tui/src/dashboard_ui.rs`

`Settings` gains `show_controls: bool`, default **false**.

- `true` — today's full keybind line.
- `false` — show only what is not guessable or is easy to forget. Arrow keys to
  move and `q` to quit are neither: anyone who has used a terminal will try
  them, and quitting also answers to `Esc` and `Ctrl+C`. What survives is the
  non-obvious: `space` select, `h` handoff, `<>` latency.

Merge the two footer lines into one discreet line. Settings that are *state*
(handoff on, latency, volume) read as values, not instructions.

The setting is the first entry for the future settings page (D); until that
exists it is editable only in `settings.json`, which is acceptable for a
view-preference default nobody needs to change.

**Tests:** the footer omits the obvious keys when `show_controls` is false and
includes them when true; `Settings` round-trips the new field and defaults to
false when absent from an existing file.

**Commit:** `feat(tui): discreet footer with a show_controls setting`

---

## Task 5 — Lazy list rendering

**Files:** `crates/tui/src/picker.rs`

Cap the rows built to ~50, extending as the cursor approaches the end.

**This is a rendering cap, not a discovery cap.** `DeviceSet` keeps every device
it hears about; `rows()` exposes a window. Limiting discovery would mean
missing a device that announces late — precisely the receiver someone is waiting
for on a busy network.

```rust
const ROW_LIMIT_STEP: usize = 50;
// visible_limit grows by ROW_LIMIT_STEP when cursor > visible_limit - 10
```

**Tests (write first):** with 200 devices known, `rows()` returns 50; moving the
cursor to 45 extends it to 100; selection and sort survive an extension; a
selected device beyond the window is still reported by `chosen()` — the trap
here is filtering the selection to the visible window and silently dropping a
receiver the user picked before more arrived.

**Commit:** `feat(tui): cap rendered rows on large networks`

---

## Self-review notes

- Task 5 has the subtlest failure mode (selection outside the window) and its
  test is written to catch exactly that.
- Task 1's table is the only place in this project where being wrong is worse
  than being absent, hence the fallback rule stated in the task rather than left
  implicit.
- Tasks 1, 4 and 5 do not depend on B and could be pulled forward if B slips.
