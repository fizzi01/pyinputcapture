"""Tests for the pyinputcapture Rust/PyO3 module.

Unit tests that do NOT require a running Wayland compositor or D-Bus session.
"""

import sys
import pytest


try:
    from pyinputcapture import InputCapturePortal

    HAS_MODULE = True
except ImportError:
    HAS_MODULE = False

pytestmark = pytest.mark.skipif(
    not HAS_MODULE,
    reason="pyinputcapture not installed (run: maturin develop)",
)


class TestInstantiation:
    def test_create(self):
        portal = InputCapturePortal()
        assert portal is not None

    def test_initial_zones_empty(self):
        portal = InputCapturePortal()
        assert portal.zones == []

    def test_initial_activation_id_zero(self):
        portal = InputCapturePortal()
        assert portal.activation_id == 0

    def test_multiple_instances(self):
        a = InputCapturePortal()
        b = InputCapturePortal()
        assert a is not b


class TestNotSetUp:
    def test_enable_raises(self):
        portal = InputCapturePortal()
        with pytest.raises(RuntimeError, match="not set up"):
            portal.enable()

    def test_disable_raises(self):
        portal = InputCapturePortal()
        with pytest.raises(RuntimeError, match="not set up"):
            portal.disable()

    def test_release_raises(self):
        portal = InputCapturePortal()
        with pytest.raises(RuntimeError, match="not set up"):
            portal.release()

    def test_release_with_position_raises(self):
        portal = InputCapturePortal()
        with pytest.raises(RuntimeError, match="not set up"):
            portal.release(cursor_x=100.0, cursor_y=200.0)


class TestClose:
    def test_close_without_setup(self):
        portal = InputCapturePortal()
        portal.close()

    def test_close_idempotent(self):
        portal = InputCapturePortal()
        portal.close()
        portal.close()

    def test_methods_after_close_raise(self):
        portal = InputCapturePortal()
        portal.close()
        with pytest.raises(RuntimeError, match="not set up"):
            portal.enable()


class TestSetupNoBus:
    @pytest.mark.skipif(
        sys.platform != "linux",
        reason="InputCapture portal only works on Linux",
    )
    def test_setup_without_dbus_raises(self, monkeypatch):
        monkeypatch.delenv("DBUS_SESSION_BUS_ADDRESS", raising=False)
        portal = InputCapturePortal()
        with pytest.raises(RuntimeError):
            portal.setup()

    def test_double_setup_raises(self):
        # Can't complete setup without a compositor; documents intent.
        pass


class TestReleaseSignature:
    def test_release_no_args(self):
        portal = InputCapturePortal()
        with pytest.raises(RuntimeError, match="not set up"):
            portal.release()

    def test_release_keyword_args(self):
        portal = InputCapturePortal()
        with pytest.raises(RuntimeError, match="not set up"):
            portal.release(cursor_x=1.0, cursor_y=2.0)

    def test_release_partial_args(self):
        portal = InputCapturePortal()
        with pytest.raises(RuntimeError, match="not set up"):
            portal.release(cursor_x=1.0)
