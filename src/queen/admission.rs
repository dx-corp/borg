//! The queen's view of each drone's admission: the CPU pressure window it
//! averages, the state each drone settled in last pass (for hysteresis), and
//! the live memory facts the allocator sizes drones by.

use super::Queen;
use crate::allocator::{AdmissionState, DroneAdmission, HostMemory, LiveFacts};
use crate::config::AdmissionSettings;
use crate::envcontract::EnvFacts;
use crate::probe::Capacity;

/// Samples kept per drone: 10 s heartbeats over a 180 s window need 18.
const CPU_PRESSURE_SAMPLES: usize = 64;

/// The mean of the samples observed within `max_age_secs` of `now`, rounded
/// to permille, or `None` without one. A drone reports `some avg10`, a 10 s
/// window that a reclaim scan, container teardown or a git fetch pushes past
/// the thresholds for one or two heartbeats; the queen samples it once a
/// tick, so a drone with nothing running could flip to `light_only` or
/// `closed` on a burst (10/23 explain history: gha-heavy-pve50-1 changed
/// state at nearly every 300 s tick). Admission answers whether the host is
/// oversubscribed for minutes, so it reads the window mean.
pub(super) fn pressure_window_mean(
    samples: &[(i64, u16)],
    now: i64,
    max_age_secs: i64,
) -> Option<u16> {
    let fresh = samples
        .iter()
        .filter(|(at, _)| (0..=max_age_secs).contains(&(now - at)))
        .map(|&(_, pm)| u64::from(pm.min(1000)));
    let (sum, n) = fresh.fold((0u64, 0u64), |(s, n), pm| (s + pm, n + 1));
    (n > 0).then(|| u16::try_from((sum + n / 2) / n).unwrap_or(1000))
}

impl Queen {
    /// Record a heartbeat's CPU pressure reading for the admission window.
    /// Without a reading nothing is recorded: the window then empties and
    /// admission falls back to the load estimate.
    pub(crate) fn note_cpu_pressure(&self, drone: &str, observed_at: i64, psi_pm: Option<u16>) {
        let Some(pm) = psi_pm else { return };
        let mut all = self.cpu_pressure.lock().expect("cpu pressure lock");
        let samples = all.entry(drone.to_owned()).or_default();
        if samples.back().is_some_and(|&(at, _)| at >= observed_at) {
            return;
        }
        samples.push_back((observed_at, pm));
        while samples.len() > CPU_PRESSURE_SAMPLES {
            samples.pop_front();
        }
    }

    /// The admission state `drone` settled in on the previous pass.
    fn admission_prev(&self, drone: &str) -> Option<AdmissionState> {
        self.admission_prev
            .lock()
            .expect("admission state lock")
            .get(drone)
            .copied()
    }

    /// Remember this pass's states; drones absent from it are forgotten.
    pub(super) fn note_admission(&self, admission: &[DroneAdmission]) {
        *self.admission_prev.lock().expect("admission state lock") = admission
            .iter()
            .map(|a| (a.drone_id.clone(), a.state))
            .collect();
    }

    /// The drone's admission pressure: the mean of its readings within
    /// `max_age_secs`, see [`pressure_window_mean`].
    pub(super) fn cpu_pressure_mean(
        &self,
        drone: &str,
        now: i64,
        max_age_secs: i64,
    ) -> Option<u16> {
        let all = self.cpu_pressure.lock().expect("cpu pressure lock");
        let samples: Vec<(i64, u16)> = all.get(drone)?.iter().copied().collect();
        pressure_window_mean(&samples, now, max_age_secs)
    }

    /// What fresh `facts` say about `drone`'s memory; see [`LiveFacts`].
    pub(super) fn live_facts(
        &self,
        drone: &str,
        cap: &Capacity,
        facts: &EnvFacts,
        settings: &AdmissionSettings,
    ) -> LiveFacts {
        LiveFacts {
            mem_used_mib: facts
                .mem_available_mib
                .map(|a| cap.mem_mib.saturating_sub(a)),
            host: facts
                .host_key
                .clone()
                .zip(facts.host_mem_available_mib)
                .map(|(key, available_mib)| HostMemory {
                    key,
                    available_mib,
                    reserve_mib: settings.host_reserve_mem_mib,
                }),
            mem_psi_pm: facts.mem_psi_some_avg10_pm,
            admission_prev: self.admission_prev(drone),
        }
    }
}
