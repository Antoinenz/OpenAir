# Differential-testing oracle: a minimal HAP pair-setup (M1-M4) server built on
# srptools -- the same SRP library pyatv uses, which is proven to interoperate
# with Shairport Sync / pair_ap. Prints every SRP intermediate so the Rust
# client's values can be diffed against it.
#
# Usage: python tools/hap_oracle_server.py  (listens on 127.0.0.1:7001)
import hashlib
import socket

from srptools import SRPContext
from srptools.utils import int_from_hex, int_to_bytes

PRIME_3072 = (
    "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD1"
    "29024E088A67CC74020BBEA63B139B22514A08798E3404DD"
    "EF9519B3CD3A431B302B0A6DF25F14374FE1356D6D51C245"
    "E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7ED"
    "EE386BFB5A899FA5AE9F24117C4B1FE649286651ECE45B3D"
    "C2007CB8A163BF0598DA48361C55D39A69163FA8FD24CF5F"
    "83655D23DCA3AD961C62F356208552BB9ED529077096966D"
    "670C354E4ABC9804F1746C08CA18217C32905E462E36CE3B"
    "E39E772C180E86039B2783A2EC07A28FB5C55DF06F4C52C9"
    "DE2BCBF6955817183995497CEA956AE515D2261898FA0510"
    "15728E5A8AAAC42DAD33170D04507A33A85521ABDF1CBA64"
    "ECFB850458DBEF0A8AEA71575D060C7DB3970F85A6E1E4C7"
    "ABF5AE8CDB0933D71E8C94E04A25619DCEE3D2261AD2EE6B"
    "F12FFA06D98A0864D87602733EC86A64521F2B18177B200C"
    "BBE117577A615D6C770988C0BAD946E208E24FA074E5AB31"
    "43DB5BFCE0FD108E4B82D120A93AD2CAFFFFFFFFFFFFFFFF"
)

# Fixed salt and server private key for reproducibility (no leading-zero salt).
SALT = bytes.fromhex("38f36210687af19dcf59207698cbd71c")
B_PRIV_HEX = "60975527035CF2AD1989806F0407210BC81EDC04E2762A56AFD529DDDA2D4393"

ctx = SRPContext(
    "Pair-Setup", "3939",
    prime=PRIME_3072, generator="5",
    hash_func=hashlib.sha512, bits_random=512,
)


def hx(b: bytes) -> str:
    return b.hex()


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


def read_request(conn):
    buf = b""
    while b"\r\n\r\n" not in buf:
        chunk = conn.recv(4096)
        if not chunk:
            return None, None
        buf += chunk
    head, _, rest = buf.partition(b"\r\n\r\n")
    headers = head.decode("latin1").split("\r\n")
    clen = 0
    for h in headers:
        if h.lower().startswith("content-length:"):
            clen = int(h.split(":", 1)[1].strip())
    while len(rest) < clen:
        chunk = conn.recv(4096)
        if not chunk:
            break
        rest += chunk
    return headers, rest[:clen]


def respond(conn, cseq, body):
    hdr = (
        f"RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\n"
        f"Content-Type: application/octet-stream\r\n"
        f"Content-Length: {len(body)}\r\n\r\n"
    ).encode()
    conn.sendall(hdr + body)


def get_cseq(headers):
    for h in headers:
        if h.lower().startswith("cseq:"):
            return h.split(":", 1)[1].strip()
    return "1"


def handle(conn):
    salt_int = int.from_bytes(SALT, "big")
    b_priv = int_from_hex(B_PRIV_HEX)
    x = ctx.get_common_password_hash(salt_int)
    v = ctx.get_common_password_verifier(x)
    B = ctx.get_server_public(v, b_priv)
    print(f"ORACLE salt = {hx(SALT)}")
    print(f"ORACLE x    = {hx(int_to_bytes(x))}")
    print(f"ORACLE B    = {hx(int_to_bytes(B))}")
    print(f"ORACLE k    = {hx(int_to_bytes(ctx._mult))}")

    while True:
        headers, body = read_request(conn)
        if headers is None:
            print("ORACLE connection closed by client")
            return
        cseq = get_cseq(headers)
        tlv = tlv_decode(body)
        state = tlv.get(6, b"\x00")[0]
        print(f"ORACLE << {headers[0]} state=M{state}")

        if state == 1:
            flags = tlv.get(0x13, tlv.get(0x10))
            print(f"ORACLE M1 method={tlv.get(0, b'?').hex()} flags="
                  f"{flags.hex() if flags else None} extra_tags={sorted(tlv)}")
            resp = tlv_encode([
                (6, b"\x02"),
                (2, SALT),
                (3, int_to_bytes(B).rjust(384, b"\x00")),
            ])
            respond(conn, cseq, resp)

        elif state == 3:
            A = int.from_bytes(tlv[3], "big")
            client_M1 = tlv[4]
            u = ctx.get_common_secret(B, A)
            S = ctx.get_server_premaster_secret(v, b_priv, A, u)
            K = ctx.get_common_session_key(S)
            expect_M1 = ctx.get_common_session_key_proof(K, salt_int, B, A)
            print(f"ORACLE A(recv)    = {hx(tlv[3])}  ({len(tlv[3])} bytes)")
            print(f"ORACLE u          = {hx(int_to_bytes(u))}")
            print(f"ORACLE S          = {hx(int_to_bytes(S))}")
            print(f"ORACLE K          = {hx(K)}")
            print(f"ORACLE M1 expect  = {hx(expect_M1)}")
            print(f"ORACLE M1 client  = {hx(client_M1)}")
            if client_M1 == expect_M1:
                print("ORACLE *** M1 PROOF OK ***")
                M2 = ctx.get_common_session_key_proof_hash(K, expect_M1, A)
                respond(conn, cseq, tlv_encode([(6, b"\x04"), (4, M2)]))
            else:
                print("ORACLE *** M1 PROOF MISMATCH ***")
                respond(conn, cseq, tlv_encode([(6, b"\x04"), (7, b"\x02")]))
                return
        else:
            print(f"ORACLE unhandled state {state}; closing")
            return


def main():
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", 7001))
    srv.listen(1)
    print("ORACLE listening on 127.0.0.1:7001")
    while True:
        conn, _ = srv.accept()
        try:
            handle(conn)
        except Exception as e:  # noqa: BLE001
            print(f"ORACLE error: {e!r}")
        finally:
            conn.close()


if __name__ == "__main__":
    main()
