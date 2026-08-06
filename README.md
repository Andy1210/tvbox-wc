# tvbox-wc

The Wayland compositor for the [tvbox](https://github.com/Andy1210/tvbox), a
Raspberry Pi 5 TV box. Built on [Smithay](https://github.com/Smithay/smithay).

**Not a general-purpose compositor, on purpose.** It exists to make the decisions a
general one refuses to make:

- the film takes the display's **primary plane**, straight from the hardware decoder
- the shell's **translucent fullscreen UI** takes an **overlay plane** above it
- the output's colour space follows the content (HDR on for a PQ film, off after)
- the compositor itself does **no per-frame GPU work** while a film is playing

On a Pi 5 that is not an optimisation, it is the difference between working and not:
at 4K there are two full-screen GPU passes to pay for and the chip fits one. The
display engine can compose those layers for free, which is exactly what Kodi-gbm and
Fire OS do by owning the display outright. This does the same without giving up
Electron apps, RetroArch or a browser.

## Why a new compositor instead of patching labwc

The previous route was labwc + wlroots with a local patch set. It works - 4K HDR
film on the primary plane, UI on an overlay, zero dropped frames - but it took
**eight patches across two upstream projects**, and every version bump moves them.
Of those, only four fixed actual bugs; the rest supply features nobody wrote
(`wlr_scene` never drove output layers, GLES2 has no colour transform, labwc's
reconfigure never applied `<hdr>`).

Smithay's `DrmCompositor` already does plane assignment, including overlay planes,
with an atomic test commit per candidate. Measured on the hardware before starting
this: a **translucent** fullscreen client gets an overlay plane with the compositor
at **1 ms/s** of GPU time, out of the box. That is the half that cost eight patches
elsewhere.

The half Smithay got wrong is one function, and it is fixed here in
[`src/kms/framebuffer.rs`](src/kms/framebuffer.rs) - see
[`docs/measurements.md`](docs/measurements.md) for the full chain and the three
hypotheses that turned out wrong on the way.

## Status

Runs the box. `tvbox-gaming` boots on it: greetd starts `tvbox-wc -- tvbox-session`,
the shell comes up as a client, and mode setting, focus, typing, screenshots and
remote input all go through it. The film-on-a-plane arrangement is what it was
built for and what was measured first.

| | |
| --- | --- |
| `src/kms/framebuffer.rs` | direct dmabuf -> KMS framebuffer export, bypassing gbm for client buffers |
| `src/kms/hdr.rs` | the output's colour space and its HDR metadata blob |
| `src/stacking.rs` | the shell's window stays in front of everything else |
| `src/ipc.rs` + `docs/ipc.md` | the control socket the shell drives all of this from |
| `src/typing.rs` | typing a string no ordinary keymap can produce |
| `tools/` | the probe harness the measurements were taken with |
| `docs/measurements.md` | what the hardware actually does, and what it refuses |

Not there yet: XWayland (the shell's picture-in-picture player needs a home), and
HDR is implemented but unverified against a set that accepts PQ.

## Decisions worth knowing

- **No labwc fallback.** The box keeps running labwc until this is finished, and then
  switches over in one step. A runtime "try the new one, fall back to the old one"
  path would be permanent noise for a transition that happens once. The existing
  `tvbox-compositor` wrapper on the boxes stays as it is until switch-over, and is
  retired with it.
- **Integration lands on a branch** and stays there until the box can boot on this
  alone. `main` is not expected to be bootable before that.
- **The name is 8 characters for a reason.** `/proc/<pid>/comm` truncates at 15, so
  `pgrep -x` and every comm-based tool silently miss a longer name - measured:
  `tvbox-compositor` reads back as `tvbox-composito`. Diagnostics here match the
  process by exact comm.
- **MIT**, like Smithay and like tvbox, so the framebuffer fix can go upstream.
- **Do not copy code from niri.** It is GPL-3.0-or-later and was read here as a
  reference implementation. Anything learned from it is re-derived from measurements.

## Installing

The box takes a release binary: `tvbox-wc-aarch64` is attached to every `v*` tag,
with its sha256 next to it, and tvbox's `deploy/install-compositor.sh` pins both.
Nothing else is needed at runtime beyond the libraries it links against
(`libgbm1 libseat1 libinput10 libxkbcommon0 libwayland-server0 libegl1 libgles2`).

greetd starts it as the whole session:

```
command = "tvbox-wc -- /usr/local/bin/tvbox-session"
```

Everything after `--` is started once the Wayland socket is listening, and the
compositor stops when it exits.

## Building

Needs a recent stable Rust (1.85+) and the Smithay build dependencies:

```sh
sudo apt install gcc clang libclang-dev libudev-dev libgbm-dev libxkbcommon-dev \
    libegl-dev libgles-dev libwayland-dev libinput-dev libseat-dev libdisplay-info-dev
cargo build --release
```

On a 4 GB Pi 5, build with `-j3`.

## Tools

The harness in [`tools/`](tools/) is how every claim in `docs/measurements.md` was
produced, and how a regression gets caught:

```sh
make -C tools                       # overlay, gbmprobe
sudo tools/planes.py --comp tvbox-wc # what is actually on each plane, and who did the work
tools/gpuprobe.sh tvbox-wc          # GPU time per configuration
```

Two measurement rules learned the hard way, both in `docs/measurements.md`: the
plane count cannot distinguish compositing from scan-out (the compositor's GPU time
can), and a video player will happily report 60 fps at a frozen screen (compare
consecutive screenshot hashes).
