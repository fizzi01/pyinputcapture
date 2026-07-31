"""Async wrapper around the blocking InputCapturePortal (Rust/PyO3)."""

from __future__ import annotations

import asyncio
from functools import partial
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
    def barrier_map(self) -> list[tuple[int, str]]:
        return self._portal.barrier_map

    @property
    def zones_generation(self) -> int:
        return self._portal.zones_generation

    @property
    def activation_id(self) -> int:
        return self._portal.activation_id

    @property
    def barrier_id(self) -> int:
        return self._portal.barrier_id

    @property
    def cursor_position(self) -> tuple[float, float]:
        return self._portal.cursor_position

    @property
    def activation(self) -> tuple[int, int, tuple[float, float]]:
        """`(activation_id, barrier_id, (x, y))` of the last barrier hit.

        Read activation_id first: it is written last, with release ordering, so
        reading it before the other two is what guarantees they belong to the
        same activation. Reading the properties separately in the wrong order
        can mix an old barrier with a new id.
        """
        activation_id = self._portal.activation_id
        return activation_id, self._portal.barrier_id, self._portal.cursor_position

    async def setup(
        self,
        edges: Optional[list[str]] = None,
        timeout: Optional[float] = 120.0,
    ) -> tuple[list[tuple[int, int, int, int]], int, list[tuple[int, str]]]:
        loop = asyncio.get_running_loop()
        return await loop.run_in_executor(
            None, partial(self._portal.setup, edges, timeout)
        )

    async def enable(self) -> None:
        loop = asyncio.get_running_loop()
        await loop.run_in_executor(None, self._portal.enable)

    async def disable(self) -> None:
        loop = asyncio.get_running_loop()
        await loop.run_in_executor(None, self._portal.disable)

    async def set_barriers(
        self,
        edges: Optional[list[str]] = None,
        *,
        segments: Optional[list[tuple[str, int, int, int, int]]] = None,
    ) -> list[tuple[int, str]]:
        loop = asyncio.get_running_loop()
        return await loop.run_in_executor(
            None, partial(self._portal.set_barriers, edges, segments=segments)
        )

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
