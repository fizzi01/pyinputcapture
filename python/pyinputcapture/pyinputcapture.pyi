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
