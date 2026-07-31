# pyinputcapture

Python bindings for the [XDG InputCapture portal](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.InputCapture.html) on Wayland, built with [ashpd](https://github.com/bilelmoussaoui/ashpd) + [PyO3](https://pyo3.rs).

Capture pointer input across monitor boundaries via `libei`. Provides both blocking and async APIs.

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

`timeout` (seconds) bounds the wait for the permission dialog: `None` waits
indefinitely and Ctrl-C still works, `0` or negative raises `ValueError`.

`set_barriers` takes either `edges` (whole zone edges by name) or `segments` —
`(label, x1, y1, x2, y2)` axis-aligned lines in absolute desktop coordinates,
for when a client abuts only part of an edge — and returns the new
`barrier_map`. Passing both raises `TypeError`; a diagonal segment raises
`ValueError` naming its index. It reuses the live session on purpose:
re-running `setup` is what hangs the GNOME portal.

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
