#!/usr/bin/env python3
"""Send input the way a remote or a mouse would, through a virtual device.

    fakeremote.py back            press and release KEY_BACK
    fakeremote.py enter
    fakeremote.py move 200 150    move the pointer by that much
    fakeremote.py click 200 150   move there and click

The device is created with uinput, so it arrives through libinput exactly as real
hardware does. Needs membership of the `input` group.
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


def move(dx, dy):
    capabilities = {
        ecodes.EV_REL: [ecodes.REL_X, ecodes.REL_Y],
        ecodes.EV_KEY: [ecodes.BTN_LEFT],
    }
    with UInput(capabilities, name="tvbox-fake-mouse") as device:
        time.sleep(1.5)
        # In steps, because a single large jump is one event and easy to miss.
        for _ in range(10):
            device.write(ecodes.EV_REL, ecodes.REL_X, dx // 10)
            device.write(ecodes.EV_REL, ecodes.REL_Y, dy // 10)
            device.syn()
            time.sleep(0.03)
        time.sleep(0.3)


def click(dx, dy):
    capabilities = {
        ecodes.EV_REL: [ecodes.REL_X, ecodes.REL_Y],
        ecodes.EV_KEY: [ecodes.BTN_LEFT],
    }
    with UInput(capabilities, name="tvbox-fake-mouse") as device:
        time.sleep(1.5)
        for _ in range(10):
            device.write(ecodes.EV_REL, ecodes.REL_X, dx // 10)
            device.write(ecodes.EV_REL, ecodes.REL_Y, dy // 10)
            device.syn()
            time.sleep(0.03)
        time.sleep(0.2)
        device.write(ecodes.EV_KEY, ecodes.BTN_LEFT, 1)
        device.syn()
        time.sleep(0.08)
        device.write(ecodes.EV_KEY, ecodes.BTN_LEFT, 0)
        device.syn()
        time.sleep(0.3)


def main():
    if len(sys.argv) > 1 and sys.argv[1] == "click":
        click(int(sys.argv[2]), int(sys.argv[3]))
        return 0

    if len(sys.argv) > 1 and sys.argv[1] == "move":
        move(int(sys.argv[2]), int(sys.argv[3]))
        return 0

    if len(sys.argv) < 2 or sys.argv[1] not in KEYS:
        print(f"usage: fakeremote.py <{'|'.join(KEYS)}|move dx dy>")
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
