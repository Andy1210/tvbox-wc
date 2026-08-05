#!/usr/bin/env python3
"""Read the vc4 atomic state and report, per plane, what is actually being
scanned out: format, size, zpos and which process allocated the framebuffer.

Also reports the compositor's GPU time (0 ns means the display engine composed,
not the GPU) and mpv's drop counters when its IPC socket is up.

usage: sudo planes.py [--mpv-sock /tmp/mpvsock] [--comp niri]
"""
import argparse
import json
import os
import re
import socket
import sys

STATE = "/sys/kernel/debug/dri/1/state"  # dri/1 is vc4 on this box, dri/0 is v3d


def planes():
    try:
        text = open(STATE).read()
    except PermissionError:
        sys.exit("need root to read " + STATE)
    out = []
    cur = None
    for line in text.splitlines():
        m = re.match(r"^plane\[(\d+)\]: (\S+)", line)
        if m:
            if cur:
                out.append(cur)
            cur = {"id": m.group(1), "name": m.group(2), "fb": None, "crtc": None,
                   "fmt": None, "size": None, "zpos": None, "owner": None}
            continue
        if cur is None:
            continue
        if (m := re.match(r"\s*crtc=(\S+)", line)):
            cur["crtc"] = m.group(1)
        elif (m := re.match(r"\s*fb=(\d+)", line)):
            cur["fb"] = m.group(1)
        elif (m := re.match(r"\s*allocated by\s*=\s*(\S+)", line)):
            cur["owner"] = m.group(1)
        elif (m := re.match(r"\s*format=(\S+)", line)):
            cur["fmt"] = m.group(1)
        elif (m := re.match(r"\s*size=(\S+)", line)) and cur["size"] is None:
            cur["size"] = m.group(1)
        elif (m := re.match(r"\s*zpos=(\d+)", line)):
            cur["zpos"] = m.group(1)
    if cur:
        out.append(cur)
    return [p for p in out if p["fb"] and p["crtc"] not in (None, "(null)")]


def gpu_ns(name):
    """Sum drm-engine-* over a process's fdinfo, deduped by drm-client-id."""
    total, seen = 0, set()
    for pid in os.listdir("/proc"):
        if not pid.isdigit():
            continue
        try:
            if open(f"/proc/{pid}/comm").read().strip() != name:
                continue
            for fd in os.listdir(f"/proc/{pid}/fdinfo"):
                info = open(f"/proc/{pid}/fdinfo/{fd}").read()
                cid = re.search(r"drm-client-id:\s*(\d+)", info)
                if not cid or cid.group(1) in seen:
                    continue
                seen.add(cid.group(1))
                for ns in re.findall(r"drm-engine-\S+:\s*(\d+) ns", info):
                    total += int(ns)
        except (OSError, ValueError):
            continue
    return total


def mpv(sock_path):
    props = ["frame-drop-count", "vo-delayed-frame-count", "video-params/w",
             "video-params/pixelformat", "video-params/gamma", "hwdec-current",
             "current-vo", "time-pos"]
    out = {}
    try:
        s = socket.socket(socket.AF_UNIX)
        s.settimeout(2)
        s.connect(sock_path)
        for p in props:
            s.sendall(json.dumps({"command": ["get_property", p]}).encode() + b"\n")
            buf = b""
            while b"\n" not in buf:
                buf += s.recv(65536)
            for line in buf.split(b"\n"):
                if not line.strip():
                    continue
                try:
                    r = json.loads(line)
                except ValueError:
                    continue
                if "data" in r or r.get("error") != "success":
                    out[p] = r.get("data", r.get("error"))
                    break
        s.close()
    except OSError as e:
        out["_error"] = str(e)
    return out


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--mpv-sock", default="/tmp/mpvsock")
    ap.add_argument("--comp", default="niri")
    a = ap.parse_args()

    print("== vc4 planes with a framebuffer")
    for p in planes():
        print(f"  plane[{p['id']:>3}] {p['name']:<9} {p['crtc']:<7} "
              f"{str(p['fmt']):<22} {str(p['size']):<12} zpos={p['zpos']} "
              f"owner={p['owner']}")
    print(f"\n== {a.comp} GPU time: {gpu_ns(a.comp)} ns   "
          f"(mpv: {gpu_ns('mpv')} ns)")
    print("\n== mpv")
    for k, v in mpv(a.mpv_sock).items():
        print(f"  {k} = {v}")
