# Desktop UI — options, not yet a decision

**Status: analysis.** Nothing here is committed. The framework choice belongs
to the maintainer, and this document exists so that choice is made against
evidence rather than vibes. Written overnight so the decision can be made in
one sitting rather than discovered halfway through an implementation.

The brief was: a native UI that feels intentional rather than templated,
features that are reliable and easy to use, Linux later but genuinely wanted,
and no sloppy last-minute interface.

---

## 1. The target list is shorter than it looks

OpenAir is a *sender*. macOS already sends AirPlay natively, so a macOS build
has no audience — the MacBook Air in the test set is a receiver and a reference
implementation, not a port target.

So the platforms are **Windows now, Linux later**. Not three platforms, two.
That removes most of the usual cross-platform pressure and, in particular, it
removes the argument that normally rules out writing a platform-specific UI.

---

## 2. "Native" and "intentional" pull against each other

Worth separating before comparing anything, because these two words in the
brief do not point the same way.

**Native widgets** mean inheriting the platform's own controls and look. On
Windows that inheritance is not a clean thing to receive: the platform is a
layered mix of Win32, WinForms, WPF, UWP and WinUI 3, and "looks native" has no
single referent. Worse, native widgets are exactly the part that transfers
*least* to Linux — a Win32 or WinUI front end gives Linux nothing at all, so
the UI gets written twice.

**Intentional** means a considered visual identity — the terracotta icon work,
a layout that reflects what this app actually does. That is *custom-rendered*
by definition. A design with a point of view is not the platform's default
look, whatever toolkit draws it.

The reading of the brief that I think is correct: **it should feel like a real
desktop application and not a website in a box.** That is about window
behaviour, tray integration, startup time, memory, keyboard handling and
responsiveness — not about whether the buttons are `HWND`s. That reading is
achievable without native widgets, and it is what rules the windows-rs path out
rather than in.

---

## 3. The engine boundary already exists (and is in the wrong crate)

This is the useful finding, and it changes the shape of the decision.

`crates/tui/src/app.rs` does not own the streaming engine. It takes it by
injection:

```rust
pub type StreamLauncher<'a> = Box<
    dyn FnMut(Vec<GroupTarget>, Settings, Arc<StreamStats>, Arc<AtomicBool>) -> StreamHandle + 'a,
>;
pub type SettingsApplier<'a> = Box<dyn FnMut(&Settings, &Settings) -> Result<(), String> + 'a>;
pub type ReadyHook<'a> = Box<dyn FnMut() -> Result<(), String> + 'a>;
```

The engine already runs on its own thread (`StreamHandle` wraps a
`JoinHandle`), reports through `Arc<StreamStats>`, and stops through an
`Arc<AtomicBool>`. `openair-tui` deliberately does not depend on
`openair-capture`; the CLI supplies the platform-specific parts — handoff,
Windows now-playing — as closures.

**So the audio engine does not need restructuring for a GUI.** The same
injection points a GUI would want are already there and already proven by two
consumers (the real CLI and the tests). This de-risks the framework decision
enormously: the front end becomes swappable, and choosing wrong costs a front
end rather than an engine.

Two real problems with it as it stands:

1. **It lives in `crates/tui`.** A GUI cannot depend on `openair-tui` without
   dragging in ratatui and crossterm. These types, plus `Settings`,
   `GroupTarget`, `StreamStats` and the pairing/picker state machines, need to
   move to a UI-agnostic crate — `crates/session` — leaving `crates/tui` as one
   front end among several.

2. **The closures are `'a`-bound `FnMut`, not `Send + 'static`.** This suits a
   TUI that owns its loop and drives everything synchronously. Most GUI
   frameworks invert that: the framework owns the event loop and demands
   `'static`, usually `Send`, for anything shared with it. So the signatures
   will need revisiting, and that is the one piece of real engineering the
   extraction implies rather than a pure move.

**This extraction is worth doing regardless of which framework wins**, which
makes it the obvious first task and the one that can start before the framework
question is settled.

---

## 4. Candidates

