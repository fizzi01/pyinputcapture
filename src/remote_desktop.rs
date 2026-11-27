//! RemoteDesktop portal: an input-only session whose grant survives restarts.
//!
//! The session asks for pointer devices only and hands back the EIS fd, so the
//! caller emulates input with libei. With `persist=true` the grant is kept by
//! the portal until the user revokes it, and the `restore_token` returned by
//! `setup` reopens it with no dialog next time. The token is single use:
//! always store the latest one.

use std::os::fd::IntoRawFd;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ashpd::desktop::remote_desktop::{DeviceType, RemoteDesktop};
use ashpd::desktop::{PersistMode, Session};
use pyo3::exceptions::{PyPermissionError, PyRuntimeError, PyTimeoutError, PyValueError};
use pyo3::prelude::*;
use tokio::sync::oneshot;

use crate::{close_orphan_fd, deadline_capped, lock, runtime, wait_for, SetupBudget, Waited};

/// Ceiling for the consent wait. A dialog is a human answering, so the
/// InputCapture ceiling (60 s) is far too short.
const MAX_CONSENT_SECS: f64 = 3600.0;

/// `Register` is a plain method call, answered at once by any live portal.
const REGISTER_TIMEOUT: Duration = Duration::from_secs(10);

/// Why no session was had. Only `Denied` (the portal answered the request
/// with "cancelled" or "other") says a saved token is no longer good.
#[derive(Debug, Clone, PartialEq)]
enum RdFailure {
    Denied(String),
    Failed(String),
}

impl RdFailure {
    fn message(&self) -> &str {
        match self {
            RdFailure::Denied(m) | RdFailure::Failed(m) => m,
        }
    }
}

impl From<String> for RdFailure {
    fn from(message: String) -> Self {
        RdFailure::Failed(message)
    }
}

fn classify(step: &str, e: ashpd::Error) -> RdFailure {
    match e {
        ashpd::Error::Response(r) => RdFailure::Denied(format!("{step}: {r}")),
        other => RdFailure::Failed(format!("{step}: {other}")),
    }
}

#[derive(Debug)]
struct RdSetup {
    eis_raw_fd: i32,
    restore_token: Option<String>,
}

type SetupTx = oneshot::Sender<Result<RdSetup, RdFailure>>;

fn aborted() -> RdFailure {
    RdFailure::Failed("session closed before it was granted".to_string())
}

/// Report a failure to the waiting caller unless the result already went out.
async fn rd_task(
    setup_tx: SetupTx,
    close_rx: oneshot::Receiver<()>,
    budget: SetupBudget,
    restore_token: Option<String>,
    persist: bool,
) -> Result<(), String> {
    let mut slot = Some(setup_tx);
    let result = rd_session(&mut slot, close_rx, &budget, restore_token, persist).await;
    if let Err(e) = &result {
        if let Some(tx) = slot.take() {
            tx.send(Err(e.clone())).ok();
        }
    }
    result.map_err(|e| e.message().to_string())
}

async fn rd_session(
    slot: &mut Option<SetupTx>,
    mut close_rx: oneshot::Receiver<()>,
    budget: &SetupBudget,
    restore_token: Option<String>,
    persist: bool,
) -> Result<(), RdFailure> {
    // Every await before the grant also listens for `close`: the caller gives
    // up on a timeout or a stop, and the dialog must go away with it.
    let rd = tokio::select! {
        r = budget.guard("RemoteDesktop::new", async {
            RemoteDesktop::new().await.map_err(|e| classify("RemoteDesktop::new", e))
        }) => r?,
        _ = &mut close_rx => return Err(aborted()),
    };
    let session = tokio::select! {
        r = budget.guard("create_session", async {
            rd.create_session().await.map_err(|e| classify("create_session", e))
        }) => r?,
        _ = &mut close_rx => return Err(aborted()),
    };

    let granted = tokio::select! {
        r = start(&rd, &session, budget, restore_token.as_deref(), persist) => r,
        _ = &mut close_rx => Err(aborted()),
    };

    let result = match granted {
        Ok(setup) => {
            let tx = slot.take().ok_or_else(aborted)?;
            if let Err(Ok(unsent)) = tx.send(Ok(setup)) {
                // The caller gave up after the grant: nobody owns the fd.
                close_orphan_fd(unsent.eis_raw_fd);
            } else {
                // Held open until the owner closes it or goes away.
                close_rx.await.ok();
            }
            Ok(())
        }
        Err(e) => Err(e),
    };

    // Dropping the proxy leaves the session alive on the compositor side.
    session.close().await.ok();
    result
}

