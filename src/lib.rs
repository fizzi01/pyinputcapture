//! PyO3 wrapper around ashpd's XDG InputCapture portal.
//!
//! Exposes `InputCapturePortal` to Python: a blocking API backed by a
//! tokio runtime.  Python communicates with the background task through
//! channels.
//!
//! Activation data (barrier_id, cursor position) is shared via atomics
//! packed in a single `SharedActivation` struct behind one `Arc`.
//! `activation_id` is written **last** with `Release` ordering so that
//! a Python `Acquire` load that sees the new ID is guaranteed to also
//! see the corresponding barrier_id and cursor position.

use std::num::NonZeroU32;
use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use ashpd::desktop::input_capture::{ActivatedBarrier, Barrier, Capabilities, InputCapture};
use ashpd::desktop::Session;
use futures_util::StreamExt;
use pyo3::exceptions::{PyRuntimeError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use tokio::sync::{mpsc, oneshot};

/// A poisoned lock only means some other thread panicked while holding it; the
/// data behind it is a plain snapshot of portal geometry, so there is nothing
/// to salvage or invalidate. Taking the inner value keeps a panic in one Python
/// thread from turning every later call into an error.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

enum Cmd {
    Enable(oneshot::Sender<Result<(), String>>),
    Disable(oneshot::Sender<Result<(), String>>),
    /// Re-issue SetPointerBarriers on the *existing* session, so the set of
    /// armed barriers can change without a CreateSession round trip
    /// (recreating the session is what hangs the GNOME portal).
    SetBarriers {
        spec: BarrierSpec,
        reply: oneshot::Sender<Result<Vec<(u32, String)>, String>>,
    },
    Release {
        cursor_position: Option<(f64, f64)>,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Close,
}

/// How the caller wants barriers described.
///
/// `Edges` covers whole zone edges by name. `Segments` names explicit lines in
/// absolute desktop coordinates, which is the only way to express an edge a
/// client abuts only *part* of - an armed barrier holds the pointer, so
/// covering the unbound remainder of an edge would stop the cursor short of
/// the real screen border where there is nothing to cross to.
enum BarrierSpec {
    Edges(Option<Vec<String>>),
    Segments(Vec<(String, i32, i32, i32, i32)>),
}

#[derive(Debug)]
struct SetupResult {
    zones: Vec<(u32, u32, i32, i32)>,
    eis_raw_fd: i32,
    barrier_map: Vec<(u32, String)>,
}

/// Portal geometry as of the last time the portal told us about it.
///
/// Not a set-once snapshot: the compositor invalidates the `zone_set` whenever
/// the zones change (monitor hotplug, resolution change), and every later
/// SetPointerBarriers carrying the stale id is refused. The portal task
/// refreshes this on `ZonesChanged`, so it lives behind a mutex that Python
/// readers share.
#[derive(Default)]
struct SessionState {
    zones: Vec<(u32, u32, i32, i32)>,
    barrier_map: Vec<(u32, String)>,
    zone_set: u32,
}

/// Shared activation data between the tokio task and Python readers.
/// Laid out so that `activation_id` (the sequencing field) and
/// `barrier_id` share the same cache line.
#[repr(C)]
struct SharedActivation {
    activation_id: AtomicU32,
    barrier_id: AtomicU32,
    cursor_pos_x: AtomicU64,
    cursor_pos_y: AtomicU64,
    /// Bumped every time the portal reported new zones and the barriers were
    /// re-armed. A caller that cached `zones`/`barrier_map` watches this to
    /// know the cached copy is stale.
    zones_generation: AtomicU32,
}

impl SharedActivation {
    fn new() -> Self {
        Self {
            activation_id: AtomicU32::new(0),
            barrier_id: AtomicU32::new(0),
            cursor_pos_x: AtomicU64::new(0),
            cursor_pos_y: AtomicU64::new(0),
            zones_generation: AtomicU32::new(0),
        }
    }

    fn reset(&self) {
        self.activation_id.store(0, Ordering::Relaxed);
        self.barrier_id.store(0, Ordering::Relaxed);
        self.zones_generation.store(0, Ordering::Relaxed);
    }
}

/// Build edge barriers for every zone, filtering by `active_edges` if given.
/// End-coordinates use `size - 1` (inclusive) per the InputCapture spec.
fn build_barriers(
    zones: &[(u32, u32, i32, i32)],
    active_edges: Option<&[String]>,
) -> (Vec<Barrier>, Vec<(u32, String)>) {
    let max_count = zones.len() * 4;
    let mut barriers = Vec::with_capacity(max_count);
    let mut barrier_map = Vec::with_capacity(max_count);
    let mut bid: u32 = 1;
    for &(w, h, x_off, y_off) in zones {
        let w = w as i32;
        let h = h as i32;
        let edges = [
            ("top", (x_off, y_off, x_off + w - 1, y_off)),
            ("bottom", (x_off, y_off + h, x_off + w - 1, y_off + h)),
            ("left", (x_off, y_off, x_off, y_off + h - 1)),
            ("right", (x_off + w, y_off, x_off + w, y_off + h - 1)),
        ];
        for (edge_name, pos) in &edges {
            let include = match active_edges {
                Some(ae) => ae.iter().any(|e| e.as_str() == *edge_name),
                None => true,
            };
            if include {
                if let Some(barrier_id) = NonZeroU32::new(bid) {
                    barriers.push(Barrier::new(barrier_id, *pos));
                    barrier_map.push((bid, edge_name.to_string()));
                }
                bid += 1;
            }
        }
    }
    (barriers, barrier_map)
}

/// Reject segments the portal cannot express, naming the offender.
///
/// The spec only accepts axis-aligned barriers, and one diagonal makes the
/// compositor refuse the whole SetPointerBarriers call. Skipping it silently
/// was worse: labels may legitimately repeat (two clients against the same
/// edge), so the returned barrier map does not say which input went missing
/// and the caller ends up with an unguarded span it believes is armed. The
/// index is what pins the offender down.
fn validate_segments(segments: &[(String, i32, i32, i32, i32)]) -> Result<(), String> {
    for (i, (label, x1, y1, x2, y2)) in segments.iter().enumerate() {
        if x1 != x2 && y1 != y2 {
            return Err(format!(
                "segment {i} ({label:?}) is not axis-aligned: \
                 ({x1}, {y1}) -> ({x2}, {y2}); barriers must be horizontal or vertical"
            ));
        }
    }
    Ok(())
}

/// Build barriers from explicit line segments in absolute desktop coordinates.
///
/// Each entry is `(label, x1, y1, x2, y2)`; the label is what comes back in the
/// barrier map, so the caller can tell which edge an activation belongs to.
/// Entries are taken verbatim — `validate_segments` has already rejected the
/// ones the portal would refuse.
fn build_segment_barriers(
    segments: &[(String, i32, i32, i32, i32)],
) -> (Vec<Barrier>, Vec<(u32, String)>) {
    let mut barriers = Vec::with_capacity(segments.len());
    let mut barrier_map = Vec::with_capacity(segments.len());

    for (i, (label, x1, y1, x2, y2)) in segments.iter().enumerate() {
        let bid = (i as u32) + 1;
        if let Some(barrier_id) = NonZeroU32::new(bid) {
            barriers.push(Barrier::new(barrier_id, (*x1, *y1, *x2, *y2)));
            barrier_map.push((bid, label.clone()));
        }
    }

    (barriers, barrier_map)
}

/// Run the portal session, guaranteeing Python hears about a setup failure.
///
/// Every step of the setup phase exits via `?`, which used to drop `setup_tx`
/// without sending — so the caller could only ever report the generic
/// "portal setup channel closed" while the real reason (a denied permission
/// dialog, a missing portal) went to stderr and was typically discarded. The
/// sender is threaded through as an `Option` that `portal_session` takes when it
/// succeeds, so exactly one of the two paths answers: `Ok` from inside, `Err`
/// from here.
/// Close an fd that was already flattened to a bare int for the trip to Python
/// but never made it there.
///
/// # Safety-relevant invariant
/// Only call this with an fd that was produced by `into_raw_fd` on this side
/// and was never handed out: taking ownership back is only sound while nothing
/// else can close it.
fn close_orphan_fd(raw_fd: i32) {
    // SAFETY: per the invariant above, this is the sole remaining owner.
    drop(unsafe { OwnedFd::from_raw_fd(raw_fd) });
}

async fn run_portal(
    setup_tx: oneshot::Sender<Result<SetupResult, String>>,
    cmd_rx: mpsc::Receiver<Cmd>,
    shared: &SharedActivation,
    state: &Mutex<SessionState>,
    active_edges: Option<Vec<String>>,
) -> Result<(), String> {
    let mut setup_tx = Some(setup_tx);
    let result = portal_session(&mut setup_tx, cmd_rx, shared, state, active_edges).await;
    if let Err(e) = &result {
        // Still Some ⇒ we failed before handing the session over.
        if let Some(tx) = setup_tx.take() {
            tx.send(Err(e.clone())).ok();
        }
    }
    result
}

/// Arm `spec` against the zones currently recorded in `state`, and record the
/// resulting map there.
///
/// Barriers the compositor rejected are dropped from the map, so what Python
/// holds only ever names barriers that exist.
async fn arm_barriers(
    ic: &InputCapture<'_>,
    session: &Session<'_, InputCapture<'_>>,
    spec: &BarrierSpec,
    state: &Mutex<SessionState>,
) -> Result<Vec<(u32, String)>, String> {
    let (zone_geometry, zone_set) = {
        let st = lock(state);
        (st.zones.clone(), st.zone_set)
    };

    let (barriers, map) = match spec {
        BarrierSpec::Edges(edges) => build_barriers(&zone_geometry, edges.as_deref()),
        BarrierSpec::Segments(segments) => build_segment_barriers(segments),
    };

    let resp = ic
        .set_pointer_barriers(session, &barriers, zone_set)
        .await
        .map_err(|e| format!("set_pointer_barriers request: {e}"))?
        .response()
        .map_err(|e| format!("set_pointer_barriers response: {e}"))?;

    let failed = resp.failed_barriers();
    if !failed.is_empty() {
        eprintln!("pyinputcapture: failed barrier ids: {failed:?}");
    }

    let map: Vec<(u32, String)> = map
        .into_iter()
        .filter(|(bid, _)| !failed.iter().any(|f| f.get() == *bid))
        .collect();

    lock(state).barrier_map = map.clone();
    Ok(map)
}

/// Re-read the zones and re-arm, after the compositor invalidated the zone_set.
async fn refresh_zones(
    ic: &InputCapture<'_>,
    session: &Session<'_, InputCapture<'_>>,
    spec: &BarrierSpec,
    state: &Mutex<SessionState>,
) -> Result<(), String> {
    let zones_resp = ic
        .zones(session)
        .await
        .map_err(|e| format!("zones request: {e}"))?
        .response()
        .map_err(|e| format!("zones response: {e}"))?;

    {
        let mut st = lock(state);
        st.zones = zones_resp
            .regions()
            .iter()
            .map(|r| (r.width(), r.height(), r.x_offset(), r.y_offset()))
            .collect();
        st.zone_set = zones_resp.zone_set();
    }

    arm_barriers(ic, session, spec, state).await.map(|_| ())
}

async fn portal_session(
    setup_tx: &mut Option<oneshot::Sender<Result<SetupResult, String>>>,
    cmd_rx: mpsc::Receiver<Cmd>,
    shared: &SharedActivation,
    state: &Mutex<SessionState>,
    active_edges: Option<Vec<String>>,
) -> Result<(), String> {
    // Create portal proxy
    let ic = InputCapture::new()
        .await
        .map_err(|e| format!("InputCapture::new: {e}"))?;

    // Create session. On GNOME this is what raises the permission dialog, so it
    // stays pending for as long as the user takes to answer it - which is why the
    // Python caller must not be holding the GIL while it runs.
    let (session, _caps) = ic
        .create_session(
            None::<&ashpd::WindowIdentifier>,
            Capabilities::Keyboard | Capabilities::Pointer | Capabilities::Touchscreen,
        )
        .await
        .map_err(|e| format!("create_session: {e}"))?;

    let result = live_session(&ic, &session, setup_tx, cmd_rx, shared, state, active_edges).await;

    // Hand the session back however we got here. Dropping the proxy alone
    // leaves it alive on the compositor side, and one of the ways here is a
    // `setup()` that gave up on its timeout while the permission dialog was
    // still on screen - the session it left behind would never be closed.
    session.close().await.ok();

    result
}

#[allow(clippy::too_many_arguments)]
async fn live_session(
    ic: &InputCapture<'_>,
    session: &Session<'_, InputCapture<'_>>,
    setup_tx: &mut Option<oneshot::Sender<Result<SetupResult, String>>>,
    mut cmd_rx: mpsc::Receiver<Cmd>,
    shared: &SharedActivation,
    state: &Mutex<SessionState>,
    active_edges: Option<Vec<String>>,
) -> Result<(), String> {
    // Get zones
    let zones_resp = ic
        .zones(session)
        .await
        .map_err(|e| format!("zones request: {e}"))?
        .response()
        .map_err(|e| format!("zones response: {e}"))?;

    {
        let mut st = lock(state);
        st.zones = zones_resp
            .regions()
            .iter()
            .map(|r| (r.width(), r.height(), r.x_offset(), r.y_offset()))
            .collect();
        st.zone_set = zones_resp.zone_set();
    }

    // The spec stays around: ZonesChanged and Cmd::SetBarriers both re-arm, and
    // a refresh must reproduce what the caller last asked for, not the default.
    let mut spec = BarrierSpec::Edges(active_edges);

    let barrier_map = arm_barriers(ic, session, &spec, state).await?;

    // Connect to EIS
    let eis_fd: OwnedFd = ic
        .connect_to_eis(session)
        .await
        .map_err(|e| format!("connect_to_eis: {e}"))?;

    // Send setup results back to Python
    let zones = lock(state).zones.clone();
    let tx = setup_tx
        .take()
        .ok_or_else(|| "setup already reported".to_string())?;

    if let Err(unsent) = tx.send(Ok(SetupResult {
        zones,
        eis_raw_fd: eis_fd.into_raw_fd(),
        barrier_map,
    })) {
        // Nobody is listening: `setup()` timed out and returned. The fd left
        // ownership as a bare int the moment it went into the message, so this
        // is the last place that can close it - otherwise every timed-out
        // setup leaks one.
        if let Ok(result) = unsent {
            close_orphan_fd(result.eis_raw_fd);
        }
        return Err("setup result channel closed".to_string());
    }

    // Subscribe to Activated signal
    let mut activated_stream = ic
        .receive_activated()
        .await
        .map_err(|e| format!("receive_activated: {e}"))?;

    // ZonesChanged invalidates the zone_set we hold: from then on every
    // SetPointerBarriers with it is refused and the remembered geometry
    // describes a screen that no longer exists.
    let mut zones_changed_stream = ic
        .receive_zones_changed()
        .await
        .map_err(|e| format!("receive_zones_changed: {e}"))?;

    // Event + command loop
    loop {
        tokio::select! {
            Some(activated) = activated_stream.next() => {
                // Write barrier_id and cursor position FIRST (Relaxed).
                if let Some(ab) = activated.barrier_id() {
                    if let ActivatedBarrier::Barrier(bid) = ab {
                        shared.barrier_id.store(bid.get(), Ordering::Relaxed);
                    }
                }
                if let Some((cx, cy)) = activated.cursor_position() {
                    shared.cursor_pos_x.store((cx as f64).to_bits(), Ordering::Relaxed);
                    shared.cursor_pos_y.store((cy as f64).to_bits(), Ordering::Relaxed);
                }
                // Write activation_id LAST with Release ordering.
                // Python reads it with Acquire, so it is guaranteed to
                // see the barrier_id and cursor_position written above.
                if let Some(aid) = activated.activation_id() {
                    shared.activation_id.store(aid, Ordering::Release);
                }
            }
            Some(_) = zones_changed_stream.next() => {
                match refresh_zones(ic, session, &spec, state).await {
                    // Only now is the new geometry consistent with the armed
                    // barriers, so this is the point a reader can trust.
                    Ok(()) => {
                        shared.zones_generation.fetch_add(1, Ordering::Release);
                    }
                    Err(e) => {
                        eprintln!("pyinputcapture: zones changed but re-arming failed: {e}");
                    }
                }
            }
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(Cmd::Enable(reply)) => {
                        let r = ic.enable(session).await.map_err(|e| e.to_string());
                        reply.send(r).ok();
                    }
                    Some(Cmd::Disable(reply)) => {
                        let r = ic.disable(session).await.map_err(|e| e.to_string());
                        reply.send(r).ok();
                    }
                    Some(Cmd::SetBarriers { spec: new_spec, reply }) => {
                        let r = arm_barriers(ic, session, &new_spec, state).await;
                        // Remember it only if it took: a failed re-arm must not
                        // become what a later ZonesChanged reproduces.
                        if r.is_ok() {
                            spec = new_spec;
                        }
                        reply.send(r).ok();
                    }
                    Some(Cmd::Release { cursor_position, reply }) => {
                        let aid_val = shared.activation_id.load(Ordering::Acquire);
                        let aid_opt = if aid_val > 0 { Some(aid_val) } else { None };
                        let r = ic
                            .release(session, aid_opt, cursor_position)
                            .await
                            .map_err(|e| e.to_string());
                        reply.send(r).ok();
                    }
                    Some(Cmd::Close) | None => {
                        ic.disable(session).await.ok();
                        break;
                    }
                }
            }
        }
    }

    Ok(())
}

