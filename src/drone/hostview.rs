//! The node a container drone runs on, read from that node's Prometheus
//! `node_exporter` (`host_metrics_url` in the drone config).
//!
//! Inside a Proxmox LXC, lxcfs gives the drone its own `/proc/meminfo` and
//! `/proc/pressure`, so it cannot see what the node's other guests use. The
//! node already exports its real figures for monitoring; the drone reads the
//! same endpoint each heartbeat. Memory pressure comes from the
//! `node_pressure_memory_waiting_seconds_total` counter (PSI `some`) as the
//! share of wall time stalled since the previous reading.
//!
//! A failed or partial read reports nothing: the drone then sizes itself by
//! what it sees, as without a host view.

use std::sync::Mutex;
use std::time::Duration;

/// How long one read may take; a heartbeat never waits longer for it.
const READ_TIMEOUT: Duration = Duration::from_secs(2);
/// Two readings further apart than this give no pressure figure.
const MAX_SAMPLE_GAP_MS: i64 = 120_000;
/// Longest node key reported.
const MAX_KEY_LEN: usize = 64;

/// What one read of the node says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HostReading {
    /// Identifies the node (its name and boot time): drones that report the
    /// same key share its memory.
    pub(crate) key: String,
    /// The node's `MemAvailable`, MiB.
    pub(crate) available_mib: u64,
    /// Memory `some` pressure since the previous reading, permille; `None`
    /// on the first reading or after a gap.
    pub(crate) mem_psi_pm: Option<u16>,
}

/// The figures parsed from one exposition.
#[derive(Debug, Clone, PartialEq)]
struct Exposition {
    available_bytes: f64,
    mem_waiting_secs: Option<f64>,
    key: String,
}

#[derive(Debug, Clone, PartialEq)]
struct Sample {
    at_ms: i64,
    key: String,
    mem_waiting_secs: f64,
}

pub(crate) struct HostView {
    url: String,
    http: reqwest::Client,
    last: Mutex<Option<Sample>>,
}

impl HostView {
    pub(crate) fn new(url: &str) -> Result<Self, reqwest::Error> {
        Ok(Self {
            url: url.to_owned(),
            http: reqwest::Client::builder().timeout(READ_TIMEOUT).build()?,
            last: Mutex::new(None),
        })
    }

    /// Read the node now (`now_ms`, Unix milliseconds).
    pub(crate) async fn read(&self, now_ms: i64) -> Option<HostReading> {
        let text = match self.fetch().await {
            Ok(text) => text,
            Err(e) => {
                tracing::debug!("host view {}: {e}", self.url);
                return None;
            }
        };
        let exp = parse(&text)?;
        // The lock is taken only after the await and held for a few lines.
        let mut last = self.last.lock().ok()?;
        let prev = last.take();
        Some(reading(&exp, prev.as_ref(), now_ms, &mut last))
    }

    async fn fetch(&self) -> Result<String, reqwest::Error> {
        self.http
            .get(&self.url)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await
    }
}

/// The reading for `exp` against the previous sample, which it replaces.
fn reading(
    exp: &Exposition,
    prev: Option<&Sample>,
    now_ms: i64,
    slot: &mut Option<Sample>,
) -> HostReading {
    let mem_psi_pm = exp.mem_waiting_secs.zip(prev).and_then(|(waiting, p)| {
        let dt_ms = now_ms - p.at_ms;
        if p.key != exp.key || !(1..=MAX_SAMPLE_GAP_MS).contains(&dt_ms) {
            return None;
        }
        let stalled = waiting - p.mem_waiting_secs;
        (stalled >= 0.0).then(|| {
            (stalled * 1000.0 * 1000.0 / dt_ms as f64)
                .round()
                .clamp(0.0, 1000.0) as u16
        })
    });
    *slot = exp.mem_waiting_secs.map(|w| Sample {
        at_ms: now_ms,
        key: exp.key.clone(),
        mem_waiting_secs: w,
    });
    HostReading {
        key: exp.key.clone(),
        available_mib: (exp.available_bytes / 1_048_576.0).floor() as u64,
        mem_psi_pm,
    }
}

