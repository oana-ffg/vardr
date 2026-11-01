//! Best-effort scheduler diagnostics. Locks, never this report, establish liveness.
use crate::{
    model::ServiceState,
    store::{Store, read_json},
};
use anyhow::Result;
use chrono::Utc;

pub(crate) const INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

pub struct Report {
    pub state: Option<ServiceState>,
    pub running: bool,
    pub warning: Option<String>,
}
impl Report {
    /// Old reports remain available to migration as historical failure evidence.
    pub fn current(&self) -> Option<&ServiceState> {
        self.state.as_ref().filter(|_| self.warning.is_none())
    }
}

/// A crash may leave this non-durable file missing, empty, malformed or stale.
/// Preserve real filesystem/security errors; unavailable diagnostics are explicit.
pub fn read(store: &Store) -> Result<Report> {
    let running = store.is_locked("service")?;
    let state = match read_json::<ServiceState>(&store.root.join("service.json")) {
        Ok(state) => state,
        Err(error)
            if error.chain().any(|e| {
                e.downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
            }) =>
        {
            return Ok(Report {
                state: None,
                running,
                warning: None,
            });
        }
        Err(error) if error.downcast_ref::<serde_json::Error>().is_some() => {
            return Ok(Report {
                state: None,
                running,
                warning: Some(
                    "Scheduler report is incomplete or invalid; liveness is determined by its lock"
                        .into(),
                ),
            });
        }
        Err(error) => return Err(error),
    };
    let fresh = (state.pid == 0 || running)
        && state.updated_at.is_some_and(|updated| {
            let age = Utc::now().signed_duration_since(updated).num_seconds();
            // Three missed heartbeats, with one interval of clock skew allowed.
            let interval = INTERVAL.as_secs() as i64;
            (-interval..=3 * interval).contains(&age)
        });
    Ok(Report {
        state: Some(state),
        running,
        warning: (!fresh)
            .then(|| "Scheduler report is stale; liveness is determined by its lock".into()),
    })
}
