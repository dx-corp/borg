//! Operator control of burst capacity at runtime.
//!
//! The queen config sets burst's `enabled`, `max_drones` and drone-hour
//! budgets. An operator or agent may override those four values through the
//! authenticated control API (`/v1/control/*`, see [`crate::api`]) or
//! `borg queen-ctl`, never above the ceilings in `burst.ceilings` (which
//! default to the configured values, so without them a write can only lower
//! or restore). Every write names an actor and a reason and is recorded in the
//! store's `control_audit` table with the old and new effective values.
//! Overrides are stored too, so they survive a restart, and `/status` shows
//! each effective value with its source (`config` or `override`).
//!
//! Turning burst off stops new drones; the drones it made are drained and
//! deleted once they hold no lease, as when the budget is spent.
//!
//! The control API authenticates with one bearer token read at start from
//! `control.token_path`, a file only the queen's user may read.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::burst::BurstSettings;

/// One value the control API may override.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Knob {
    Enabled,
    MaxDrones,
    DroneHoursPerDay,
    DroneHoursPerMonth,
}

impl Knob {
    pub const ALL: [Knob; 4] = [
        Knob::Enabled,
        Knob::MaxDrones,
        Knob::DroneHoursPerDay,
        Knob::DroneHoursPerMonth,
    ];

    /// The store and audit key.
    pub fn key(self) -> &'static str {
        match self {
            Knob::Enabled => "burst.enabled",
            Knob::MaxDrones => "burst.max_drones",
            Knob::DroneHoursPerDay => "burst.drone_hours_per_day",
            Knob::DroneHoursPerMonth => "burst.drone_hours_per_month",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.key() == key)
    }
}

/// Hard limits on what the control API may set, from `burst.ceilings`.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Ceilings {
    pub max_drones: u32,
    pub drone_hours_per_day: f64,
    pub drone_hours_per_month: f64,
}

/// A stored override: the value, and who set it, when and why.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OverrideRow {
    pub key: String,
    pub value: Value,
    pub set_at: i64,
    pub actor: String,
    pub reason: String,
}

/// The overrides in force, by knob. Rows with an unknown key or a value of
/// the wrong type are ignored (and reported by [`Overrides::from_rows`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Overrides(pub BTreeMap<Knob, OverrideRow>);

impl Overrides {
    /// Typed overrides from store rows, plus the keys that were ignored.
    pub fn from_rows(rows: Vec<OverrideRow>) -> (Self, Vec<String>) {
        let mut map = BTreeMap::new();
        let mut ignored = Vec::new();
        for r in rows {
            match Knob::from_key(&r.key) {
                Some(k) if typed_ok(k, &r.value) => {
                    map.insert(k, r);
                }
                _ => ignored.push(r.key),
            }
        }
        (Self(map), ignored)
    }

    fn get(&self, k: Knob) -> Option<&OverrideRow> {
        self.0.get(&k)
    }

    fn bool(&self, k: Knob) -> Option<bool> {
        self.get(k).and_then(|r| r.value.as_bool())
    }

    fn u32(&self, k: Knob) -> Option<u32> {
        self.get(k)
            .and_then(|r| r.value.as_u64())
            .and_then(|v| u32::try_from(v).ok())
    }

    fn f64(&self, k: Knob) -> Option<f64> {
        self.get(k).and_then(|r| r.value.as_f64())
    }
}

fn typed_ok(k: Knob, v: &Value) -> bool {
    match k {
        Knob::Enabled => v.is_boolean(),
        Knob::MaxDrones => v.as_u64().is_some_and(|n| u32::try_from(n).is_ok()),
        Knob::DroneHoursPerDay | Knob::DroneHoursPerMonth => {
            v.as_f64().is_some_and(|f| f.is_finite() && f > 0.0)
        }
    }
}

/// Where an effective value comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Config,
    Override,
}

/// One effective value with its source, the configured value and ceiling, and
/// who set the override.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Setting<T> {
    pub value: T,
    pub source: Source,
    pub config: T,
    pub ceiling: Option<T>,
    pub set_by: Option<String>,
    pub set_at: Option<i64>,
    pub reason: Option<String>,
}

/// The effective burst settings, as `/status` and the control API show them.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ControlView {
    pub enabled: Setting<bool>,
    pub max_drones: Setting<u32>,
    pub drone_hours_per_day: Setting<f64>,
    pub drone_hours_per_month: Setting<f64>,
}