/// Send a command and wait for its reply **with the GIL released**.
///
/// Every one of these round trips can block for an unbounded time: the portal
/// task may be parked on a D-Bus reply, and `create_session` in particular waits
/// for a human to answer a permission dialog. Holding the GIL across that froze
/// the caller's entire interpreter - no event loop, no logging, no shutdown - so
/// `allow_threads` here is load-bearing, not an optimisation.
fn send_simple_cmd(
    py: Python<'_>,
    tx: &mpsc::Sender<Cmd>,
    make: impl FnOnce(oneshot::Sender<Result<(), String>>) -> Cmd + Send,
) -> PyResult<()> {
    py.allow_threads(|| {
        let (reply_tx, reply_rx) = oneshot::channel();
        tx.blocking_send(make(reply_tx))
            .map_err(|_| PyRuntimeError::new_err("portal task not running"))?;
        reply_rx
            .blocking_recv()
            .map_err(|_| PyRuntimeError::new_err("portal task dropped reply"))?
            .map_err(PyRuntimeError::new_err)
    })
}

/// Longest bounded wait we will honour. Past this a `timeout` is indistinguish-
/// able from "forever", and `Duration::from_secs_f64` starts to panic on the
/// values a caller reaches for when they mean forever.
const MAX_TIMEOUT_SECS: f64 = 60.0;

