"""Dump every reverse-channel message the receiver sent, from a run log."""
import io
import re
import sys
import plistlib

path = sys.argv[1]
raw = io.open(path, 'rb').read()

for m in re.finditer(rb'event message \(full\) bytes=(\d+) text=', raw):
    n = int(m.group(1))
    msg = raw[m.end():m.end() + n]
    head, _, body = msg.partition(b'\r\n\r\n')
    print('=' * 70)
    print('bytes=%d' % n)
    print(head.decode('ascii', 'replace').strip())
    print('-' * 70)
    if not body.startswith(b'bplist00'):
        print('body is not a bplist:', body[:60])
        continue
    try:
        pl = plistlib.loads(body)
        print(repr(pl)[:2000])
    except Exception as e:
        # plistlib chokes on some of Apple's archives; fall back to strings so
        # the shape is still legible.
        print('(plist parse failed: %s) key-ish strings:' % e)
        seen = []
        for s in re.finditer(rb'[ -~]{4,}', body):
            t = s.group().decode('ascii')
            if t not in seen:
                seen.append(t)
        print(' | '.join(seen[:80]))
