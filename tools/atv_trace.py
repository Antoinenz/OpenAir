"""Capture what pyatv sends to an Apple TV, in a form diffable against ours.

Why this exists
---------------
Issue #29: an Apple TV plays our audio but shows no AirPlay UI for any session
after the first one following a reboot. Nine experiments have ruled out
orphaned sessions, stale sender records, and anything about *what we send* --
the RTSP exchange is byte-identical between a working run and a failing one.

pyatv is a third-party sender that does get the UI, so the remaining question
is what it does that we do not. Answering that needs its traffic in a form
that can be put next to ours, which is what this produces: a normalised trace,
one line per protocol event, with volatile fields (ports, timestamps, session
ids) replaced by placeholders so a diff shows real differences rather than
noise.

Usage
-----
    # once, to pair (prints credentials -- save them as shown)
    atvremote --id <DEVICE_ID> --protocol airplay pair

    # then, from the repo root
    python tools/atv_trace.py capture                 # writes logs/trace-pyatv-*.txt
    python tools/atv_trace.py normalise <openair.log> # writes logs/trace-openair-*.txt
    python tools/atv_trace.py diff <a.txt> <b.txt>

Which comparison is worth making
--------------------------------
**pyatv against pyatv** is the experiment. Run `capture` twice in a row without
rebooting the Apple TV, watching the television both times, and diff the two
traces. They are the same logger in the same format, so the diff aligns and
means something:

  - second session still shows the UI  ->  the receiver is willing, and what we
    are missing is in what *we* send. Read pyatv's trace for what we omit.
  - second session shows no UI either  ->  the receiver does this to everyone,
    the bug is not ours, and #29 can be closed as upstream behaviour.

That single answer decides whether there is anything left for us to fix, which
is why it comes first.

**pyatv against OpenAir** is a *read*, not a diff. The two log through
different machinery, so the lines will not line up no matter how much is
normalised. Put them side by side and look for protocol events one has and the
other does not -- that is what the normalisation is for here.
"""

import os
import re
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
LOGS = os.path.join(ROOT, "logs")

# Default target: the Apple TV, since every open question is about it.
#   Apple TV (test):  3AA1CB971A87
#   Living Room:      C869CD679216
#   Pool Room:        002324B60750
DEFAULT_DEVICE = "3AA1CB971A87"

# Fields that change every run and would swamp a diff.
VOLATILE = [
    (re.compile(r"\b\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}[.,]\d+Z?\b"), "<TIME>"),
    (re.compile(r"\bport=\d+"), "port=<PORT>"),
    (re.compile(r"\b(?:data_port|control_port|event_port|timing_port)=\d+"), "<PORT_FIELD>=<PORT>"),
    (re.compile(r"\b\d{1,3}(?:\.\d{1,3}){3}:\d+\b"), "<ADDR>"),
    (re.compile(r"\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b"), "<UUID>"),
    (re.compile(r"\brtptime=\d+"), "rtptime=<N>"),
    (re.compile(r"\bseq(?:uence)?=\d+"), "seq=<N>"),
    (re.compile(r"\bCSeq: \d+"), "CSeq: <N>"),
    (re.compile(r"\b\d{13,}\b"), "<BIGNUM>"),
]

# Lines worth keeping: protocol events, not byte-level chatter.
KEEP = re.compile(
    r"(RTSP|SETUP|RECORD|TEARDOWN|FLUSH|SET_PARAMETER|GET_PARAMETER|OPTIONS|ANNOUNCE"
    r"|/info|/feedback|/command|/audioMode|/auth-setup|pair-setup|pair-verify|pair-pin"
    r"|statusFlags|flags=0x|streamConnection|sessionUUID|SETPEERS|SETRATEANCHORTIME"
    r"|Content-Type|X-Apple|Active-Remote|DACP|User-Agent)",
    re.IGNORECASE,
)

# Noise that matches KEEP but says nothing about the session.
DROP = re.compile(r"(mdns|zeroconf|Scanning|Discovered|knock|udns)", re.IGNORECASE)


