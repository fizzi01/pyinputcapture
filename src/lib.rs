//! PyO3 wrapper around ashpd's XDG InputCapture portal.
//!
//! Exposes `InputCapturePortal` to Python: a blocking API backed by a
//! tokio runtime.  Python communicates with the background task through
//! channels.

use std::collections::VecDeque;
use std::num::NonZeroU32;
use std::os::fd::IntoRawFd;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use ashpd::desktop::input_capture::{ActivatedBarrier, Barrier, Capabilities, InputCapture};
use futures_util::StreamExt;
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use tokio::sync::{mpsc, oneshot};

enum Cmd {
    Enable(oneshot::Sender<Result<(), String>>),
    Disable(oneshot::Sender<Result<(), String>>),
    Release {
        cursor_position: Option<(f64, f64)>,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Close,
}

struct SetupResult {
    zones: Vec<(u32, u32, i32, i32)>,
    eis_raw_fd: i32,
    barrier_map: Vec<(u32, String)>,
}

/// Build edge barriers for every zone, filtering by `active_edges` if given.
/// End-coordinates use `size - 1` (inclusive) per the InputCapture spec.
fn build_barriers(
    zones: &[(u32, u32, i32, i32)],
    active_edges: Option<&[String]>,
) -> (Vec<Barrier>, Vec<(u32, String)>) {
    let mut barriers = Vec::new();
    let mut barrier_map = Vec::new();
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

/// Task entry point; catches and logs errors.
async fn portal_task(
    setup_tx: oneshot::Sender<Result<SetupResult, String>>,
    cmd_rx: mpsc::Receiver<Cmd>,
    activation_id: Arc<AtomicU32>,
    barrier_id: Arc<AtomicU32>,
    cursor_pos_x: Arc<AtomicU64>,
    cursor_pos_y: Arc<AtomicU64>,
    activated_queue: Arc<Mutex<VecDeque<(u32, f64, f64)>>>,
    active_edges: Option<Vec<String>>,
) {
    if let Err(e) = run_portal(
        setup_tx,
        cmd_rx,
        &activation_id,
        &barrier_id,
        &cursor_pos_x,
        &cursor_pos_y,
        &activated_queue,
        active_edges,
    )
    .await
    {
        eprintln!("pyinputcapture: portal task error: {e}");
    }
}

async fn run_portal(
    setup_tx: oneshot::Sender<Result<SetupResult, String>>,
    mut cmd_rx: mpsc::Receiver<Cmd>,
    activation_id: &Arc<AtomicU32>,
    barrier_id: &Arc<AtomicU32>,
    cursor_pos_x: &Arc<AtomicU64>,
    cursor_pos_y: &Arc<AtomicU64>,
    activated_queue: &Arc<Mutex<VecDeque<(u32, f64, f64)>>>,
    active_edges: Option<Vec<String>>,
) -> Result<(), String> {
    // Create portal proxy
    let ic = InputCapture::new()
        .await
        .map_err(|e| format!("InputCapture::new: {e}"))?;

    // Create session
    let (session, _caps) = ic
        .create_session(
            None::<&ashpd::WindowIdentifier>,
            Capabilities::Keyboard | Capabilities::Pointer | Capabilities::Touchscreen,
        )
        .await
        .map_err(|e| format!("create_session: {e}"))?;

    // Get zones
    let zones_resp = ic
        .zones(&session)
        .await
        .map_err(|e| format!("zones request: {e}"))?
        .response()
        .map_err(|e| format!("zones response: {e}"))?;

    let regions = zones_resp.regions();
    let zone_set = zones_resp.zone_set();

    let zones: Vec<(u32, u32, i32, i32)> = regions
        .iter()
        .map(|r| (r.width(), r.height(), r.x_offset(), r.y_offset()))
        .collect();

    // Build edge barriers
    let (barriers, barrier_map) = build_barriers(&zones, active_edges.as_deref());

    let barrier_resp = ic
        .set_pointer_barriers(&session, &barriers, zone_set)
        .await
        .map_err(|e| format!("set_pointer_barriers request: {e}"))?
        .response()
        .map_err(|e| format!("set_pointer_barriers response: {e}"))?;

    let failed = barrier_resp.failed_barriers();
    if !failed.is_empty() {
        eprintln!("pyinputcapture: failed barrier ids: {failed:?}");
    }

    // Connect to EIS
    let eis_fd = ic
        .connect_to_eis(&session)
        .await
        .map_err(|e| format!("connect_to_eis: {e}"))?;
    let eis_raw_fd = eis_fd.into_raw_fd();

    // enable() must be called by Python after creating the EIS Receiver,
    // otherwise the compositor handshake has no consumer.

    // Send setup results back to Python
    setup_tx
        .send(Ok(SetupResult {
            zones,
            eis_raw_fd,
            barrier_map,
        }))
        .map_err(|_| "setup result channel closed".to_string())?;

    // Subscribe to Activated signal
    let mut activated_stream = ic
        .receive_activated()
        .await
        .map_err(|e| format!("receive_activated: {e}"))?;

    // Event + command loop
    loop {
        tokio::select! {
            Some(activated) = activated_stream.next() => {
                let mut bid_val = 0u32;
                if let Some(aid) = activated.activation_id() {
                    activation_id.store(aid, Ordering::Relaxed);
                }
                if let Some(ab) = activated.barrier_id() {
                    if let ActivatedBarrier::Barrier(bid) = ab {
                        bid_val = bid.get();
                        barrier_id.store(bid_val, Ordering::Relaxed);
                    }
                }
                let (cx, cy) = if let Some((cx, cy)) = activated.cursor_position() {
                    let cxf = cx as f64;
                    let cyf = cy as f64;
                    cursor_pos_x.store(cxf.to_bits(), Ordering::Relaxed);
                    cursor_pos_y.store(cyf.to_bits(), Ordering::Relaxed);
                    (cxf, cyf)
                } else {
                    (0.0, 0.0)
                };
                // Push to queue so Python can poll the exact activation data
                if bid_val > 0 {
                    if let Ok(mut q) = activated_queue.lock() {
                        q.push_back((bid_val, cx, cy));
                    }
                }
            }
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(Cmd::Enable(reply)) => {
                        let r = ic.enable(&session).await.map_err(|e| e.to_string());
                        reply.send(r).ok();
                    }
                    Some(Cmd::Disable(reply)) => {
                        let r = ic.disable(&session).await.map_err(|e| e.to_string());
                        reply.send(r).ok();
                    }
                    Some(Cmd::Release { cursor_position, reply }) => {
                        let aid_val = activation_id.load(Ordering::Relaxed);
                        let aid_opt = if aid_val > 0 { Some(aid_val) } else { None };
                        let r = ic
                            .release(&session, aid_opt, cursor_position)
                            .await
                            .map_err(|e| e.to_string());
                        reply.send(r).ok();
                    }
                    Some(Cmd::Close) | None => {
                        ic.disable(&session).await.ok();
                        break;
                    }
                }
            }
        }
    }