impl ControlView {
    fn value_of(&self, k: Knob) -> (Value, Source) {
        match k {
            Knob::Enabled => (Value::from(self.enabled.value), self.enabled.source),
            Knob::MaxDrones => (Value::from(self.max_drones.value), self.max_drones.source),
            Knob::DroneHoursPerDay => (
                Value::from(self.drone_hours_per_day.value),
                self.drone_hours_per_day.source,
            ),
            Knob::DroneHoursPerMonth => (
                Value::from(self.drone_hours_per_month.value),
                self.drone_hours_per_month.source,
            ),
        }
    }
}

fn setting<T: Copy + PartialOrd>(
    ov: &Overrides,
    k: Knob,
    config: T,
    ceiling: Option<T>,
    get: impl Fn(&Overrides, Knob) -> Option<T>,
) -> Setting<T> {
    let row = ov.get(k);
    match get(ov, k) {
        Some(v) => {
            // A ceiling lowered in the config after the write still binds.
            let value = match ceiling {
                Some(c) if v > c => c,
                _ => v,
            };
            Setting {
                value,
                source: Source::Override,
                config,
                ceiling,
                set_by: row.map(|r| r.actor.clone()),
                set_at: row.map(|r| r.set_at),
                reason: row.map(|r| r.reason.clone()),
            }
        }
        None => Setting {
            value: config,
            source: Source::Config,
            config,
            ceiling,
            set_by: None,
            set_at: None,
            reason: None,
        },
    }
}

/// The effective values of `cfg` under `ov`.
pub fn view(cfg: &BurstSettings, ov: &Overrides) -> ControlView {
    let c = cfg.ceilings();
    ControlView {
        enabled: setting(ov, Knob::Enabled, cfg.enabled, None, Overrides::bool),
        max_drones: setting(
            ov,
            Knob::MaxDrones,
            cfg.max_drones,
            Some(c.max_drones),
            Overrides::u32,
        ),
        drone_hours_per_day: setting(
            ov,
            Knob::DroneHoursPerDay,
            cfg.budget.drone_hours_per_day,
            Some(c.drone_hours_per_day),
            Overrides::f64,
        ),
        drone_hours_per_month: setting(
            ov,
            Knob::DroneHoursPerMonth,
            cfg.budget.drone_hours_per_month,
            Some(c.drone_hours_per_month),
            Overrides::f64,
        ),
    }
}

/// `cfg` with the overrides applied: what the burst rule runs with.
pub fn effective(cfg: &BurstSettings, ov: &Overrides) -> BurstSettings {
    let v = view(cfg, ov);
    let mut out = cfg.clone();
    out.enabled = v.enabled.value;
    out.max_drones = v.max_drones.value;
    out.budget.drone_hours_per_day = v.drone_hours_per_day.value;
    out.budget.drone_hours_per_month = v.drone_hours_per_month.value;
    out
}

/// `POST /v1/control/burst`. Set any of the four values; `clear` drops
/// overrides so the configured value applies again.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BurstWrite {
    pub actor: String,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_drones: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drone_hours_per_day: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drone_hours_per_month: Option<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clear: Vec<Knob>,
}

/// One accepted change: the override to store (`None` drops it) and the
/// effective value before and after, for the audit log.
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub knob: Knob,
    pub set: Option<Value>,
    pub old: Value,
    pub old_source: Source,
    pub new: Value,
    pub new_source: Source,
}

/// A persisted audit entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditRow {
    pub id: i64,
    pub at: i64,
    pub actor: String,
    pub reason: String,
    pub key: String,
    pub old_value: Value,
    pub old_source: String,
    pub new_value: Value,
    pub new_source: String,
}

pub const MAX_ACTOR: usize = 64;
pub const MAX_REASON: usize = 500;

fn printable(s: &str) -> bool {
    !s.chars().any(char::is_control)
}

