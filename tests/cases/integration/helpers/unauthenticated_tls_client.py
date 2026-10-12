#!/usr/bin/env python3
"""A local TLS 1.3 client that proves REALITY forwards unauthenticated traffic.

The client deliberately shapes its ClientHello so it cannot carry a REALITY auth
envelope:

* ``empty-session-id`` disables the OpenSSL middlebox-compatibility option, so the
  legacy session id is empty.
* ``p256-only`` restricts the offered groups to secp256r1, so the ClientHello
  offers a valid P-256 key share and no X25519 key share.

The client then completes a TLS handshake with the configured cover *through* the
proxy, echoes one payload, and prints the result. Exit status 0 means the payload
was echoed byte for byte; 1 means the handshake or the echo failed.

This helper runs offline against loopback addresses only. Its identity is pinned
by the verification adapter before and after every execution.
"""

import argparse
import socket
import ssl
import sys


def build_context(mode: str) -> ssl.SSLContext:
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    context.check_hostname = False
    context.verify_mode = ssl.CERT_NONE
    context.minimum_version = ssl.TLSVersion.TLSv1_3
    context.maximum_version = ssl.TLSVersion.TLSv1_3
    if hasattr(ssl, "OP_ENABLE_MIDDLEBOX_COMPAT"):
        # Without middlebox compatibility the legacy session id stays empty.
        context.options &= ~ssl.OP_ENABLE_MIDDLEBOX_COMPAT
    if mode == "p256-only":
        context.set_ecdh_curve("prime256v1")
    return context


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--mode", choices=("empty-session-id", "p256-only"), required=True)
    parser.add_argument("--server-name", default="localhost")
    parser.add_argument("--payload", default="reality-forwarding")
    parser.add_argument("--timeout", type=float, default=8.0)
    args = parser.parse_args()

    payload = args.payload.encode()
    try:
        raw = socket.create_connection(("127.0.0.1", args.port), timeout=args.timeout)
        raw.settimeout(args.timeout)
    except OSError as error:
        print(f"FAILED connect: {error}")
        return 1

    try:
        with build_context(args.mode).wrap_socket(raw, server_hostname=args.server_name) as tls:
            tls.sendall(payload)
            received = b""
            while len(received) < len(payload):
                chunk = tls.recv(len(payload) - len(received))
                if not chunk:
                    break
                received += chunk
    except Exception as error:  # noqa: BLE001 - the mode is reported to the test
        print(f"FAILED {type(error).__name__}: {error}")
        return 1

    if received != payload:
        print(f"MISMATCH {received!r}")
        return 1
    print(f"OK {received.decode()}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
