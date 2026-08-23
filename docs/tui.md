# The terminal UI

`openair` with no arguments is one continuous terminal application: from
choosing receivers to streaming, it never hands the screen back to a shell.

`--no-tui` gives plain scrolling output instead, and is selected automatically
when stdout is not a terminal.

## The picker

```
┌ OpenAir (4 found) ─────────────────────────────────────────────────────────┐
│ [x] Living Room         Apple TV 4K     192.168.1.106:7000                 │
│ [ ] Pool Room           Shairport Sync  192.168.1.51:7000                  │
│>[x] Kitchen             HomePod mini    192.168.1.88:7000                  │
│                                                              ┌──────────┐  │
│                                                              │  ⏎ READY │  │
└──────────────────────────────────────────────────────────────└──────────┘──┘
  handoff on · 500 ms · -8 dB · 2 selected   space select · h handoff · <> latency
```

Receivers appear as they answer — there is no fixed wait — and **nothing is
contacted until you press Enter**. Names, models and capabilities all come from
the mDNS record.

Model identifiers are shown as marketing names where we can name one with
confidence, and left as the raw identifier where we cannot: inventing a wrong
name is worse than showing none.

The button turns green once at least one receiver is chosen, so "can I start?"
is answerable at a glance. Press Enter with nothing selected and it turns yellow
while the footer explains why.

| Key | Does |
|-----|------|
| `↑` / `↓` | move |
| `space` | select or deselect |
| `h` | toggle handoff |
| `<` / `>` | latency |
| `s` | settings |
| `⏎` | start |
| `q` | quit |

## Pairing and connecting

Press Enter and the flow continues in place.

1. **Pairing**, if any chosen receiver wants a HomeKit PIN. Type the four digits
   shown on the device. `Esc` skips *that* receiver and carries on with the
   rest — one un-pairable speaker should not cost you the group.
2. **Connecting**, with per-receiver progress. Any receiver that fails shows
   why. If some connect and some do not, streaming starts with the ones that
   worked; if none do, you are returned to the picker with the reason and your
   selection still made, so a retry is one keystroke.

## The dashboard

```
┌ latency ────────┐┌ bandwidth ───────────┐┌ now playing ─────────────────────┐
│500 ms           ││1.4 Mbps              ││Talk Talk — It's My Life          │
│420 ms ahead     ││391 MB                ││                                  │
└─────────────────┘└──────────────────────┘└──────────────────────────────────┘
┌ bandwidth over time ───────────────────────────────────────────────────────┐
│      ▂▃▄▅▆▇█▇▆▅▄▃▂▃▄▅▆▇█▇▆▅▄▃▂▃▄▅▆▇█▇▆▅▄▃▂▃▄▅▆▇█▇▆▅▄▃▂▃▄▅▆▇█▇▆▅▄▃▂▃▄▅▆▇█   │
└────────────────────────────────────────────────────────────────────────────┘
  now 1.4 Mbps
┌ receivers (2)   [+/-] vol · [<>] offset · [a] add · [r] retry · [d] drop ──┐
│ ▸ Living Room              -6 dB   +80 ms  █████████░  connected           │
│   Pool Room                +0 dB    +0 ms  ███░░░░░░░  connected           │
└────────────────────────────────────────────────────────────────────────────┘
┌ logs   [PgUp/PgDn] scroll ─────────────────────────────────────────────────┐
│ 14:02:19  INFO  latency stepped up to 550 ms                               │
└────────────────────────────────────────────────────────────────────────────┘
```

`↑↓` selects a receiver, and:

| Key | Does |
|-----|------|
| `+` / `-` | volume trim for that receiver, ±1 dB |
| `<` / `>` | play offset for that receiver, ∓/±10 ms |
| `a` | add another receiver mid-stream |
| `r` | retry one that failed |
| `d` | drop it |
| `s` | settings |
| `PgUp` / `PgDn` | scroll the log panel |
| `q` / `Ctrl+C` | stop |

Per-receiver volume is a **trim** on the group level, not an absolute level, so
`--handoff` moving the Windows master preserves the balance you dialled in.

### Reading the buffer bars

**Buffer headroom is per receiver**, drawn as the bar on each row: how much of
the target latency that receiver still has in hand before it runs dry.

It is the number that predicts a dropout — it is what auto-latency watches to
decide when to step up — so you can usually see trouble coming, and see *which
room* is in trouble. The bars move together when auto-latency steps, because
headroom is measured against the latency currently in force.

The bar is deliberately not a graph. Group headroom has one history but many
receivers, so a single line could only ever show the group minimum; it could
never tell you which room was about to cut out. The graph shows bandwidth,
where one line does say something.

On a narrower terminal the buffer bar goes first, then the graph, then the
offset column. The receiver list and the log panel are never dropped.

## Settings

Press `s` from either the picker or the dashboard: handoff, latency, volume,
metadata, smooth fix, and the keybind-line preference.

From the dashboard it is drawn *over* the live frame rather than replacing it,
so you can watch the buffer bars react while you adjust the latency. That
feedback loop is the only thing that makes a latency control comprehensible.

Everything on it applies to a running stream. Toggling handoff mid-stream
switches the Windows default device and moves capture to it without rebuilding
the audio pipeline. The new capture is started and proven *before* the old one
is dropped, so if it fails you keep the stream you had and the setting stays
where it was — with the reason shown on the row that caused it.

> Sample rates are followed across the swap. Your speakers at 48 kHz and a
> virtual cable at 44.1 kHz are different rates, and a consumer that kept
> resampling at the old ratio would shift *pitch* rather than glitch — easy to
> misdiagnose as a receiver fault.

Preferences persist in `settings.json` beside `pairings.json`. Command-line
flags override the file for that run without rewriting it.

## Keybind lines

They list only what you would not guess. Arrow keys and `q` are left off,
because anyone will try those anyway and naming them crowds out the keys that
matter. Set `"show_controls": true` in `settings.json`, or toggle it in
settings, for the full list.

## Quitting

`q` or `Ctrl+C` stops, restores the terminal and your audio device, and prints
one summary line — duration, data sent, final latency, and where the log went
if you passed `--log`. Nothing else is printed during a TUI run; the log panel
carries the narration instead.
