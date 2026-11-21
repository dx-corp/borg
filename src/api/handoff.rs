//! Handing a disk-labelled lease to its drone (see `docs/operations/borg.md`).

use crate::classes::disk_labels_mib;

/// Whether a lease with `labels` may be handed over now: a lease whose job
/// asked for more free disk than its class floor (`borg-disk-<N>g`) goes only
/// while the drone's latest reading, less what earlier such leases in this
/// response take, still holds it; otherwise it waits for a later heartbeat.
pub(super) fn fits(left: &mut Option<u64>, labels: &[String]) -> bool {
    disk_fits_at_handoff(left, disk_labels_mib(labels))
}

/// Whether a lease needing `need` MiB of free work disk may be handed over
/// with `left` MiB not yet promised in this response; takes it off `left`.
/// An unmeasured drone, or a lease with only its class floor, is not held
/// back here.
fn disk_fits_at_handoff(left: &mut Option<u64>, need: Option<u64>) -> bool {
    match (left.as_mut(), need) {
        (Some(free), Some(need)) if *free < need => false,
        (Some(free), Some(need)) => {
            *free -= need;
            true
        }
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::{disk_fits_at_handoff, fits};

    #[test]
    fn a_disk_labelled_lease_waits_when_the_reading_no_longer_holds_it() {
        const GIB: u64 = 1024;
        // 40 GiB free: one 32g lease is handed over, the next waits.
        let mut left = Some(40 * GIB);
        assert!(disk_fits_at_handoff(&mut left, Some(32 * GIB)));
        assert!(!disk_fits_at_handoff(&mut left, Some(32 * GIB)));
        // A class-floor lease is left to the class check.
        assert!(disk_fits_at_handoff(&mut left, None));
        // No reading: nothing is held back here.
        assert!(disk_fits_at_handoff(&mut None, Some(32 * GIB)));
        // Read from the lease's labels.
        let mut left = Some(20 * GIB);
        assert!(!fits(&mut left, &["borg-disk-32g".to_owned()]));
        assert!(fits(&mut left, &["borg-ci-medium-pr".to_owned()]));
    }
}
