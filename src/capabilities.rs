//! Which portal interfaces this desktop offers, so a caller picks its backend
//! from what the compositor can do rather than from the desktop's name.

use std::collections::HashMap;
use std::ops::Deref;
use std::time::Duration;

use ashpd::desktop::input_capture::InputCapture;
use ashpd::desktop::remote_desktop::RemoteDesktop;
use ashpd::zbus;
use pyo3::exceptions::{PyRuntimeError, PyTimeoutError, PyValueError};
use pyo3::prelude::*;

use crate::runtime;

pub(crate) const REMOTE_DESKTOP: &str = "remote_desktop";
pub(crate) const INPUT_CAPTURE: &str = "input_capture";

/// An interface the portal does not offer reads as version 0.
fn present(version: Result<u32, ashpd::Error>) -> Result<u32, ashpd::Error> {
    match version {
        Err(ashpd::Error::PortalNotFound(_)) => Ok(0),
        other => other,
    }
}

async fn version_of<'a, P: Deref<Target = zbus::Proxy<'a>>>(
    proxy: Result<P, ashpd::Error>,
) -> Result<u32, ashpd::Error> {
    present(match proxy {
        Ok(p) => p.get_property::<u32>("version").await.map_err(Into::into),
        Err(e) => Err(e),
    })
}

/// Versions of the portal interfaces this library drives, 0 when absent.
///
/// RemoteDesktop needs version 2 for `ConnectToEIS`, the libei path; any
/// InputCapture version speaks libei. Goes through the process-wide D-Bus
/// connection, so a host app calls `register_host_app` first. Raises
/// `TimeoutError` when the portal does not answer within `timeout` seconds and
/// `RuntimeError` when there is no portal service to ask.
#[pyfunction]
#[pyo3(signature = (timeout = 2.0))]
pub(crate) fn portal_versions(py: Python<'_>, timeout: f64) -> PyResult<HashMap<String, u32>> {
    let limit = Duration::try_from_secs_f64(timeout)
        .ok()
        .filter(|d| !d.is_zero())
        .ok_or_else(|| PyValueError::new_err("timeout must be a positive number of seconds"))?;

    let rt = runtime()?;
    let (rd, ic) = py
        .allow_threads(|| {
            // The timer must be built inside the runtime, or tokio panics.
            rt.block_on(async {
                tokio::time::timeout(limit, async {
                    let rd = version_of(RemoteDesktop::new().await).await?;
                    let ic = version_of(InputCapture::new().await).await?;
                    Ok::<_, ashpd::Error>((rd, ic))
                })
                .await
            })
        })
        .map_err(|_| PyTimeoutError::new_err("the portal did not answer"))?
        .map_err(|e| PyRuntimeError::new_err(format!("portal: {e}")))?;

    Ok(HashMap::from([
        (REMOTE_DESKTOP.to_owned(), rd),
        (INPUT_CAPTURE.to_owned(), ic),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_interface_is_version_zero() {
        let missing = ashpd::Error::PortalNotFound(
            zbus::names::OwnedInterfaceName::try_from("org.freedesktop.portal.InputCapture")
                .unwrap(),
        );
        assert_eq!(present(Err(missing)).unwrap(), 0);
    }

    #[test]
    fn a_present_interface_reports_its_version() {
        assert_eq!(present(Ok(2)).unwrap(), 2);
    }

    #[test]
    fn any_other_failure_is_an_error() {
        let err = ashpd::Error::RequiresVersion(2, 1);
        assert!(present(Err(err)).is_err());
    }
}
