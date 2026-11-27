"""Type stubs for the native Rust/PyO3 module."""

class InputCapturePortal:
    """Wayland InputCapture portal. All methods are blocking."""

    def __init__(self) -> None: ...

    @property
    def zones(self) -> list[tuple[int, int, int, int]]:
        """Screen zones as [(width, height, x_offset, y_offset), ...].

        Re-read after `zones_generation` changes: the compositor can replace
        them mid-session.
        """
        ...

    @property
    def barrier_map(self) -> list[tuple[int, str]]:
        """Currently armed barriers as [(barrier_id, label), ...]."""
        ...

    @property
    def zones_generation(self) -> int:
        """Bumped whenever the zones changed and the barriers were re-armed."""
        ...

    @property
    def activation_id(self) -> int:
        """Latest activation ID received from the compositor."""
        ...

    @property
    def barrier_id(self) -> int:
        """Barrier ID from the last Activated signal."""
        ...

    @property
    def cursor_position(self) -> tuple[float, float]:
        """Cursor position (x, y) from the last Activated signal."""
        ...

    @property
    def last_error(self) -> str | None:
        """The portal task's last error, or None.

        The only way to recover a *late* failure: the task can fail only after
        `setup()` has returned or given up, and a caller that redirects fd 2
        (to silence libei's dispatch spam) would otherwise never see the
        reason. Cleared at the start of every `setup()`.
        """
        ...

    def setup(
        self,
        edges: list[str] | None = None,
        timeout: float | None = 120.0,
    ) -> tuple[list[tuple[int, int, int, int]], int, list[tuple[int, str]]]:
        """Create session, set barriers, connect to EIS.

        Returns (zones, eis_fd, barrier_map).

        Blocks until the portal answers — on GNOME, until the user answers the
        permission dialog — but releases the GIL while waiting, so Ctrl-C still
        works. `timeout` (seconds) bounds that wait; pass None to wait
        indefinitely. Zero or negative raises ValueError.
        """
        ...

    def enable(self) -> None:
        """Re-enable capture (barriers become active again)."""
        ...

    def set_barriers(
        self,
        edges: list[str] | None = None,
        *,
        segments: list[tuple[str, int, int, int, int]] | None = None,
    ) -> list[tuple[int, str]]:
        """Replace the armed pointer barriers on the existing session.

        Pass either `edges` (whole zone edges by name, None = all of them) or
        `segments` ((label, x1, y1, x2, y2) axis-aligned lines in absolute
        desktop coordinates, for an edge only partly abutted) — not both,
        which raises TypeError. A non-axis-aligned segment raises ValueError
        naming its index.

        Returns the new barrier_map, minus any barrier the compositor
        rejected. Reuses the current session (re-running setup hangs the
        GNOME portal).
        """
        ...

    def disable(self) -> None:
        """Disable capture (barriers deactivated)."""
        ...

    def release(
        self,
        cursor_x: float | None = None,
        cursor_y: float | None = None,
    ) -> None:
        """Release captured input. Optional cursor reposition on release."""
        ...

    def close(self) -> None:
        """Close the session and shut down the background task."""
        ...


class RemoteDesktopPortal:
    """Wayland RemoteDesktop portal, pointer devices only, for libei emulation.

    One object, one session: `setup` once, `close` (or drop) to end it.
    """

    def __init__(self) -> None: ...

    @property
    def last_error(self) -> str | None:
        """The portal task's last error, or None. Cleared by `setup()`."""
        ...

    def setup(
        self,
        restore_token: str | None = None,
        persist: bool = True,
        timeout: float | None = 300.0,
    ) -> tuple[int, str | None]:
        """Open the session and connect to EIS. Returns (eis_fd, restore_token).

        Blocks, with the GIL released, until the portal grants the session: at
        once when `restore_token` is still valid, otherwise when the user
        answers the dialog. The returned token is the one to present next time
        (single use: always store the latest).

        Raises PermissionError when the portal refuses (a saved token is then
        worthless), TimeoutError past `timeout`, RuntimeError otherwise.
        `close()` from another thread ends the wait. `timeout` is capped at
        one hour; None waits indefinitely.
        """
        ...

    def close(self) -> None:
        """End the session, or the wait for it. Never blocks."""
        ...

def portal_versions(timeout: float = 2.0) -> dict[str, int]:
    """Versions of the portal interfaces this library drives, 0 when absent.

    Keys are ``"remote_desktop"`` and ``"input_capture"``. RemoteDesktop needs
    version 2 for libei (``ConnectToEIS``). Call ``register_host_app`` first:
    this goes over the shared D-Bus connection. Raises TimeoutError when the
    portal does not answer and RuntimeError when there is no portal to ask.
    """
    ...

def register_host_app(app_id: str) -> None:
    """Tell the portal which app this non-sandboxed process is.

    Grants are stored per app id. Call before any other portal object in the
    process. Repeating the same id is a no-op; a different one raises
    ValueError, as does an invalid id. Raises RuntimeError when the portal has
    no Registry (xdg-desktop-portal < 1.19.4) and TimeoutError when it does not
    answer; the process can carry on unregistered.
    """
    ...
