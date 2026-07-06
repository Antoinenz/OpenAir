# Drive pyatv (reference AirPlay 2 client) against a receiver with full debug
# logging, to capture its pair-setup TLV exchange for differential comparison.
import asyncio
import logging
import sys

if sys.platform == "win32":
    asyncio.set_event_loop_policy(asyncio.WindowsSelectorEventLoopPolicy())

logging.basicConfig(
    level=logging.DEBUG, stream=sys.stdout,
    format="%(asctime)s %(name)s: %(message)s",
)

import pyatv  # noqa: E402


async def main():
    ident = sys.argv[1] if len(sys.argv) > 1 else "002324B60750"
    wav = sys.argv[2] if len(sys.argv) > 2 else "C:/Users/antoi/AppData/Local/Temp/test.wav"
    loop = asyncio.get_event_loop()
    confs = await pyatv.scan(loop, identifier=ident, timeout=5)
    if not confs:
        print("DEVICE NOT FOUND")
        return
    atv = await pyatv.connect(confs[0], loop)
    try:
        await atv.stream.stream_file(wav)
        print("STREAM OK")
    finally:
        atv.close()


asyncio.run(main())
