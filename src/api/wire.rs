//! The lease messages drones and the queen exchange: a grant and its result.

use serde::{Deserialize, Serialize};

use crate::classes::{Class, Scope};
use crate::drone::memlimit::LeaseMemory;
use crate::hostdiag::LeaseHostReport;
use crate::secret::Secret;
use crate::telemetry::JobTelemetry;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaseGrant {
    pub lease_id: String,
    pub class: Class,
    pub scope: Scope,
    pub labels: Vec<String>,
    pub runner_name: String,
    pub jit_config: Secret,
    pub expires_at: i64,
    /// The class's free-disk floor, sent with a standby so the drone holds
    /// it (and keeps it across a restart) for fail-open: a standby never
    /// starts below its class's floor while the queen is unreachable. Older
    /// queens omit it and the drone uses the global floor alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_free_disk_bytes: Option<u64>,
    /// The lease's memory limit (its class size plus the queen's headroom);
    /// see [`crate::drone::memlimit`]. Older queens omit it: no limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<LeaseMemory>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ResultRequest {
    pub exit_code: i32,
    pub duration_secs: u64,
    #[serde(default)]
    pub peak_rss_mib: Option<u64>,
    /// Compact per-job summary; see [`crate::telemetry`] for the wire format.
    #[serde(default)]
    pub telemetry: Option<JobTelemetry>,
    /// The drone stopped the job because the queen preempted the lease.
    #[serde(default)]
    pub preempted: bool,
    /// The kernel OOM-killed a process of the job at its memory limit. The
    /// job's own outcome, never a host failure.
    #[serde(default)]
    pub oom_killed: bool,
    /// Host load when the runner started and when it exited; see
    /// [`crate::hostdiag`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) host: Option<LeaseHostReport>,
}