async fn start(
    rd: &RemoteDesktop<'_>,
    session: &Session<'_, RemoteDesktop<'_>>,
    budget: &SetupBudget,
    restore_token: Option<&str>,
    persist: bool,
) -> Result<RdSetup, RdFailure> {
    let mode = if persist {
        PersistMode::ExplicitlyRevoked
    } else {
        PersistMode::DoNot
    };

    budget
        .guard("select_devices", async {
            rd.select_devices(session, DeviceType::Pointer.into(), restore_token, mode)
                .await
                .map_err(|e| classify("select_devices", e))?
                .response()
                .map_err(|e| classify("select_devices", e))
        })
        .await?;

    // The dialog, when there is one, is up for the whole of this step.
    let selected = budget
        .guard("start", async {
            rd.start(session, None)
                .await
                .map_err(|e| classify("start", e))?
                .response()
                .map_err(|e| classify("start", e))
        })
        .await?;

    let fd = budget
        .guard("connect_to_eis", async {
            rd.connect_to_eis(session)
                .await
                .map_err(|e| RdFailure::Failed(format!("connect_to_eis: {e}")))
        })
        .await?;

    Ok(RdSetup {
        eis_raw_fd: fd.into_raw_fd(),
        restore_token: selected.restore_token().map(str::to_owned),
    })
}

/// Wayland RemoteDesktop portal, pointer devices only, for libei emulation.
///
/// One object, one session: `setup` once, `close` (or drop) to end it.
#[pyclass]
pub(crate) struct RemoteDesktopPortal {
    /// Present from `setup` until `close`; sending or dropping it ends the
    /// session, including one still waiting on its dialog.
    close_tx: Mutex<Option<oneshot::Sender<()>>>,
    used: Mutex<bool>,
    last_error: Arc<Mutex<Option<String>>>,
}

impl RemoteDesktopPortal {
    fn end_session(&self) {
        if let Some(tx) = lock(&self.close_tx).take() {
            tx.send(()).ok();
        }
    }
}

#[pymethods]
impl RemoteDesktopPortal {
    #[new]
    fn new() -> PyResult<Self> {
        runtime()?;
        Ok(Self {
            close_tx: Mutex::new(None),
            used: Mutex::new(false),
            last_error: Arc::new(Mutex::new(None)),
        })
    }

    /// Open the session and connect to EIS.
    /// Returns `(eis_fd, restore_token)`.
    ///
    /// Blocks, with the GIL released, until the portal grants the session:
    /// at once when `restore_token` is still valid, otherwise when the user
    /// answers the dialog. Raises `PermissionError` when the portal refuses
    /// (the token, if any, is then worthless), `TimeoutError` past `timeout`,
    /// `RuntimeError` for anything else. `close()` from another thread ends
    /// the wait.
    #[pyo3(signature = (restore_token=None, persist=true, timeout=300.0))]
    fn setup(
        &self,
        py: Python<'_>,
        restore_token: Option<String>,
        persist: bool,
        timeout: Option<f64>,
    ) -> PyResult<(i32, Option<String>)> {
        let deadline = deadline_capped(timeout, MAX_CONSENT_SECS)?;
        {
            let mut used = lock(&self.used);
            if *used {
                return Err(PyRuntimeError::new_err("already set up"));
            }
            *used = true;
        }

        let (setup_tx, setup_rx) = oneshot::channel();
        let (close_tx, close_rx) = oneshot::channel();
        *lock(&self.close_tx) = Some(close_tx);
        *lock(&self.last_error) = None;
        let last_error = self.last_error.clone();

        let rt = runtime()?;
        rt.spawn(async move {
            let budget = SetupBudget::new(deadline);
            if let Err(e) = rd_task(setup_tx, close_rx, budget, restore_token, persist).await {
                *lock(&last_error) = Some(e);
            }
        });

        let outcome = wait_for(py, rt.handle(), setup_rx, deadline);
        let result = match outcome {
            Ok(Waited::Answer(Ok(setup))) => return Ok((setup.eis_raw_fd, setup.restore_token)),
            Ok(Waited::Answer(Err(RdFailure::Denied(m)))) => Err(PyPermissionError::new_err(m)),
            Ok(Waited::Answer(Err(RdFailure::Failed(m)))) => Err(PyRuntimeError::new_err(m)),
            Ok(Waited::TimedOut(limit)) => Err(PyTimeoutError::new_err(format!(
                "permission dialog not answered within {}s",
                limit.as_secs_f64()
            ))),
            Ok(Waited::Gone) => Err(PyRuntimeError::new_err(
                "portal task ended without an answer",
            )),
            Err(e) => Err(e),
        };
        self.end_session();
        result
    }