/// Turn the Python-facing `timeout` into a deadline.
///
/// `None` (and `inf`, which spells the same intent) means wait indefinitely.
/// Zero and negatives are rejected rather than silently read as "forever" -
/// `setup(timeout=0)` is a caller asking to fail fast, and answering that with
/// an unbounded wait is the worst possible reading.
fn deadline_from(timeout: Option<f64>) -> PyResult<Option<Duration>> {
    match timeout {
        None => Ok(None),
        Some(secs) if secs.is_nan() => Err(PyValueError::new_err(
            "timeout must be a number of seconds or None, not NaN",
        )),
        Some(secs) if secs <= 0.0 => Err(PyValueError::new_err(format!(
            "timeout must be greater than 0 seconds (got {secs}); \
             pass None to wait indefinitely"
        ))),
        Some(secs) if secs.is_infinite() => Ok(None),
        Some(secs) => Ok(Some(Duration::from_secs_f64(secs.min(MAX_TIMEOUT_SECS)))),
    }
}

/// Wait for the portal task's answer without pinning the interpreter.
///
/// The wait runs in short slices with the GIL released; between slices the GIL
/// comes back just long enough to run Python's signal handlers. Without that,
/// a wait with no deadline is un-interruptible: the handler for Ctrl-C only
/// runs when the thread executes bytecode, so the process has to be killed.
fn wait_for_setup(
    py: Python<'_>,
    handle: &tokio::runtime::Handle,
    mut setup_rx: oneshot::Receiver<Result<SetupResult, String>>,
    deadline: Option<Duration>,
) -> PyResult<SetupResult> {
    const SLICE: Duration = Duration::from_millis(100);
    let started = Instant::now();

    loop {
        let slice = match deadline {
            Some(limit) => {
                let left = limit.saturating_sub(started.elapsed());
                if left.is_zero() {
                    return Err(PyRuntimeError::new_err(format!(
                        "portal setup timed out after {}s (permission dialog unanswered?)",
                        limit.as_secs_f64()
                    )));
                }
                left.min(SLICE)
            }
            None => SLICE,
        };

        let outcome = py.allow_threads(|| {
            handle.block_on(async { tokio::time::timeout(slice, &mut setup_rx).await })
        });

        match outcome {
            Ok(Ok(result)) => return result.map_err(PyRuntimeError::new_err),
            Ok(Err(_)) => return Err(PyRuntimeError::new_err("portal setup channel closed")),
            // Slice expired with no answer: let Python raise a pending
            // KeyboardInterrupt, then keep waiting.
            Err(_) => py.check_signals()?,
        }
    }
}