/// Validate `w` against `cfg`'s ceilings and the overrides in force; return
/// the changes to store. A write that sets a value to what it already is is
/// accepted and audited (it pins the value as an override).
pub fn check_write(
    cfg: &BurstSettings,
    current: &Overrides,
    w: &BurstWrite,
) -> Result<Vec<Change>, String> {
    let actor = w.actor.trim();
    if actor.is_empty() || actor.len() > MAX_ACTOR || !printable(actor) {
        return Err(format!(
            "actor is required: 1-{MAX_ACTOR} printable characters"
        ));
    }
    let reason = w.reason.trim();
    if reason.len() < 3 || reason.len() > MAX_REASON || !printable(reason) {
        return Err(format!(
            "reason is required: 3-{MAX_REASON} printable characters"
        ));
    }
    let mut sets: BTreeMap<Knob, Value> = BTreeMap::new();
    if let Some(v) = w.enabled {
        sets.insert(Knob::Enabled, Value::from(v));
    }
    if let Some(v) = w.max_drones {
        sets.insert(Knob::MaxDrones, Value::from(v));
    }
    if let Some(v) = w.drone_hours_per_day {
        sets.insert(Knob::DroneHoursPerDay, Value::from(v));
    }
    if let Some(v) = w.drone_hours_per_month {
        sets.insert(Knob::DroneHoursPerMonth, Value::from(v));
    }
    if sets.is_empty() && w.clear.is_empty() {
        return Err("nothing to change: set a value or clear one".to_owned());
    }
    if let Some(k) = w.clear.iter().find(|k| sets.contains_key(k)) {
        return Err(format!("{} is both set and cleared", k.key()));
    }
    let c = cfg.ceilings();
    if let Some(n) = w.max_drones
        && !(1..=c.max_drones).contains(&n)
    {
        return Err(format!(
            "max_drones must be between 1 and the ceiling {}",
            c.max_drones
        ));
    }
    for (name, v, ceiling) in [
        (
            "drone_hours_per_day",
            w.drone_hours_per_day,
            c.drone_hours_per_day,
        ),
        (
            "drone_hours_per_month",
            w.drone_hours_per_month,
            c.drone_hours_per_month,
        ),
    ] {
        if let Some(v) = v
            && !(v.is_finite() && v > 0.0 && v <= ceiling)
        {
            return Err(format!(
                "{name} must be above 0 and at most the ceiling {ceiling}"
            ));
        }
    }

    let before = view(cfg, current);
    let mut next = current.clone();
    for (k, v) in &sets {
        next.0.insert(
            *k,
            OverrideRow {
                key: k.key().to_owned(),
                value: v.clone(),
                set_at: 0,
                actor: actor.to_owned(),
                reason: reason.to_owned(),
            },
        );
    }
    for k in &w.clear {
        next.0.remove(k);
    }
    let after = view(cfg, &next);
    if after.drone_hours_per_day.value > after.drone_hours_per_month.value {
        return Err(format!(
            "drone_hours_per_day ({}) would exceed drone_hours_per_month ({})",
            after.drone_hours_per_day.value, after.drone_hours_per_month.value
        ));
    }
    let mut knobs: Vec<Knob> = sets
        .keys()
        .copied()
        .chain(w.clear.iter().copied())
        .collect();
    knobs.sort();
    knobs.dedup();
    Ok(knobs
        .into_iter()
        .map(|k| {
            let (old, old_source) = before.value_of(k);
            let (new, new_source) = after.value_of(k);
            Change {
                knob: k,
                set: sets.get(&k).cloned(),
                old,
                old_source,
                new,
                new_source,
            }
        })
        .collect())
}

/// Shortest control token accepted (a `borg_` token is 69 characters).
pub const MIN_TOKEN_LEN: usize = 32;

/// Read the control token from `path`: the first line, at least
/// [`MIN_TOKEN_LEN`] characters. On Unix the file must not be readable by
/// group or others.
pub fn load_token(path: &Path) -> Result<String, String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(format!(
                "{}: must not be readable by group or others (chmod 600)",
                path.display()
            ));
        }
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let token = text.lines().next().unwrap_or_default().trim().to_owned();
    if token.len() < MIN_TOKEN_LEN {
        return Err(format!(
            "{}: token must be at least {MIN_TOKEN_LEN} characters",
            path.display()
        ));
    }
    Ok(token)
}

