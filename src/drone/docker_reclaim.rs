//! The Docker tier of build-box reclaim: unused images, stopped containers,
//! build cache and dangling volumes. Policy (what may go, and why) is the
//! pure [`plan`]; [`run`] lists through the docker CLI, re-checks each item
//! immediately before removing it, removes it with a single-item command
//! (`docker image rm <id>`, `docker rm -v <id>`, `docker volume rm <name>`,
//! never `-f`, never `system prune`), and measures what was freed on the
//! filesystem holding Docker's data root.
//!
//! On 2026-11-01 borg-deploy-1 held 104 tagged platform-api images and 60 GB
//! of build cache, failed disk admission 20:40-21:25Z and stalled the prod
//! deploy lane. The only image handling before this tier ran at 85% disk
//! usage, which that 291 GB disk never reached while its free space was
//! below the 10 GiB admission floor.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;

use crate::drone::cleanup::{self, Scope};
use crate::drone::exec::Exec;
use crate::reclaim::{
    Cycle, DeletionRecord, Disk, DockerReclaimSettings, Kind, Mode, Outcome, ReclaimSettings,
};

/// Label that pins an image, container or volume against reclaim.
pub const KEEP_LABEL: &str = "borg.keep";

// `inspect` output is read as JSON, not through a `--format` template: a Go
// template aborts on a key the object lacks, and Docker 29 omits
// `Config.Labels` for unlabelled images (27 of 40 images on borg-deploy-1 on
// 2026-11-02), so the template run failed and the tier freed nothing.

#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct ConfigJson {
    labels: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct ImageMetaJson {
    last_tag_time: Option<String>,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct ImageJson {
    id: String,
    size: Option<u64>,
    created: Option<String>,
    metadata: Option<ImageMetaJson>,
    repo_tags: Option<Vec<String>>,
    repo_digests: Option<Vec<String>>,
    config: Option<ConfigJson>,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct StateJson {
    status: String,
    finished_at: Option<String>,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct ContainerJson {
    id: String,
    name: String,
    image: String,
    created: Option<String>,
    state: Option<StateJson>,
    config: Option<ConfigJson>,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct VolumeJson {
    name: String,
    created_at: Option<String>,
    labels: Option<BTreeMap<String, String>>,
}

fn config_labels(c: Option<ConfigJson>) -> BTreeMap<String, String> {
    c.and_then(|c| c.labels).unwrap_or_default()
}

/// One image as `docker image inspect` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub id: String,
    pub size_bytes: u64,
    /// Seconds since it was built or last pulled/tagged, whichever is newer.
    pub age_secs: u64,
    pub tags: Vec<String>,
    /// Repositories it belongs to (from tags, else digests); empty when
    /// dangling.
    pub repos: Vec<String>,
    pub keep: bool,
}

/// One container as `docker inspect` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Container {
    pub id: String,
    pub name: String,
    pub image_id: String,
    /// `running`, `paused`, `restarting`, `exited`, `created`, `dead`, ...
    pub state: String,
    /// Seconds since it finished (or was created, if it never ran).
    pub idle_secs: u64,
    pub labels: BTreeMap<String, String>,
}

impl Container {
    fn stopped(&self) -> bool {
        matches!(self.state.as_str(), "exited" | "created" | "dead")
    }
}

/// One dangling volume as `docker volume inspect` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Volume {
    pub name: String,
    pub age_secs: u64,
    pub labels: BTreeMap<String, String>,
}

/// What the Docker tier would remove, in order, each with its reason.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Plan {
    pub containers: Vec<(Container, String)>,
    pub images: Vec<(Image, String)>,
    pub volumes: Vec<(Volume, String)>,
    /// `builder prune` arguments `(until_secs, keep_bytes)`, when due.
    pub build_cache: Option<(u64, u64)>,
    pub kept: BTreeMap<String, u32>,
}

fn age(secs: u64) -> String {
    if secs >= 86_400 {
        format!("{}d", secs / 86_400)
    } else if secs >= 3600 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}m", secs / 60)
    }
}

fn why(mode: Mode, usage_pct: u8, what: &str, idle: u64, min: u64) -> String {
    match mode {
        Mode::Hygiene | Mode::Idle => {
            format!("{what} {} (>= {} hygiene limit)", age(idle), age(min))
        }
        _ => format!(
            "disk {usage_pct}% ({mode:?}); {what} {} (>= {})",
            age(idle),
            age(min)
        ),
    }
}