/// Wayland InputCapture portal (ashpd).  All methods are blocking, but they
/// release the GIL while they wait (see `send_simple_cmd`), so a Python program
/// stays responsive even while a permission dialog is on screen.
///
/// Activation data is exposed through atomic getters (`activation_id`,
/// `barrier_id`, `cursor_position`).  `activation_id` is the sequence
/// number: when it changes, a new barrier was hit.
#[pyclass]
struct InputCapturePortal {
    rt: tokio::runtime::Runtime,
    /// Behind a mutex so every method can take `&self`. With `&mut self` on
    /// `close`, PyO3's runtime borrow check refused the call whenever another
    /// thread sat inside `release`/`enable` with the GIL released - i.e.
    /// exactly when a second thread needs to break the deadlock.
    cmd_tx: Mutex<Option<mpsc::Sender<Cmd>>>,
    /// The task of a `setup()` that timed out may still be finishing (a late
    /// answer to the permission dialog). Starting a second one meanwhile would
    /// leave two portal sessions alive, only one of them reachable.
    pending: Mutex<Option<tokio::task::JoinHandle<()>>>,
    shared: Arc<SharedActivation>,
    state: Arc<Mutex<SessionState>>,
}

impl InputCapturePortal {
    /// A clone of the command sender, or the "not set up" error.
    ///
    /// Cloned out rather than borrowed: the caller then blocks on the portal
    /// for an unbounded time, and holding the lock across that would block
    /// `close()` - the one call that can end the wait.
    fn sender(&self) -> PyResult<mpsc::Sender<Cmd>> {
        lock(&self.cmd_tx)
            .clone()
            .ok_or_else(|| PyRuntimeError::new_err("not set up"))
    }
}

#[pymethods]
impl InputCapturePortal {
    /// Create a new portal handle.  Call `setup` to connect.
    #[new]
    fn new() -> PyResult<Self> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .map_err(|e| PyRuntimeError::new_err(format!("tokio runtime: {e}")))?;

