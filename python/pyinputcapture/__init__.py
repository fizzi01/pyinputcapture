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

RemoteDesktop (input emulation via libei, grant kept across runs)::

    from pyinputcapture import RemoteDesktopPortal, register_host_app

    register_host_app("org.example.App")  # before any other portal object
    rd = RemoteDesktopPortal()
    eis_fd, token = rd.setup(restore_token=saved_token)
    # … store `token`, emulate input on eis_fd with libei …
    rd.close()
"""

from pyinputcapture.pyinputcapture import (
    InputCapturePortal,
    RemoteDesktopPortal,
    register_host_app,
)
from pyinputcapture._async import AsyncInputCapturePortal

__version__ = "0.3.0"  # x-release-please-version

__all__ = [
    "InputCapturePortal",
    "AsyncInputCapturePortal",
    "RemoteDesktopPortal",
    "register_host_app",
    "__version__",
]
