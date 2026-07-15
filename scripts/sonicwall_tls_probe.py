#!/usr/bin/env python3
"""Probe SonicWall EVPN TLS routing without authenticating or sending EVPN data."""

from __future__ import annotations

import argparse
import socket
import ssl
import time


EVPN_Z_METHOD = 0xEC


def inject_evpn_z(record: bytes) -> bytes:
    data = bytearray(record)
    if len(data) < 44 or data[0] != 0x16 or data[5] != 0x01:
        raise ValueError("buffer is not a TLS ClientHello record")
    record_len = int.from_bytes(data[3:5], "big")
    if record_len + 5 > len(data):
        raise ValueError("TLS ClientHello record is incomplete")
    handshake_len = int.from_bytes(data[6:9], "big")
    if handshake_len + 4 != record_len:
        raise ValueError("TLS ClientHello handshake length mismatch")

    session_len_offset = 9 + 2 + 32
    session_len = data[session_len_offset]
    cipher_len_offset = session_len_offset + 1 + session_len
    cipher_len = int.from_bytes(data[cipher_len_offset : cipher_len_offset + 2], "big")
    compression_len_offset = cipher_len_offset + 2 + cipher_len
    compression_len = data[compression_len_offset]
    compression_start = compression_len_offset + 1
    compression_end = compression_start + compression_len
    if compression_end > record_len + 5:
        raise ValueError("TLS ClientHello compression method vector is incomplete")
    if EVPN_Z_METHOD in data[compression_start:compression_end]:
        return bytes(data)

    data.insert(compression_start, EVPN_Z_METHOD)
    data[compression_len_offset] = compression_len + 1
    data[3:5] = (record_len + 1).to_bytes(2, "big")
    data[6:9] = (handshake_len + 1).to_bytes(3, "big")
    return bytes(data)


def connect_underlay(server: str, port: int, proxy: str | None, timeout: float) -> socket.socket:
    if proxy is None:
        return socket.create_connection((server, port), timeout=timeout)
    proxy_host, proxy_port_text = proxy.rsplit(":", 1)
    sock = socket.create_connection((proxy_host, int(proxy_port_text)), timeout=timeout)
    request = (
        f"CONNECT {server}:{port} HTTP/1.1\r\n"
        f"Host: {server}:{port}\r\n"
        "Proxy-Connection: keep-alive\r\n\r\n"
    ).encode("ascii")
    sock.sendall(request)
    response = bytearray()
    while b"\r\n\r\n" not in response:
        chunk = sock.recv(4096)
        if not chunk:
            raise ConnectionError("proxy closed before CONNECT response")
        response.extend(chunk)
        if len(response) > 65536:
            raise ConnectionError("proxy CONNECT response is too large")
    status_line = bytes(response).split(b"\r\n", 1)[0]
    parts = status_line.split(b" ", 2)
    if len(parts) < 2 or parts[1] != b"200":
        raise ConnectionError(f"proxy CONNECT failed: {status_line.decode('ascii', 'replace')}")
    return sock


def run_probe(server: str, port: int, proxy: str | None, timeout: float) -> tuple[str, tuple[str, str, int]]:
    sock = connect_underlay(server, port, proxy, timeout)
    sock.settimeout(timeout)
    incoming = ssl.MemoryBIO()
    outgoing = ssl.MemoryBIO()
    context = ssl.create_default_context()
    context.minimum_version = ssl.TLSVersion.TLSv1_2
    tls = context.wrap_bio(incoming, outgoing, server_hostname=server)
    patched = False
    deadline = time.monotonic() + timeout
    try:
        while time.monotonic() < deadline:
            try:
                tls.do_handshake()
                return tls.version() or "unknown", tls.cipher() or ("unknown", "unknown", 0)
            except ssl.SSLWantReadError:
                pass

            pending = outgoing.read()
            if pending:
                if not patched:
                    pending = inject_evpn_z(pending)
                    patched = True
                sock.sendall(pending)

            chunk = sock.recv(65536)
            if not chunk:
                raise ConnectionError("gateway closed during TLS handshake")
            incoming.write(chunk)
        raise TimeoutError("TLS handshake timed out")
    finally:
        sock.close()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--server", required=True)
    parser.add_argument("--port", type=int, default=443)
    parser.add_argument("--proxy", help="HTTP CONNECT proxy as host:port")
    parser.add_argument("--timeout", type=float, default=15.0)
    args = parser.parse_args()
    version, cipher = run_probe(args.server, args.port, args.proxy, args.timeout)
    print(f"EVPN TLS handshake succeeded: version={version} cipher={cipher[0]}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
