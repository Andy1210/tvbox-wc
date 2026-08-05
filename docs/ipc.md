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
