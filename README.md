# pyinputcapture

Python bindings for the [XDG InputCapture portal](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.InputCapture.html) on Wayland, built with [ashpd](https://github.com/bilelmoussaoui/ashpd) + [PyO3](https://pyo3.rs).

Capture pointer input across monitor boundaries via `libei`. Provides both blocking and async APIs. Also wraps the RemoteDesktop portal for input emulation with a grant that survives restarts.

## Requirements

- Linux with a Wayland compositor supporting `org.freedesktop.portal.InputCapture` (e.g. GNOME 46+, KDE Plasma 6.1+)
- Python >= 3.11

## Installation

```bash
pip install pyinputcapture
```

## Usage

### Blocking API

```python
import select
from pyinputcapture import InputCapturePortal

portal = InputCapturePortal()

# Create a session with barriers on specific edges (default: all edges)
zones, eis_fd, barrier_map = portal.setup(edges=["left", "right"])
# zones:       [(width, height, x_offset, y_offset), ...]
# eis_fd:      file descriptor for reading EIS events via libei
# barrier_map: [(barrier_id, edge_name), ...]

# Enable capture (barriers become active)
portal.enable()

# Map barrier IDs to edge names for quick lookup
barriers = {bid: edge for bid, edge in barrier_map}

# Poll for barrier activations (cursor crossing a screen edge)
poller = select.poll()
poller.register(eis_fd, select.POLLIN)

last_activation = 0
running = True

while running:
    # Check if a new barrier was activated
    current_id = portal.activation_id
    if current_id != last_activation:
        last_activation = current_id
        edge = barriers.get(portal.barrier_id)
        cx, cy = portal.cursor_position
        print(f"Barrier hit: {edge} at ({cx}, {cy})")

        # Read EIS events from eis_fd with your preferred libei binding
        # (e.g. snegg, python-libei) to get pointer motion, buttons, scroll...
        if poller.poll(10):
            pass  # dispatch EIS events here

        # Release capture and reposition cursor at absolute coordinates
        portal.release(cursor_x=960.0, cursor_y=540.0)

        # Or release without repositioning
        # portal.release()

        # Re-enable to capture again
        portal.enable()

# Disable barriers and close the session
portal.disable()
portal.close()
```

### Async API

```python
import asyncio
from pyinputcapture import AsyncInputCapturePortal

async def main():
    portal = AsyncInputCapturePortal()
    zones, eis_fd, barrier_map = await portal.setup()

    await portal.enable()
    # ... poll eis_fd and handle activations ...
    await portal.release(cursor_x=960.0, cursor_y=540.0)
    await portal.close()

asyncio.run(main())
```

### Properties

Once a session is active, you can inspect activation state via properties:

```python
portal.zones             # [(width, height, x_offset, y_offset), ...]
portal.barrier_map       # [(barrier_id, label), ...] currently armed
portal.zones_generation  # bumped when the zones changed and barriers re-armed
portal.activation_id     # latest activation ID from the compositor
portal.barrier_id        # barrier ID from the last Activated signal
portal.cursor_position   # (x, y) cursor position at activation
```

The compositor can replace the zones mid-session (monitor plugged in, resolution
changed). When that happens the barriers are re-armed automatically and
`zones_generation` changes — a cached `zones`/`barrier_map` must be re-read.

## API Reference

| Method / Property                            | Description                                                             |
|----------------------------------------------|-------------------------------------------------------------------------|
| `setup(edges=None, timeout=120.0)`           | Create session and set barriers. Returns `(zones, eis_fd, barrier_map)` |
| `set_barriers(edges=None, *, segments=None)` | Re-arm barriers on the live session                                     |
| `enable()`                                   | Enable capture (barriers become active)                                 |
| `disable()`                                  | Disable capture (barriers deactivated)                                  |
| `release(cursor_x, cursor_y)`                | Release captured input, optionally reposition cursor                    |
| `close()`                                    | Close the session and shut down the background task                     |
| `.zones`                                     | Screen zones `[(w, h, x_off, y_off), ...]`                              |
| `.barrier_map`                               | Armed barriers `[(barrier_id, label), ...]`                             |
| `.zones_generation`                          | Bumped when the zones changed and barriers were re-armed                |
| `.activation_id`                             | Latest activation ID                                                    |
| `.barrier_id`                                | Barrier ID from last activation                                         |
| `.cursor_position`                           | Cursor `(x, y)` at last activation                                      |
| `.last_error`                                | Portal task's last error, or `None`; cleared by `setup()`               |

`timeout` (seconds) bounds the wait for the permission dialog: `None` waits
indefinitely and Ctrl-C still works, `0` or negative raises `ValueError`.

`last_error` is the only way to see a failure the task hit *after* `setup()`
returned or gave up — a late answer to a dialog, a session the compositor
dropped. It used to go to stderr, which a caller that redirects fd 2 discards.

**One tokio runtime per process, shared by every portal object, never shut
down.** `ashpd` caches the D-Bus session connection in a process-global static,
so every `InputCapture::new()` reuses it and its zbus tasks live on whichever
runtime created it first. Before 0.3.0 each portal object built its own runtime
and shut it down on `Drop`, killing those tasks while the dead connection stayed
cached: from then on every portal request in the process was accepted and never
answered, so a permission dialog cancelled once never came back. The setup phase
of the task is bounded (the caller's `timeout` plus a small margin) because the
task now outlives the object that spawned it.

`set_barriers` takes either `edges` (whole zone edges by name) or `segments` —
`(label, x1, y1, x2, y2)` axis-aligned lines in absolute desktop coordinates,
for when a client abuts only part of an edge — and returns the new
`barrier_map`. Passing both raises `TypeError`; a diagonal segment raises
`ValueError` naming its index. It reuses the live session on purpose:
re-running `setup` is what hangs the GNOME portal.

## RemoteDesktop (input emulation with a persistent grant)

`RemoteDesktopPortal` opens a pointer-only RemoteDesktop session and returns
the EIS fd for emulating input with libei. With `persist=True` the portal keeps
the grant until the user revokes it, and the returned `restore_token` reopens
the session with no dialog next time.

```python
from pyinputcapture import RemoteDesktopPortal, register_host_app

register_host_app("org.example.App")  # before any other portal object
rd = RemoteDesktopPortal()
eis_fd, token = rd.setup(restore_token=saved_token, persist=True, timeout=300)
save(token)  # single use: always keep the latest one
# … emulate pointer input on eis_fd with libei …
rd.close()
```

| Function / Method                                          | Description                                                       |
|------------------------------------------------------------|-------------------------------------------------------------------|
| `register_host_app(app_id)`                                | Register the app id grants are stored under (x-d-p >= 1.19.4)     |
| `RemoteDesktopPortal.setup(restore_token, persist, timeout)` | Open the session. Returns `(eis_fd, restore_token)`              |
| `RemoteDesktopPortal.close()`                              | End the session, or the wait for it, from any thread              |
| `RemoteDesktopPortal.last_error`                           | Portal task's last error, or `None`                               |

`setup` raises `PermissionError` when the portal refuses (a saved token is then
worthless), `TimeoutError` when the dialog is not answered in time, and
`RuntimeError` otherwise. `timeout` is capped at one hour.

**Register before anything else.** The portal fixes a host process's app id on
the first call over its D-Bus connection, and every portal object here shares
that connection. Grants are keyed by app id, so it must name the app's
`.desktop` file and stay the same across launches. Without a Registry
(xdg-desktop-portal < 1.19.4) `register_host_app` raises `RuntimeError` and the
portal falls back to the id of the process's systemd scope.

## Portal capabilities

`portal_versions(timeout=2.0)` asks the portal which of the interfaces this
library drives it offers, so a caller can pick its backend from what the
desktop can do instead of from `XDG_CURRENT_DESKTOP`:

```python
from pyinputcapture import portal_versions, register_host_app

register_host_app("org.example.App")  # still first: same D-Bus connection
versions = portal_versions()           # {"remote_desktop": 2, "input_capture": 1}
can_emulate = versions["remote_desktop"] >= 2   # ConnectToEIS (libei)
can_capture = versions["input_capture"] >= 1
```

An interface the portal does not offer reads as `0`. It raises `TimeoutError`
when the portal does not answer within `timeout` and `RuntimeError` when there
is no portal service at all (no session bus, no xdg-desktop-portal).

## Development

```bash
# Dev tooling (maturin, pytest, pytest-timeout)
pip install -e ".[dev]"

# Build and install in dev mode
maturin develop

# Run tests (Rust + Python)
make test
```

## License

GPL-3.0-or-later
