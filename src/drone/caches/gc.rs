//! Size accounting and least-recently-used eviction for cache directories.
//!
//! A cache is garbage collected in whole *units*: the entries at a fixed depth
//! below a bucket directory (a downloaded crate, a content-addressed blob, an
//! installed tool version). A unit is as recent as the newest file in it, and
//! the oldest units go first until the cache is back under 90% of its cap, so
//! it is not collected again after the next job.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::drone::fsops::FsOps;

/// Collection stops once the cache is at or below this share of its cap.
const TARGET_PERCENT: u64 = 90;

/// Parse a size such as `512M` or `50G` (suffix K, M, G or T; powers of 1024).
pub fn parse_size(text: &str) -> Option<u64> {
    let unit = text.chars().last()?;
    let shift = match unit {
        'K' => 10,
        'M' => 20,
        'G' => 30,
        'T' => 40,
        _ => return None,
    };
    let digits = &text[..text.len() - 1];
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u64>().ok()?.checked_mul(1u64 << shift)
}

/// Total size and the newest file modification/access time under `path`.
/// Symlinks are counted as themselves, never followed.
pub fn measure(path: &Path) -> (u64, SystemTime) {
    let m = measure_tree(path);
    (m.size, m.newest)
}

/// What [`measure_tree`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Measured {
    pub(super) size: u64,
    pub(super) newest: SystemTime,
    /// Directories that could not be listed (for example owned by the job
    /// user and closed to the drone's user): `size` is then a lower bound.
    pub(super) unreadable_dirs: u64,
}

/// [`measure`], also counting directories that could not be listed.
pub(super) fn measure_tree(path: &Path) -> Measured {
    let mut size = 0;
    let mut newest = SystemTime::UNIX_EPOCH;
    let mut unreadable_dirs = 0;
    let mut stack = vec![path.to_owned()];
    while let Some(dir) = stack.pop() {
        let Ok(meta) = fs::symlink_metadata(&dir) else {
            continue;
        };
        size += meta.len();
        if meta.is_dir() {
            // Directory times only record when entries were added or listed;
            // so recency comes from the files.
            match fs::read_dir(&dir) {
                Ok(entries) => stack.extend(entries.flatten().map(|e| e.path())),
                // Removed between the listing and now: nothing to count.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => unreadable_dirs += 1,
            }
        } else {
            for t in [meta.modified(), meta.accessed()].into_iter().flatten() {
                newest = newest.max(t);
            }
        }
    }
    Measured {
        size,
        newest,
        unreadable_dirs,
    }
}

/// One evictable entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unit {
    pub path: PathBuf,
    pub size: u64,
    pub recency: SystemTime,
}

/// The entries exactly `depth` levels below each existing bucket (depth 1 is
/// the bucket's children).
pub fn collect_units(buckets: &[(PathBuf, usize)]) -> Vec<Unit> {
    let mut units = Vec::new();
    for (bucket, depth) in buckets {
        let mut level = vec![bucket.clone()];
        for _ in 0..*depth {
            level = level
                .iter()
                .filter(|p| fs::symlink_metadata(p).is_ok_and(|m| m.is_dir()))
                .filter_map(|p| fs::read_dir(p).ok())
                .flat_map(|rd| rd.flatten().map(|e| e.path()))
                .collect();
        }
        for path in level {
            let (size, recency) = measure(&path);
            units.push(Unit {
                path,
                size,
                recency,
            });
        }
    }
    units
}

/// Result of one collection.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Collected {
    pub removed_units: u64,
    pub freed_bytes: u64,
}

