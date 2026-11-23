//! Bounds on what a drone reports in its heartbeat, applied before the queen
//! stores it.

use crate::bid::DroneBid;
use crate::envcontract::EnvFacts;

/// Bound the free text a drone reports; the observation time cannot be in the
/// future.
pub(super) fn facts(f: &EnvFacts, now: i64) -> EnvFacts {
    let clip = |s: &String| {
        s.chars()
            .filter(|c| !c.is_control())
            .take(300)
            .collect::<String>()
    };
    let mut out = f.clone();
    out.observed_at = out.observed_at.min(now);
    out.cpu_busy_pm = out.cpu_busy_pm.map(|b| b.min(1000));
    out.cpu_psi_some_avg10_pm = out.cpu_psi_some_avg10_pm.map(|p| p.min(1000));
    out.mem_psi_some_avg10_pm = out.mem_psi_some_avg10_pm.map(|p| p.min(1000));
    out.host_key = out.host_key.filter(|k| {
        !k.is_empty()
            && k.len() <= 64
            && k.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'))
    });
    for fact in out.capabilities.values_mut() {
        fact.observed_at = fact.observed_at.min(now);
    }
    out.rustc_wrapper = out.rustc_wrapper.as_ref().map(clip);
    out.host_invariant_failures.truncate(16);
    for f in &mut out.host_invariant_failures {
        f.check = f
            .check
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
            .take(40)
            .collect();
        f.detail = clip(&f.detail);
    }
    out.sccache_dir = out.sccache_dir.as_ref().map(clip);
    if let Some(list) = &mut out.toolchains {
        list.truncate(32);
        for t in list {
            t.channel = clip(&t.channel);
            t.rustc_version = clip(&t.rustc_version);
        }
    }
    // Only probe-list names, with bounded printable versions.
    out.tools = out.tools.as_ref().map(|m| {
        m.iter()
            .filter(|(k, _)| crate::envcontract::PROBE_TOOLS.contains(&k.as_str()))
            .map(|(k, v)| {
                (
                    k.clone(),
                    v.chars().filter(|c| !c.is_control()).take(80).collect(),
                )
            })
            .collect()
    });
    out
}

/// At most one bid per key (the first), clamped, at most one per key overall.
pub(super) fn bids(bids: &[DroneBid]) -> Vec<DroneBid> {
    let mut out: Vec<DroneBid> = Vec::new();
    for b in bids {
        if !out.iter().any(|o| o.class == b.class && o.scope == b.scope) {
            out.push(b.clamped());
        }
    }
    out
}

/// A drone-reported version is shown in metric labels, so accept only short
/// printable ASCII.
pub(super) fn version(v: Option<&str>) -> Option<&str> {
    v.filter(|v| !v.is_empty() && v.len() <= 64 && v.bytes().all(|b| b.is_ascii_graphic()))
}
