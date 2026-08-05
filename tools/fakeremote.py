#!/usr/bin/env python3
"""Send a key the way a remote would, through a virtual input device.

    fakeremote.py back        press and release KEY_BACK
    fakeremote.py enter

The device is created with uinput, so it arrives through libinput exactly as a real
remote does. Needs membership of the `input` group.
"""
import sys
import time

from evdev import UInput, ecodes

KEYS = {
    "back": ecodes.KEY_BACK,
    "backspace": ecodes.KEY_BACKSPACE,
    "enter": ecodes.KEY_ENTER,
    "up": ecodes.KEY_UP,
    "down": ecodes.KEY_DOWN,
}


def main():
    if len(sys.argv) < 2 or sys.argv[1] not in KEYS:
        print(f"usage: fakeremote.py <{'|'.join(KEYS)}>")
        return 2

    key = KEYS[sys.argv[1]]
    with UInput({ecodes.EV_KEY: list(KEYS.values())}, name="tvbox-fake-remote") as device:
        # libinput has to see the device appear before it will report anything from
        # it, and that goes through udev.
        time.sleep(1.5)
        device.write(ecodes.EV_KEY, key, 1)
        device.syn()
        time.sleep(0.05)
        device.write(ecodes.EV_KEY, key, 0)
        device.syn()
        time.sleep(0.3)
    return 0


if __name__ == "__main__":
    sys.exit(main())
