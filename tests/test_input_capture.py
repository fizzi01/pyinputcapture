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

    def test_initial_barrier_map_empty(self):
        portal = InputCapturePortal()
        assert portal.barrier_map == []

    def test_initial_zones_generation_zero(self):
        # Bumped only when the compositor replaces the zones mid-session.
        portal = InputCapturePortal()
        assert portal.zones_generation == 0

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


class TestSetBarriers:
    """``set_barriers`` re-arms barriers on a live session.

    Without a compositor only the argument handling is reachable, which is
    still the part a caller can get wrong. The barrier geometry itself is
    covered by the Rust unit tests for ``build_segment_barriers``.
    """

    def test_exists(self):
        portal = InputCapturePortal()
        assert hasattr(portal, "set_barriers")

    def test_raises_when_not_set_up(self):
        portal = InputCapturePortal()
        with pytest.raises(RuntimeError, match="not set up"):
            portal.set_barriers(["left"])

    def test_no_args_raises_when_not_set_up(self):
        portal = InputCapturePortal()
        with pytest.raises(RuntimeError, match="not set up"):
            portal.set_barriers()

    def test_empty_edges_raises_when_not_set_up(self):
        # An empty list means "arm nothing", a legitimate request that must
        # not be confused with None ("arm every edge").
        portal = InputCapturePortal()
        with pytest.raises(RuntimeError, match="not set up"):
            portal.set_barriers([])

    def test_segments_keyword_raises_when_not_set_up(self):
        portal = InputCapturePortal()
        with pytest.raises(RuntimeError, match="not set up"):
            portal.set_barriers(segments=[("left", 0, 300, 0, 800)])

    def test_segments_is_keyword_only(self):
        portal = InputCapturePortal()
        with pytest.raises(TypeError):
            portal.set_barriers(["left"], [("left", 0, 0, 0, 100)])

    def test_edges_and_segments_together_rejected(self):
        # Conflicting arguments is a call error, so TypeError like the rest of
        # Python. Checked before the "not set up" guard, so it surfaces here.
        portal = InputCapturePortal()
        with pytest.raises(TypeError, match="not both"):
            portal.set_barriers(["left"], segments=[("left", 0, 0, 0, 100)])

    def test_malformed_segment_is_rejected(self):
        # PyO3 reports a wrong-length tuple as ValueError, not the TypeError a
        # pure-Python signature would raise; pin what the binding actually does.
        portal = InputCapturePortal()
        with pytest.raises(ValueError, match="length 5"):
            portal.set_barriers(segments=[("left", 0, 300)])

    def test_diagonal_segment_is_rejected_by_index(self):
        # Dropping it silently would leave the caller believing that span is
        # armed; labels can repeat, so the returned map cannot say which one
        # went missing. Checked before the "not set up" guard.
        portal = InputCapturePortal()
        with pytest.raises(ValueError, match="segment 1"):
            portal.set_barriers(
                segments=[
                    ("left", 0, 0, 0, 500),
                    ("diagonal", 0, 0, 500, 500),
                ]
            )

    def test_after_close_raises_not_set_up(self):
        portal = InputCapturePortal()
        portal.close()
        with pytest.raises(RuntimeError, match="not set up"):
            portal.set_barriers(["left"])


