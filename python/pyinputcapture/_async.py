"""Async wrapper around the blocking InputCapturePortal (Rust/PyO3)."""

from __future__ import annotations

import asyncio
from typing import Optional

from pyinputcapture.pyinputcapture import InputCapturePortal


class AsyncInputCapturePortal:
    """Async facade over InputCapturePortal.

    Every method offloads work to the default thread-pool executor.
    """

    def __init__(self) -> None:
        self._portal = InputCapturePortal()

    @property
    def zones(self) -> list[tuple[int, int, int, int]]:
        return self._portal.zones

    @property
    def activation_id(self) -> int:
        return self._portal.activation_id

    async def setup(self) -> tuple[list[tuple[int, int, int, int]], int]:
        loop = asyncio.get_running_loop()
        return await loop.run_in_executor(None, self._portal.setup)

    async def enable(self) -> None:
        loop = asyncio.get_running_loop()
        await loop.run_in_executor(None, self._portal.enable)

    async def disable(self) -> None:
        loop = asyncio.get_running_loop()
        await loop.run_in_executor(None, self._portal.disable)

    async def release(
        self,
        cursor_x: Optional[float] = None,
        cursor_y: Optional[float] = None,
    ) -> None:
        loop = asyncio.get_running_loop()
        await loop.run_in_executor(
            None, self._portal.release, cursor_x, cursor_y
        )

    async def close(self) -> None:
        loop = asyncio.get_running_loop()
        await loop.run_in_executor(None, self._portal.close)
