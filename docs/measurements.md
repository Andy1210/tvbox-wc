# What the hardware does

Every number here was measured on `tvbox-gaming` (Raspberry Pi 5, 4 GB, Raspberry Pi
OS trixie, kernel 6.18.39, LG panel at 1920x1080@60) on 2026-08-05, with niri v26.04
standing in for a compositor and Smithay at
`ff5fa7df392cecfba049ffed55cdaa4e98a8e7ef`.

The point of writing it down is that most of it is invisible from the code.

## The result this project is built on

A hardware-decoded film and a translucent fullscreen UI above it, 20 second soak:

```
plane[83]  plane-2   NV12 1920x1080   <- the film's own decoder buffer
plane[272] plane-19  AR24 1920x1080   <- the translucent UI, on an overlay plane
plane[657] plane-54  AR24 64x64       <- cursor
compositor GPU: 0 ms/s      mpv: 1 dropped frame      59.8 C, throttled=0x0
```

Getting there needed **one** change to how a client buffer becomes a KMS
framebuffer. Everything else - deciding which element gets which plane, testing the
combination against the driver, falling back to composition when it fails - Smithay
already does.

## The framebuffer defect, as instrumented

Smithay's `GbmFramebufferExporter` imports the client dmabuf into gbm, then builds
the framebuffer from the **gbm bo's** per-plane GEM handles. Each log line below is
real:

```
can_add_framebuffer: NV12 + Broadcom_sand128, node Some(57984 Render), filter Node(57984 Render) -> eligible
gbm import succeeded for NV12 + Broadcom_sand128
drmModeAddFB2 from the gbm bo failed ... (Invalid argument, EINVAL)
direct dmabuf import: framebuffer created for NV12 Some(Broadcom_sand128)
```

gbm can only describe the planes of a format the **render** driver knows, and Mesa's
v3d/vc4 knows no YUV format at all: `gbm_device_get_format_modifier_plane_count`
answers `-1` for NV12 and P030 with every modifier, LINEAR included, on both the
display node and the render node. The import still reports success, so the handle
for the second plane cannot be right, and the kernel refuses the framebuffer.

wlroots never hits this because it goes from the dmabuf's plane fds straight to
`drmPrimeFDToHandle` + `drmModeAddFB2WithModifiers`. That is what
`src/kms/framebuffer.rs` does.

## Three hypotheses that were wrong

Kept because each one looked convincing and cost a cycle.

1. **"gbm refuses the format."** `gbm_bo_create` refuses NV12 and P030;
   `gbm_bo_import` accepts them. A create-based probe is not a proxy for the import
   path. `tools/gbmprobe.c` still exists, but read its output as "what gbm can
   allocate", never as "what gbm can import".
2. **"the node filter rejects client dmabufs."** Smithay's `can_add_framebuffer`
   compares the dmabuf's recorded device node against a `NodeFilter`, and both
   `create_dmabuf` call sites in `wayland/dmabuf/dispatch.rs` pass `None` - yet the
   node is set elsewhere: measured `node Some(57984 Render)`, matching the filter.
   The filter was never the blocker. (This exporter still does not filter by node:
   whether a node was recorded says nothing about whether KMS will take the buffer.)
