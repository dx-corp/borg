//! Wake-on-LAN for idle hosts, driven by unmet demand.
//!
//! When demand for a class stays unmet for `after_secs` (the fleet has no
//! capacity for it), the queen sends a magic packet to a configured host that
//! can serve the class and is not awake, rate limited three ways: at most one
//! wake per `min_interval_secs` anywhere, at most `max_per_hour` per hour, and
//! at most one per host per `host_cooldown_secs` (a host that was woken and
//! never came up is retried only after that). A host counts as awake when a
//! drone of the same `name` is fresh.
//!
//! The planner ([`WakePlanner`]) is pure and takes the clock as an argument; the
//! sender is a trait ([`WakeSender`]) so tests use a fake and production sends
//! UDP. The packet code ([`parse_mac`], [`magic_packet`], the broadcast
//! address math and the port pair 9 and 7) is ported from the wake sender in
//! haasonsaas/flotilla (`crates/flotilla-core/src/api.rs` and
//! `crates/flotillad/src/server.rs`, MIT, same author), without its interface
//! discovery: broadcast targets are configuration here.
//!
//! The magic packet must originate on the target's LAN; run the queen on (or
//! route directed broadcasts to) the network the hosts sleep on.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::future::Future;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::pin::Pin;

use serde::{Deserialize, Serialize};

use crate::classes::Class;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Parse `aa:bb:cc:dd:ee:ff`, `aa-bb-...` or `aabbccddeeff`.
/// (Ported from flotilla-core `parse_mac`.)
pub fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let hex: String = s.chars().filter(char::is_ascii_hexdigit).collect();
    if hex.len() != 12 {
        return None;
    }
    let mut out = [0u8; 6];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// A wake-on-LAN magic packet: six 0xFF bytes, then the MAC sixteen times.
/// (Ported from flotilla-core `magic_packet`.)
pub fn magic_packet(mac: [u8; 6]) -> Vec<u8> {
    let mut p = vec![0xFFu8; 6];
    for _ in 0..16 {
        p.extend_from_slice(&mac);
    }
    p
}

/// The directed broadcast address of `ip/prefix`. (The mask math is ported
/// from the interface loop in flotillad `post_wake`.)
pub fn directed_broadcast(ip: Ipv4Addr, prefix: u8) -> Ipv4Addr {
    let prefix = u32::from(prefix.min(32));
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    Ipv4Addr::from(u32::from(ip) | !mask)
}

/// A broadcast target from configuration: a plain IPv4 address (used as is) or
/// `a.b.c.d/len` (its directed broadcast).
pub fn parse_target(s: &str) -> Option<Ipv4Addr> {
    match s.split_once('/') {
        Some((ip, len)) => Some(directed_broadcast(ip.parse().ok()?, len.parse().ok()?)),
        None => s.parse().ok(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WakeHost {
    /// Drone name this machine runs under; the host counts as awake while that
    /// drone is fresh. Also the label in logs and metrics.
    pub name: String,
    pub mac: String,
    /// Classes the host can serve; all when empty.
    #[serde(default)]
    pub classes: Vec<Class>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WakeSettings {
    pub enabled: bool,
    pub hosts: Vec<WakeHost>,
    /// Demand must stay unmet this long before anything is woken.
    pub after_secs: u64,
    pub min_interval_secs: u64,
    pub max_per_hour: u32,
    pub host_cooldown_secs: u64,
    /// IPv4 addresses or `addr/len` networks to send the packet to.
    pub broadcast: Vec<String>,
    pub ports: Vec<u16>,
}

impl Default for WakeSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            hosts: Vec::new(),
            after_secs: 300,
            min_interval_secs: 300,
            max_per_hour: 6,
            host_cooldown_secs: 900,
            broadcast: vec!["255.255.255.255".into()],
            ports: vec![9, 7],
        }
    }
}

impl WakeSettings {
    pub fn validate(&self) -> Result<(), String> {
        if self.after_secs == 0 || self.max_per_hour == 0 || self.ports.is_empty() {
            return Err(
                "wake after_secs and max_per_hour must be positive and ports non-empty".into(),
            );
        }
        if self.enabled && self.hosts.is_empty() {
            return Err("wake is enabled but lists no hosts".into());
        }
        let mut names = BTreeSet::new();
        for h in &self.hosts {
            if parse_mac(&h.mac).is_none() {
                return Err(format!("wake host {}: bad mac", h.name));
            }
            if h.name.is_empty() || !names.insert(&h.name) {
                return Err("wake host names must be non-empty and unique".into());
            }
        }
        if self.broadcast.iter().any(|b| parse_target(b).is_none()) {
            return Err("wake broadcast entries are IPv4 addresses or addr/len".into());
        }
        Ok(())
    }

    /// Where the packet goes: every target on every port.
    pub fn destinations(&self) -> Vec<SocketAddrV4> {
        let mut out = Vec::new();
        for ip in self.broadcast.iter().filter_map(|b| parse_target(b)) {
            for &port in &self.ports {
                let d = SocketAddrV4::new(ip, port);
                if !out.contains(&d) {
                    out.push(d);
                }
            }
        }
        out
    }
}

/// One wake the planner decided on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WakeEvent {
    pub at: i64,
    pub host: String,
    pub class: Class,
    pub reason: String,
}