| | What it is | Look | Linux | Toolchain | Risk |
|---|---|---|---|---|---|
| **Tauri v2** | Rust backend + system webview | HTML/CSS, as good as you design it | WebKitGTK — the weak spot | adds npm/JS | low build risk, real Linux risk |
| **iced** | Pure Rust, Elm-style, custom renderer | Custom; proven in System76's COSMIC | strong | pure Rust | medium; you draw everything |
| **slint** | DSL + Rust, own renderers | Custom, designer-oriented | strong | extra DSL | medium; smaller community |
| **egui** | Pure Rust, immediate mode | Distinctly not native | fine | pure Rust | low effort, hardest to make look intentional |
| **tray-icon (+muda)** | Native tray and menus only | Genuinely native | fine | pure Rust | very low; cannot host a dashboard |
| **windows-rs / WinUI 3** | Real native Windows | Actually native | nothing reusable | heavy | **highest** — XAML from Rust is not practically supported, and Linux means a second UI |

Notes that matter more than the table:

- **Tauri is the competitor's choice**, which is evidence it works for this
  problem, not evidence we should copy it. Its costs land precisely where this
  project is strongest and weakest respectively: it adds a JavaScript toolchain
  to an all-Rust codebase, and WebKitGTK is the least pleasant part of shipping
  Linux desktop software. For a tray utility meant to sit resident for hours,
  a webview is also the heaviest option in memory.
- **iced** has the strongest Linux credibility of the pure-Rust options because
  System76 builds an entire desktop environment on it. Everything is
  custom-drawn, which is a cost in effort and an advantage for "intentional".
- **slint**'s licensing includes a GPL option, which fits a GPL-3.0 project
  cleanly. Verify the current terms before relying on this.
- **Accessibility** is the argument that most often gets forgotten and most
  favours Tauri: HTML is accessible by default, whereas custom-rendered
  toolkits depend on AccessKit integration. If screen-reader support matters,
  check the current state for iced and slint specifically rather than assuming.
- **`tray-icon` is not really a competitor** to the others. It is the resident
  surface, and it composes with any of them.

---

## 5. What I would recommend

**Stage it, and make the tray the first thing that ships.**

1. **Extract `crates/session`** from `crates/tui`. Framework-independent, makes
   the rest reversible, and needs no decision from anyone.
2. **Tray-first release.** `tray-icon` + `muda`: pick receivers, volume,
   connect/disconnect, quit, launch on login, reconnect to the last set. Small,
   genuinely native, and it fixes the actual first-run problem — a
   non-developer double-clicking an exe and getting a terminal. This can ship
   while the window is still being designed.
3. **Then the window**, on whichever framework wins, for the dashboard that a
   menu genuinely cannot hold: per-room volume and offset, buffer headroom, PIN
   entry, pairing management.
4. **Keep the TUI.** It stays the power-user and debugging interface, and it
   keeps the extracted boundary honest by being a second consumer.

On the framework itself I lean **iced**, because it keeps the project all-Rust,
carries no webview into a resident tray app, has the best Linux story of the
pure-Rust options, and forces the custom-drawn design the "intentional"
requirement implies anyway. **Tauri** is the pragmatic counter-argument and a
defensible choice — faster to a rich dashboard, better accessibility for free —
paid for with a JS toolchain and WebKitGTK.

I do not think this should be decided from a table. Step 1 is genuinely
framework-independent, so the honest sequence is to do the extraction, then
build the *same* small screen — the receiver picker — in the two leading
candidates and choose from the result. A day's spike beats an argument.

---

## 6. Open questions

Needed before any of this becomes a spec:

1. **Tray-first, or hold everything for one complete app?** Tray-first ships
   something usable to non-developers much sooner; holding back gives one
   coherent reveal.
2. **Framework by spike, or by decision now?** A spike costs a day and settles
   it with evidence.
3. **Does the GUI replace the TUI as the default, or sit alongside it?** This
   determines whether `openair` with no arguments opens a window or the picker.
4. **Does accessibility need to be a first-class requirement?** It is the one
   criterion that would decide this on its own, in Tauri's favour.
5. **One process or two?** A resident tray app that survives the window being
   closed is the conventional shape; it is also more moving parts than a single
   process that shows and hides a window.