/// A volume name Docker generated: 64 lowercase hex characters.
fn anonymous(name: &str) -> bool {
    name.len() == 64
        && name
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Whether a container or volume belongs to a lease the drone is running.
fn of_held_lease(name: &str, labels: &BTreeMap<String, String>, held: &HashSet<String>) -> bool {
    held.iter()
        .any(|id| cleanup::belongs(Scope::Lease(id), name, labels))
}

fn keep(out: &mut Plan, why: &str) {
    *out.kept.entry(why.to_owned()).or_default() += 1;
}

/// Decide what the Docker tier removes in `mode`. Pure and deterministic.
pub fn plan(
    s: &ReclaimSettings,
    mode: Mode,
    usage_pct: u8,
    images: &[Image],
    containers: &[Container],
    volumes: &[Volume],
    held: &HashSet<String>,
) -> Plan {
    let d: &DockerReclaimSettings = &s.docker;
    let mut out = Plan::default();

    // Stopped containers first: removing one can release its image.
    let mut removed_containers: BTreeSet<&str> = BTreeSet::new();
    if let Some(min) = d.min_age(s, mode, d.stopped_container_max_age_secs) {
        for c in containers.iter().filter(|c| c.stopped()) {
            if c.labels.contains_key(KEEP_LABEL) {
                keep(&mut out, "container labelled borg.keep");
            } else if of_held_lease(&c.name, &c.labels, held) {
                keep(&mut out, "container of a running lease");
            } else if c.idle_secs < min {
                keep(&mut out, "container stopped recently");
            } else {
                removed_containers.insert(c.id.as_str());
                let reason = why(mode, usage_pct, "stopped", c.idle_secs, min);
                out.containers.push((c.clone(), reason));
            }
        }
    }

    // Images: never one a remaining container uses (running, paused or
    // stopped), never the newest of its repository, never a pinned one.
    let in_use: BTreeSet<&str> = containers
        .iter()
        .filter(|c| !removed_containers.contains(c.id.as_str()))
        .map(|c| c.image_id.as_str())
        .collect();
    let mut newest: BTreeMap<&str, (u64, &str)> = BTreeMap::new();
    for i in images {
        for r in &i.repos {
            let e = newest.entry(r.as_str()).or_insert((i.age_secs, &i.id));
            if (i.age_secs, i.id.as_str()) < *e {
                *e = (i.age_secs, &i.id);
            }
        }
    }
    let newest_ids: BTreeSet<&str> = newest.values().map(|(_, id)| *id).collect();
    if let Some(min) = d.min_age(s, mode, d.image_max_age_secs) {
        let mut eligible: Vec<(Image, String)> = Vec::new();
        for i in images {
            if i.keep {
                keep(&mut out, "image labelled borg.keep");
            } else if in_use.contains(i.id.as_str()) {
                keep(&mut out, "image used by a container");
            } else if newest_ids.contains(i.id.as_str()) {
                keep(&mut out, "newest image of its repository");
            } else if i.age_secs < min {
                keep(&mut out, "image pulled recently");
            } else {
                let what = if i.repos.is_empty() {
                    "dangling image, unused"
                } else {
                    "superseded image, unused"
                };
                eligible.push((i.clone(), why(mode, usage_pct, what, i.age_secs, min)));
            }
        }
        // Oldest first, then biggest; the cap applies in `run`.
        eligible.sort_by(|(a, _), (b, _)| {
            b.age_secs
                .cmp(&a.age_secs)
                .then(b.size_bytes.cmp(&a.size_bytes))
                .then(a.id.cmp(&b.id))
        });
        out.images = eligible;
    }

    if d.prune_dangling_volumes
        && let Some(min) = d.min_age(s, mode, d.volume_max_age_secs)
    {
        for v in volumes {
            let ours = cleanup::belongs(Scope::AnyLease, &v.name, &v.labels);
            if v.labels.contains_key(KEEP_LABEL) {
                keep(&mut out, "volume labelled borg.keep");
            } else if !(anonymous(&v.name) || ours) {
                keep(&mut out, "named volume");
            } else if of_held_lease(&v.name, &v.labels, held) {
                keep(&mut out, "volume of a running lease");
            } else if v.age_secs < min {
                keep(&mut out, "volume created recently");
            } else {
                let reason = why(mode, usage_pct, "dangling volume", v.age_secs, min);
                out.volumes.push((v.clone(), reason));
            }
        }
    }

    if let Some(min) = d.min_age(s, mode, d.build_cache_max_age_secs) {
        let keep_bytes = if mode == Mode::Critical {
            0
        } else {
            d.build_cache_keep_bytes
        };
        out.build_cache = Some((min, keep_bytes));
    }
    out
}

/// Parse an RFC 3339 time Docker printed into unix seconds. Docker prints
/// `0001-01-01T00:00:00Z` for "never", which is `None`.
fn unix(s: &str) -> Option<i64> {
    let t = time::OffsetDateTime::parse(s.trim(), &time::format_description::well_known::Rfc3339)
        .ok()?
        .unix_timestamp();
    (t > 0).then_some(t)
}

fn since(now: i64, at: Option<i64>) -> u64 {
    at.map_or(0, |t| u64::try_from(now.saturating_sub(t)).unwrap_or(0))
}

/// `registry:5000/team/app:tag` -> `registry:5000/team/app`;
/// `app@sha256:...` -> `app`.
fn repository(reference: &str) -> &str {
    if let Some((repo, _)) = reference.split_once('@') {
        return repo;
    }
    match reference.rsplit_once(':') {
        Some((repo, tag)) if !tag.contains('/') => repo,
        _ => reference,
    }
}

fn parse_image(j: ImageJson, now: i64) -> Option<Image> {
    if j.id.is_empty() {
        return None;
    }
    let tags: Vec<String> = j
        .repo_tags
        .unwrap_or_default()
        .into_iter()
        .filter(|t| t != "<none>:<none>")
        .collect();
    let digests = j.repo_digests.unwrap_or_default();
    let refs = if tags.is_empty() { &digests } else { &tags };
    let mut repos: Vec<String> = refs.iter().map(|r| repository(r).to_owned()).collect();
    repos.sort();
    repos.dedup();
    let tagged = j.metadata.and_then(|m| m.last_tag_time);
    let newest = j
        .created
        .as_deref()
        .and_then(unix)
        .max(tagged.as_deref().and_then(unix));
    Some(Image {
        id: j.id,
        // An image Docker reports no size for counts as free to remove.
        size_bytes: j.size.unwrap_or(0),
        age_secs: since(now, newest),
        tags,
        repos,
        keep: config_labels(j.config).contains_key(KEEP_LABEL),
    })
}

fn parse_container(j: ContainerJson, now: i64) -> Option<Container> {
    if j.id.is_empty() {
        return None;
    }
    let state = j.state.unwrap_or_default();
    // A container whose state Docker did not report is treated as running,
    // which keeps it and its image.
    let status = if state.status.is_empty() {
        "running".to_owned()
    } else {
        state.status
    };
    let at = state
        .finished_at
        .as_deref()
        .and_then(unix)
        .or_else(|| j.created.as_deref().and_then(unix));
    Some(Container {
        id: j.id,
        name: j.name.trim_start_matches('/').to_owned(),
        image_id: j.image,
        state: status,
        idle_secs: since(now, at),
        labels: config_labels(j.config),
    })
}

fn parse_volume(j: VolumeJson, now: i64) -> Option<Volume> {
    if j.name.is_empty() {
        return None;
    }
    Some(Volume {
        name: j.name,
        // An unparsable creation time reads as brand new, which keeps it.
        age_secs: since(now, j.created_at.as_deref().and_then(unix)),
        labels: j.labels.unwrap_or_default(),
    })
}

/// `docker builder prune` prints the space it freed as `Total: 1.2GB` (new
/// buildx) or `Total reclaimed space: 1.2GB` (old). Docker's sizes are
/// decimal.
fn parse_freed(out: &str) -> u64 {
    let Some(v) = out
        .lines()
        .filter(|l| l.trim_start().starts_with("Total"))
        .filter_map(|l| l.split_whitespace().last())
        .next_back()
    else {
        return 0;
    };
    let split = v
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(v.len());
    let (num, unit) = v.split_at(split);
    let Ok(n) = num.parse::<f64>() else { return 0 };
    let mult: f64 = match unit.to_ascii_uppercase().as_str() {
        "" | "B" => 1.0,
        "KB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        "TB" => 1e12,
        _ => return 0,
    };
    let bytes = (n * mult).round();
    if bytes.is_finite() && bytes >= 0.0 && bytes < u64::MAX as f64 {
        bytes as u64
    } else {
        0
    }
}

/// The docker CLI and the disk under Docker's data root.
pub struct DockerCli<'a> {
    pub exec: &'a dyn Exec,
    pub docker: &'a str,
}