#[derive(Debug, Default)]
pub struct WakePlanner {
    /// Since when each class has had unmet demand without a break.
    unmet_since: BTreeMap<Class, i64>,
    last_wake: Option<i64>,
    per_host: BTreeMap<String, i64>,
    recent: VecDeque<i64>,
    attempts: BTreeMap<String, u64>,
    events: VecDeque<WakeEvent>,
}

/// What `/status` shows.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct WakeView {
    pub enabled: bool,
    /// Seconds each class has had unmet demand.
    pub unmet_secs: BTreeMap<Class, u64>,
    pub wakes_last_hour: u32,
    /// Wake attempts per host since the queen started.
    pub attempts: BTreeMap<String, u64>,
    /// The newest wakes, newest first.
    pub events: Vec<WakeEvent>,
}

impl WakePlanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decide wakes for this pass. `unmet` are classes with demand the fleet
    /// could not place; `awake` are names of drones that are fresh.
    pub fn step(
        &mut self,
        now: i64,
        s: &WakeSettings,
        unmet: &BTreeSet<Class>,
        awake: &BTreeSet<&str>,
    ) -> Vec<(usize, WakeEvent)> {
        self.unmet_since.retain(|c, _| unmet.contains(c));
        for c in unmet {
            self.unmet_since.entry(*c).or_insert(now);
        }
        while self.recent.front().is_some_and(|&t| now - t >= 3600) {
            self.recent.pop_front();
        }
        let secs = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
        let mut out: Vec<(usize, WakeEvent)> = Vec::new();
        if !s.enabled {
            return out;
        }
        // One wake per pass: the next one needs the min interval to have
        // passed, which gives the woken host time to boot and heartbeat.
        if self
            .last_wake
            .is_some_and(|t| now - t < secs(s.min_interval_secs))
            || u32::try_from(self.recent.len()).unwrap_or(u32::MAX) >= s.max_per_hour
        {
            return out;
        }
        for (&class, &since) in &self.unmet_since {
            if now - since < secs(s.after_secs) {
                continue;
            }
            let pick = s.hosts.iter().enumerate().find(|(_, h)| {
                (h.classes.is_empty() || h.classes.contains(&class))
                    && !awake.contains(h.name.as_str())
                    && self
                        .per_host
                        .get(&h.name)
                        .is_none_or(|&t| now - t >= secs(s.host_cooldown_secs))
            });
            if let Some((i, h)) = pick {
                self.last_wake = Some(now);
                self.per_host.insert(h.name.clone(), now);
                self.recent.push_back(now);
                *self.attempts.entry(h.name.clone()).or_default() += 1;
                let event = WakeEvent {
                    at: now,
                    host: h.name.clone(),
                    class,
                    reason: format!(
                        "{class} demand unmet for {}s (>= {}s)",
                        now - since,
                        s.after_secs
                    ),
                };
                self.events.push_front(event.clone());
                self.events.truncate(20);
                out.push((i, event));
                break;
            }
        }
        out
    }

    pub fn view(&self, now: i64, enabled: bool) -> WakeView {
        WakeView {
            enabled,
            unmet_secs: self
                .unmet_since
                .iter()
                .map(|(c, &t)| (*c, u64::try_from(now - t).unwrap_or(0)))
                .collect(),
            wakes_last_hour: u32::try_from(self.recent.len()).unwrap_or(u32::MAX),
            attempts: self.attempts.clone(),
            events: self.events.iter().cloned().collect(),
        }
    }
}

/// Sends magic packets. Returns the destinations it reached.
pub trait WakeSender: Send + Sync {
    fn send<'a>(
        &'a self,
        mac: [u8; 6],
        to: &'a [SocketAddrV4],
    ) -> BoxFuture<'a, Result<Vec<SocketAddrV4>, String>>;
}