        Ok(Self {
            rt,
            cmd_tx: Mutex::new(None),
            pending: Mutex::new(None),
            shared: Arc::new(SharedActivation::new()),
            state: Arc::new(Mutex::new(SessionState::default())),
        })
    }

    /// Create session, set barriers, connect to EIS.
    /// Returns `(zones, eis_fd, barrier_map)`.
    ///
    /// Blocks until the portal answers — which on GNOME means until the user
    /// answers the permission dialog — but **releases the GIL while it waits**, so
    /// the calling program keeps running and Ctrl-C still lands. `timeout`
    /// (seconds, default 120) bounds that wait so an ignored dialog fails cleanly
    /// instead of pinning the thread forever; `None` waits indefinitely. Zero or
    /// negative values are an error, not a synonym for "forever".
    #[pyo3(signature = (edges=None, timeout=120.0))]
    fn setup(
        &self,
        py: Python<'_>,
        edges: Option<Vec<String>>,
        timeout: Option<f64>,
    ) -> PyResult<(Vec<(u32, u32, i32, i32)>, i32, Vec<(u32, String)>)> {
        let deadline = deadline_from(timeout)?;

        if lock(&self.cmd_tx).is_some() {
            return Err(PyRuntimeError::new_err("already set up"));
        }
        {
            // A task from an earlier attempt that timed out may still be
            // creating a session; a second one now would leave the first alive
            // and unreachable.
            let mut pending = lock(&self.pending);
            match pending.as_ref() {
                Some(handle) if !handle.is_finished() => {
                    return Err(PyRuntimeError::new_err(
                        "a previous setup is still winding down (the portal has not \
                         answered it yet); retry once it has",
                    ));
                }
                _ => *pending = None,
            }
        }

        let (setup_tx, setup_rx) = oneshot::channel();
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        let shared = self.shared.clone();
        let state = self.state.clone();

        // Reset atomics for the new session
        self.shared.reset();
        *lock(&self.state) = SessionState::default();

        let task = self.rt.spawn(async move {
            if let Err(e) = run_portal(setup_tx, cmd_rx, &shared, &state, edges).await {
                eprintln!("pyinputcapture: portal task error: {e}");
            }
        });

        let handle = self.rt.handle().clone();
        let result = match wait_for_setup(py, &handle, setup_rx, deadline) {
            Ok(result) => result,
            Err(e) => {
                // Dropping `cmd_tx` here closes the command channel, which the
                // task reads as a Close; `portal_session` then closes whatever
                // session it managed to create. Track the task so the next
                // `setup()` does not race it.
                drop(cmd_tx);
                *lock(&self.pending) = Some(task);
                return Err(e);
            }
        };

        *lock(&self.cmd_tx) = Some(cmd_tx);
        *lock(&self.pending) = None;

        Ok((result.zones, result.eis_raw_fd, result.barrier_map))
    }

    /// Screen zones as `[(width, height, x_offset, y_offset), ...]`.
    ///
    /// Re-read after `zones_generation` changes: the compositor can replace
    /// them mid-session.
    #[getter]
    fn zones(&self) -> Vec<(u32, u32, i32, i32)> {
        lock(&self.state).zones.clone()
    }

    /// The barriers currently armed, as `[(barrier_id, label), ...]`.
    ///
    /// Same list `setup`/`set_barriers` returned, kept up to date when the
    /// portal task re-arms after a zone change.
    #[getter]
    fn barrier_map(&self) -> Vec<(u32, String)> {
        lock(&self.state).barrier_map.clone()
    }

    /// Counter bumped every time the zones changed and the barriers were
    /// re-armed. Watch it to know a cached `zones`/`barrier_map` is stale.
    #[getter]
    fn zones_generation(&self) -> u32 {
        self.shared.zones_generation.load(Ordering::Acquire)
    }

    /// Latest activation ID received from the compositor.
    /// Read with `Acquire` ordering -- if the value changed, the
    /// corresponding `barrier_id` and `cursor_position` are visible.
    #[getter]
    fn activation_id(&self) -> u32 {
        self.shared.activation_id.load(Ordering::Acquire)
    }

    /// Barrier ID from the last Activated signal.
    #[getter]
    fn barrier_id(&self) -> u32 {
        self.shared.barrier_id.load(Ordering::Relaxed)
    }

    /// Cursor position `(x, y)` from the last Activated signal.
    #[getter]
    fn cursor_position(&self) -> (f64, f64) {
        let x = f64::from_bits(self.shared.cursor_pos_x.load(Ordering::Relaxed));
        let y = f64::from_bits(self.shared.cursor_pos_y.load(Ordering::Relaxed));
        (x, y)
    }

    /// Re-enable capture (barriers become active again).
    fn enable(&self, py: Python<'_>) -> PyResult<()> {
        send_simple_cmd(py, &self.sender()?, Cmd::Enable)
    }

    /// Replace the armed pointer barriers on the existing session.
    ///
    /// Two forms, and they are mutually exclusive:
    ///
    /// * `edges` -- the same whole-edge filter `setup` takes (`None` = all four
    ///   edges of every zone).
    /// * `segments` -- explicit `(label, x1, y1, x2, y2)` lines in absolute
    ///   desktop coordinates, for when a client abuts only *part* of an edge.
    ///   Must be axis-aligned - a diagonal is rejected by index rather than
    ///   dropped, since labels may repeat and the returned map would not say
    ///   which span was left unguarded.
    ///
    /// Returns the new `barrier_map`, with any barrier the compositor rejected
    /// already removed.
    ///
    /// This exists so the caller can arm barriers *only* where they lead
    /// somewhere: an armed barrier stops the pointer, so arming a whole edge
    /// means the cursor can never reach the real border along the parts with
    /// nothing behind them.  It reuses the current session deliberately --
    /// re-running `setup` is what hangs the GNOME portal.
    #[pyo3(signature = (edges=None, *, segments=None))]
    fn set_barriers(
        &self,
        py: Python<'_>,
        edges: Option<Vec<String>>,
        segments: Option<Vec<(String, i32, i32, i32, i32)>>,
    ) -> PyResult<Vec<(u32, String)>> {
        if edges.is_some() && segments.is_some() {
            // A caller mistake in how the function was called, so TypeError -
            // the same thing Python raises for conflicting arguments.
            return Err(PyTypeError::new_err(
                "pass either edges or segments, not both",
            ));
        }
        if let Some(s) = segments.as_deref() {
            validate_segments(s).map_err(PyValueError::new_err)?;
        }

        let tx = self.sender()?;

        let spec = match segments {
            Some(s) => BarrierSpec::Segments(s),
            None => BarrierSpec::Edges(edges),
        };

        py.allow_threads(|| {
            let (reply_tx, reply_rx) = oneshot::channel();
            tx.blocking_send(Cmd::SetBarriers {
                spec,
                reply: reply_tx,
            })
            .map_err(|_| PyRuntimeError::new_err("portal task not running"))?;

            reply_rx
                .blocking_recv()
                .map_err(|_| PyRuntimeError::new_err("portal task dropped reply"))?
                .map_err(PyRuntimeError::new_err)
        })
    }

    /// Disable capture (barriers deactivated).
    fn disable(&self, py: Python<'_>) -> PyResult<()> {
        send_simple_cmd(py, &self.sender()?, Cmd::Disable)
    }

    /// Release captured input.  Optional `cursor_x`/`cursor_y` reposition
    /// the cursor on release (absolute desktop coordinates).
    #[pyo3(signature = (cursor_x=None, cursor_y=None))]
    fn release(
        &self,
        py: Python<'_>,
        cursor_x: Option<f64>,
        cursor_y: Option<f64>,
    ) -> PyResult<()> {
        let tx = self.sender()?;

        let cursor_position = match (cursor_x, cursor_y) {
            (Some(x), Some(y)) => Some((x, y)),
            _ => None,
        };

        py.allow_threads(|| {
            let (reply_tx, reply_rx) = oneshot::channel();
            tx.blocking_send(Cmd::Release {
                cursor_position,
                reply: reply_tx,
            })
            .map_err(|_| PyRuntimeError::new_err("portal task not running"))?;

            reply_rx
                .blocking_recv()
                .map_err(|_| PyRuntimeError::new_err("portal task dropped reply"))?
                .map_err(PyRuntimeError::new_err)
        })
    }

    /// Close the session and shut down the background task.
    ///
    /// Does not wait for the portal task to acknowledge: this is called from
    /// teardown paths where the task may be parked on a D-Bus reply that will
    /// never arrive, and blocking there (with or without the GIL) is how a
    /// shutdown turns into a hang. `try_send` is enough to ask it to stop.
    ///
    /// Takes `&self` on purpose: another thread may be parked inside
    /// `release()` waiting for that same portal, and this is its way out.
    fn close(&self) -> PyResult<()> {
        if let Some(tx) = lock(&self.cmd_tx).take() {
            tx.try_send(Cmd::Close).ok();
        }
        Ok(())
    }
}