3. **"no log line means the code did not run."** Two independent reasons that is
   false. A compositor's log filter commonly drops the `smithay` target entirely, so
   `warn!` from inside Smithay is invisible - and a release build compiles `trace!`
   out (`tracing`'s `release_max_level_debug`). On top of that, the first instrument
   only logged failures, so a *successful* gbm import was indistinguishable from
   never reaching the function. Log both arms of every decision, with `eprintln!` if
   the log plumbing is not yours.

## What the display engine will and will not scan out

From the kernel's `IN_FORMATS` on the vc4 planes (`modetest -M vc4 -p`):

```
NV12:  BROADCOM_SAND128  BROADCOM_SAND64  BROADCOM_SAND256  LINEAR
P030:  BROADCOM_SAND128
AR24:  BROADCOM_VC4_T_TILED  LINEAR
XR24:  BROADCOM_VC4_T_TILED  LINEAR
```

Consequences that bite:

- **`AR24 + BROADCOM_UIF` cannot be scanned out**, and `drmModeAddFB2` says EINVAL
  for it whichever path you take. That is correct, not a bug: UIF is not in the
  plane's list. Mesa renegotiates to LINEAR and gets its plane. Do not "fix" it.
- **The vc4 primary plane's `zpos` is immutable at 0**, so the video must be the
  bottom layer and the UI above it - which is the arrangement we want anyway. Setting
  an immutable property makes the kernel reject the *whole* atomic request with
  EINVAL, which reads exactly like "the hardware refuses this arrangement".
- A 1920x1080 buffer scans out fine on a 1360x768 output: the plane scales.
- A TV may advertise a DCI-4K mode (4096 wide) this hardware cannot drive. Use the
  preferred mode.

## 10-bit video: the format could not be named

**Resolved.** With the fix below, a 10-bit film plays with its own P030 buffer on
the primary plane and a translucent fullscreen UI on an overlay plane, compositor
at 0 ms/s, 0 dropped frames.

The fix is one line of `Cargo.toml`: `drm-fourcc` gained P030 when its enums were
regenerated on kernel 6.15.9, but there has been no crates.io release since 2.2.0
in 2021, so the crate every Smithay build resolves to cannot name the format. We
pin the crate to the upstream git revision until a release lands
(danielzfranklin/drm-fourcc-rs#31 already asks for one). Smithay itself needs no
change: `has_alpha` answers false for a format it does not list, which is right
for P030, and `get_bpp`/`get_depth` are only used by the legacy AddFB fallback
that the direct exporter never takes.

The rest of this section is what it took to find that, kept because two plausible
answers came first and both were wrong.

With everything above in place, a 10-bit film still fails before it starts:

```
[vo/dmabuf-wayland] Format 'P030' with modifier '(0700000000000004)' is not supported by the compositor.
```

The first answer was that this is a dmabuf feedback **policy** question - niri
builds its scan-out tranche as `plane formats ∩ renderer formats`, which drops
P030, and then strips non-LINEAR modifiers when the display device has no render
node. Both are true, and both were fixed here: the tranche is built from the plane
formats without intersecting the renderer's, and its target device is the render
node (a target device is where a client must be able to **allocate**; naming the
card node makes clients ignore the tranche entirely).

It made no difference, and the reason is lower down: the released **`drm-fourcc`
has no `P030` variant.** It knows P010, P012 and P016, and P030 - the Pi's 10-bit
decoder output - is simply absent. Smithay converts the kernel's `IN_FORMATS`
into `DrmFourcc`, so P030 was dropped there and could never reach a tranche, a
plane assignment or a framebuffer through the typed API. The kernel advertises it
happily (`modetest -M vc4 -p` lists `P030: BROADCOM_SAND128` on the primary
plane), and labwc scans out exactly that buffer on this hardware.

Both tranche corrections were still right, and both are kept: the intersection
dropped formats that exist to be scanned out, and a tranche naming a device the
client cannot allocate on is ignored wholesale.

## Measurement discipline

- **Plane count cannot tell compositing from scan-out.** A composited frame and a
  directly scanned out one both occupy one plane. The compositor's own GPU time is
  the discriminator: sum `drm-engine-*` from `/proc/<pid>/fdinfo/*`, deduped by
  `drm-client-id`. `tools/planes.py` does both.
- **Always check the picture moves.** `mpv --vo=gpu` under niri reported a live
  `time-pos` and 60 fps with a frozen screen and no GPU work from anyone. Compare
  consecutive screenshot hashes (`grim`).
- **A test client must produce a dmabuf.** An shm buffer can never reach a KMS plane,
  so `tools/overlay.c` uses EGL/GLES2 - the same reason the real UI (Chromium) can be
  offloaded at all.
- **Check what the player actually got.** A streaming client can hand you 1080p when
  you asked for 4K, which looks identical to the offload breaking. Read
  `video-params/w` before attributing anything to the compositor.
