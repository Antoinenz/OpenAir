# Transparent TCP proxy: 127.0.0.1:7010 -> receiver:7000, hex-dumping both
# directions to stdout for wire-level differential analysis.
import socket
import sys
import threading

TARGET = (sys.argv[1] if len(sys.argv) > 1 else "192.168.1.106", 7000)


def dump(tag, data):
    print(f"--- {tag} {len(data)} bytes ---")
    print(data.hex())
    sys.stdout.flush()


def pump(src, dst, tag):
    try:
        while True:
            data = src.recv(65536)
            if not data:
                break
            dump(tag, data)
            dst.sendall(data)
    except OSError:
        pass
    finally:
        try:
            dst.shutdown(socket.SHUT_WR)
        except OSError:
            pass


def main():
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", 7010))
    srv.listen(2)
    print(f"proxy 127.0.0.1:7010 -> {TARGET[0]}:{TARGET[1]}")
    sys.stdout.flush()
    while True:
        client, _ = srv.accept()
        upstream = socket.create_connection(TARGET, timeout=15)
        threading.Thread(target=pump, args=(client, upstream, ">>C2S"), daemon=True).start()
        threading.Thread(target=pump, args=(upstream, client, "<<S2C"), daemon=True).start()


if __name__ == "__main__":
    main()
