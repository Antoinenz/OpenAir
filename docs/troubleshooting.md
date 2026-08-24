# Troubleshooting

## No receivers appear in the picker

Discovery is mDNS, so anything that blocks multicast blocks OpenAir.

- **Guest or "client isolation" Wi-Fi** stops devices seeing each other. This is
  the most common cause by far.
- **A VPN** will usually capture the route to your own LAN. Disconnect it, or
  exclude the local subnet.
- **Different subnets.** mDNS does not cross them without a relay. A PC on the
  wired network and a speaker on Wi-Fi are often on different subnets even
  though it is the same router.
- **The firewall.** Allow `openair.exe` on the private network profile.

You can bypass discovery entirely if you know the address:

```console
openair capture 192.168.1.106:7000
```

If that works and the picker does not, the problem is mDNS, not OpenAir's
ability to reach the device.

## The connection is reset while pairing

Almost always the wrong network interface — common on machines with a VPN, a
virtual machine bridge, or several adapters.

OpenAir asks the OS which local address it would use to reach the receiver, and
binds both RTSP and PTP to that address so the receiver's clock daemon sees us
consistently. When it guesses wrong, override it:

```console
openair capture "Living Room" --bind 192.168.1.50
```

OpenAir will usually suggest the address to try in the error.

## A receiver that used to work now refuses

Its stored credentials are no longer valid — a factory reset or a tvOS update
will do it.

OpenAir detects this and offers to pair again: the receiver goes back to the
PIN prompt, and once you have typed the four digits the stream starts. Each
receiver is offered this once per run, so skipping it does not trap you in a
loop.

From the command line, or if you would rather do it yourself:

```console
openair pair "Living Room"
```

You can also drop the stored pairing outright — settings (`s`) → **pairings**,
then `d` twice on the row. That is the same as deleting its entry from
`%APPDATA%\OpenAir\pairings.json` by hand.

## The audio keeps cutting out

Watch the **buffer bars** on the dashboard: they show how much margin each
receiver still has. A room whose bar keeps emptying is the room with the
problem, which usually means Wi-Fi.

- Raise the latency (`<` `>` in settings, or `--latency 1500`). More margin
  absorbs more trouble. OpenAir also does this on its own, in 250 ms steps.
- Wired beats wireless, for the PC as much as the receiver.
- 5 GHz beats 2.4 GHz.
- On a busy network, one room dropping while the others are fine points at that
  receiver's link, not at OpenAir.

See [audio.md](audio.md) for what the latency setting actually buys you.

If you want the Wi-Fi prioritisation to actually apply on Windows, `--debug`
will tell you whether it does — see the DSCP note in
[audio.md](audio.md#network-priority-and-scheduling).

## One room is late

Some receivers add their own delay downstream of AirPlay — a soundbar's DSP, an
AV receiver's room correction. Nothing in the protocol knows about it, so
correct it by hand:

```console
openair capture "Living Room" "Pool Room" --offset "pool=+80ms"
```

Or `<` and `>` on that receiver's row in the dashboard, ±10 ms at a time, while
listening.

## Everything is silent after OpenAir crashed

`--handoff` switches your default output device and restores it on exit,
including on `Ctrl+C`. If it was killed outright, it never got the chance and
your PC is still routed to a silent virtual cable.

```console
openair restore-audio
```

## The pitch sounds slightly off

A sample-rate mismatch, not a codec problem. Check that the device you are
capturing from is set to the rate you think it is — Sound Control Panel →
device → Properties → Advanced. Setting it to 44.1 kHz means no resampling at
all, which is both the best-sounding and the simplest case.

## The Apple TV plays audio but shows nothing on screen

Known, and under investigation: an Apple TV activates its AirPlay screen for
the first session after a reboot and not for later ones. Rebooting it brings
the display back for one session. Audio and metadata delivery are unaffected —
this is only about what is drawn on the television.

If you want to help pin it down, `tools/atv_trace.py` captures what pyatv (a
third-party sender that does get the screen) sends, in a form comparable with
ours. Running it twice without rebooting the Apple TV answers the question that
matters — whether the receiver does this to every sender or only to us.

## Reporting a bug

Run with a log and attach the file rather than pasting a scrollback:

```console
openair capture "Living Room" --log --debug
```

The log lands in `logs/openair-YYYYMMDD-HHMMSS.log`. It is plain text with UTC
timestamps and no colour codes, so it greps and diffs cleanly, and it keeps
full detail even when the console is quiet.

`--debug 2` adds the decrypted body of everything the receiver sends. That is
the level to use for a protocol-level problem, and it makes for a large file.