/// Evict least-recently-used `units` until `total_bytes` (the combined size of
/// all `units`; data that is never evicted does not count) is at most 90% of
/// `cap_bytes`. Does nothing while the units are within the cap.
pub fn evict_lru(
    fs: &dyn FsOps,
    mut units: Vec<Unit>,
    total_bytes: u64,
    cap_bytes: u64,
) -> Collected {
    let mut out = Collected::default();
    if total_bytes <= cap_bytes {
        return out;
    }
    let target = cap_bytes / 100 * TARGET_PERCENT;
    let mut remaining = total_bytes;
    units.sort_by(|a, b| a.recency.cmp(&b.recency).then_with(|| a.path.cmp(&b.path)));
    for unit in units {
        if remaining <= target {
            break;
        }
        match fs.remove_all(&unit.path) {
            Ok(()) => {
                remaining = remaining.saturating_sub(unit.size);
                out.removed_units += 1;
                out.freed_bytes += unit.size;
            }
            Err(e) => tracing::warn!("cache gc: remove {}: {e}", unit.path.display()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn file(path: &Path, bytes: usize, age_secs: u64) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, vec![0u8; bytes]).unwrap();
        let when = SystemTime::now() - Duration::from_secs(age_secs);
        let times = std::fs::FileTimes::new()
            .set_modified(when)
            .set_accessed(when);
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(times)
            .unwrap();
    }

    #[test]
    fn sizes_parse_with_binary_suffixes() {
        assert_eq!(parse_size("512M"), Some(512 << 20));
        assert_eq!(parse_size("50G"), Some(50 << 30));
        assert_eq!(parse_size("2T"), Some(2 << 40));
        for bad in ["", "G", "50", "5g", "5.5G", "-1G", "99999999999999999999G"] {
            assert_eq!(parse_size(bad), None, "{bad}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn an_unlistable_directory_makes_the_size_a_lower_bound() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        file(&dir.path().join("open/a"), 10, 5);
        file(&dir.path().join("closed/b"), 10, 5);
        let closed = dir.path().join("closed");
        fs::set_permissions(&closed, fs::Permissions::from_mode(0o000)).unwrap();
        let m = measure_tree(dir.path());
        let listable = fs::read_dir(&closed).is_ok(); // root reads anything
        fs::set_permissions(&closed, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(m.unreadable_dirs, u64::from(!listable));
        assert_eq!(measure_tree(dir.path()).unreadable_dirs, 0);
    }

    #[test]
    fn measure_sums_a_tree_and_finds_the_newest_file() {
        let dir = tempfile::tempdir().unwrap();
        file(&dir.path().join("a/old"), 100, 1000);
        file(&dir.path().join("a/b/new"), 50, 10);
        let (size, newest) = measure(dir.path());
        assert!(size >= 150);
        assert!(
            newest.elapsed().unwrap() < Duration::from_secs(500),
            "newest file counts"
        );
    }

    #[test]
    fn units_are_the_entries_at_the_requested_depth() {
        let dir = tempfile::tempdir().unwrap();
        file(&dir.path().join("cas/ab/h1"), 10, 5);
        file(&dir.path().join("cas/cd/h2"), 10, 5);
        file(&dir.path().join("other/x"), 10, 5);
        let mut names: Vec<String> =
            collect_units(&[(dir.path().join("cas"), 2), (dir.path().join("missing"), 1)])
                .iter()
                .map(|u| u.path.file_name().unwrap().to_string_lossy().into_owned())
                .collect();
        names.sort();
        assert_eq!(names, ["h1", "h2"]);
        let shallow = collect_units(&[(dir.path().join("cas"), 1)]);
        assert_eq!(
            shallow.len(),
            2,
            "depth 1 is the bucket directories themselves"
        );
    }

    #[test]
    fn evicts_oldest_first_down_to_ninety_percent_of_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        for (name, age) in [("oldest", 400), ("old", 300), ("mid", 200), ("new", 100)] {
            file(&dir.path().join(format!("b/{name}/data")), 1000, age);
        }
        let units = collect_units(&[(dir.path().join("b"), 1)]);
        let total: u64 = units.iter().map(|u| u.size).sum();
        // A cap of half the total must drop the two oldest units.
        let got = evict_lru(&crate::drone::fsops::LocalFs, units, total, total / 2);
        assert!(got.removed_units >= 2, "{got:?}");
        assert!(!dir.path().join("b/oldest").exists() && !dir.path().join("b/old").exists());
        assert!(
            dir.path().join("b/new").exists(),
            "the newest unit survives"
        );
    }

    #[test]
    fn nothing_is_evicted_within_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        file(&dir.path().join("b/u/data"), 1000, 100);
        let units = collect_units(&[(dir.path().join("b"), 1)]);
        let total: u64 = units.iter().map(|u| u.size).sum();
        assert_eq!(
            evict_lru(&crate::drone::fsops::LocalFs, units, total, total),
            Collected::default()
        );
        assert!(dir.path().join("b/u").exists());
    }

    #[cfg(unix)]
    #[test]
    fn read_only_trees_are_still_removed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        file(&dir.path().join("b/mod@v1/src/lib.go"), 1000, 100);
        fs::set_permissions(
            dir.path().join("b/mod@v1/src"),
            fs::Permissions::from_mode(0o555),
        )
        .unwrap();
        fs::set_permissions(
            dir.path().join("b/mod@v1"),
            fs::Permissions::from_mode(0o555),
        )
        .unwrap();
        let units = collect_units(&[(dir.path().join("b"), 1)]);
        let total: u64 = units.iter().map(|u| u.size).sum();
        let got = evict_lru(&crate::drone::fsops::LocalFs, units, total, 1);
        assert_eq!(got.removed_units, 1);
        assert!(!dir.path().join("b/mod@v1").exists());
    }
}
