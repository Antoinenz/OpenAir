# Design: `--handoff` v2 — virtual audio device routing

**Date:** 2026-07-25
**Status:** Approved (direction), supersedes the endpoint-mute approach in
`2026-07-22-handoff-local-mute-volume-mirror-design.md`
**Task:** #17
**Scope:** Windows only, `capture` mode only

## Why v1 (endpoint mute) is being replaced

v1 shipped and the volume mirroring worked well, but silencing the speakers by
holding `IAudioEndpointVolume::SetMute(TRUE)` is **structurally unwinnable**:
Windows auto-unmutes the endpoint on every volume change, we re-mute ~50 ms
later, and the gap is audible as a glitch on each volume keypress.

Approach B (event-driven `IAudioEndpointVolumeCallback`) would shrink that window
but **not close it** — Windows still unmutes first and we still react second. Any
mute-based design is a race against the OS. Dropped.

## v2 approach: route around the speakers entirely

On `--handoff`, OpenAir:

1. **Finds a virtual output device** (an installed virtual audio cable).
2. **Saves the current default output device**, then **switches the Windows
   default** to the virtual device.
3. **Captures loopback from the virtual device** (not the default speakers).
4. **Mirrors the Windows volume** to AirPlay (unchanged from v1 — and now much
   simpler, see below).
5. **Restores the original default device** on exit.

Nothing is ever muted, so there is no race and no glitch: the physical speakers
simply stop receiving audio because they are no longer the default endpoint.

### Bonus: per-app split tunneling comes nearly free

Once a virtual output device exists, Windows 10 1803+ lets users route individual
apps to different output devices (Settings → System → Sound → App volume and
device preferences). So users get split tunneling immediately with no work from
us. Driving it programmatically (undocumented `IAudioPolicyConfigFactory`) is a
later nicety, not required.

## Where the virtual device comes from — phased

**Phase 1 (now): use an existing installed cable.** OpenAir detects a virtual
output device (VB-CABLE, VAC, VoiceMeeter, …) and uses it. The user installs
VB-CABLE once. Zero driver work, full UX today.

**Phase 2 (later): ship our own signed driver.** Removes the third-party
dependency. Deliberately deferred — see constraints below.

Crucially the Phase-1 plumbing (switch-default + capture-from-named-device) is
**exactly what our own driver would need**, so none of it is throwaway.

### Why our own driver is a separate project

- A Windows audio endpoint can **only** be created by a kernel-mode driver;
  there is no user-mode API. (Hence VB-CABLE/VAC/VoiceMeeter all ship one.)
- Requires a PortCls/WDM driver (Microsoft's `SysVAD` sample is the starting
  point), **Microsoft attestation signing** via Partner Center, an **EV
  code-signing certificate** (~$250–400/yr) and a registered company entity.
- Admin-rights installer, plus breakage risk across Windows updates.
- Test-signing mode is dev-only (needs Secure Boot off) — not shippable.

## Volume mirroring gets *simpler*

v1's `classify` state machine existed only because we were fighting the mute
flag: we held mute on, so a user mute-keypress showed up as an ambiguous
`true→false` transition we had to reinterpret.

In v2 **we never mute anything**, so the mute flag is unambiguous — muted means
the user wants silence. The bridge reduces to: poll `(scalar, muted)` on the
*virtual* device, emit `scalar_to_dbfs(scalar)`, or `-144` when muted. The
`classify` machine and all re-assert logic are **deleted**.

Loopback still taps pre-volume, so captured audio stays full-scale regardless of
the slider position — the AirPlay level is driven purely by the mirrored value.

## Key technical points

- **Switching the default device has no public API.** `IPolicyConfig` is
  undocumented COM (what nircmd / SoundVolumeView / AudioSwitcher use). Works
  Win7–Win11 in practice, but unofficial — must fail gracefully with a clear
  message rather than panicking.
- **Capture from a chosen device:** `SystemCapture::start()` currently hardcodes
  `default_output_device()`. Needs a `start_on(device)` variant; cpal can build a
  loopback input stream on any enumerated output device.
- **Device detection:** match enumerated output devices against known virtual
  cable names (`CABLE Input (VB-Audio Virtual Cable)`, `VoiceMeeter Input`,
  `Virtual Audio Cable`, …). Also accept an explicit override flag so users with
  an unusual device aren't stuck on our name list.

## ⚠️ Restore-on-crash is a real hazard

If OpenAir dies without restoring the default device, the user is left with their
audio routed to a silent virtual cable **and no obvious cause** — a genuinely bad
failure mode. Must be designed for, not bolted on:

- Restore in `Drop` **and** on Ctrl+C (as v1 does for volume).
- Consider persisting the saved device id to disk on switch, so a later run can
  detect "we switched and never restored" and offer to fix it.
- Provide an escape hatch command (e.g. `openair restore-audio`) that resets the
  default output device without needing a stream.

## Failure handling

- **No virtual device found** → clear message telling the user how to install
  VB-CABLE, and stream normally (speakers keep playing, degrade off) rather than
  failing the stream.
- **Default-device switch fails** → warn, stream normally without routing.

## Compatibility

- v1's `--handoff` behavior (endpoint mute) is **removed**, not kept as an
  option — it's structurally glitchy and keeping two silencing paths isn't worth
  the complexity. The flag name and the volume-mirroring UX stay the same.
- Default capture behavior (no `--handoff`) is unchanged: loopback of the default
  output device. Once there's a UI, virtual-device routing likely becomes the
  default — but not yet.

## Out of scope (this phase)

- Our own signed virtual audio driver (Phase 2).
- Programmatic per-app routing (Windows Settings covers it for now).
- Non-Windows platforms.