fn utc(at: &Value) -> String {
    at.as_i64()
        .and_then(|t| time::OffsetDateTime::from_unix_timestamp(t).ok())
        .and_then(|t| {
            t.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_else(|| "-".to_owned())
}

fn plain(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "-".to_owned(),
        other => other.to_string(),
    }
}

/// Text lines for `GET /v1/control/audit` (`borg queen-ctl audit`).
pub fn render_audit(rows: &Value) -> Vec<String> {
    let rows = rows.as_array().map(Vec::as_slice).unwrap_or_default();
    if rows.is_empty() {
        return vec!["no control writes recorded".to_owned()];
    }
    rows.iter()
        .map(|r| {
            format!(
                "{}  {}  {}: {} ({}) -> {} ({})  reason: {}",
                utc(&r["at"]),
                plain(&r["actor"]),
                plain(&r["key"]),
                plain(&r["old_value"]),
                plain(&r["old_source"]),
                plain(&r["new_value"]),
                plain(&r["new_source"]),
                plain(&r["reason"]),
            )
        })
        .collect()
}

/// Text lines for `GET|POST /v1/control/burst` (`borg queen-ctl status`).
pub fn render_status(v: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let c = &v["burst_control"];
    if c.is_null() {
        out.push("burst: not configured on this queen".to_owned());
    } else {
        out.push("burst settings (effective):".to_owned());
        for k in [
            "enabled",
            "max_drones",
            "drone_hours_per_day",
            "drone_hours_per_month",
        ] {
            let s = &c[k];
            let mut line = format!(
                "  {k:<22} {:<6} source={}",
                plain(&s["value"]),
                plain(&s["source"])
            );
            if s["source"] == "override" {
                line.push_str(&format!(
                    " (config {}; set by {} at {}: {})",
                    plain(&s["config"]),
                    plain(&s["set_by"]),
                    utc(&s["set_at"]),
                    plain(&s["reason"])
                ));
            }
            if !s["ceiling"].is_null() {
                line.push_str(&format!(" ceiling={}", plain(&s["ceiling"])));
            }
            out.push(line);
        }
    }
    let b = &v["burst"];
    if b.is_null() {
        out.push("burst rule: no pass yet".to_owned());
    } else {
        out.push(format!(
            "burst rule: outcome={} ({}), dry_run={}, drones managed={} provisioning={} unmanaged={}",
            plain(&b["outcome"]),
            plain(&b["reason"]),
            plain(&b["dry_run"]),
            plain(&b["drones"]["managed"]),
            plain(&b["drones"]["provisioning"]),
            plain(&b["drones"]["unmanaged"]),
        ));
        let hours = |x: &Value| {
            x.as_f64()
                .map_or_else(|| "-".to_owned(), |h| format!("{h:.1}"))
        };
        out.push(format!(
            "spend: {}/{} drone-hours today, {}/{} this month{}",
            hours(&b["spend"]["day_hours"]),
            plain(&b["budget"]["drone_hours_per_day"]),
            hours(&b["spend"]["month_hours"]),
            plain(&b["budget"]["drone_hours_per_month"]),
            if b["over_budget"] == true {
                " (over budget)"
            } else {
                ""
            },
        ));
    }
    if let Some(cap) = v["capacity"].as_object() {
        out.push("capacity per class:".to_owned());
        for (class, c) in cap {
            out.push(format!(
                "  {class:<8} slots={} free={} unmet main={} pr={} deploy={}{}",
                plain(&c["schedulable_slots"]),
                plain(&c["free_slots"]),
                plain(&c["unmet_main"]),
                plain(&c["unmet_pr"]),
                plain(&c["unmet_deploy"]),
                if c["alarm"] == true { " STARVED" } else { "" },
            ));
        }
    }
    if let Some(rows) = v["recent_audit"].as_array()
        && !rows.is_empty()
    {
        out.push("recent control writes:".to_owned());
        out.extend(
            render_audit(&v["recent_audit"])
                .into_iter()
                .map(|l| format!("  {l}")),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg(ceilings: Option<Value>) -> BurstSettings {
        let mut b = json!({
            "enabled": true, "dry_run": true, "max_drones": 3,
            "budget": {"drone_hours_per_day": 32, "drone_hours_per_month": 150},
            "scale_up": {"classes": ["heavy"], "weights": {"main": 3, "pr": 1},
                         "min_weighted_unmet": 6, "sustained_secs": 600},
            "scale_down": {"idle_secs": 900},
            "gce": {"project": "p", "network": "n", "subnet": "s",
                    "zones": ["us-west1-b"], "machine_type": "c2d-highcpu-56"}
        });
        if let Some(c) = ceilings {
            b["ceilings"] = c;
        }
        serde_json::from_value(b).unwrap()
    }

    fn write(f: impl FnOnce(&mut BurstWrite)) -> BurstWrite {
        let mut w = BurstWrite {
            actor: "lead".into(),
            reason: "re-enable after starvation".into(),
            ..BurstWrite::default()
        };
        f(&mut w);
        w
    }

    fn apply(ov: &mut Overrides, changes: &[Change], actor: &str) {
        for c in changes {
            match &c.set {
                Some(v) => {
                    ov.0.insert(
                        c.knob,
                        OverrideRow {
                            key: c.knob.key().into(),
                            value: v.clone(),
                            set_at: 1,
                            actor: actor.into(),
                            reason: "r".into(),
                        },
                    );
                }
                None => {
                    ov.0.remove(&c.knob);
                }
            }
        }
    }

    #[test]
    fn without_overrides_the_config_applies() {
        let c = cfg(None);
        let v = view(&c, &Overrides::default());
        assert!(v.enabled.value);
        assert_eq!(v.enabled.source, Source::Config);
        assert_eq!(v.max_drones.value, 3);
        assert_eq!(v.max_drones.ceiling, Some(3));
        let e = effective(&c, &Overrides::default());
        assert!(e.enabled);
        assert_eq!(e.max_drones, 3);
    }

    #[test]
    fn turning_burst_off_is_audited_and_effective() {
        let c = cfg(None);
        let mut ov = Overrides::default();
        let changes = check_write(&c, &ov, &write(|w| w.enabled = Some(false))).unwrap();
        assert_eq!(
            changes,
            [Change {
                knob: Knob::Enabled,
                set: Some(json!(false)),
                old: json!(true),
                old_source: Source::Config,
                new: json!(false),
                new_source: Source::Override,
            }]
        );
        apply(&mut ov, &changes, "lead");
        assert!(!effective(&c, &ov).enabled);
        let v = view(&c, &ov);
        assert_eq!(v.enabled.source, Source::Override);
        assert_eq!(v.enabled.set_by.as_deref(), Some("lead"));
    }

    #[test]
    fn a_noop_write_is_accepted_and_pins_the_value() {
        let c = cfg(None);
        let changes = check_write(
            &c,
            &Overrides::default(),
            &write(|w| w.enabled = Some(true)),
        )
        .unwrap();
        assert_eq!(changes[0].old, changes[0].new);
        assert_eq!(changes[0].new_source, Source::Override);
    }

    #[test]
    fn actor_and_reason_are_required() {
        let c = cfg(None);
        let ov = Overrides::default();
        assert!(
            check_write(
                &c,
                &ov,
                &write(|w| {
                    w.actor = " ".into();
                    w.enabled = Some(false);
                })
            )
            .unwrap_err()
            .contains("actor")
        );
        assert!(
            check_write(
                &c,
                &ov,
                &write(|w| {
                    w.reason = String::new();
                    w.enabled = Some(false);
                })
            )
            .unwrap_err()
            .contains("reason")
        );
        assert!(
            check_write(&c, &ov, &write(|_| {}))
                .unwrap_err()
                .contains("nothing")
        );
    }

    #[test]
    fn values_above_the_ceiling_are_refused() {
        // Without ceilings the configured values are the ceilings.
        let c = cfg(None);
        let ov = Overrides::default();
        assert!(check_write(&c, &ov, &write(|w| w.max_drones = Some(4))).is_err());
        assert!(check_write(&c, &ov, &write(|w| w.max_drones = Some(0))).is_err());
        assert!(check_write(&c, &ov, &write(|w| w.max_drones = Some(2))).is_ok());
        assert!(check_write(&c, &ov, &write(|w| w.drone_hours_per_day = Some(33.0))).is_err());
        assert!(check_write(&c, &ov, &write(|w| w.drone_hours_per_day = Some(f64::NAN))).is_err());

        let c = cfg(Some(
            json!({"max_drones": 5, "drone_hours_per_day": 48, "drone_hours_per_month": 300}),
        ));
        assert!(check_write(&c, &ov, &write(|w| w.max_drones = Some(5))).is_ok());
        assert!(check_write(&c, &ov, &write(|w| w.max_drones = Some(6))).is_err());
        assert!(check_write(&c, &ov, &write(|w| w.drone_hours_per_day = Some(48.0))).is_ok());
    }

    #[test]
    fn day_may_not_exceed_month() {
        let c = cfg(Some(
            json!({"max_drones": 5, "drone_hours_per_day": 48, "drone_hours_per_month": 300}),
        ));
        let err = check_write(
            &c,
            &Overrides::default(),
            &write(|w| w.drone_hours_per_month = Some(20.0)),
        )
        .unwrap_err();
        assert!(err.contains("exceed"), "{err}");
    }

    #[test]
    fn clear_restores_the_configured_value() {
        let c = cfg(None);
        let mut ov = Overrides::default();
        let ch = check_write(&c, &ov, &write(|w| w.max_drones = Some(1))).unwrap();
        apply(&mut ov, &ch, "lead");
        assert_eq!(effective(&c, &ov).max_drones, 1);
        let ch = check_write(&c, &ov, &write(|w| w.clear = vec![Knob::MaxDrones])).unwrap();
        assert_eq!(ch[0].set, None);
        assert_eq!(ch[0].old, json!(1));
        assert_eq!(ch[0].new, json!(3));
        assert_eq!(ch[0].new_source, Source::Config);
        apply(&mut ov, &ch, "lead");
        assert_eq!(effective(&c, &ov).max_drones, 3);
        assert!(
            check_write(
                &c,
                &ov,
                &write(|w| {
                    w.max_drones = Some(2);
                    w.clear = vec![Knob::MaxDrones];
                })
            )
            .is_err()
        );
    }

    #[test]
    fn an_override_above_a_lowered_ceiling_is_clamped() {
        let c = cfg(None);
        let (ov, ignored) = Overrides::from_rows(vec![
            OverrideRow {
                key: "burst.max_drones".into(),
                value: json!(9),
                set_at: 1,
                actor: "a".into(),
                reason: "r".into(),
            },
            OverrideRow {
                key: "burst.unknown".into(),
                value: json!(1),
                set_at: 1,
                actor: "a".into(),
                reason: "r".into(),
            },
            OverrideRow {
                key: "burst.enabled".into(),
                value: json!("yes"),
                set_at: 1,
                actor: "a".into(),
                reason: "r".into(),
            },
        ]);
        assert_eq!(ignored, ["burst.unknown", "burst.enabled"]);
        assert_eq!(effective(&c, &ov).max_drones, 3);
        assert!(effective(&c, &ov).enabled);
    }

    #[test]
    fn unknown_fields_in_a_write_are_rejected() {
        let e = serde_json::from_value::<BurstWrite>(
            json!({"actor": "a", "reason": "rrr", "dry_run": false}),
        );
        assert!(e.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn token_file_must_be_private_and_long() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("control.token");
        std::fs::write(&p, format!("{}\n", "x".repeat(40))).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load_token(&p).unwrap_err().contains("chmod 600"));
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(load_token(&p).unwrap(), "x".repeat(40));
        std::fs::write(&p, "short\n").unwrap();
        assert!(load_token(&p).is_err());
    }

    #[test]
    fn status_text_shows_sources_spend_and_audit() {
        let v = json!({
            "burst_control": {
                "enabled": {"value": true, "source": "override", "config": true, "ceiling": null,
                            "set_by": "lead", "set_at": 0, "reason": "re-enable"},
                "max_drones": {"value": 3, "source": "config", "config": 3, "ceiling": 3},
                "drone_hours_per_day": {"value": 32.0, "source": "config", "config": 32.0, "ceiling": 32.0},
                "drone_hours_per_month": {"value": 150.0, "source": "config", "config": 150.0, "ceiling": 150.0}
            },
            "burst": {"outcome": "no_demand", "reason": "weighted unmet 0 below 6", "dry_run": false,
                      "drones": {"managed": 0, "provisioning": 0, "unmanaged": 0},
                      "spend": {"day_hours": 1.25, "month_hours": 40.0},
                      "budget": {"drone_hours_per_day": 32.0, "drone_hours_per_month": 150.0},
                      "over_budget": false},
            "capacity": {"heavy": {"schedulable_slots": 4, "free_slots": 1, "unmet_main": 0,
                                   "unmet_pr": 2, "unmet_deploy": 0, "alarm": false}},
            "recent_audit": [{"at": 0, "actor": "lead", "key": "burst.enabled", "old_value": true,
                              "old_source": "config", "new_value": true, "new_source": "override",
                              "reason": "re-enable"}]
        });
        let text = render_status(&v).join("\n");
        assert!(
            text.contains(
                "enabled                true   source=override (config true; set by lead"
            ),
            "{text}"
        );
        assert!(
            text.contains("spend: 1.2/32.0 drone-hours today, 40.0/150.0 this month"),
            "{text}"
        );
        assert!(
            text.contains("heavy    slots=4 free=1 unmet main=0 pr=2"),
            "{text}"
        );
        assert!(
            text.contains(
                "lead  burst.enabled: true (config) -> true (override)  reason: re-enable"
            ),
            "{text}"
        );
    }
}
