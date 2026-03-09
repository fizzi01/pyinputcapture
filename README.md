# pyinputcapture

Python bindings for the [XDG InputCapture portal](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.InputCapture.html) on Wayland, built with [ashpd](https://github.com/bilelmoussaoui/ashpd) + [PyO3](https://pyo3.rs).

Provides both blocking and async APIs to capture pointer input across monitor boundaries via `libei`.

## Requirements

- Linux with a Wayland compositor supporting `org.freedesktop.portal.InputCapture`
- Python >= 3.11

## Installation

```bash
pip install pyinputcapture
```

## Usage

```python
from pyinputcapture import InputCapturePortal

portal = InputCapturePortal()
zones, eis_fd, barrier_map = portal.setup()
# read EIS events from eis_fd ...
portal.release(cursor_x=960.0, cursor_y=540.0)
portal.close()
```

Async variant:

```python
from pyinputcapture import AsyncInputCapturePortal

portal = AsyncInputCapturePortal()
zones, eis_fd, barrier_map = await portal.setup()
await portal.release(cursor_x=960.0, cursor_y=540.0)
await portal.close()
```

## Development

```bash
# Build and install in dev mode
maturin develop

# Run tests
make test
```

## License

GPL-3.0-or-later
