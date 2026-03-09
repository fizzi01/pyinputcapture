"""
pyinputcapture — Wayland InputCapture portal for Python.

Wraps ashpd (Rust) via PyO3 to provide both a blocking and an async
interface to the XDG InputCapture portal (org.freedesktop.portal.InputCapture).

Quick start (blocking)::

    from pyinputcapture import InputCapturePortal

    portal = InputCapturePortal()
    zones, eis_fd, barrier_map = portal.setup()
    # … read EIS events from eis_fd …
    portal.release(cursor_x=960.0, cursor_y=540.0)
    portal.close()

Quick start (async)::

    from pyinputcapture import AsyncInputCapturePortal

    portal = AsyncInputCapturePortal()
    zones, eis_fd, barrier_map = await portal.setup()
    # …
    await portal.release(cursor_x=960.0, cursor_y=540.0)
    await portal.close()
"""

from pyinputcapture.pyinputcapture import InputCapturePortal
from pyinputcapture._async import AsyncInputCapturePortal

__version__ = "0.1.0"

__all__ = [
    "InputCapturePortal",
    "AsyncInputCapturePortal",
    "__version__",
]