impl DockerCli<'_> {
    fn run(&self, args: &[&str]) -> Result<String, String> {
        match self.exec.run(self.docker, args) {
            Ok(o) if o.success => Ok(o.stdout),
            Ok(_) => Err(format!("`docker {}` failed", args.join(" "))),
            Err(e) => Err(format!("{}: {e}", self.docker)),
        }
    }

    fn ids(&self, args: &[&str]) -> Result<Vec<String>, String> {
        let mut ids: Vec<String> = self
            .run(args)?
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect();
        ids.sort();
        ids.dedup();
        Ok(ids)
    }

    /// `inspect` every id in one call and read Docker's JSON array; an
    /// object that does not parse is dropped (the item stays).
    fn inspect<J: serde::de::DeserializeOwned, T>(
        &self,
        cmd: &[&str],
        ids: &[String],
        parse: impl Fn(J) -> Option<T>,
    ) -> Result<Vec<T>, String> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut args: Vec<&str> = cmd.to_vec();
        args.extend(ids.iter().map(String::as_str));
        let out = self
            .exec
            .run(self.docker, &args)
            .map_err(|e| format!("{}: {e}", self.docker))?;
        // Docker exits non-zero when any id vanished since the listing (jobs
        // pull, prune and remove concurrently) yet still prints the others.
        // A vanished item is simply not a candidate; only a run that printed
        // nothing usable is a failure.
        let items: Vec<serde_json::Value> = match serde_json::from_str(out.stdout.trim()) {
            Ok(items) => items,
            Err(e) if out.success => {
                return Err(format!(
                    "`docker {}`: unreadable output: {e}",
                    cmd.join(" ")
                ));
            }
            Err(_) => return Err(format!("`docker {}` failed", cmd.join(" "))),
        };
        Ok(items
            .into_iter()
            .filter_map(|v| serde_json::from_value::<J>(v).ok())
            .filter_map(parse)
            .collect())
    }

    pub fn images(&self, now: i64) -> Result<Vec<Image>, String> {
        let ids = self.ids(&["image", "ls", "-q", "--no-trunc"])?;
        self.inspect(&["image", "inspect"], &ids, |j| parse_image(j, now))
    }

    pub fn containers(&self, now: i64) -> Result<Vec<Container>, String> {
        let ids = self.ids(&["ps", "-aq", "--no-trunc"])?;
        self.inspect(&["container", "inspect"], &ids, |j| parse_container(j, now))
    }

    pub fn dangling_volumes(&self, now: i64) -> Result<Vec<Volume>, String> {
        let ids = self.ids(&["volume", "ls", "-q", "--filter", "dangling=true"])?;
        self.inspect(&["volume", "inspect"], &ids, |j| parse_volume(j, now))
    }

    /// Docker's data root (`/var/lib/docker` unless configured otherwise).
    pub fn root_dir(&self) -> String {
        self.run(&["info", "--format", "{{.DockerRootDir}}"])
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| s.starts_with('/'))
            .unwrap_or_else(|| "/var/lib/docker".to_owned())
    }

    /// The flag this buildx takes for "cache to keep regardless of age":
    /// `--reserved-space` (buildx 0.17+; `--keep-storage` is deprecated
    /// there) or `--keep-storage` (older).
    fn keep_flag(&self) -> Option<&'static str> {
        let help = self.run(&["builder", "prune", "--help"]).ok()?;
        if help.contains("--reserved-space") {
            Some("--reserved-space")
        } else if help.contains("--keep-storage") {
            Some("--keep-storage")
        } else {
            None
        }
    }

    /// Fresh state of one container immediately before removal.
    fn container_now(&self, id: &str, now: i64) -> Option<Container> {
        self.inspect(&["container", "inspect"], &[id.to_owned()], |j| {
            parse_container(j, now)
        })
        .ok()?
        .into_iter()
        .next()
    }

    /// Whether any container (in any state) uses image `id` right now.
    fn image_in_use(&self, id: &str) -> Result<bool, String> {
        let filter = format!("ancestor={id}");
        Ok(!self
            .ids(&["ps", "-aq", "--no-trunc", "--filter", &filter])?
            .is_empty())
    }
}

/// What a Docker tier run needs from the drone.
pub struct Inputs<'a> {
    pub settings: &'a ReclaimSettings,
    /// Disk of the filesystem holding Docker's data root, before the run.
    pub disk: Disk,
    /// The queen's admission floor (0 when unknown).
    pub floor_bytes: u64,
    /// Leases the drone is running now.
    pub held: &'a HashSet<String>,
    pub now: i64,
}