    Ok(())
}

fn send_simple_cmd(
    tx: &mpsc::Sender<Cmd>,
    make: impl FnOnce(oneshot::Sender<Result<(), String>>) -> Cmd,
) -> PyResult<()> {
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.blocking_send(make(reply_tx))
        .map_err(|_| PyRuntimeError::new_err("portal task not running"))?;
    reply_rx
        .blocking_recv()
        .map_err(|_| PyRuntimeError::new_err("portal task dropped reply"))?
        .map_err(PyRuntimeError::new_err)
}

/// Wayland InputCapture portal (ashpd).  All methods are blocking.
#[pyclass]
struct InputCapturePortal {
    rt: tokio::runtime::Runtime,
    cmd_tx: Option<mpsc::Sender<Cmd>>,
    activation_id: Arc<AtomicU32>,
    barrier_id: Arc<AtomicU32>,
    cursor_pos_x: Arc<AtomicU64>,
    cursor_pos_y: Arc<AtomicU64>,
    activated_queue: Arc<Mutex<VecDeque<(u32, f64, f64)>>>,
    zones: Vec<(u32, u32, i32, i32)>,
}

#[pymethods]
impl InputCapturePortal {
    /// Create a new portal handle.  Call `setup` to connect.
    #[new]
    fn new() -> PyResult<Self> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| PyRuntimeError::new_err(format!("tokio runtime: {e}")))?;

        Ok(Self {
            rt,
            cmd_tx: None,
            activation_id: Arc::new(AtomicU32::new(0)),
            barrier_id: Arc::new(AtomicU32::new(0)),
            cursor_pos_x: Arc::new(AtomicU64::new(0)),
            cursor_pos_y: Arc::new(AtomicU64::new(0)),
            activated_queue: Arc::new(Mutex::new(VecDeque::new())),
            zones: Vec::new(),
        })
    }

    /// Create session, set barriers, connect to EIS.
    /// Returns `(zones, eis_fd, barrier_map)`.
    #[pyo3(signature = (edges=None))]
    fn setup(
        &mut self,
        edges: Option<Vec<String>>,
    ) -> PyResult<(Vec<(u32, u32, i32, i32)>, i32, Vec<(u32, String)>)> {
        if self.cmd_tx.is_some() {
            return Err(PyRuntimeError::new_err("already set up"));
        }

        let (setup_tx, setup_rx) = oneshot::channel();
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        let aid = self.activation_id.clone();
        let bid = self.barrier_id.clone();
        let cpx = self.cursor_pos_x.clone();
        let cpy = self.cursor_pos_y.clone();
        let aq = self.activated_queue.clone();

        // Clear any stale activations from a previous session
        if let Ok(mut q) = self.activated_queue.lock() {
            q.clear();
        }

        self.rt
            .spawn(portal_task(setup_tx, cmd_rx, aid, bid, cpx, cpy, aq, edges));

        let result = setup_rx
            .blocking_recv()
            .map_err(|_| PyRuntimeError::new_err("portal setup channel closed"))?
            .map_err(PyRuntimeError::new_err)?;

        self.cmd_tx = Some(cmd_tx);
        self.zones = result.zones.clone();

        Ok((result.zones, result.eis_raw_fd, result.barrier_map))
    }

    /// Screen zones as `[(width, height, x_offset, y_offset), ...]`.
    #[getter]
    fn zones(&self) -> Vec<(u32, u32, i32, i32)> {
        self.zones.clone()
    }

    /// Latest activation ID received from the compositor.
    #[getter]
    fn activation_id(&self) -> u32 {
        self.activation_id.load(Ordering::Relaxed)
    }

    /// Barrier ID from the last Activated signal.
    #[getter]
    fn barrier_id(&self) -> u32 {
        self.barrier_id.load(Ordering::Relaxed)
    }

    /// Cursor position `(x, y)` from the last Activated signal.
    #[getter]
    fn cursor_position(&self) -> (f64, f64) {
        let x = f64::from_bits(self.cursor_pos_x.load(Ordering::Relaxed));
        let y = f64::from_bits(self.cursor_pos_y.load(Ordering::Relaxed));
        (x, y)
    }

    /// Pop the next Activated event from the queue, or `None` if empty.
    /// Preferred over the atomic getters because the D-Bus signal may
    /// arrive after EIS START_EMULATING.
    fn poll_activated(&self) -> Option<(u32, f64, f64)> {
        self.activated_queue.lock().ok()?.pop_front()
    }

    /// Re-enable capture (barriers become active again).
    fn enable(&self) -> PyResult<()> {
        let tx = self
            .cmd_tx
            .as_ref()
            .ok_or_else(|| PyRuntimeError::new_err("not set up"))?;
        send_simple_cmd(tx, Cmd::Enable)
    }

    /// Disable capture (barriers deactivated).
    fn disable(&self) -> PyResult<()> {
        let tx = self
            .cmd_tx
            .as_ref()
            .ok_or_else(|| PyRuntimeError::new_err("not set up"))?;
        send_simple_cmd(tx, Cmd::Disable)
    }

    /// Release captured input.  Optional `cursor_x`/`cursor_y` reposition
    /// the cursor on release (absolute desktop coordinates).
    #[pyo3(signature = (cursor_x=None, cursor_y=None))]
    fn release(&self, cursor_x: Option<f64>, cursor_y: Option<f64>) -> PyResult<()> {
        let tx = self
            .cmd_tx
            .as_ref()
            .ok_or_else(|| PyRuntimeError::new_err("not set up"))?;

        let cursor_position = match (cursor_x, cursor_y) {
            (Some(x), Some(y)) => Some((x, y)),
            _ => None,
        };

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
    }

    /// Close the session and shut down the background task.
    fn close(&mut self) -> PyResult<()> {
        if let Some(tx) = self.cmd_tx.take() {
            // Best-effort; task may already be gone.
            tx.blocking_send(Cmd::Close).ok();
        }
        Ok(())
    }
}