/// The value of an unlabelled sample `name` in Prometheus text format.
fn value(text: &str, name: &str) -> Option<f64> {
    text.lines()
        .filter(|l| !l.starts_with('#'))
        .find_map(|l| l.strip_prefix(name)?.strip_prefix(' '))?
        .split_whitespace()
        .next()?
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v >= 0.0)
}

/// `nodename` from `node_uname_info{...,nodename="x",...}`.
fn nodename(text: &str) -> Option<String> {
    let line = text.lines().find(|l| l.starts_with("node_uname_info{"))?;
    let rest = line.split("nodename=\"").nth(1)?;
    Some(rest.split('"').next()?.to_owned())
}

fn parse(text: &str) -> Option<Exposition> {
    let available_bytes = value(text, "node_memory_MemAvailable_bytes")?;
    let boot = value(text, "node_boot_time_seconds")?;
    let name: String = nodename(text)?
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'))
        .collect();
    if name.is_empty() {
        return None;
    }
    let mut key = format!("{name}-{}", boot.round() as u64);
    key.truncate(MAX_KEY_LEN);
    Some(Exposition {
        available_bytes,
        mem_waiting_secs: value(text, "node_pressure_memory_waiting_seconds_total"),
        key,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Trimmed from a homelab node's exporter on 2026-11-08, renamed.
    const NODE: &str = "# HELP node_memory_MemAvailable_bytes x\n\
        # TYPE node_memory_MemAvailable_bytes gauge\n\
        node_memory_MemAvailable_bytes 2.0267360256e+10\n\
        node_boot_time_seconds 1.793925029e+09\n\
        node_pressure_memory_stalled_seconds_total 1508.837176\n\
        node_pressure_memory_waiting_seconds_total 1802.4855519999999\n\
        node_uname_info{domainname=\"(none)\",machine=\"x86_64\",nodename=\"node-a\",release=\"7.0.14-28-pve\"} 1\n";

    #[test]
    fn a_node_exposition_gives_memory_and_a_stable_key() {
        let exp = parse(NODE).expect("parses");
        let expected = format!("node-a-{}", 1_793_925_029);
        assert_eq!(exp.key, expected);
        assert_eq!((exp.available_bytes / 1_048_576.0) as u64, 19_328);
        assert_eq!(exp.mem_waiting_secs, Some(1802.4855519999999));
        // `stalled` is `full`, not `some`: never mistaken for it.
        assert_eq!(
            value(NODE, "node_pressure_memory_waiting_seconds"),
            None,
            "a prefix of a longer name does not match"
        );
        assert_eq!(parse("node_boot_time_seconds 1\n"), None);
        assert_eq!(parse(&NODE.replace("node-a", "\"")), None);
    }

    #[test]
    fn pressure_is_the_stalled_share_of_wall_time_between_readings() {
        let exp = parse(NODE).expect("parses");
        let mut slot = None;
        let first = reading(&exp, None, 10_000, &mut slot);
        assert_eq!(first.mem_psi_pm, None, "one reading has no rate");
        assert_eq!(first.available_mib, 19_328);
        let prev = slot.clone();
        // 0.62 s stalled over 10 s: 62 permille.
        let later = Exposition {
            mem_waiting_secs: exp.mem_waiting_secs.map(|w| w + 0.62),
            ..exp.clone()
        };
        let r = reading(&later, prev.as_ref(), 20_000, &mut slot);
        assert_eq!(r.mem_psi_pm, Some(62));
        // A reboot (new key), a counter reset or a long gap gives no rate.
        let rebooted = Exposition {
            key: format!("node-a-{}", 1),
            ..later.clone()
        };
        assert_eq!(
            reading(&rebooted, slot.as_ref(), 30_000, &mut None).mem_psi_pm,
            None
        );
        let reset = Exposition {
            mem_waiting_secs: Some(1.0),
            ..later.clone()
        };
        assert_eq!(
            reading(&reset, slot.as_ref(), 30_000, &mut None).mem_psi_pm,
            None
        );
        assert_eq!(
            reading(&later, slot.as_ref(), 200_000, &mut None).mem_psi_pm,
            None
        );
    }
}