/// Run the Docker tier. `measure` re-reads the disk after the removals; the
/// cycle's `reclaimed_bytes` is the free space it gained (never negative),
/// because image sizes overlap through shared layers and only the
/// filesystem knows what was really released. `seq` numbers the records.
pub fn run(
    cli: &DockerCli<'_>,
    input: &Inputs<'_>,
    measure: impl Fn() -> Option<Disk>,
    seq: &mut u64,
) -> Result<Cycle, String> {
    let s = input.settings;
    let before = input.disk.usage_pct();
    let mode = crate::reclaim::mode_for_floor(s, &input.disk, input.floor_bytes);
    let mut cycle = Cycle {
        mode,
        usage_pct_before: before,
        usage_pct_after: before,
        records: Vec::new(),
        kept: BTreeMap::new(),
        reclaimed_bytes: 0,
        would_reclaim_bytes: 0,
        errors: 0,
    };
    let containers = cli.containers(input.now)?;
    let images = cli.images(input.now)?;
    let volumes = if s.docker.prune_dangling_volumes {
        cli.dangling_volumes(input.now)?
    } else {
        Vec::new()
    };
    let p = plan(s, mode, before, &images, &containers, &volumes, input.held);
    cycle.kept = p.kept.clone();
    let mut record = |cycle: &mut Cycle, path: String, kind: Kind, size: u64, reason: &str| {
        *seq += 1;
        cycle.records.push(DeletionRecord {
            seq: *seq,
            at: input.now,
            path,
            kind,
            size_bytes: size,
            reason: reason.to_owned(),
            outcome: Outcome::WouldDelete,
            error: None,
        });
    };
    let mut removals = 0u32;
    let cap = s.max_deletions_per_cycle;
    let finish = |cycle: &mut Cycle, ok: Result<(), String>, est: u64, refused_as: &str| {
        let Some(rec) = cycle.records.last_mut() else {
            return;
        };
        match ok {
            _ if s.dry_run => cycle.would_reclaim_bytes += est,
            Ok(()) => rec.outcome = Outcome::Deleted,
            Err(e) => {
                rec.outcome = Outcome::Failed;
                rec.error = Some(e);
                // Docker refuses removals that would break something (an
                // image a container started using since the listing, a
                // volume that got mounted): that is a keep, not an error.
                *cycle.kept.entry(refused_as.to_owned()).or_default() += 1;
            }
        }
    };

    for (c, reason) in &p.containers {
        if removals >= cap {
            break;
        }
        // Re-check: still stopped, still not a running lease's, still not
        // pinned.
        match cli.container_now(&c.id, input.now) {
            Some(now)
                if now.stopped()
                    && !now.labels.contains_key(KEEP_LABEL)
                    && !of_held_lease(&now.name, &now.labels, input.held) => {}
            _ => {
                *cycle
                    .kept
                    .entry("container changed since listing".into())
                    .or_default() += 1;
                continue;
            }
        }
        record(
            &mut cycle,
            format!("container {} ({})", short(&c.id), c.name),
            Kind::DockerContainer,
            0,
            reason,
        );
        let ok = if s.dry_run {
            Ok(())
        } else {
            cli.run(&["rm", "-v", &c.id]).map(drop)
        };
        finish(&mut cycle, ok, 0, "container refused by docker");
        removals += 1;
    }

    for (i, reason) in &p.images {
        if removals >= cap {
            break;
        }
        // Re-check against containers created since the listing (a dry run
        // keeps the plan's view: the containers it would remove still exist).
        if !s.dry_run {
            match cli.image_in_use(&i.id) {
                Ok(false) => {}
                Ok(true) => {
                    *cycle
                        .kept
                        .entry("image used by a container".into())
                        .or_default() += 1;
                    continue;
                }
                Err(e) => {
                    *cycle.kept.entry(format!("unknown: {e}")).or_default() += 1;
                    continue;
                }
            }
        }
        let label = if i.tags.is_empty() {
            format!("image {}", short(&i.id))
        } else {
            format!("image {} ({})", short(&i.id), i.tags.join(", "))
        };
        record(&mut cycle, label, Kind::DockerImage, i.size_bytes, reason);
        let ok = if s.dry_run {
            Ok(())
        } else if i.tags.len() <= 1 {
            cli.run(&["image", "rm", &i.id]).map(drop)
        } else {
            // By id Docker refuses an image with several tags unless forced;
            // untag each instead (the last one removes it, and Docker still
            // refuses if a container uses it).
            i.tags
                .iter()
                .try_for_each(|t| cli.run(&["image", "rm", t]).map(drop))
        };
        finish(&mut cycle, ok, i.size_bytes, "image refused by docker");
        removals += 1;
    }

    for (v, reason) in &p.volumes {
        if removals >= cap {
            break;
        }
        record(
            &mut cycle,
            format!("volume {}", v.name),
            Kind::DockerVolume,
            0,
            reason,
        );
        // `volume rm` without -f refuses a volume a container mounts.
        let ok = if s.dry_run {
            Ok(())
        } else {
            cli.run(&["volume", "rm", &v.name]).map(drop)
        };
        finish(&mut cycle, ok, 0, "volume refused by docker");
        removals += 1;
    }

    if let Some((until, keep_bytes)) = p.build_cache {
        let until_arg = format!("until={until}s");
        let keep = keep_bytes.to_string();
        let mut args: Vec<&str> = vec!["builder", "prune", "-f", "--filter", &until_arg];
        if let Some(flag) = cli.keep_flag() {
            args.extend([flag, keep.as_str()]);
        }
        let reason = format!(
            "build cache unused >= {} ({mode:?}), keeping {} MiB",
            age(until),
            keep_bytes >> 20
        );
        if s.dry_run {
            record(
                &mut cycle,
                "build cache".into(),
                Kind::DockerBuildCache,
                0,
                &reason,
            );
        } else {
            match cli.run(&args) {
                Ok(out) => {
                    let freed = parse_freed(&out);
                    if freed > 0 {
                        record(
                            &mut cycle,
                            "build cache".into(),
                            Kind::DockerBuildCache,
                            freed,
                            &reason,
                        );
                        if let Some(r) = cycle.records.last_mut() {
                            r.outcome = Outcome::Deleted;
                        }
                    }
                }
                Err(e) => {
                    record(
                        &mut cycle,
                        "build cache".into(),
                        Kind::DockerBuildCache,
                        0,
                        &reason,
                    );
                    if let Some(r) = cycle.records.last_mut() {
                        r.outcome = Outcome::Failed;
                        r.error = Some(e);
                    }
                    cycle.errors += 1;
                }
            }
        }
    }

    if !s.dry_run
        && let Some(after) = measure()
    {
        cycle.reclaimed_bytes = after.free_bytes.saturating_sub(input.disk.free_bytes);
        cycle.usage_pct_after = after.usage_pct();
    }
    Ok(cycle)
}

