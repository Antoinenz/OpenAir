# Differential probe: replicate pyatv's exact transient pair-setup (M1-M4) over
# raw RTSP against a real receiver, then attempt an encrypted GET /info.
# If this succeeds, the Rust client just needs to match this wire format.
import binascii
import hashlib
import socket
import struct
import sys

from cryptography.hazmat.primitives.ciphers.aead import ChaCha20Poly1305
from cryptography.hazmat.primitives.kdf.hkdf import HKDF
from cryptography.hazmat.primitives import hashes
from srptools import SRPClientSession, SRPContext
from srptools.constants import PRIME_3072, PRIME_3072_GEN

HOST = sys.argv[1] if len(sys.argv) > 1 else "192.168.1.106"
PORT = 7000


def tlv_encode(items):
    out = bytearray()
    for tag, value in items:
        if not value:
            out += bytes([tag, 0])
            continue
        for i in range(0, len(value), 255):
            chunk = value[i:i + 255]
            out += bytes([tag, len(chunk)]) + chunk
    return bytes(out)


def tlv_decode(data):
    out = {}
    i = 0
    while i + 1 < len(data):
        tag, ln = data[i], data[i + 1]
        out.setdefault(tag, bytearray()).extend(data[i + 2:i + 2 + ln])
        i += 2 + ln
    return {t: bytes(v) for t, v in out.items()}


def recv_response(sock):
    buf = b""
    while b"\r\n\r\n" not in buf:
        chunk = sock.recv(4096)
        if not chunk:
            raise ConnectionError("closed while reading headers")
        buf += chunk
    head, _, body = buf.partition(b"\r\n\r\n")
    headers = head.decode("latin1").split("\r\n")
    clen = 0
    for h in headers:
        if h.lower().startswith("content-length:"):
            clen = int(h.split(":", 1)[1].strip())
    while len(body) < clen:
        chunk = sock.recv(4096)
        if not chunk:
            break
        body += chunk
    return headers[0], body[:clen]


def post(sock, cseq, path, body):
    req = (
        f"POST {path} RTSP/1.0\r\n"
        f"CSeq: {cseq}\r\n"
        f"User-Agent: AirPlay/320.20\r\n"
        f"X-Apple-HKP: 4\r\n"
        f"Content-Type: application/octet-stream\r\n"
        f"Content-Length: {len(body)}\r\n\r\n"
    ).encode() + body
    sock.sendall(req)
    return recv_response(sock)


def hkdf_expand(salt, info, key):
    return HKDF(
        algorithm=hashes.SHA512(), length=32,
        salt=salt.encode(), info=info.encode(),
    ).derive(key)


def main():
    sock = socket.create_connection((HOST, PORT), timeout=10)

    # --- M1: exactly like pyatv: Method=0, State=1, Flags(0x13)=0x10 ---
    m1 = tlv_encode([(0x00, b"\x00"), (0x06, b"\x01"), (0x13, b"\x10")])
    status, body = post(sock, 1, "/pair-setup", m1)
    print(f"M2 status: {status} ({len(body)} bytes)")
    t = tlv_decode(body)
    if 0x07 in t:
        print(f"M2 ERROR code {t[0x07].hex()}")
        return
    salt, b_pub = t[0x02], t[0x03]
    print(f"M2 salt={salt.hex()} B={b_pub[:8].hex()}...({len(b_pub)}B)")

    # --- SRP via srptools (same as pyatv) ---
    ctx = SRPContext(
        "Pair-Setup", "3939",
        prime=PRIME_3072, generator=PRIME_3072_GEN,
        hash_func=hashlib.sha512,
    )
    session = SRPClientSession(ctx)
    session.process(b_pub.hex(), salt.hex())
    pub = binascii.unhexlify(session.public)
    proof = binascii.unhexlify(session.key_proof)
    print(f"A={pub[:8].hex()}...({len(pub)}B) M1proof={proof[:8].hex()}...")

    # --- M3: State=3, PublicKey=A, Proof ---
    m3 = tlv_encode([(0x06, b"\x03"), (0x03, pub), (0x04, proof)])
    status, body = post(sock, 2, "/pair-setup", m3)
    print(f"M4 status: {status} ({len(body)} bytes)")
    t = tlv_decode(body)
    if 0x07 in t:
        print(f"M4 ERROR code {t[0x07].hex()}  *** PROOF REJECTED ***")
        return
    m2_proof = t.get(0x04, b"")
    print(f"M4 state={t.get(0x06, b'?').hex()} serverproof={m2_proof[:8].hex()}...")
    if not session.verify_proof(m2_proof.hex().encode()):
        print("*** server M2 proof verification FAILED locally ***")
    else:
        print("*** PAIRING OK — server proof verified ***")

    # --- Encrypted GET /info over control channel ---
    shared = binascii.unhexlify(session.key)
    out_key = hkdf_expand("Control-Salt", "Control-Write-Encryption-Key", shared)
    in_key = hkdf_expand("Control-Salt", "Control-Read-Encryption-Key", shared)
    out_c, in_c = ChaCha20Poly1305(out_key), ChaCha20Poly1305(in_key)
    out_n = in_n = 0

    req = (
        "GET /info RTSP/1.0\r\n"
        "CSeq: 3\r\n"
        "User-Agent: AirPlay/320.20\r\n"
        "Content-Length: 0\r\n\r\n"
    ).encode()
    # Frame: len_le16 || ciphertext || tag16, AAD = len_le16, nonce = 4x00 || ctr_le64
    frames = bytearray()
    for i in range(0, len(req), 0x400):
        block = req[i:i + 0x400]
        length = struct.pack("<H", len(block))
        nonce = b"\x00" * 4 + struct.pack("<Q", out_n)
        out_n += 1
        ct = out_c.encrypt(nonce, block, length)
        frames += length + ct
    sock.sendall(bytes(frames))

    data = b""
    sock.settimeout(5)
    try:
        while True:
            chunk = sock.recv(4096)
            if not chunk:
                break
            data += chunk
            if len(data) >= 2:
                need = 2 + struct.unpack("<H", data[:2])[0] + 16
                if len(data) >= need:
                    break
    except socket.timeout:
        pass
    if len(data) < 18:
        print(f"no encrypted response ({len(data)} bytes)")
        return
    length = struct.unpack("<H", data[:2])[0]
    nonce = b"\x00" * 4 + struct.pack("<Q", in_n)
    plain = in_c.decrypt(nonce, data[2:2 + length + 16], data[:2])
    print(f"*** ENCRYPTED GET /info RESPONSE ({len(plain)} bytes) ***")
    print(plain[:300])


if __name__ == "__main__":
    main()