    /// The portal task's last error, or `None`. Cleared by `setup()`.
    #[getter]
    fn last_error(&self) -> Option<String> {
        lock(&self.last_error).clone()
    }

    /// End the session, or the wait for it. Never blocks.
    fn close(&self) -> PyResult<()> {
        self.end_session();
        Ok(())
    }
}

impl Drop for RemoteDesktopPortal {
    fn drop(&mut self) {
        self.end_session();
    }
}

static REGISTERED: Mutex<Option<String>> = Mutex::new(None);

/// Tell the portal which app this (non-sandboxed) process is.
///
/// Grants are stored per app id, and a host process otherwise gets one from
/// its systemd scope, which differs between a menu launch and a terminal. Must
/// precede every other portal call in the process; repeating the same id is a
/// no-op, a different one raises `ValueError`. Needs xdg-desktop-portal
/// >= 1.19.4: older ones raise `RuntimeError`, and the caller carries on
/// unregistered. A sandboxed process returns at once.
#[pyfunction]
pub(crate) fn register_host_app(py: Python<'_>, app_id: &str) -> PyResult<()> {
    {
        let done = lock(&REGISTERED);
        if let Some(prev) = done.as_deref() {
            if prev == app_id {
                return Ok(());
            }
            return Err(PyValueError::new_err(format!(
                "already registered as {prev:?}"
            )));
        }
    }
    let id = ashpd::AppID::try_from(app_id)
        .map_err(|e| PyValueError::new_err(format!("invalid app id {app_id:?}: {e}")))?;

    let rt = runtime()?;
    py.allow_threads(|| {
        rt.block_on(async {
            tokio::time::timeout(REGISTER_TIMEOUT, ashpd::register_host_app(id)).await
        })
    })
    .map_err(|_| PyTimeoutError::new_err("Registry.Register did not answer"))?
    .map_err(|e| PyRuntimeError::new_err(format!("Registry.Register: {e}")))?;

    *lock(&REGISTERED) = Some(app_id.to_owned());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_portal_response_is_a_denial() {
        let e = ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled);
        assert!(matches!(classify("start", e), RdFailure::Denied(m) if m.starts_with("start: ")));
    }

    #[test]
    fn anything_else_is_a_failure() {
        let e = ashpd::Error::RequiresVersion(2, 1);
        assert!(matches!(
            classify("connect_to_eis", e),
            RdFailure::Failed(_)
        ));
    }

    #[test]
    fn a_budget_timeout_is_a_failure_not_a_denial() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .unwrap();
        let err = rt.block_on(async {
            SetupBudget::new(Some(Duration::ZERO))
                .guard("start", async {
                    std::future::pending::<Result<(), RdFailure>>().await
                })
                .await
                .unwrap_err()
        });
        assert!(matches!(err, RdFailure::Failed(m) if m.contains("setup deadline")));
    }

    #[test]
    fn consent_wait_is_not_capped_at_the_input_capture_ceiling() {
        let d = deadline_capped(Some(300.0), MAX_CONSENT_SECS).unwrap();
        assert_eq!(d, Some(Duration::from_secs(300)));
    }
}
