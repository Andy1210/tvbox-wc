#!/usr/bin/env python3
"""Talk to the compositor's control socket.

    wcctl.py get_outputs
    wcctl.py set_mode HDMIA-1 1920 1080

The socket is $TVBOX_WC_SOCKET, or $XDG_RUNTIME_DIR/tvbox-wc.sock.
"""
import json
import os
import socket
import sys


def request(payload, timeout=5.0):
    path = os.environ.get("TVBOX_WC_SOCKET") or os.path.join(
        os.environ.get("XDG_RUNTIME_DIR", "/tmp"), "tvbox-wc.sock"
    )
    sock = socket.socket(socket.AF_UNIX)
    sock.settimeout(timeout)
    sock.connect(path)
    sock.sendall((json.dumps(payload) + "\n").encode())

    buffer = b""
    while b"\n" not in buffer:
        chunk = sock.recv(65536)
        if not chunk:
            break
        buffer += chunk
    sock.close()
    return json.loads(buffer.split(b"\n")[0])


def main():
    if len(sys.argv) < 2:
        print(__doc__.strip())
        return 2

    command = sys.argv[1]
    payload = {"id": 1, "request": command}
    if command == "type_text":
        if len(sys.argv) < 3:
            print("usage: wcctl.py type_text <text>")
            return 2
        payload["text"] = " ".join(sys.argv[2:])
    if command == "screenshot":
        payload["path"] = sys.argv[2] if len(sys.argv) > 2 else "/tmp/tvbox-wc.png"
    if command == "set_focus":
        if len(sys.argv) < 3:
            print("usage: wcctl.py set_focus <launcher|app> [app-id]")
            return 2
        payload["owner"] = sys.argv[2]
        if len(sys.argv) > 3:
            payload["app"] = sys.argv[3]
    if command == "set_hdr":
        if len(sys.argv) < 4:
            print("usage: wcctl.py set_hdr <output> <on|off>")
            return 2
        payload["output"] = sys.argv[2]
        payload["on"] = sys.argv[3] in ("on", "true", "1")
    if command == "set_mode":
        if len(sys.argv) < 5:
            print("usage: wcctl.py set_mode <output> <w> <h> [refresh]")
            return 2
        payload["output"] = sys.argv[2]
        payload["w"] = int(sys.argv[3])
        payload["h"] = int(sys.argv[4])
        if len(sys.argv) > 5:
            payload["refresh"] = int(sys.argv[5])

    reply = request(payload)
    print(json.dumps(reply, indent=2))
    return 0 if "error" not in reply else 1


if __name__ == "__main__":
    sys.exit(main())