impl Drop for InputCapturePortal {
    fn drop(&mut self) {
        if let Some(tx) = self.cmd_tx.take() {
            // try_send avoids panic if called from within the tokio runtime.
            tx.try_send(Cmd::Close).ok();
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

    // channel command flow

    #[tokio::test]
    async fn cmd_channel_enable_disable() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<Cmd>(4);

        // Simulate sending Enable + Disable from "Python side"
        let (reply_tx, reply_rx) = oneshot::channel();
        cmd_tx.send(Cmd::Enable(reply_tx)).await.unwrap();

        // Simulate the portal task handling it
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
            Some(Cmd::Close) => {} // ok
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

    // activation_id atomic

    #[test]
    fn activation_id_store_load() {
        let aid = Arc::new(AtomicU32::new(0));

        assert_eq!(aid.load(Ordering::Relaxed), 0);

        aid.store(42, Ordering::Relaxed);
        assert_eq!(aid.load(Ordering::Relaxed), 42);

        // Verify the "no activation" check used in run_portal
        let val = aid.load(Ordering::Relaxed);
        let opt = if val > 0 { Some(val) } else { None };
        assert_eq!(opt, Some(42));
    }

    #[test]
    fn activation_id_zero_means_none() {
        let aid = AtomicU32::new(0);
        let val = aid.load(Ordering::Relaxed);
        let opt = if val > 0 { Some(val) } else { None };
        assert_eq!(opt, None);
    }

    // tokio runtime creation

    #[test]
    fn tokio_runtime_creates_successfully() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build();
        assert!(rt.is_ok());
    }
}
