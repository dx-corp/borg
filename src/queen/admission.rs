//! The queen's view of each drone's admission: the CPU pressure window it
//! averages, the state each drone settled in last pass (for hysteresis), and
//! the live memory facts the allocator sizes drones by.

use std::collections::HashMap;

use super::{Queen, platform_of};
use crate::allocator::{AdmissionState, DroneAdmission, HostMemory, LiveFacts};
use crate::config::AdmissionSettings;
use crate::envcontract::EnvFacts;
use crate::probe::Capacity;

/// Whether drone `id` may be given a work unit for `platform`: it runs that
/// platform and its last heartbeat said it runs units. An older drone build
/// never says so and would leave the unit unstarted until it runs out of
/// attempts, so a rollout's mixed fleet only places units on new drones.
pub(super) fn unit_host(
    id: &str,
    capacities: &HashMap<String, Capacity>,
    rows: &HashMap<&str, &crate::store::DroneRow>,
    platform: &str,
) -> bool {
    capacities
        .get(id)
        .is_some_and(|c| platform_of(c) == platform)
        && rows.get(id).is_some_and(|r| r.runs_units)
}

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
    /// `budget_mib` is the drone's memory budget (net of reserve and caps).
    pub(super) fn live_facts(
        &self,
        drone: &crate::store::DroneRow,
        cap: &Capacity,
        facts: &EnvFacts,
        settings: &AdmissionSettings,
        budget_mib: u64,
    ) -> LiveFacts {
        LiveFacts {
            mem_used_mib: facts.mem_available_mib.map(|a| {
                used_within_budget(cap.mem_mib, a, drone.caps.reserve_mem_mib, budget_mib)
            }),
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
            admission_prev: self.admission_prev(&drone.name),
        }
    }
}

/// Host memory in use that eats into a drone's budget. The reading is the
/// whole host's (total less available), but a capped drone lends only part
/// of the host: what the host uses up to the memory the budget leaves free
/// beyond the OS reserve (`total - reserve - budget`) costs the budget
/// nothing. An uncapped drone leaves nothing free, so all of it counts.
/// (2026-11-16: wsl-desktop, 48 GiB VM, 15 GiB budget, 3.3 GiB of VM-wide use
/// charged against the budget left 12 GiB, so a heavy job never fit.)
pub(crate) fn used_within_budget(
    total_mib: u64,
    available_mib: u64,
    reserve_mib: u64,
    budget_mib: u64,
) -> u64 {
    let used = total_mib.saturating_sub(available_mib);
    let slack = total_mib
        .saturating_sub(reserve_mib)
        .saturating_sub(budget_mib);
    used.saturating_sub(slack)
}

#[cfg(test)]
mod tests {
    use super::used_within_budget;

    #[test]
    fn a_capped_drone_is_charged_only_host_use_beyond_what_its_budget_leaves_free() {
        // wsl-desktop on 2026-11-16: 48174 MiB VM, 2048 MiB reserve, 15360 MiB
        // budget, 3323 MiB in use VM-wide. All of it fits in the 30766 MiB the
        // budget leaves free, so the budget keeps its full room.
        assert_eq!(used_within_budget(48174, 48174 - 3323, 2048, 15360), 0);
        // Host use beyond that slack is charged: 34766 MiB in use, 4000 over.
        assert_eq!(used_within_budget(48174, 48174 - 34766, 2048, 15360), 4000);
    }

    #[test]
    fn an_uncapped_drone_is_charged_all_host_use_as_before() {
        // Budget = total - reserve leaves no slack: the reading counts in full.
        assert_eq!(used_within_budget(32768, 32768 - 9000, 2048, 30720), 9000);
        // A reserve or cap larger than the host never underflows.
        assert_eq!(used_within_budget(4096, 0, 8192, 30720), 4096);
    }
}
