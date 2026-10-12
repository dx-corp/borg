//! Placement history captured by the allocator, rather than reconstructed from
//! today's fleet. Capacity, bids and reservations change after each grant, so
//! `/status` cannot explain an earlier choice. The existing queen SQLite store
//! owns this bounded diagnostic history; it contains no argv, env or tokens.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use crate::allocator::PlacementDecision;
use crate::secret::Secret;

/// Both the row count and per-row size are bounded. Old decisions are deleted
/// transactionally on insert; dropping this table loses diagnostics only.
pub const MAX_HISTORY: i64 = 1000;
pub const MAX_DECISION_BYTES: usize = 128 * 1024;
pub const MAX_QUERY_ROWS: i64 = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlacementKind {
    Runner,
    Unit,
}

impl PlacementKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Runner => "runner",
            Self::Unit => "unit",
        }
    }
}

/// Allocator selection is not proof that minting or the persisted unit lease
/// succeeded. Record the result only after the owning operation finishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlacementOutcome {
    Granted,
    Unplaced,
    Failed,
}

pub struct PlacementRecord<'a> {
    pub at: i64,
    pub kind: PlacementKind,
    pub key: &'a str,
    pub client: Option<&'a str>,
    pub outcome: PlacementOutcome,
    pub decision: &'a PlacementDecision,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub id: i64,
    pub at: i64,
    pub kind: PlacementKind,
    pub key: String,
    pub outcome: PlacementOutcome,
    pub decision: PlacementDecision,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Explanation {
    pub key: String,
    /// Empty explicitly means no retained decision, not successful placement.
    pub history: Vec<HistoryEntry>,
    pub retention_limit: i64,
}

impl Explanation {
    pub fn new(key: impl Into<String>, history: Vec<HistoryEntry>) -> Self {
        Self {
            key: key.into(),
            history,
            retention_limit: MAX_HISTORY,
        }
    }
}

pub fn render_text(report: &Explanation) -> String {
    let mut out = String::new();
    if report.history.is_empty() {
        let _ = writeln!(out, "{}: no retained placement decision", report.key);
        return out;
    }
    for row in &report.history {
        let d = &row.decision;
        let _ = writeln!(
            out,
            "{} {} at {}: {:?}, selected {} (grant order {})",
            row.kind.as_str(),
            row.key,
            row.at,
            row.outcome,
            d.winner.as_deref().unwrap_or("none"),
            d.grant_order
        );
        let _ = writeln!(
            out,
            "  {} {} needs {} vCPU / {} MiB; pressure {}",
            d.class,
            d.scope.as_str(),
            d.requested.vcpu,
            d.requested.mem_mib,
            d.pressure
        );
        for host in &d.candidates {
            let _ = writeln!(
                out,
                "  {}: {} / {} MiB, {} / {} vCPU free; lease slots {:?}; score {:?} (bid {}, profile {}); {:?}; reservation {:?}",
                host.drone_id,
                host.room.mem_mib,
                host.budget.mem_mib,
                host.room.vcpu,
                host.budget.vcpu,
                host.room.leases_left,
                host.score,
                host.bid_score,
                host.profile_score,
                host.rejection_reasons,
                host.reserved_for
            );
            if !host.environment_checks.is_empty() {
                let _ = writeln!(out, "    environment checks: {:?}", host.environment_checks);
            }
            if !host.capability_checks.is_empty() {
                let _ = writeln!(out, "    capability checks: {:?}", host.capability_checks);
            }
        }
        if d.candidates.len() < d.total_candidates {
            let _ = writeln!(
                out,
                "  showing {} of {} considered hosts",
                d.candidates.len(),
                d.total_candidates
            );
        }
        if let Some(host) = &d.reservation {
            let _ = writeln!(out, "  reserved {host} for this demand to free enough room");
        }
    }
    out
}

/// Fetch the same typed history the CLI reads locally. Unit keys require the
/// existing client bearer token; runner labels expose runner metadata only.
pub async fn fetch(
    queen: &str,
    key: &str,
    token: Option<&Secret>,
) -> Result<Explanation, reqwest::Error> {
    let url = format!("{}/explain", queen.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?;
    let mut request = client.get(url).query(&[("key", key)]);
    if let Some(token) = token {
        request = request.bearer_auth(token.expose());
    }
    request.send().await?.error_for_status()?.json().await
}