def normalise_lines(lines):
    out = []
    for raw in lines:
        line = raw.rstrip("\n")
        if not KEEP.search(line) or DROP.search(line):
            continue
        for pattern, repl in VOLATILE:
            line = pattern.sub(repl, line)
        # Collapse the logger prefix so the two sources line up.
        line = re.sub(r"^\s*<TIME>\s+\S+\s*", "", line)
        line = re.sub(r"^\s*<TIME>\s+", "", line)
        line = re.sub(r"^(DEBUG|INFO|WARN|ERROR)\s+", "", line.strip())
        # Drop the logger's own module prefix. It differs between pyatv and
        # OpenAir and says nothing about the protocol.
        line = re.sub(r"^[\w.:]+:\s+", "", line)
        line = line.strip()
        if line:
            out.append(line)
    return out


def stamp():
    return time.strftime("%Y%m%d-%H%M%S", time.gmtime())


def ensure_wav():
    """A few seconds of tone for pyatv to stream. Built here so the trace does
    not depend on a file someone happened to leave in a temp directory."""
    path = os.path.join(LOGS, "trace-tone.wav")
    if os.path.exists(path):
        return path
    import math
    import struct
    import wave

    os.makedirs(LOGS, exist_ok=True)
    rate, seconds, freq = 44100, 6, 440.0
    with wave.open(path, "w") as w:
        w.setnchannels(2)
        w.setsampwidth(2)
        w.setframerate(rate)
        frames = bytearray()
        for i in range(rate * seconds):
            v = int(math.sin(2 * math.pi * freq * i / rate) * 0.3 * 32767)
            frames += struct.pack("<hh", v, v)
        w.writeframes(bytes(frames))
    print(f"built a test tone at {path}")
    return path


def credentials_path():
    return os.path.join(HERE, "atv_credentials.txt")


def capture(device):
    creds = credentials_path()
    if not os.path.exists(creds):
        print("No credentials found. Pair once, then save what it prints:\n")
        print(f"    atvremote --id {device} --protocol airplay pair")
        print(f"    # then put the credentials string in {creds}\n")
        print("The Apple TV shows a PIN on screen; atvremote asks for it.")
        return 1

    wav = ensure_wav()
    out = os.path.join(LOGS, f"trace-pyatv-{stamp()}.txt")
    raw_path = out.replace(".txt", "-raw.log")

    print(f"streaming {os.path.basename(wav)} to {device} via pyatv...")
    print("watch the television: note whether the AirPlay screen appears.\n")

    proc = subprocess.run(
        [sys.executable, os.path.join(HERE, "pyatv_probe.py"), device, wav],
        capture_output=True,
        text=True,
        errors="replace",
    )
    raw = (proc.stdout or "") + (proc.stderr or "")
    with open(raw_path, "w", encoding="utf-8") as f:
        f.write(raw)

    lines = normalise_lines(raw.splitlines())
    with open(out, "w", encoding="utf-8") as f:
        f.write("\n".join(lines) + "\n")

    print(f"raw log:    {raw_path}")
    print(f"normalised: {out}  ({len(lines)} protocol lines)")
    if "STREAM OK" not in raw:
        print("\npyatv did not report STREAM OK -- check the raw log first.")
        return 1
    print("\nRun this again without rebooting the Apple TV, then:")
    print(f"    python tools/atv_trace.py diff <first> <second>")
    return 0


def normalise_file(path):
    with open(path, encoding="utf-8", errors="replace") as f:
        lines = normalise_lines(f.readlines())
    out = os.path.join(LOGS, f"trace-openair-{stamp()}.txt")
    with open(out, "w", encoding="utf-8") as f:
        f.write("\n".join(lines) + "\n")
    print(f"normalised: {out}  ({len(lines)} protocol lines)")
    return 0


def diff(a, b):
    import difflib

    with open(a, encoding="utf-8") as f:
        left = f.read().splitlines()
    with open(b, encoding="utf-8") as f:
        right = f.read().splitlines()
    delta = list(
        difflib.unified_diff(left, right, fromfile=a, tofile=b, lineterm="", n=2)
    )
    if not delta:
        print("identical once normalised.")
        print("If these are two pyatv sessions and only the first showed the UI,")
        print("then the receiver decides on something outside the RTSP exchange.")
        return 0
    print("\n".join(delta))
    return 0


def main(argv):
    os.makedirs(LOGS, exist_ok=True)
    if len(argv) < 2:
        print(__doc__)
        return 2
    cmd = argv[1]
    if cmd == "capture":
        return capture(argv[2] if len(argv) > 2 else DEFAULT_DEVICE)
    if cmd == "normalise" and len(argv) > 2:
        return normalise_file(argv[2])
    if cmd == "diff" and len(argv) > 3:
        return diff(argv[2], argv[3])
    print(__doc__)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv))