class TestSetupNoBus:
    @pytest.fixture(autouse=True)
    def _no_session_bus(self, monkeypatch):
        # Every test in here calls setup(). Without this the calls would reach a
        # real portal on a desktop session and pop a permission dialog mid-suite.
        #
        # Unsetting DBUS_SESSION_BUS_ADDRESS is not enough: zbus then falls back
        # to $XDG_RUNTIME_DIR/bus, which exists on a CI runner, and setup() parks
        # on a D-Bus reply that never comes. Point the address at a socket that
        # cannot exist so the connection fails immediately on every machine.
        monkeypatch.setenv(
            "DBUS_SESSION_BUS_ADDRESS",
            "unix:path=/nonexistent/pyinputcapture-test-bus",
        )
        monkeypatch.delenv("XDG_RUNTIME_DIR", raising=False)

    @pytest.mark.skipif(
        sys.platform != "linux",
        reason="InputCapture portal only works on Linux",
    )
    def test_setup_without_dbus_raises(self):
        portal = InputCapturePortal()
        with pytest.raises(RuntimeError):
            portal.setup()

    @pytest.mark.skipif(
        sys.platform != "linux",
        reason="InputCapture portal only works on Linux",
    )
    def test_setup_failure_names_the_failing_step(self):
        """The reason must survive the hop back to Python.

        Every setup step exits via `?`, which used to drop the result sender
        unsent — leaving only the generic "portal setup channel closed" while the
        real cause went to stderr and was usually discarded.
        """
        portal = InputCapturePortal()
        with pytest.raises(RuntimeError) as excinfo:
            portal.setup()
        assert "channel closed" not in str(excinfo.value)

    def test_setup_accepts_a_positional_timeout(self):
        portal = InputCapturePortal()
        with pytest.raises(RuntimeError):
            portal.setup(None, 0.25)

    def test_setup_accepts_timeout_by_keyword(self):
        portal = InputCapturePortal()
        with pytest.raises(RuntimeError):
            portal.setup(edges=["left"], timeout=0.25)

    @pytest.mark.timeout(30)
    def test_setup_accepts_none_timeout(self):
        # None means "wait indefinitely", so this test is only bounded by the
        # bus being unreachable. The explicit mark makes a regression here fail
        # as itself instead of hanging the whole suite, which is how it first
        # showed up in CI.
        portal = InputCapturePortal()
        with pytest.raises(RuntimeError):
            portal.setup(["left"], None)

    def test_setup_rejects_a_zero_timeout(self):
        # Zero reads as "fail fast", never as "wait forever".
        portal = InputCapturePortal()
        with pytest.raises(ValueError, match="greater than 0"):
            portal.setup(None, 0.0)

    def test_setup_rejects_a_negative_timeout(self):
        portal = InputCapturePortal()
        with pytest.raises(ValueError):
            portal.setup(None, -1.0)

    def test_setup_rejects_nan_timeout(self):
        portal = InputCapturePortal()
        with pytest.raises(ValueError, match="NaN"):
            portal.setup(None, float("nan"))

    @pytest.mark.timeout(30)
    def test_setup_accepts_infinite_timeout(self):
        # inf is a legitimate spelling of "wait forever"; it used to panic
        # inside Duration::from_secs_f64.
        portal = InputCapturePortal()
        with pytest.raises(RuntimeError):
            portal.setup(None, float("inf"))


class TestLastError:
    """The late-failure channel.

    A portal task can only fail *after* ``setup()`` has returned or given up, and
    its reason used to go to stderr - which the daemon points at /dev/null for the
    lifetime of its capture thread (libei's dispatch spam), so the one account of
    the failure was discarded.
    """

    def test_starts_empty(self):
        portal = InputCapturePortal()
        assert portal.last_error is None

    def test_survives_a_close(self):
        portal = InputCapturePortal()
        portal.close()
        assert portal.last_error is None

    @pytest.mark.skipif(
        sys.platform != "linux",
        reason="InputCapture portal only works on Linux",
    )
    def test_a_failed_setup_leaves_a_reason(self, monkeypatch):
        monkeypatch.setenv(
            "DBUS_SESSION_BUS_ADDRESS",
            "unix:path=/nonexistent/pyinputcapture-test-bus",
        )
        monkeypatch.delenv("XDG_RUNTIME_DIR", raising=False)
        portal = InputCapturePortal()
        with pytest.raises(RuntimeError):
            portal.setup(["left"], 0.25)

        assert portal.last_error, "the task's reason has nowhere else to go"


class TestSharedRuntime:
    """Dropping a portal object must not disturb the next one.

    ashpd caches the D-Bus session connection in a process-global static, so the
    runtime carrying that connection's zbus tasks is process-global too. When it
    was per-object, dropping one object shut its runtime down and killed those
    tasks while the dead connection stayed cached: every later request in the
    process was then accepted and never answered - the reason a cancelled
    permission dialog never came back. Only a live compositor can show that
    end to end; what is reachable here is that the churn is survivable at all.
    """

    def test_many_objects_come_and_go(self):
        for _ in range(20):
            portal = InputCapturePortal()
            portal.close()
            del portal

    def test_an_object_outlives_its_predecessors(self):
        survivor = InputCapturePortal()
        for _ in range(5):
            InputCapturePortal().close()
        # Reaching the "not set up" guard proves the object is still functional
        # after every predecessor was dropped.
        with pytest.raises(RuntimeError, match="not set up"):
            survivor.enable()


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