/// Sends the packet over UDP with broadcast enabled.
pub struct UdpWakeSender;

impl WakeSender for UdpWakeSender {
    fn send<'a>(
        &'a self,
        mac: [u8; 6],
        to: &'a [SocketAddrV4],
    ) -> BoxFuture<'a, Result<Vec<SocketAddrV4>, String>> {
        Box::pin(async move {
            let packet = magic_packet(mac);
            let sock = tokio::net::UdpSocket::bind("0.0.0.0:0")
                .await
                .map_err(|e| e.to_string())?;
            sock.set_broadcast(true).map_err(|e| e.to_string())?;
            let mut sent = Vec::new();
            for d in to {
                if sock.send_to(&packet, d).await.is_ok() {
                    sent.push(*d);
                }
            }
            if sent.is_empty() {
                Err("no destination accepted the packet".into())
            } else {
                Ok(sent)
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(name: &str, mac: &str, classes: &[Class]) -> WakeHost {
        WakeHost {
            name: name.into(),
            mac: mac.into(),
            classes: classes.to_vec(),
        }
    }

    fn settings() -> WakeSettings {
        WakeSettings {
            enabled: true,
            hosts: vec![
                host("a", "aa:bb:cc:00:00:01", &[Class::Heavy]),
                host("b", "aa:bb:cc:00:00:02", &[]),
            ],
            after_secs: 300,
            min_interval_secs: 600,
            max_per_hour: 2,
            host_cooldown_secs: 1200,
            ..WakeSettings::default()
        }
    }

    fn unmet(c: &[Class]) -> BTreeSet<Class> {
        c.iter().copied().collect()
    }

    fn step(p: &mut WakePlanner, now: i64, u: &[Class], awake: &[&str]) -> Vec<String> {
        let awake: BTreeSet<&str> = awake.iter().copied().collect();
        p.step(now, &settings(), &unmet(u), &awake)
            .into_iter()
            .map(|(_, e)| e.host)
            .collect()
    }

    #[test]
    fn ported_packet_helpers_match_flotilla() {
        let m = parse_mac("A4:83:E7:12:34:56").unwrap();
        assert_eq!(m, [0xa4, 0x83, 0xe7, 0x12, 0x34, 0x56]);
        assert_eq!(parse_mac("a4-83-e7-12-34-56"), Some(m));
        assert_eq!(parse_mac("a483e7123456"), Some(m));
        assert!(parse_mac("nope").is_none());
        let p = magic_packet(m);
        assert_eq!(p.len(), 102);
        assert_eq!(&p[..6], &[0xff; 6]);
        assert_eq!(&p[6..12], &m);
        assert_eq!(&p[96..], &m);
    }

    #[test]
    fn broadcast_math_and_targets() {
        assert_eq!(
            directed_broadcast("192.0.2.77".parse().unwrap(), 24).to_string(),
            "192.0.2.255"
        );
        assert_eq!(
            directed_broadcast("198.51.100.7".parse().unwrap(), 16).to_string(),
            "198.51.255.255"
        );
        assert_eq!(
            directed_broadcast("198.51.100.7".parse().unwrap(), 0).to_string(),
            "255.255.255.255"
        );
        assert_eq!(
            directed_broadcast("198.51.100.7".parse().unwrap(), 32).to_string(),
            "198.51.100.7"
        );
        assert_eq!(
            parse_target("192.0.2.0/24").unwrap().to_string(),
            "192.0.2.255"
        );
        assert_eq!(
            parse_target("255.255.255.255").unwrap().to_string(),
            "255.255.255.255"
        );
        assert!(parse_target("nope").is_none() && parse_target("1.2.3.4/x").is_none());
        let s = WakeSettings {
            broadcast: vec![
                "255.255.255.255".into(),
                "192.0.2.0/24".into(),
                "192.0.2.255".into(),
            ],
            ..WakeSettings::default()
        };
        // Both spellings of the same broadcast address are sent once per port.
        let d: Vec<String> = s.destinations().iter().map(ToString::to_string).collect();
        assert_eq!(
            d,
            [
                "255.255.255.255:9",
                "255.255.255.255:7",
                "192.0.2.255:9",
                "192.0.2.255:7"
            ]
        );
    }

    #[test]
    fn demand_must_stay_unmet_for_the_whole_period() {
        let mut p = WakePlanner::new();
        assert!(step(&mut p, 0, &[Class::Medium], &[]).is_empty());
        assert!(step(&mut p, 299, &[Class::Medium], &[]).is_empty());
        // A gap resets the clock.
        assert!(step(&mut p, 300, &[], &[]).is_empty());
        assert!(step(&mut p, 310, &[Class::Medium], &[]).is_empty());
        assert!(step(&mut p, 609, &[Class::Medium], &[]).is_empty());
        assert_eq!(step(&mut p, 610, &[Class::Medium], &[]), ["b"]);
    }

    #[test]
    fn only_a_sleeping_host_that_can_serve_the_class_is_woken() {
        // Heavy demand: "a" is listed first and serves heavy.
        let mut p = WakePlanner::new();
        step(&mut p, 0, &[Class::Heavy], &[]);
        assert_eq!(step(&mut p, 300, &[Class::Heavy], &[]), ["a"]);
        // "a" cannot serve light; "b" can.
        let mut p = WakePlanner::new();
        step(&mut p, 0, &[Class::Light], &[]);
        assert_eq!(step(&mut p, 300, &[Class::Light], &[]), ["b"]);
        // If "a" is awake (fresh drone), heavy demand falls to "b".
        let mut p = WakePlanner::new();
        step(&mut p, 0, &[Class::Heavy], &["a"]);
        assert_eq!(step(&mut p, 300, &[Class::Heavy], &["a"]), ["b"]);
        // If everything that could serve it is awake, nothing is sent.
        let mut p = WakePlanner::new();
        step(&mut p, 0, &[Class::Heavy], &["a", "b"]);
        assert!(step(&mut p, 900, &[Class::Heavy], &["a", "b"]).is_empty());
    }

    #[test]
    fn the_global_interval_the_hourly_cap_and_the_host_cooldown_rate_limit_wakes() {
        let mut p = WakePlanner::new();
        step(&mut p, 0, &[Class::Light], &[]);
        assert_eq!(step(&mut p, 300, &[Class::Light], &[]), ["b"]);
        // Inside the 600s interval: nothing, even though still unmet.
        assert!(step(&mut p, 599, &[Class::Light], &[]).is_empty());
        // After it, "b" is still cooling down (1200s) and "a" cannot serve light.
        assert!(step(&mut p, 950, &[Class::Light], &[]).is_empty());
        // Cooldown over: "b" is retried (a host that never came up).
        assert_eq!(step(&mut p, 1500, &[Class::Light], &[]), ["b"]);
        // Two wakes this hour is the cap.
        assert!(step(&mut p, 3000, &[Class::Light], &[]).is_empty());
        // An hour after the first wake the window slides.
        assert_eq!(step(&mut p, 3901, &[Class::Light], &[]), ["b"]);
    }

    #[test]
    fn one_wake_per_pass_even_with_several_classes_unmet() {
        let mut p = WakePlanner::new();
        step(&mut p, 0, &[Class::Light, Class::Heavy], &[]);
        let got = step(&mut p, 300, &[Class::Light, Class::Heavy], &[]);
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn disabled_planner_tracks_but_never_wakes_and_settings_validate() {
        let mut p = WakePlanner::new();
        let off = WakeSettings {
            enabled: false,
            ..settings()
        };
        let awake = BTreeSet::new();
        assert!(p.step(0, &off, &unmet(&[Class::Light]), &awake).is_empty());
        assert!(
            p.step(10_000, &off, &unmet(&[Class::Light]), &awake)
                .is_empty()
        );

        assert!(settings().validate().is_ok());
        for bad in [
            WakeSettings {
                hosts: vec![],
                ..settings()
            },
            WakeSettings {
                hosts: vec![host("a", "zz", &[])],
                ..settings()
            },
            WakeSettings {
                hosts: vec![
                    host("a", "aa:bb:cc:00:00:01", &[]),
                    host("a", "aa:bb:cc:00:00:02", &[]),
                ],
                ..settings()
            },
            WakeSettings {
                broadcast: vec!["x".into()],
                ..settings()
            },
            WakeSettings {
                max_per_hour: 0,
                ..settings()
            },
        ] {
            assert!(bad.validate().is_err());
        }
    }

    #[tokio::test]
    async fn the_udp_sender_delivers_a_magic_packet() {
        let rx = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let std::net::SocketAddr::V4(addr) = rx.local_addr().unwrap() else {
            panic!("ipv4 expected");
        };
        let mac = parse_mac("aa:bb:cc:dd:ee:ff").unwrap();
        let sent = UdpWakeSender.send(mac, &[addr]).await.unwrap();
        assert_eq!(sent, [addr]);
        let mut buf = [0u8; 256];
        let (n, _) = rx.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], magic_packet(mac).as_slice());
    }
}