impl Drop for InputCapturePortal {
    fn drop(&mut self) {
        if let Some(tx) = lock(&self.cmd_tx).take() {
            // try_send avoids panic if called from within the tokio runtime.
            tx.try_send(Cmd::Close).ok();
        }
        // Dropping a multi-threaded Runtime blocks until its workers wind down,
        // and a worker awaiting a portal reply may never finish - which, during
        // Python GC with the GIL held, freezes the whole interpreter. Hand the
        // runtime to a background thread to wind down on its own instead.
        //
        // Replace the runtime with a throwaway current-thread one (which has
        // nothing to wind down) so we can move the real one out of &mut self.
        if let Ok(placeholder) = tokio::runtime::Builder::new_current_thread().build() {
            let rt = std::mem::replace(&mut self.rt, placeholder);
            rt.shutdown_background();
        }
    }
}

#[pymodule]
fn pyinputcapture(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<InputCapturePortal>()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // build_barriers

    #[test]
    fn barriers_single_zone() {
        let zones = vec![(1920, 1080, 0, 0)];
        let (barriers, barrier_map) = build_barriers(&zones, None);

        assert_eq!(barriers.len(), 4, "4 edges per zone");
        assert_eq!(barrier_map.len(), 4);
    }

    #[test]
    fn barriers_two_zones() {
        let zones = vec![(1920, 1080, 0, 0), (2560, 1440, 1920, 0)];
        let (barriers, barrier_map) = build_barriers(&zones, None);

        assert_eq!(barriers.len(), 8, "4 edges x 2 zones");
        assert_eq!(barrier_map.len(), 8);
    }

    #[test]
    fn barriers_empty_zones() {
        let (barriers, _) = build_barriers(&[], None);
        assert!(barriers.is_empty());
    }

    #[test]
    fn barriers_selective_edges() {
        let zones = vec![(1920, 1080, 0, 0)];
        let edges = vec!["left".to_string(), "right".to_string()];
        let (barriers, barrier_map) = build_barriers(&zones, Some(&edges));

        assert_eq!(barriers.len(), 2, "only left + right");
        assert_eq!(barrier_map.len(), 2);
        assert_eq!(barrier_map[0].1, "left");
        assert_eq!(barrier_map[1].1, "right");
    }

    #[test]
    fn barriers_selective_single_edge() {
        let zones = vec![(1920, 1080, 0, 0), (2560, 1440, 1920, 0)];
        let edges = vec!["top".to_string()];
        let (barriers, barrier_map) = build_barriers(&zones, Some(&edges));

        assert_eq!(barriers.len(), 2, "1 edge x 2 zones");
        assert!(barrier_map.iter().all(|(_, name)| name == "top"));
    }

    #[test]
    fn barrier_ids_are_sequential_and_nonzero() {
        let zones = vec![(800, 600, 0, 0), (800, 600, 800, 0)];
        let (_, barrier_map) = build_barriers(&zones, None);

        for (i, (bid, _)) in barrier_map.iter().enumerate() {
            assert_eq!(*bid, (i as u32) + 1);
        }
    }

    #[test]
    fn barriers_no_edges_arms_nothing() {
        // An empty filter must not fall through to "all edges": with no client
        // anywhere the whole screen border has to stay reachable.
        let zones = vec![(1920, 1080, 0, 0)];
        let (barriers, barrier_map) = build_barriers(&zones, Some(&[]));

        assert!(barriers.is_empty());
        assert!(barrier_map.is_empty());
    }

    #[test]
    fn barriers_ids_stable_across_identical_calls() {
        // set_barriers re-issues these on a live session, so an id must keep
        // meaning the same edge - the barrier_map is how an activation is
        // resolved back to an edge.
        let zones = vec![(1920, 1080, 0, 0), (1280, 1024, 1920, 0)];
        let edges = vec!["left".to_string()];
        let (_, a) = build_barriers(&zones, Some(&edges));
        let (_, b) = build_barriers(&zones, Some(&edges));

        assert_eq!(a, b);
    }

    // build_segment_barriers

    #[test]
    fn segments_are_built_verbatim_and_labelled() {
        let segments = vec![
            ("left".to_string(), 0, 300, 0, 800),
            ("top".to_string(), 100, 0, 900, 0),
        ];
        let (barriers, barrier_map) = build_segment_barriers(&segments);

        assert_eq!(barriers.len(), 2);
        assert_eq!(barrier_map[0].1, "left");
        assert_eq!(barrier_map[1].1, "top");
        assert!(barrier_map.iter().all(|(bid, _)| *bid != 0));
    }

    #[test]
    fn segments_partial_edge_is_shorter_than_the_zone_edge() {
        // The whole point: a client abutting only part of an edge must not
        // arm a barrier along the rest of it.
        let zones = vec![(1920, 1080, 0, 0)];
        let (whole_edge, _) = build_barriers(&zones, Some(&["left".to_string()]));
        let (partial, map) =
            build_segment_barriers(&[("left".to_string(), 0, 300, 0, 800)]);

        assert_eq!(whole_edge.len(), 1);
        assert_eq!(partial.len(), 1);
        assert_eq!(map[0].1, "left");
    }

    #[test]
    fn segments_reject_non_axis_aligned_naming_the_index() {
        // A diagonal makes the portal reject the entire SetPointerBarriers
        // call. Skipping it silently left the caller believing a span was
        // armed when it was not, and with repeated labels the returned map
        // could not say which one - so the whole call is refused, by index.
        let segments = vec![
            ("left".to_string(), 0, 0, 0, 500),
            ("diagonal".to_string(), 0, 0, 500, 500),
            ("right".to_string(), 1920, 0, 1920, 500),
        ];
        let err = validate_segments(&segments).unwrap_err();

        assert!(err.contains("segment 1"), "{err}");
        assert!(err.contains("diagonal"), "{err}");
    }

    #[test]
    fn segments_accept_horizontal_and_vertical() {
        let segments = vec![
            ("left".to_string(), 0, 0, 0, 500),
            ("top".to_string(), 0, 0, 500, 0),
            ("point".to_string(), 7, 7, 7, 7),
        ];
        assert!(validate_segments(&segments).is_ok());
    }

    #[test]
    fn segments_single_point_is_accepted() {
        // Degenerate but axis-aligned (a one-pixel placement). Dropping it
        // would silently lose a real, if tiny, crossing.
        let (barriers, _) = build_segment_barriers(&[("left".to_string(), 0, 42, 0, 42)]);
        assert_eq!(barriers.len(), 1);
    }

    #[test]
    fn segments_empty_arms_nothing() {
        let (barriers, barrier_map) = build_segment_barriers(&[]);
        assert!(barriers.is_empty());
        assert!(barrier_map.is_empty());
    }

    #[test]
    fn segments_ids_are_sequential_and_nonzero() {
        let segments = vec![
            ("left".to_string(), 0, 0, 0, 100),
            ("left".to_string(), 0, 200, 0, 300),
            ("top".to_string(), 0, 0, 100, 0),
        ];
        let (_, barrier_map) = build_segment_barriers(&segments);

        for (i, (bid, _)) in barrier_map.iter().enumerate() {
            assert_eq!(*bid, (i as u32) + 1);
        }
    }

    #[test]
    fn segments_allow_several_disjoint_spans_on_one_edge() {
        // Two clients stacked against the same edge, with a gap between them:
        // the gap must stay barrier-free.
        let segments = vec![
            ("left".to_string(), 0, 0, 0, 300),
            ("left".to_string(), 0, 700, 0, 1000),
        ];
        let (barriers, barrier_map) = build_segment_barriers(&segments);

        assert_eq!(barriers.len(), 2);
        assert!(barrier_map.iter().all(|(_, name)| name == "left"));
        assert_ne!(barrier_map[0].0, barrier_map[1].0);
    }

    // setup-result plumbing
    //
    // The contract these pin: a setup failure must reach Python as the *real*
    // reason. Every step of the setup phase exits via `?`, which used to drop
    // `setup_tx` unsent, so the caller could only report "portal setup channel
    // closed" while the actual cause went to stderr.

    /// Stand-in for `portal_session`'s use of the sender: takes it on success,
    /// leaves it in place on failure so the wrapper can report the error.
    fn fake_session(
        setup_tx: &mut Option<oneshot::Sender<Result<SetupResult, String>>>,
        fail_at_step: Option<&str>,
    ) -> Result<(), String> {
        if let Some(step) = fail_at_step {
            return Err(format!("{step}: boom"));
        }
        setup_tx
            .take()
            .ok_or_else(|| "setup already reported".to_string())?
            .send(Ok(SetupResult {
                zones: vec![(1920, 1080, 0, 0)],
                eis_raw_fd: 7,
                barrier_map: vec![(1, "left".to_string())],
            }))
            .map_err(|_| "setup result channel closed".to_string())
    }

    /// Mirrors `run_portal`'s wrapper: report an error only if unreported.
    fn fake_run(
        setup_tx: oneshot::Sender<Result<SetupResult, String>>,
        fail_at_step: Option<&str>,
    ) -> Result<(), String> {
        let mut slot = Some(setup_tx);
        let result = fake_session(&mut slot, fail_at_step);
        if let Err(e) = &result {
            if let Some(tx) = slot.take() {
                tx.send(Err(e.clone())).ok();
            }
        }
        result
    }

    #[test]
    fn setup_failure_reports_the_real_reason() {
        let (tx, rx) = oneshot::channel();
        assert!(fake_run(tx, Some("create_session")).is_err());

        // Not a bare channel-closed: the cause survives the hop to Python.
        let err = rx.blocking_recv().expect("sender must not be dropped unsent");
        assert_eq!(err.unwrap_err(), "create_session: boom");
    }

    #[test]
    fn setup_success_delivers_the_result() {
        let (tx, rx) = oneshot::channel();
        assert!(fake_run(tx, None).is_ok());

        let result = rx.blocking_recv().unwrap().unwrap();
        assert_eq!(result.eis_raw_fd, 7);
        assert_eq!(result.zones, vec![(1920, 1080, 0, 0)]);
    }

    #[test]
    fn setup_is_reported_exactly_once() {
        // A failure *after* the session was handed over must not try to send a
        // second time - Python already has its Ok.
        let (tx, rx) = oneshot::channel();
        let mut slot = Some(tx);
        fake_session(&mut slot, None).unwrap();
        assert!(slot.is_none(), "the sender is consumed by the success path");

        assert!(rx.blocking_recv().unwrap().is_ok());
    }

    #[test]
    fn orphan_fd_is_closed_when_nobody_takes_the_setup_result() {
        // The timed-out-setup path: the fd left ownership as a bare int inside
        // the message, so if the receiver is gone the portal task is the only
        // one left that can close it. Closing the write end of a socket pair
        // is observable as EOF on the read end.
        use std::io::Read;
        use std::os::unix::net::UnixStream;

        let (mut reader, writer) = UnixStream::pair().unwrap();
        let raw = OwnedFd::from(writer).into_raw_fd();

        close_orphan_fd(raw);

        let mut buf = [0u8; 1];
        assert_eq!(
            reader.read(&mut buf).unwrap(),
            0,
            "peer still open: the orphaned fd was leaked"
        );
    }

    // timeout handling

    #[test]
    fn timeout_none_and_infinity_both_mean_forever() {
        assert_eq!(deadline_from(None).unwrap(), None);
        assert_eq!(deadline_from(Some(f64::INFINITY)).unwrap(), None);
    }

    #[test]
    fn timeout_infinity_does_not_panic() {
        // Duration::from_secs_f64(inf) panics; inf is a reasonable way for a
        // caller to spell "wait forever", so it must not reach it.
        assert!(deadline_from(Some(f64::INFINITY)).is_ok());
        assert!(deadline_from(Some(f64::MAX)).is_ok());
    }

    #[test]
    fn timeout_zero_and_negative_are_rejected() {
        // Not "forever": a zero timeout is someone asking to fail fast, and
        // reading it as an unbounded wait is the exact opposite.
        assert!(deadline_from(Some(0.0)).is_err());
        assert!(deadline_from(Some(-1.0)).is_err());
        assert!(deadline_from(Some(f64::NAN)).is_err());
    }

    #[test]
    fn timeout_positive_survives_as_a_duration() {
        assert_eq!(
            deadline_from(Some(0.25)).unwrap(),
            Some(Duration::from_millis(250))
        );
    }

    #[test]
    fn setup_timeout_is_an_error_not_a_hang() {
        // Nothing ever answers: the bounded wait must give up. This is the
        // unanswered-permission-dialog case.
        let (_tx, rx) = oneshot::channel::<Result<SetupResult, String>>();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();

        let timed_out = rt.block_on(async {
            tokio::time::timeout(Duration::from_millis(50), rx)
                .await
                .is_err()
        });
        assert!(timed_out);
    }

    // shared activation atomics

    #[test]
    fn shared_activation_new_is_zero() {
        let s = SharedActivation::new();
        assert_eq!(s.activation_id.load(Ordering::Acquire), 0);
        assert_eq!(s.barrier_id.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn shared_activation_reset() {
        let s = SharedActivation::new();
        s.activation_id.store(42, Ordering::Release);
        s.barrier_id.store(7, Ordering::Relaxed);
        s.reset();
        assert_eq!(s.activation_id.load(Ordering::Acquire), 0);
        assert_eq!(s.barrier_id.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn shared_activation_store_load_ordering() {
        let s = SharedActivation::new();

        // Simulate the write path: barrier_id + cursor first, activation_id last
        s.barrier_id.store(3, Ordering::Relaxed);
        s.cursor_pos_x.store(100.0_f64.to_bits(), Ordering::Relaxed);
        s.cursor_pos_y.store(200.0_f64.to_bits(), Ordering::Relaxed);
        s.activation_id.store(1, Ordering::Release);

        // Simulate the read path: activation_id first (Acquire), then the rest
        let aid = s.activation_id.load(Ordering::Acquire);
        assert_eq!(aid, 1);
        assert_eq!(s.barrier_id.load(Ordering::Relaxed), 3);
        assert_eq!(f64::from_bits(s.cursor_pos_x.load(Ordering::Relaxed)), 100.0);
        assert_eq!(f64::from_bits(s.cursor_pos_y.load(Ordering::Relaxed)), 200.0);
    }

    #[test]
    fn activation_id_zero_means_none() {
        let s = SharedActivation::new();
        let val = s.activation_id.load(Ordering::Acquire);
        let opt = if val > 0 { Some(val) } else { None };
        assert_eq!(opt, None);
    }

    // channel command flow

    #[tokio::test]
    async fn cmd_channel_enable_disable() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<Cmd>(4);

        let (reply_tx, reply_rx) = oneshot::channel();
        cmd_tx.send(Cmd::Enable(reply_tx)).await.unwrap();

        if let Some(Cmd::Enable(reply)) = cmd_rx.recv().await {
            reply.send(Ok(())).unwrap();
        } else {
            panic!("expected Enable");
        }

        assert!(reply_rx.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn cmd_channel_release_with_position() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<Cmd>(4);

        let (reply_tx, reply_rx) = oneshot::channel();
        cmd_tx
            .send(Cmd::Release {
                cursor_position: Some((960.0, 540.0)),
                reply: reply_tx,
            })
            .await
            .unwrap();

        if let Some(Cmd::Release {
            cursor_position,
            reply,
        }) = cmd_rx.recv().await
        {
            assert_eq!(cursor_position, Some((960.0, 540.0)));
            reply.send(Ok(())).unwrap();
        } else {
            panic!("expected Release");
        }

        assert!(reply_rx.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn cmd_channel_release_without_position() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<Cmd>(4);

        let (reply_tx, reply_rx) = oneshot::channel();
        cmd_tx
            .send(Cmd::Release {
                cursor_position: None,
                reply: reply_tx,
            })
            .await
            .unwrap();

        if let Some(Cmd::Release {
            cursor_position,
            reply,
        }) = cmd_rx.recv().await
        {
            assert!(cursor_position.is_none());
            reply.send(Ok(())).unwrap();
        } else {
            panic!("expected Release");
        }

        assert!(reply_rx.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn cmd_channel_close() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<Cmd>(4);

        cmd_tx.send(Cmd::Close).await.unwrap();

        match cmd_rx.recv().await {
            Some(Cmd::Close) => {}
            _ => panic!("expected Close"),
        }
    }

    #[tokio::test]
    async fn cmd_reply_error_propagates() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<Cmd>(4);

        let (reply_tx, reply_rx) = oneshot::channel();
        cmd_tx.send(Cmd::Disable(reply_tx)).await.unwrap();

        if let Some(Cmd::Disable(reply)) = cmd_rx.recv().await {
            reply
                .send(Err("simulated portal error".to_string()))
                .unwrap();
        }

        let result = reply_rx.await.unwrap();
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "simulated portal error");
    }

    // tokio runtime creation

    #[test]
    fn tokio_runtime_creates_successfully() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build();
        assert!(rt.is_ok());
    }
}
