# Drive pyatv (reference AirPlay 2 client) against a receiver with full debug
# logging, to capture its pair-setup TLV exchange for differential comparison.
import asyncio
import logging
import os
import sys

if sys.platform == "win32":
    asyncio.set_event_loop_policy(asyncio.WindowsSelectorEventLoopPolicy())

logging.basicConfig(
    level=logging.DEBUG, stream=sys.stdout,
    format="%(asctime)s %(name)s: %(message)s",
)

import pyatv  # noqa: E402


async def main():
    # Defaults to the Apple TV, not Pool Room: the interesting questions are
    # all about the Apple TV, and Shairport rejects pyatv's transient
    # /pair-pin-start with 400 anyway, which looks like a failure but is not.
    #   Apple TV (test):  3AA1CB971A87
    #   Living Room:      C869CD679216
    #   Pool Room:        002324B60750
    ident = sys.argv[1] if len(sys.argv) > 1 else "3AA1CB971A87"
    wav = sys.argv[2] if len(sys.argv) > 2 else "C:/Users/antoi/AppData/Local/Temp/test.wav"
    loop = asyncio.get_event_loop()
    confs = await pyatv.scan(loop, identifier=ident, timeout=5)
    if not confs:
        print("DEVICE NOT FOUND")
        return
    # An Apple TV needs Normal (PIN) pairing; pyatv defaults to Transient and
    # gets 470 Connection Authorization Required without credentials. Pair once
    # with:
    #     atvremote --id 3AA1CB971A87 --protocol airplay pair
    # then save the printed credentials to tools/atv_credentials.txt (or pass
    # them as argv[3]). Shairport needs none of this -- it takes Transient.
    creds = sys.argv[3] if len(sys.argv) > 3 else None
    if creds is None:
        here = os.path.dirname(os.path.abspath(__file__))
        cred_file = os.path.join(here, "atv_credentials.txt")
        if os.path.exists(cred_file):
            creds = open(cred_file).read().strip()
    if creds:
        confs[0].set_credentials(pyatv.const.Protocol.AirPlay, creds)
        confs[0].set_credentials(pyatv.const.Protocol.RAOP, creds)
        print("USING SAVED CREDENTIALS")

    atv = await pyatv.connect(confs[0], loop)
    try:
        await atv.stream.stream_file(wav)
        print("STREAM OK")
    finally:
        atv.close()


asyncio.run(main())
