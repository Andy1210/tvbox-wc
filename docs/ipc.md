# The control socket

The shell drives the compositor over a unix socket: one JSON object per line,
request and response matched by `id`. The path is in `TVBOX_WC_SOCKET`, which the
compositor exports for its children, and defaults to
`$XDG_RUNTIME_DIR/tvbox-wc.sock`.

The framing is deliberately the same as mpv's control socket. The shell already
speaks it, so this needs no new client code, and a person can drive it from a
terminal with `tools/wcctl.py`.

Access control is the socket's own: it lives in the session user's runtime
directory. Anything that can reach it can already reach the Wayland socket.

## Requests

### `get_outputs`

```json
{"id": 1, "request": "get_outputs"}
{"id": 1, "ok": {"outputs": [{
  "name": "HDMIA-1",
  "current": {"w": 1360, "h": 768, "refresh": 60000, "preferred": true},
  "modes":   [{"w": 1360, "h": 768, "refresh": 60000, "preferred": true}, ...]
}]}}
```

Refresh is in mHz, the same unit the Wayland output protocol uses.

### `set_mode`

```json
{"id": 2, "request": "set_mode", "output": "HDMIA-1", "w": 1920, "h": 1080}
{"id": 2, "ok": null}
```

`refresh` is optional. A TV usually offers one rate per size, and the shell should
not have to know whether the kernel calls it 59.94 or 60; leave it out and the
first mode of that size wins.

The compositor's own surfaces follow the new mode: layer surfaces are laid out
again and fullscreen windows are reconfigured. A film already on a plane stays
there, scaled by the display engine, which is why a mode change does not interrupt
playback.

### `set_hdr`

```json
{"id": 3, "request": "set_hdr", "output": "HDMIA-1", "on": true}
{"id": 3, "ok": null}
```

A claim, not a setting. The colour space covers the whole output, so while it is
held the SDR UI on its overlay plane is read as PQ. Claim it for the duration of PQ
playback and release it after, the same way the mode is claimed.

`get_outputs` reports `"hdr": {"supported": true, "on": false}`. `supported` means
the driver exposes the connector properties a claim needs; it says nothing about
the panel. Whether the TV can show HDR is in its EDID, which the shell already
reads.

### `set_focus` and `get_state`

```json
{"id": 4, "request": "set_focus", "owner": "app", "app": "plex"}
{"id": 4, "ok": null}

{"id": 5, "request": "get_state"}
{"id": 5, "ok": {"focus": {"app": "plex"}}}
```

`owner` is `launcher` or `app`. The compositor cannot work this out for itself: the
launcher and an app can be windows of the same process, and "an app is on screen"
is the shell's own state machine rather than a property of any surface.

It is not bookkeeping. The remote's Back key (`KEY_BACK`) reaches a web app as
`BrowserBack`, and the app UIs the box runs only act on Backspace, so the key is
rewritten while an app owns the screen and left alone while the launcher does,
because the launcher handles it itself. The shell does this today with
`sendInputEvent` in three separate places; here it happens once, for every client,
including the ones that are not Electron.

### `type_text`

```json
{"id": 7, "request": "type_text", "text": "arvizturo tukorfurogep"}
{"id": 7, "ok": {"delivered": true}}
```

How the on-screen keyboard and a paired phone put text into a focused field. The
alternative is synthesising key events, which needs an xkb keymap carrying every
character in the string, generated per string.

`delivered` says a client was there to take it, not that it appeared in the field.

**Not finished.** The text reaches the client and Chromium enables its text input
once the compositor sends `enter` (which it now does, tied to keyboard focus), but
the field stays empty. Measured cause: smithay discards a client's `enable` while no
input-method client is bound (`has_instance()` in `text_input_handle.rs`), so the
text input never becomes "active" and `done` is never sent - and without `done` a
client discards the committed string. Two ways out, neither taken yet: bind an
input-method instance from inside the compositor, or change smithay so a compositor
can serve text-input on its own. The generated-keymap route remains the fallback
that needs no agreement from anybody.

### `screenshot`

```json
{"id": 6, "request": "screenshot", "path": "/tmp/screen.png"}
{"id": 6, "ok": {"path": "/tmp/screen.png", "w": 1360, "h": 768}}
```

Renders the scene off-screen and writes a PNG. It exists for measurement: with the
video on a plane and the compositor doing no GPU work, "everything is fine" reads
the same as a frozen screen in every counter, twice measured. Two shots a few
seconds apart, compared, is the cheapest honest check that a client is drawing.

What it captures is the scene, not the planes as the display engine composes them.
That is also what a capture protocol would hand a client. Whether the right thing
is on the right plane is a question for the plane state, not for this.

### Errors

```json
{"id": 3, "request": "set_mode", "output": "HDMIA-1", "w": 1234, "h": 567}
{"id": 3, "error": "no mode 1234x567 on HDMIA-1"}
```

An unparseable line is answered with `{"id": null, "error": "..."}`.

## Not here yet

The shell also needs to hand over what only it knows, and to be told what only the
compositor knows. In rough order of when it will be needed:

- HDR: claim a colour space for the duration of PQ playback and release it after.
- Focus: which of the launcher and an app owns the screen, so the remote's Back key
  can be remapped for one and not the other.
- Events: the compositor telling the shell about a mode or connector change,
  instead of the shell asking.