fn short(id: &str) -> &str {
    let id = id.strip_prefix("sha256:").unwrap_or(id);
    id.get(..12).unwrap_or(id)
}

/// Whether Docker is usable here (a host without it skips the tier).
pub fn available(cli: &DockerCli<'_>) -> bool {
    cli.run(&["version", "--format", "{{.Server.Version}}"])
        .is_ok()
}

/// The filesystem path whose `df` measures Docker's data.
pub fn root_path(cli: &DockerCli<'_>) -> std::path::PathBuf {
    Path::new(&cli.root_dir()).to_path_buf()
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::drone::cleanup::testing::FakeExec;

    const H: u64 = 3600;
    const GB: u64 = 1_000_000_000;

    fn img(id: &str, tags: &[&str], age_h: u64) -> Image {
        let tags: Vec<String> = tags.iter().map(|t| (*t).to_owned()).collect();
        let mut repos: Vec<String> = tags.iter().map(|t| repository(t).to_owned()).collect();
        repos.sort();
        repos.dedup();
        Image {
            id: id.to_owned(),
            size_bytes: GB,
            age_secs: age_h * H,
            tags,
            repos,
            keep: false,
        }
    }

    fn ctr(id: &str, image: &str, state: &str, idle_h: u64) -> Container {
        Container {
            id: id.to_owned(),
            name: format!("job-{id}"),
            image_id: image.to_owned(),
            state: state.to_owned(),
            idle_secs: idle_h * H,
            labels: BTreeMap::new(),
        }
    }

    fn vol(name: &str, age_h: u64, labels: &[(&str, &str)]) -> Volume {
        Volume {
            name: name.to_owned(),
            age_secs: age_h * H,
            labels: labels
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        }
    }

    fn ids<T>(v: &[(T, String)], f: impl Fn(&T) -> &str) -> Vec<String> {
        v.iter().map(|(x, _)| f(x).to_owned()).collect()
    }

    const API: &str = "ghcr.io/dx-corp/platform/platform-api";

    #[test]
    fn images_in_use_newest_per_repository_and_pinned_ones_are_kept() {
        let mut pinned = img("sha256:pin", &["tools:old"], 500);
        pinned.keep = true;
        let images = vec![
            img("sha256:a3", &[&format!("{API}:c3")], 30),
            img("sha256:a2", &[&format!("{API}:c2")], 40),
            img("sha256:a1", &[&format!("{API}:c1")], 50),
            img("sha256:run", &[&format!("{API}:c0")], 60),
            img("sha256:paused", &[&format!("{API}:cp")], 61),
            img("sha256:stopped", &[&format!("{API}:cs")], 62),
            img("sha256:dangling", &[], 70),
            img("sha256:only", &["postgres:16"], 900),
            img("sha256:fresh", &[&format!("{API}:c4")], 2),
            pinned,
        ];
        let containers = vec![
            ctr("c-run", "sha256:run", "running", 0),
            ctr("c-paused", "sha256:paused", "paused", 0),
            // Stopped 2h ago: below the 6h hygiene age, so it and its image stay.
            ctr("c-stopped", "sha256:stopped", "exited", 2),
        ];
        let s = ReclaimSettings::default();
        let p = plan(
            &s,
            Mode::Hygiene,
            50,
            &images,
            &containers,
            &[],
            &HashSet::new(),
        );
        // Oldest first; the newest platform-api (2h, below the 24h hygiene
        // age anyway) and the only postgres image stay.
        assert_eq!(
            ids(&p.images, |i| &i.id),
            ["sha256:dangling", "sha256:a1", "sha256:a2", "sha256:a3"]
        );
        assert_eq!(p.kept["image used by a container"], 3);
        assert_eq!(p.kept["newest image of its repository"], 2);
        assert_eq!(p.kept["image labelled borg.keep"], 1);
        assert!(p.containers.is_empty());
        assert_eq!(p.kept["container stopped recently"], 1);
        assert!(p.images[0].1.contains("dangling"), "{}", p.images[0].1);
    }

    #[test]
    fn a_stopped_container_due_for_removal_releases_its_image() {
        let images = vec![
            img("sha256:new", &["app:2"], 30),
            img("sha256:old", &["app:1"], 40),
        ];
        let containers = vec![ctr("c1", "sha256:old", "exited", 10)];
        let p = plan(
            &ReclaimSettings::default(),
            Mode::Hygiene,
            50,
            &images,
            &containers,
            &[],
            &HashSet::new(),
        );
        assert_eq!(ids(&p.containers, |c| &c.id), ["c1"]);
        assert_eq!(ids(&p.images, |i| &i.id), ["sha256:old"]);
    }

    #[test]
    fn thresholds_tighten_with_pressure_and_hygiene_zero_waits_for_pressure() {
        let images = vec![
            img("sha256:n", &["app:2"], 0),
            img("sha256:o", &["app:1"], 3),
        ];
        let s = ReclaimSettings::default();
        let at = |mode, s: &ReclaimSettings| {
            ids(
                &plan(s, mode, 0, &images, &[], &[], &HashSet::new()).images,
                |i| &i.id,
            )
        };
        // 3h old: too young for hygiene (24h) and pressure (6h), not critical (1h).
        assert!(at(Mode::Hygiene, &s).is_empty());
        assert!(at(Mode::Pressure, &s).is_empty());
        assert_eq!(at(Mode::Critical, &s), ["sha256:o"]);
        // Hygiene off for images: still nothing without pressure.
        let mut off = s.clone();
        off.docker.image_max_age_secs = 0;
        let old = vec![
            img("sha256:n", &["app:2"], 0),
            img("sha256:o", &["app:1"], 900),
        ];
        let p = plan(&off, Mode::Hygiene, 0, &old, &[], &[], &HashSet::new());
        assert!(p.images.is_empty());
        assert_eq!(
            plan(&off, Mode::Pressure, 0, &old, &[], &[], &HashSet::new())
                .images
                .len(),
            1
        );
        // Build cache: hygiene age normally, critical age and no reserve when critical.
        assert_eq!(
            plan(&s, Mode::Hygiene, 0, &[], &[], &[], &HashSet::new()).build_cache,
            Some((24 * H, 10 << 30))
        );
        assert_eq!(
            plan(&s, Mode::Critical, 0, &[], &[], &[], &HashSet::new()).build_cache,
            Some((H, 0))
        );
    }

    #[test]
    fn containers_of_running_leases_and_pinned_ones_stay() {
        let lease = "0123456789ab";
        let mut of_lease = ctr("c-lease", "i", "exited", 50);
        of_lease.name = format!("borg-{lease}-db-1");
        let mut pinned = ctr("c-pin", "i", "exited", 50);
        pinned.labels.insert(KEEP_LABEL.into(), "1".into());
        let old = ctr("c-old", "i", "dead", 50);
        let held = HashSet::from([lease.to_owned()]);
        let p = plan(
            &ReclaimSettings::default(),
            Mode::Hygiene,
            0,
            &[],
            &[of_lease, pinned, old],
            &[],
            &held,
        );
        assert_eq!(ids(&p.containers, |c| &c.id), ["c-old"]);
        assert_eq!(p.kept["container of a running lease"], 1);
        assert_eq!(p.kept["container labelled borg.keep"], 1);
    }

    #[test]
    fn only_anonymous_or_lease_created_dangling_volumes_go() {
        let anon = "a".repeat(64);
        let held = HashSet::from(["fedcba987654".to_owned()]);
        let volumes = vec![
            vol(&anon, 48, &[]),
            vol("postgres-data", 900, &[]),
            vol("borg-0123456789ab_data", 48, &[]),
            vol("borg-fedcba987654_data", 48, &[]),
            vol(&"b".repeat(64), 1, &[]),
            vol(&"c".repeat(64), 900, &[(KEEP_LABEL, "")]),
        ];
        let s = ReclaimSettings::default();
        let p = plan(&s, Mode::Hygiene, 0, &[], &[], &volumes, &held);
        assert_eq!(
            ids(&p.volumes, |v| &v.name),
            [anon.clone(), "borg-0123456789ab_data".to_owned()]
        );
        assert_eq!(p.kept["named volume"], 1);
        assert_eq!(p.kept["volume of a running lease"], 1);
        assert_eq!(p.kept["volume created recently"], 1);
        assert_eq!(p.kept["volume labelled borg.keep"], 1);
        let mut off = s.clone();
        off.docker.prune_dangling_volumes = false;
        assert!(
            plan(&off, Mode::Critical, 0, &[], &[], &volumes, &held)
                .volumes
                .is_empty()
        );
    }

    #[test]
    fn inspect_json_parses_including_docker_29_omissions_and_registry_ports() {
        use serde_json::{from_value, json};
        let now = unix("2026-11-01T22:00:00Z").unwrap();
        let i = parse_image(
            from_value(json!({
                "Id": "sha256:aa", "Size": 620000000,
                "Created": "2026-10-30T22:00:00.123456789Z",
                "Metadata": {"LastTagTime": "2026-11-01T20:00:00.5Z"},
                "RepoTags": ["registry:5000/team/app:v1"], "RepoDigests": [],
                "Config": {"Labels": {"borg.keep": ""}}
            }))
            .unwrap(),
            now,
        )
        .unwrap();
        // Last pulled 2h ago, though built two days ago.
        assert_eq!(
            (i.age_secs, i.size_bytes, i.keep),
            (2 * H, 620_000_000, true)
        );
        assert_eq!(i.repos, ["registry:5000/team/app"]);
        // Docker 29 on borg-deploy-1: no Config.Labels, no Metadata, null tags.
        let d = parse_image(
            from_value(json!({
                "Id": "sha256:bb", "Size": 1, "Created": "2026-10-01T00:00:00Z",
                "RepoTags": null, "RepoDigests": ["app@sha256:cc"], "Config": {"Env": []}
            }))
            .unwrap(),
            now,
        )
        .unwrap();
        assert_eq!(
            (d.repos.as_slice(), d.tags.is_empty(), d.keep),
            (&["app".to_owned()][..], true, false)
        );
        let c = parse_container(
            from_value(json!({
                "Id": "c1", "Name": "/svc", "Image": "sha256:aa",
                "Created": "2026-11-01T10:00:00Z",
                "State": {"Status": "exited", "FinishedAt": "0001-01-01T00:00:00Z"},
                "Config": {}
            }))
            .unwrap(),
            now,
        )
        .unwrap();
        // Never finished: idle since creation.
        assert_eq!((c.name.as_str(), c.idle_secs), ("svc", 12 * H));
        assert!(c.labels.is_empty());
        // No state reported: treated as running, so it and its image stay.
        let unknown =
            parse_container(from_value(json!({"Id": "c2", "Image": "i"})).unwrap(), now).unwrap();
        assert_eq!(unknown.state, "running");
        assert!(parse_image(ImageJson::default(), now).is_none());
        assert_eq!(parse_freed("ID\nabc\nTotal:\t60.19GB\n"), 60_190_000_000);
        assert_eq!(parse_freed("Total reclaimed space: 512kB"), 512_000);
        assert_eq!(parse_freed("nothing"), 0);
    }

    /// A fake docker CLI with a fixed inventory: two superseded platform-api
    /// images, the newest one, an image a running container uses, an old
    /// stopped container and an anonymous dangling volume.
    fn fake_docker(buildx_help: &str) -> FakeExec {
        use serde_json::json;
        let x = FakeExec::default();
        x.on("docker version --format {{.Server.Version}}", "29.1.3");
        x.on("docker ps -aq --no-trunc", "crun\ncold");
        let cold = json!({
            "Id": "cold", "Name": "/old", "Image": "sha256:gone",
            "Created": "2026-10-01T00:00:00Z",
            "State": {"Status": "exited", "FinishedAt": "2026-10-02T00:00:00Z"},
            "Config": {}
        });
        let crun = json!({
            "Id": "crun", "Name": "/svc", "Image": "sha256:busy",
            "Created": "2026-11-01T00:00:00Z",
            "State": {"Status": "running", "FinishedAt": "0001-01-01T00:00:00Z"},
            "Config": {"Labels": null}
        });
        x.on(
            "docker container inspect cold crun",
            &json!([cold.clone(), crun]).to_string(),
        );
        x.on("docker container inspect cold", &json!([cold]).to_string());
        x.on(
            "docker image ls -q --no-trunc",
            "sha256:a1\nsha256:a2\nsha256:a3\nsha256:busy\nsha256:multi",
        );
        // As Docker 29 prints them: no Config.Labels key, no Metadata.
        let image = |id: &str, created: &str, tags: Vec<String>| {
            json!({"Id": id, "Size": 620000000, "Created": created,
                   "RepoTags": tags, "RepoDigests": [], "Config": {"Env": []}})
        };
        x.on(
            "docker image inspect sha256:a1 sha256:a2 sha256:a3 sha256:busy sha256:multi",
            &json!([
                image(
                    "sha256:a1",
                    "2026-10-20T00:00:00Z",
                    vec![format!("{API}:c1")]
                ),
                image(
                    "sha256:a2",
                    "2026-10-25T00:00:00Z",
                    vec![format!("{API}:c2")]
                ),
                image(
                    "sha256:a3",
                    "2026-11-01T00:00:00Z",
                    vec![format!("{API}:c3")]
                ),
                image(
                    "sha256:busy",
                    "2026-10-01T00:00:00Z",
                    vec!["svc:1".to_owned()]
                ),
                image(
                    "sha256:multi",
                    "2026-10-21T00:00:00Z",
                    vec![format!("{API}:x"), format!("{API}:y")]
                ),
            ])
            .to_string(),
        );
        let anon = "d".repeat(64);
        x.on("docker volume ls -q --filter dangling=true", &anon);
        x.on(
            &format!("docker volume inspect {anon}"),
            &json!([{"Name": anon, "CreatedAt": "2026-10-01T00:00:00Z", "Labels": null}])
                .to_string(),
        );
        x.on("docker builder prune --help", buildx_help);
        x.on(
            "docker builder prune -f --filter until=86400s --reserved-space 10737418240",
            "Total:\t60.19GB",
        );
        x
    }

    fn input<'a>(
        s: &'a ReclaimSettings,
        held: &'a HashSet<String>,
        free_gb: u64,
        floor: u64,
    ) -> Inputs<'a> {
        Inputs {
            settings: s,
            disk: Disk {
                total_bytes: 291 * GB,
                free_bytes: free_gb * GB,
            },
            floor_bytes: floor,
            held,
            now: unix("2026-11-01T22:00:00Z").unwrap(),
        }
    }

    #[test]
    fn a_run_removes_one_item_per_command_never_forces_and_measures_the_disk() {
        let x = fake_docker("  --max-used-space bytes\n  --reserved-space bytes\n");
        let cli = DockerCli {
            exec: &x,
            docker: "docker",
        };
        let s = ReclaimSettings::default();
        let held = HashSet::new();
        // 62% used: hygiene, as on borg-deploy-1 after the band-aid prune.
        let measured = Mutex::new(0u32);
        let c = run(
            &cli,
            &input(&s, &held, 110, 10 << 30),
            || {
                *measured.lock().unwrap() += 1;
                Some(Disk {
                    total_bytes: 291 * GB,
                    free_bytes: 172 * GB,
                })
            },
            &mut 0,
        )
        .unwrap();
        assert_eq!(c.mode, Mode::Hygiene);
        let calls = x.calls();
        let removals: Vec<&String> = calls
            .iter()
            .filter(|c| c.contains(" rm ") || c.contains("prune -f"))
            .collect();
        assert_eq!(
            removals,
            [
                "docker rm -v cold",
                "docker image rm sha256:a1",
                &format!("docker image rm {API}:x"),
                &format!("docker image rm {API}:y"),
                "docker image rm sha256:a2",
                &format!("docker volume rm {}", "d".repeat(64)),
                "docker builder prune -f --filter until=86400s --reserved-space 10737418240",
            ]
        );
        assert!(
            !calls
                .iter()
                .any(|c| (c.contains(" -f ") && !c.contains("builder prune"))
                    || c.contains("system prune")
                    || c.contains("--force")
                    || c.contains(" -a ")),
            "{calls:?}"
        );
        // Every image was re-checked against the containers right before removal.
        assert!(calls.contains(&"docker ps -aq --no-trunc --filter ancestor=sha256:a1".to_owned()));
        // The newest platform-api and the running service's image stay.
        assert!(
            !calls
                .iter()
                .any(|c| c.contains("rm sha256:a3") || c.contains("rm sha256:busy"))
        );
        // Freed bytes come from the filesystem, not the overlapping image sizes.
        assert_eq!((c.reclaimed_bytes, *measured.lock().unwrap()), (62 * GB, 1));
        assert!(
            c.records.iter().all(|r| r.outcome == Outcome::Deleted),
            "{:?}",
            c.records
        );
        assert!(c.records.iter().all(|r| !r.kind.is_wire_v1()));
        assert_eq!(c.errors, 0);
    }

    #[test]
    fn below_the_admission_floor_is_critical_and_old_buildx_gets_keep_storage() {
        let x = fake_docker("  --keep-storage bytes   Amount of disk space to keep for cache\n");
        x.on(
            "docker builder prune -f --filter until=3600s --keep-storage 0",
            "Total reclaimed space: 1GB",
        );
        let cli = DockerCli {
            exec: &x,
            docker: "docker",
        };
        let s = ReclaimSettings::default();
        let held = HashSet::new();
        // 291 GB disk, 8 GB free: 97% used, and below the 10 GiB floor.
        let c = run(&cli, &input(&s, &held, 8, 10 << 30), || None, &mut 0).unwrap();
        assert_eq!(c.mode, Mode::Critical);
        assert!(
            x.calls().contains(
                &"docker builder prune -f --filter until=3600s --keep-storage 0".to_owned()
            )
        );
        // A disk with plenty of percentage headroom but below the floor is critical too.
        let roomy = Disk {
            total_bytes: 2000 * GB,
            free_bytes: 1000 * GB,
        };
        assert_eq!(
            crate::reclaim::mode_for_floor(&s, &roomy, 1200 * GB),
            Mode::Critical
        );
        assert_eq!(
            crate::reclaim::mode_for_floor(&s, &roomy, 10 << 30),
            Mode::Hygiene
        );
    }

    #[test]
    fn a_container_started_since_the_listing_saves_its_image_and_refusals_are_keeps() {
        let x = fake_docker("--reserved-space");
        x.on(
            "docker ps -aq --no-trunc --filter ancestor=sha256:a1",
            "cnew",
        );
        x.fail("docker image rm sha256:a2");
        let cli = DockerCli {
            exec: &x,
            docker: "docker",
        };
        let s = ReclaimSettings::default();
        let held = HashSet::new();
        let c = run(&cli, &input(&s, &held, 110, 10 << 30), || None, &mut 0).unwrap();
        assert!(!x.calls().contains(&"docker image rm sha256:a1".to_owned()));
        assert_eq!(c.kept["image used by a container"], 2);
        assert_eq!(c.kept["image refused by docker"], 1);
        assert_eq!(c.errors, 0);
        let failed: Vec<&DeletionRecord> = c
            .records
            .iter()
            .filter(|r| r.outcome == Outcome::Failed)
            .collect();
        assert_eq!(failed.len(), 1);
        assert!(failed[0].path.contains("a2"));
    }

    #[test]
    fn dry_run_lists_what_it_would_remove_and_removes_nothing() {
        let x = fake_docker("--reserved-space");
        let cli = DockerCli {
            exec: &x,
            docker: "docker",
        };
        let s = ReclaimSettings {
            dry_run: true,
            ..ReclaimSettings::default()
        };
        let held = HashSet::new();
        let c = run(
            &cli,
            &input(&s, &held, 110, 10 << 30),
            || panic!("no measuring"),
            &mut 0,
        )
        .unwrap();
        assert!(
            !x.calls()
                .iter()
                .any(|c| c.contains(" rm ") || c.contains("prune -f")),
            "{:?}",
            x.calls()
        );
        assert!(c.records.iter().all(|r| r.outcome == Outcome::WouldDelete));
        assert_eq!(c.reclaimed_bytes, 0);
        assert_eq!(c.would_reclaim_bytes, 3 * 620_000_000);
    }

    #[test]
    fn an_item_that_vanished_since_the_listing_does_not_fail_the_run() {
        use crate::drone::exec::Output;
        let x = fake_docker("--reserved-space");
        // `image ls` listed sha256:gone too; it was removed before the inspect,
        // so docker exits 1 but prints the others.
        x.on(
            "docker image ls -q --no-trunc",
            "sha256:a1\nsha256:a2\nsha256:a3\nsha256:busy\nsha256:gone\nsha256:multi",
        );
        let ok = x
            .script
            .lock()
            .unwrap()
            .get("docker image inspect sha256:a1 sha256:a2 sha256:a3 sha256:busy sha256:multi")
            .cloned()
            .unwrap();
        x.script.lock().unwrap().insert(
            "docker image inspect sha256:a1 sha256:a2 sha256:a3 sha256:busy sha256:gone sha256:multi"
                .into(),
            Output {
                success: false,
                stdout: ok.stdout,
            },
        );
        let cli = DockerCli {
            exec: &x,
            docker: "docker",
        };
        let s = ReclaimSettings::default();
        let held = HashSet::new();
        let c = run(&cli, &input(&s, &held, 110, 10 << 30), || None, &mut 0).unwrap();
        assert!(x.calls().contains(&"docker image rm sha256:a1".to_owned()));
        assert!(!x.calls().iter().any(|c| c.contains("rm sha256:gone")));
        assert_eq!(c.errors, 0);
        // A failed inspect that printed nothing usable is still a failure.
        x.fail("docker container inspect cold crun");
        assert!(run(&cli, &input(&s, &held, 110, 10 << 30), || None, &mut 0).is_err());
    }

    #[test]
    fn the_cap_bounds_removals_per_cycle() {
        let x = fake_docker("--reserved-space");
        let cli = DockerCli {
            exec: &x,
            docker: "docker",
        };
        let s = ReclaimSettings {
            max_deletions_per_cycle: 2,
            ..ReclaimSettings::default()
        };
        let held = HashSet::new();
        run(&cli, &input(&s, &held, 110, 10 << 30), || None, &mut 0).unwrap();
        let rms = x.calls().into_iter().filter(|c| c.contains(" rm ")).count();
        assert_eq!(rms, 2);
    }
}
