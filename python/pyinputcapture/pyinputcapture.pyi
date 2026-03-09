"""Type stubs for the native Rust/PyO3 module."""

class InputCapturePortal:
    """Wayland InputCapture portal. All methods are blocking."""

    def __init__(self) -> None: ...

    @property
    def zones(self) -> list[tuple[int, int, int, int]]:
        """Screen zones as [(width, height, x_offset, y_offset), ...]."""
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
        self, edges: list[str] | None = None
    ) -> tuple[list[tuple[int, int, int, int]], int, list[tuple[int, str]]]:
        """Create session, set barriers, connect to EIS.

        Returns (zones, eis_fd, barrier_map).
        """
        ...

    def poll_activated(self) -> tuple[int, float, float] | None:
        """Pop the next Activated event from the queue, or None."""
        ...

    def enable(self) -> None:
        """Re-enable capture (barriers become active again)."""
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
