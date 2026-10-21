# Operations guide

Configuration, scheduling, drone administration, and protocol details for
[Borg](../README.md). Start with the README for installation and fleet setup.

## Contents

- [Labels](#labels)
- [Classes config](#classes-config)
- [Config files](#config-files)
- [Queen](#queen)
  - [Job telemetry](#job-telemetry)
  - [Per-job host diagnostics](#per-job-host-diagnostics)
  - [Drone bids and demand pressure](#drone-bids-and-demand-pressure)
  - [Wake-on-LAN](#wake-on-lan)
  - [Build environment contract](#build-environment-contract)
  - [borg run](#borg-run)
  - [Work units (gate and hook runs)](#work-units-gate-and-hook-runs)
  - [Build-box reclaim](#build-box-reclaim)
  - [Quorum rollout](#quorum-rollout)
  - [Learned profiles](#learned-profiles)
  - [Warm-floor controller](#warm-floor-controller)
  - [Host health](#host-health)
  - [Tailnet enrollment](#tailnet-enrollment)
  - [Cloud burst drones](#cloud-burst-drones)
  - [Deploy-trust drone](#deploy-trust-drone)
  - [Starvation guard](#starvation-guard)
  - [Preemption](#preemption)
  - [Admission by CPU pressure](#admission-by-cpu-pressure)
  - [Standby runners and deploy fail-open](#standby-runners-and-deploy-fail-open)
- [Drone](#drone)
  - [Separate job user (Linux)](#separate-job-user-linux)
  - [Labels, environment, hooks](#labels-environment-hooks)
  - [Drone trust](#drone-trust)
  - [Owner caps](#owner-caps)
  - [What a lease does](#what-a-lease-does)
  - [Host caches](#host-caches)
- [Updating a drone](#updating-a-drone)

## Labels

A job asks for a class with a runner label: `<prefix><class>` for jobs from
the default branch or a merge queue, and `<prefix><class>-pr` for pull
requests. The default prefix is `borg-ci-`; set `label_prefix` in the classes
config to change it (for example `example-ci-`). A runner minted for main
demand carries only the bare label. A runner minted for pull-request demand
carries both labels. With this workflow labeling convention, default-branch
and merge-queue jobs request the bare label; pull-request jobs request the
`-pr` label. Labels route jobs; they do not enforce an event restriction.

Deploy-path jobs (prod OIDC, signing, registry push) ask for
`<prefix><class>-deploy`. A runner minted for deploy demand carries only that
label, so GitHub can hand it nothing but deploy jobs. Deploy demand is
scheduled before main demand, and main before pull-request demand.

A deploy drone never runs a pull-request job or a work unit; a shared drone
never runs a deploy job. Which drones are deploy drones is set per drone with
`trust` (see [Drone trust](#drone-trust)).

## Classes config

The `classes` table is partial; omitted fields keep their defaults:

```toml
[classes]
label_prefix = "example-ci-"
heavy = { mem_mib = 20480, vcpu = 6, docker = true }
```

Existing `runs-on` labels outside the prefix can be answered too with
`aliases`, a map from label to a class name (main scope) or to an object
naming the class and scope:

```toml
[classes.aliases]
"example-legacy-lane" = "medium"
"example-promote-lane" = { class = "medium", scope = "deploy" }
```

Queued jobs with an alias label count as demand of the alias's scope, so a
`deploy` alias is deploy demand. A runner carries an alias label only when its
grant scope equals the alias's scope: a deploy alias rides on deploy runners
only, and a main alias rides on main and pull-request runners (both carry the
bare label). Alias labels must not start with the prefix and the scope must be
`main` or `deploy`.

## Config files

Queen and drone configs are TOML or JSON with the same keys, defaults and
validation. A `.toml` or `.json` extension picks the format; any other name is
read as JSON when it starts with `{` and as TOML otherwise. Unknown keys are
errors, reported with the file, line and key. TOML has no null, so settings
that JSON turns off with `null` take `false` in either format
(`starvation_after_secs = false`).

```sh
borg config check queen.toml            # validate and print the effective config
borg config check --drone drone.toml
```

`check` prints every setting with its defaults filled in; secrets print as
`[redacted]`.

## Queen

```sh
borg queen enroll --config queen.toml host-a   # prints a one-time token
borg queen run --config queen.toml
```

Example `queen.toml` (every key except `github`, `repos` and `db_path` is
optional):

```toml
repos = ["example-org/app"]
db_path = "/var/lib/borg/queen.db"
listen = "127.0.0.1:8080"
starvation_after_secs = 120

[github]
org = "example-org"
app_id = "12345"
private_key_path = "/etc/borg/app.pem"

[classes]
label_prefix = "example-ci-"
```

Operators can stop new leases to one drone with `borg queen quarantine
--config queen.toml --reason "<why>" <drone>` and lift it with `borg queen
release --config queen.toml <drone>`. Set `rollout_target_version` in the queen
config to have `/metrics` and `/status` report how many drones run that borg
version. The queen sends that target as `desired_version`; drones update
between jobs. Configure `rollout.previous_version` to gate the rollout
through canaries and waves (see [Quorum rollout](#quorum-rollout)).

`warm_floors` keep idle runners alive per class and scope; the optional
`warm_schedule` raises them in time-of-day windows (`{"class": "light", "scope":
"pr", "count": 4, "start": "08:00", "end": "20:00", "days": ["mon", "tue"]}`)
at a fixed `utc_offset_minutes`. A window never lowers a floor. Floors and
windows may name `"scope": "deploy"`; the allocator keeps those runners on
deploy drones.

Pending runners absorb queued demand of the same label: a pending
pull-request runner (it carries the bare label) may absorb main demand, but
deploy demand is absorbed only by pending deploy runners, and a pending deploy
runner absorbs nothing else.

### Leader lease

The fenced leader lease for running several queens (`src/lead.rs`) exists as a
tested module and is not wired into the queen yet. There is no config key, no
store setting and no flag for it; run a single queen until the wiring ships.

### Job telemetry

A drone may add a `telemetry` object to `POST /v1/leases/{id}/result`:

```json
{
  "exit_code": 1, "duration_secs": 412,
  "telemetry": {
    "job_name": "rust-test (shard 2)", "repo": "example-org/app", "run_id": 123456789,
    "conclusion": "failure", "failure_class": "host", "queue_wait_secs": 18,
    "steps": [{ "name": "cargo test", "duration_secs": 401, "conclusion": "failure" }]
  }
}
```

`conclusion` is `success`, `failure`, `cancelled`, `skipped`, `timed_out` or
`unknown`; `failure_class` is `host` (machine or runner failure), `code` (the
job's own failure) or `unknown`. Unrecognized values read as `unknown`; at most
64 steps and 120 characters per name are kept; `job_name` and a `owner/name`
`repo` are required, otherwise the telemetry is dropped and the result still
recorded. The queen keeps it for `telemetry_retention_days` (default 30).
`borg report --queen <url>` (or `--config queen.json` on the queen host) and
`GET /report` summarize time by job and step, queue wait per class, the
cancellation share and host-caused failures per drone.

#### GitHub job feedback

The queen also collects outcomes directly from the GitHub job records it reads
while polling queued and in-progress workflows. It accepts a job only when both
its runner ID and runner name match a lease the queen minted. Completed jobs
supply their actual execution duration, conclusion, step timings and queue wait
to the existing telemetry store, reports and learned placement profiles.
Missing or reversed timestamps remain unavailable; they never become placeholder
zeroes. GitHub failures retain `failure_class: unknown`, because an API result
does not establish whether the host caused the failure. Drone reports can add
that evidence and take precedence over this fallback.

Once a running job is observed, its repository and job ID are retained in the
queen's existing SQLite store until completion. This lets the next poll query
the job directly after its workflow leaves the active list, including after a
queen restart. These bindings expire with `telemetry_retention_days`; a deleted
job (HTTP 404) or a completed job without usable timing drops its binding.
Collection runs after grants and the status snapshot are published. A pass has
five seconds, with at most sixteen direct lookups and one second per lookup;
the starting job rotates each tick so a slow job cannot starve other bindings.
Collection errors are logged and do not fail queue admission. No additional credentials or permissions are required beyond
the existing Actions read access.

This is polling coverage: a job whose entire workflow starts and finishes
between polls can be missed. Reports count only collected outcomes. The existing
profile minimum sample and confidence rules still apply, and profiles refresh
on `profiles.refresh_secs`. This change does not alter owner resource caps,
class sizes or warm-floor settings.

#### What a drone-side parser should produce

The queen validates but does not interpret these fields, so the drone owns
their meaning. Keep it consistent across drones:

- **Identity.** `repo`, `run_id` and `job_name` come from the job the runner
  executed (its job context: repository, run id, job display name), not from
  log text a workflow can print. `job_name` is the name as GitHub shows it,
  including matrix suffixes.
- **Steps.** One entry per step in execution order, `duration_secs` rounded
  down to whole seconds, `conclusion` mapped from the step result (succeeded to
  `success`, failed to `failure`, canceled to `cancelled`, skipped to
  `skipped`, anything else to `unknown`). Report names only; never log lines,
  environment values or step output.
- **Conclusion.** The job's result, mapped the same way; a job the drone killed
  at `max_job_secs` is `timed_out`.
- **`queue_wait_secs`.** Seconds from the job being queued on GitHub to the
  runner starting it. Omit it when it cannot be determined; do not send 0 as a
  placeholder, because the report averages the values it receives.
- **`failure_class`.** `host` when the machine or the runner failed rather than
  the job: the runner process was killed by a signal or the kernel (OOM), the
  disk filled, Docker was unreachable, the runner lost its connection, or the
  lease was torn down by the drone. `code` when the job's own step exited
  non-zero on a healthy host. `unknown` when the drone cannot tell. Leave it
  `unknown` for jobs that did not fail.
- **Failure behaviour.** Parsing is best effort and must never delay or fail
  the result: if anything cannot be read, send the result without `telemetry`
  (or with fewer fields), and send it in the same request as `exit_code` and
  `duration_secs`. A retried result may be sent again; the queen keeps the
  latest.

### Per-job host diagnostics

A job that fails on an overloaded drone looks like a code failure unless
someone compares runners by hand. The drone samples its host just before the
runner starts and again when it exits, and sends both samples with the lease
result (`host: {start, end}` next to `exit_code`). Each sample has:

| Field | Source |
| --- | --- |
| `at`, `cpus` | Unix seconds; CPUs the drone may use. |
| `load` | `/proc/loadavg` 1, 5 and 15 minute averages. Inside an LXC, lxcfs computes this and it over-reports, so use the pressure and busy fields instead. |
| `cpu_psi`, `memory_psi`, `io_psi` | `/proc/pressure/*`, `some`/`full` `avg10`/`avg60` in percent. |
| `cgroup_cpu_psi`, `cgroup_memory_psi` | `/sys/fs/cgroup/*.pressure`: the cgroup-namespace root, which is the whole container in an LXC. |
| `host_cpu_usage_usec`, `host_cpu_busy_cores` | `usage_usec` of `/sys/fs/cgroup/cpu.stat`, and the cores busy since the drone's previous reading (heartbeats feed it, so at most one heartbeat old). |
| `drone_cpu_usage_usec`, `drone_cpu_throttled_usec` | `cpu.stat` of the drone's own cgroup (`/proc/self/cgroup`). Jobs run inside it. |
| `mem_available_mib`, `work_free_mib`, `cache_free_mib` | `MemAvailable`; `df` of `work_root` and `cache_root`. |
| `active_leases` | Leases the drone held at that moment, this one included. |

Every field is optional, and a field the drone cannot read is left out. A drone
that predates this sends no `host`, and the row still records which drone ran
the job.

The queen stores one row per lease in `lease_hosts`. It adds the GitHub run id,
job id and job name when its queue poll sees the job on a runner it minted
(the same match as [GitHub job feedback](#github-job-feedback)). Rows are kept
for 7 days and at most 50,000 rows, oldest first. Two read endpoints are
unauthenticated like `/status`:

- `GET /jobs/{job_id}` returns the lease that ran the job, or 404.
- `GET /runs/{run_id}` returns `{run_id, jobs: [...]}`, oldest lease first.

```json
{
  "lease_id": "a1b2c3", "drone": "shared-box", "class": "heavy", "scope": "main",
  "repo": "example-org/app", "run_id": 123456789, "job_id": 987654321, "job_name": "ui-e2e (shard 3)",
  "leased_at": 1760650000, "finished_at": 1760651800, "duration_secs": 1790, "exit_code": 1,
  "start": { "at": 1760650010, "cpus": 16, "load": [77.1, 60.2, 50.3],
             "cgroup_cpu_psi": { "some_avg10": 54.9, "some_avg60": 52.4 },
             "host_cpu_busy_cores": 15.2, "active_leases": 2, "…": "…" },
  "end": { "…": "…" },
  "during": { "secs": 1790, "host_cpu_busy_cores": 15.5, "drone_cpu_busy_cores": 3.0,
              "drone_cpu_throttled_secs": 0.0 },
  "slow_host": true,
  "slow_host_reasons": ["cpu pressure 54.9% at start (threshold 30%)"]
}
```

`slow_host` is true when any of these held: CPU pressure (`some avg10`, the
higher of the host and cgroup files) at start at or above the admission soft
threshold (`psi_soft_pm / 10`, or 30% without an `admission` block); memory
pressure at start at or above 10%; load average per CPU at start at or above
1.5, used only when the host has no CPU pressure reading; 90% of the CPUs busy
at start or on average during the job; less than 10 GiB free on the work or
cache volume at start. `during` is computed from the two samples.

The job id is known only if a queue poll saw the job while it ran or a direct
lookup found it afterwards (see [GitHub job feedback](#github-job-feedback)).
A job that starts and finishes between polls has a row without `job_id`, and
`/jobs/{job_id}` returns 404 for it.

### Drone bids and demand pressure

A drone may add `bids` to its heartbeat: at most one per `(class, scope)` it is
willing to serve. The queen answers every heartbeat with `pressure`, the
demand pressure (0 to 1000) per class, so the drone can set its bids against it.

```json
{ "class": "medium", "scope": "pr", "available_slots": 2,
  "estimated_start_ms": 1500, "cache_affinity": 800, "resource_pressure": 150,
  "estimated_cost": 1000, "confidence": 900, "min_pressure": 0, "expires_at": 1760000000 }
```

`cache_affinity`, `resource_pressure`, `confidence` and `min_pressure` are
permille (0 to 1000; larger values are clamped); `estimated_cost` is relative
(1000 is fleet typical); `expires_at` is Unix seconds, after which the queen
ignores the bid. `min_pressure` is the drone's response threshold: it does not
want this work while the class's pressure is below it, so a warm idle host
reports 0, a cold host something higher, and a workstation whose owner is
active something high. Starving demand overrides every threshold.

The allocator drops a bid with no `available_slots` or above the current
pressure, and scores the rest with integers:
`3*cache_affinity - 3*resource_pressure - 2*start - 1*(cost-1000)`, where
`start` is `estimated_start_ms` as permille of 60 s, all scaled by
`confidence/1000`. A drone's total score is that bid score plus its learned
profile term (see [Learned profiles](#learned-profiles)) plus a host term. The
highest score wins; ties go to the tightest memory fit, then the drone name. A drone that sends no bids, or only expired ones, is
treated as before (always eligible, neutral score); a drone that bids but not
for a key declines that key. GitHub still chooses which queued job a runner
takes: bids steer only which host receives a freshly minted runner.

### Wake-on-LAN

Optional (`wake` block, off by default). When demand for a class stays unmet for
`after_secs`, the queen sends a magic packet to a listed host that can serve the
class and whose drone is not fresh, rate limited per pass, per hour and per host.
Targets are IPv4 addresses or `addr/len` networks. See `/status` `wake`.

### Build environment contract

[Capability admission](capability-admission.md) adds bounded job-user execution
proofs for sandbox, Docker, work-root and cache-root access.

Optional (`env_contract.enabled`, off by default). Before a CI runner lease or a
work unit is placed on a drone, the queen checks what the drone last reported
about its build environment. Five checks, each `required`, `advisory` or `off`
globally and per class (`env_contract.checks`, `env_contract.classes.<class>`):

* `compiler_cache`: `RUSTC_WRAPPER` is an allowed wrapper (`allowed_wrappers`,
  by file name: `sccache` by default, plus any single-flight wrapper in front of
  it), `SCCACHE_DIR` is set, the sccache server is reachable.
* `toolchain`: when the work declares a Rust toolchain, the drone has it. A unit
  declares it (`rust_toolchain`, which `borg run` reads from `rust-toolchain.toml`
  or `rust/rust-toolchain.toml` at the unit's tree); a class can declare it in
  config (`classes.<class>.rust_toolchain`). A mismatch is not admitted.
* `cache_store`: stats work, errors per thousand requests are under
  `max_cache_error_pm`, the cache directory has at least
  `min_cache_dir_free_bytes` free, and a write works. A hit rate at or under
  `hit_rate_warn_pm` after `min_requests_for_hit_rate` requests is a *warning* in
  `/status`, never a block.
* `resources`: memory available covers the class's memory (`mem_available_pct`)
  and CPU pressure is at most `max_cpu_psi_pm`. (Free capacity after active
  leases is the allocator's own rule.)
* `tools`: every tool in `required_tools` is on the job user's `PATH`.

A required check that fails **or is unknown** makes the drone ineligible for the
class; nothing else changes about placement. Facts older than
`facts_max_age_secs` (180) count as unknown. A drone that has never reported is
skipped unless `require_report`, so enabling the contract does not cut off
drones that predate it. Defaults when enabled: `toolchain` and `resources`
required, `compiler_cache`, `cache_store` and `tools` advisory.

The `tools` check covers what a job expects to find on `PATH`, such as `gh` or
`node`. `env_contract.required_tools` lists the tools every class needs and
`env_contract.classes.<class>.required_tools` replaces that list for one class.
Both lists default to empty, so the check passes until the operator names tools;
names must come from the drone's probe list (`git`, `gh`, `node`, `npm`, `pnpm`,
`python3`, `docker`, `cargo`, `rustc`, `bazel`, `go`, `uv`, `mise`, `jq`,
`cosign`). A drone whose facts show a required tool as `missing` fails the check,
and the reason names the missing tools. A drone whose facts carry no `tools` map
(an older binary) passes with a warning, and fails when `require_report` is set.
`/status` shows the missing tools and every probed version under the check's
observed values.

```json
"env_contract": { "enabled": true, "checks": { "tools": "required" }, "required_tools": ["git", "gh", "node", "docker"] }
```

`/status` shows each drone's `contract` (the checks with state, reason, observed
values and warnings, and the admission per class with the reason) and
`units.not_admitted` (why a queued unit is not admitted on each matching drone).
Metrics: `borg_env_contract_check{drone,check,state}` and
`borg_env_contract_admitted{drone,class}`.

**Drone probe spec** (the heartbeat field `env_facts`). The drone only measures;
verdicts and thresholds are the queen's. Every field is optional, and `null` /
absent means "not measured" (never "fine"). Probe as the **job user**, with the
environment a job will get, and cache results about 60 seconds (resend the cached
object with its original `observed_at`; the queen treats facts over 180 seconds
old as unknown, so probe at least that often):

| Field | How |
| --- | --- |
| `observed_at` | Unix seconds the probes ran. |
| `rustc_wrapper` | `RUSTC_WRAPPER` as a job sees it, resolved to a path through `PATH` if it is a bare name; absent if unset or empty. |
| `sccache_dir` | `SCCACHE_DIR` as a job sees it; absent if unset. |
| `sccache_server_reachable` | Whether the sccache server answers (connect to `SCCACHE_SERVER_PORT` or its socket, 5 s timeout). Never start a server to find out. |
| `sccache_stats_ok` | `sccache --show-stats --stats-format json` exits 0 within 5 s. |
| `cache_requests`, `cache_hits`, `cache_errors` | From that JSON: `compile_requests`, the sum of the `cache_hits.counts` values and the sum of the `cache_errors.counts` values (check against the installed sccache's schema). |
| `cache_dir_free_bytes` | Available bytes (`f_bavail * f_frsize`) on the filesystem holding `SCCACHE_DIR`. |
| `cache_write_ok` | Create, write and remove a small file named `.borg-probe-<pid>` in `SCCACHE_DIR`. |
| `toolchains` | For each toolchain `rustup toolchain list` shows, `{channel, rustc_version}`: the channel as `rust-toolchain.toml` would name it (`1.95.0`, `stable`, `nightly-2026-01-01`; strip the host triple) and the `release:` line of `rustc +<channel> -vV`. `null` if rustup could not be queried. |
| `mem_available_mib` | Linux `MemAvailable` from `/proc/meminfo`; macOS (free + inactive + speculative pages) times the page size, in MiB. |
| `cpu_psi_some_avg10_pm` | Linux `/proc/pressure/cpu` line `some`, `avg10` times ten, rounded; absent where PSI does not exist. |
| `load_per_cpu_pm` | `/proc/loadavg` 1-minute average divided by the usable CPUs, times 1000. |
| `cpu_busy_pm` | Share of the CPUs busy since the previous heartbeat, from `usage_usec` in `/sys/fs/cgroup/cpu.stat`, in permille. |
| `tools` | For each of `git`, `gh`, `node`, `npm`, `pnpm`, `python3`, `docker`, `cargo`, `rustc`, `bazel`, `go`, `uv`, `mise`, `jq`, `cosign`: resolve the executable on the job user's `PATH` (as the job user, with the job environment) and record the first line of `<tool> --version` (`go version` for `go`), trimmed, at most 80 characters. `"missing"` when it is not on `PATH`, `"unknown"` when it is but prints nothing. The map is absent when the probe did not run. |

A unit's toolchain (`rust_toolchain`) matches an installed `channel` or
`rustc_version` exactly.

### `borg run`

```sh
borg run [--index | --tree <sha>] [--publish] [--class medium] [--wait-secs 60] -- <command...>
```

Runs a command on the fleet when a drone has capacity, and otherwise right here;
the exit code is the command's either way. With no `BORG_QUEEN` set it just runs
the command locally, so wrappers can call it unconditionally.

* Environment: `BORG_QUEEN` (queen URL), `BORG_CLIENT_TOKEN` or
  `BORG_CLIENT_TOKEN_FILE` (default `~/.config/borg/client-token`; the token comes
  from `borg queen enroll-client`), `BORG_RUN_WAIT_SECS`, `BORG_PUBLISH`,
  `BORG_TOOLCHAIN`.
* The tree is `HEAD^{tree}` by default, `git write-tree` of the index with
  `--index` (pre-commit hooks), or `--tree <sha>`. The unit's identity is the repo,
  tree, argv, platform and toolchain, so an identical command that already passed
  for this tree is not run again (the receipt is reported, exit 0) and one that is
  running is joined.
* The drone has to be able to get the tree. A tree from a pushed commit is in
  its git mirror. Otherwise `--publish` uploads a `git bundle` to the queen
  (`PUT /v1/units/{id}/bundle`): a synthetic commit of the tree whose parent is the
  merge base with `origin/main`, bundled without everything that merge base
  already reaches, so it is small. Nothing is pushed to any remote. Without
  `--publish`, an unpublished tree runs locally.
* Falls back to running here when the queen is unset or unreachable, refuses the
  unit, no drone takes it within `--wait-secs` (the unit is cancelled), the queen
  is lost for 60 seconds mid-run, or the unit ends for infrastructure reasons
  (cancelled, timed out, out of attempts). A command that ran on a drone and
  exited nonzero is a real failure and its code is returned. Ctrl-C cancels a
  unit this invocation created.

### Work units (gate and hook runs)

See [remote execution](remote-execution.md) for remote-only runs and JSON
results, [work-unit fairness](work-unit-fairness.md) for capacity shares, and
[placement diagnostics](placement-diagnostics.md) for `borg explain`.

Optional (`units.enabled`, off by default). While off, `/status` `units` shows only `enabled: false`, with empty counts and no recent units. Clients such as `borg run`, the
pre-commit hook and `mq` submit whole commands; drones run them. Enroll a client
with `borg queen enroll-client <name>` (prints a token once).

Client API (bearer: client token):

* `POST /v1/units` with `{repo, tree_sha, argv, platform, toolchain, class, timeout_secs?, force?, scope?, supersede_key?, record?}`
  returns `201` `{outcome: "new", unit_id, ...}`, or `200` with `outcome:
  "joined"` (an identical unit is queued or running) or `"receipt"` (an
  identical unit succeeded recently; `receipt` has the proof). The key is
  (repo, tree SHA, argv, platform, toolchain); only successes are reused, never
  for another tree, and `force` skips reuse. `platform` is `<os>-<arch>`
  (`linux-x86_64`, `macos-aarch64`).
* `scope` is `deploy`, `main` or `pr` (default `pr`). The unit holds capacity
  under that scope and runs only on drones whose trust admits it (see
  [Drone trust](#drone-trust)): a `deploy` unit runs only on a deploy drone, a
  `pr` unit never does, and a `main` unit runs on either. Scope is part of the
  single-flight key, so a `main` or `deploy` unit never joins or reuses the
  result of a lower-scope run. Enroll a client with `borg queen enroll-client
  <name> --max-scope deploy|main|pr` (default `pr`) to set the highest scope
  its token may submit; a submission above that is refused with `403` and a
  message naming both scopes. `borg run --scope main` (or `BORG_RUN_SCOPE`)
  sets the scope on the client side.
* `supersede_key` (at most 128 characters) makes the newest submission win:
  when a new unit is created with a key, every queued unit from the same client
  with the same key is cancelled, and every running one is cancelled through
  the `cancel_units` heartbeat entry. A cancelled unit ends in state `failed`
  with `failure` set to `superseded by <unit_id>`. A unit submitted with
  `"record": true` (default `false`) is never superseded. The response lists
  the cancelled ids in `superseded`; `/status` `units.superseded` and the
  `borg_units_superseded_total` metric count them since the queen started.
* `GET /v1/units/{id}` returns the unit (state `queued|leased|running|done|failed`,
  `exit_code`, `receipt`).
* `GET /v1/units/{id}/log?from=N&wait=S` returns log bytes from offset `N`,
  waiting up to `S` seconds (at most 30) for new bytes while the unit is active.
  Headers: `x-borg-next` (the offset to ask for next), `x-borg-state`, and
  `x-borg-exit` once finished and fully read.
* `POST /v1/units/{id}/cancel`.
* `PUT /v1/units/{id}/bundle` (the unit's own client; body is a git bundle, at most
  `units.max_bundle_bytes`, 64 MiB by default). A unit submitted with
  `"bundle": true` is not placed until the bundle arrives, and fails after
  `units.bundle_wait_secs` (60) without it. Bundles are deleted with their unit
  (7 days, the life of its receipt) or at once if the unit fails or is cancelled.

Drone protocol (bearer: drone token):

1. The heartbeat response carries `units: [{unit_id, repo, tree_sha, argv, class,
   toolchain, timeout_secs, attempt}]` for units placed on this drone, and
   `cancel_units: [id]`. Placed units are resent until acknowledged.
2. Acknowledge by listing the unit id in `active_units` in the heartbeat request;
   that starts the unit. Keep listing it while it runs. A running unit the drone
   stops listing for 60 seconds is treated as lost and re-queued (up to
   `units.max_attempts`).
3. Run it: get `tree_sha` into the local git mirror (if `git cat-file -e
   <tree>` fails and the grant has `bundle_bytes > 0`, fetch
   `GET /v1/units/{id}/bundle` and `git bundle unbundle` it into the mirror; its
   prerequisite is a commit the mirror already has, and if that fails the unit
   fails and the client runs locally), check the tree out into a slot directory
   (`git clean` it first), run `argv` as the unprivileged job user with the same
   isolation rules as CI jobs, and enforce `timeout_secs`.
4. Stream output with `POST /v1/units/{id}/log?offset=N` (raw bytes; `N` is the
   byte offset of the first byte of the body). Repeating a chunk is harmless;
   `409` means the queen holds fewer than `N` bytes, so resend from its length
   (the `200` body of any append is the queen's length).
5. Finish with `POST /v1/units/{id}/result` `{exit_code, duration_secs}`.
   Idempotent. Stop and report on a `cancel_units` entry.

Units are placed by the runner allocator (class capacity, owner caps, bids,
profiles, quarantine and probation) on drones whose `<os>-<arch>` matches, using
capacity left after CI runner demand. Among queued units the order is the one
runner leases use: `deploy` before `main` before `pr`, older first within a
scope, and `pr` units are the first held back when a drone is under CPU
pressure (see [Admission by CPU pressure](#admission-by-cpu-pressure)).

### Build-box reclaim

A drone on a shared build host can reclaim abandoned git worktrees and cargo
`target/` and `incremental/` directories by disk pressure (`reclaim` block of the
drone config; off by default, dry-run by default). `reclaim::run_cycle` is the
policy; the drone implements `ReclaimHost` and reports `ReclaimStats` in its
heartbeat (`reclaim`), which the queen audits (`/status` `reclaim`).

What a `ReclaimHost` implementation must guarantee, because the policy relies on
it: `probe` reports **busy** if any process has its working directory or an open
file inside the path (check `/proc/*/cwd` and `fd`, or `lsof +D` on macOS);
**dirty** if `git status --porcelain` is non-empty, untracked files included;
**unpushed** if `git rev-list HEAD --not --remotes` is non-empty (or HEAD is
detached on such commits); **protected** for the main checkout (first entry of
`git worktree list`), a locked worktree, or a repository with stashes; **not_git**
when the path is not a work tree; and **unknown** whenever any step failed or
timed out. `delete` removes the directory (for a worktree, `git worktree remove`
or removal followed by `git worktree prune`) and returns the bytes freed. `scan`
lists each child of a root, and for `cargo_target` roots also each
`incremental/` directory inside (with `within` set to its target directory).

### Quorum rollout

`rollout_target_version` plus `rollout.previous_version` turns on a gated rollout
of a new drone version: the queen tells one Linux and one macOS canary the
target through `desired_version` in the heartbeat response, promotes through
waves only while success rate, host-failure rate against the control group and
heartbeat health agree, and halts and tells every drone the previous version
otherwise. State is persisted; see `rollout` in `/status` and
`borg queen rollout halt|reset`. Drones update between jobs when
`desired_version` differs from what they run, and may downgrade.

### Learned profiles

The queen builds per-drone, per-class profiles from telemetry: speed against the
fleet median for the same job family (the job name without matrix suffixes) and
reliability from host-caused failures only. They add to the bid score in
proportion to the evidence, so proven faster and more reliable hosts are
preferred when placing a runner. See `profiles` in the queen config and
`profiles[]` in `/status`.

The host term covers how loaded a drone is right now:
`-1 * pressure + 250 * free_vcpu / budget_vcpu` for medium and heavy work, and
`-1 * pressure` for light work. Here `pressure` is the drone's fresh CPU
pressure in permille: PSI, or the load estimate without PSI (see
[Admission by CPU pressure](#admission-by-cpu-pressure)), and 0 when unknown or
when admission is off. With both terms, main heavy and medium work goes to
drones that are fast and idle first. A host that is historically slow (speed
550 permille costs 450 points) or loaded (PSI 450 permille costs 450 points)
gets work only when nothing better has room, and above the admission
thresholds it gets none. Light work keeps packing onto the tightest fit.
`/explain` shows each candidate's `host` term next to `bid` and `profile`.

### Warm-floor controller

Optional (`homeostasis.enabled`). A damped feedback loop raises the warm floor of
a class and scope when queue latency stays above a target and idle room exists,
and lowers it gradually when demand subsides, with hysteresis, per-key cooldowns,
a step size, an hourly change limit and per-class ceilings. The clock schedule
(`warm_schedule`) remains a minimum. State appears in `/status` and the
`borg_warm_floor*` metrics.

### Host health

The queen scores each drone's host-caused failures (from telemetry with
`failure_class: host`) with a half-life decay. Code failures never count. It
recommends quarantine on moderate evidence, and quarantines automatically only
when failures from several distinct runs corroborate, the failures do not look
fleet-wide, and enough other hosts remain in service. A quarantined host returns
after a cooldown, on probation (one lease at a time), once a few jobs succeed.
Tune it with a `health` block in the queen config (`auto_quarantine`,
`half_life_secs`, `window_secs`, `corroboration_runs`, `cooldown_secs`,
`proof_jobs`, and the bounds); `/status` shows each drone's `health`.

### Drone availability and expiry

A drone whose last heartbeat is older than `drone_stale_secs` (60) is
`stale`. It holds no schedulable capacity: it is left out of `slots`,
`free_slots` and placement, whatever its `health` says. `health` and
`profiles` describe past failures and speed only. Each drone in `/status`
carries `availability` (`alive` or `stale`), `last_seen_age_secs` and, for a
stale drone that will be removed, `expires_in_secs`. `stale_drones` lists the
stale names. When a drone goes stale, a `drone_stale` health event is logged.

A drone that stays silent past its expiry is removed with its token and lease
records, and a `drone_expired` health event is logged. Configure the expiry in
the queen config:

```json
"drone_expiry": {"after_secs": 604800, "burst_prefixes": ["gcp-burst-"], "burst_after_secs": 1800}
```

`after_secs` (7 days by default) applies to every drone. `burst_after_secs`
(30 minutes) applies to drones whose name starts with one of
`burst_prefixes`; cloud burst drones delete themselves and never come back
under the same token. 0 turns either expiry off. A drone named in
`wake.hosts`, or one that has never sent a heartbeat, is never removed. A
removed drone that comes back has to be enrolled again.

`/status` `capacity` shows, per class, `schedulable_slots` (what the alive,
accepting drones could hold if empty) and `free_slots` next to `unmet_main`,
`unmet_pr` and `unmet_deploy`. The unmet counts are the last pass's
`unmet_placeable`. When main demand of a class in `capacity_alarm.classes`
stays unmet for `capacity_alarm.after_secs` while that class has at most
`capacity_alarm.max_slots` schedulable slots, the queen logs a
`capacity_starved` event (drone `fleet`; the reason names the stale drones).
When the condition clears, it logs `capacity_restored`. `capacity.<class>.alarm`
and the metric `borg_capacity_starved{class}` show whether the alarm is
raised, and `borg_unmet_jobs{class,scope}` exports the unmet counts. The
alarm is on by default for heavy and medium, with `max_slots` 2 and
`after_secs` 600:

```json
"capacity_alarm": {"enabled": true, "classes": ["heavy", "medium"], "max_slots": 2, "after_secs": 600}
```

`/status` `static_runners_offline` lists the organization's non-borg runners
that GitHub reports offline (a static runner whose service stopped, was
OOM-killed or lost its host), each with its custom labels and the time this
queen first saw it offline (the time resets when the queen restarts). The
metric `borg_static_runners_offline{runner}` is 1 for each. Borg's own runners
are never listed; they are ephemeral and expire with their lease.

All of these events go to the same audit trail as quarantines: `health_log`,
shown as `health_events` in `/status`, and a `WARN` line in the queen journal.

### Tailnet enrollment

Optional and off by default. With a `tailnet_enroll` block
(`allowed_tags`, `allowed_users`, optional `socket_path` for the local
tailscaled), `POST /v1/enroll` identifies the caller through the local
tailscaled (`whois` by remote address) and, if the node carries an allowed tag
(or, for an untagged node, belongs to an allowed user), enrolls a drone named
after the node and returns `{"name", "token"}` once. The queen must see the
caller's tailnet address directly (bind it to the tailnet address, no proxy in
front), and its service user needs access to the tailscaled socket
(`tailscale set --operator=<user>`). A node that is already enrolled gets 409.

The optional supersede janitor cancels queued pull-request workflow runs whose
head commit is no longer the pull request's head. Add a `supersede_janitor`
block (optionally `token_path` or `BORG_JANITOR_TOKEN` for a static token with
`actions: write`; without either the janitor uses the queen's own App installation
token, minted and refreshed on demand, so the App needs Actions: read and write;
`dry_run` defaulting to true, `interval_secs`, `max_cancels_per_pass`) to turn
it on. By default it never cancels a run that has started.

Two further switches, both off by default and both subject to `dry_run` and
`max_cancels_per_pass`, remove other waste. `cancel_running = true` also
cancels superseded pull-request runs that are already in progress; a run that
finished between the listing and the cancel is counted as `raced`.
`dedupe_same_sha = true` looks at the queued and in-progress runs of each
repository and groups them by workflow, head commit, event and pull request
number. In a group of more than one it keeps the in-progress run (the lowest id
if several are running), or else the queued run with the lowest id, and cancels
the others only if they are still queued; a running duplicate is never
cancelled. `workflow_dispatch`, `repository_dispatch` and `merge_group` runs
are never grouped. Dispatches at the same head can carry different rollout
payloads, and publication waits on those exact runs. Each cancellation is logged with
the id of the run that was kept. With `dry_run` the log and `would_cancel` show
what would have been cancelled.

The pull request's current head comes from the run's own `pull_requests`
entry, which GitHub fills at read time with the open pull requests whose head
branch matches. The janitor reads the pulls API only when that entry carries no
head, so the App does not need pull-request read access.

Two more lists, both empty by default, cover the runs a closed pull request or
a newer push leaves behind:

* `closed_pr_workflows`: workflow file paths (`.github/workflows/ci.yml`) or
  names. A queued `pull_request` run of one of them (a running one too with
  `cancel_running`) whose head repository is the repository itself and which
  names no open pull request belongs to a pull request that was closed or
  merged, and is cancelled. A fresh read must still show no pull request. Do
  not list a workflow that runs on the `closed` event (teardown, cleanup): its
  runs look the same.
* `superseded_push_branches`: branch names, for example `["main"]`. A queued
  `push` run on one of them is cancelled when a newer queued or running run of
  the same workflow on the same branch is on another commit. Running push runs
  are never cancelled, so the branch always finishes its newest started run.
  GitHub reports a run as `queued` while any of its jobs waits for a runner, so
  the janitor also reads the run's jobs and keeps the run when one is running
  (`started_push`).

When the janitor is enabled, `GET /status` has a `janitor` object:

* `since`: unix time the queen started; `total` counts from here.
* `last_pass_at`: unix time of the last pass.
* `passes`: passes since `since`.
* `last`: what the last pass alone found. A run that stays queued is found
  again by every pass, so `last` is the current state.
* `total`: `last` summed over every pass. Because a run that stays queued or
  running is counted again on each pass, `duplicates`, `raced`,
  `uncancellable` and `errors` grow with time even when nothing changes; read
  `last` to judge the present and the rate of `total` to judge a trend.

| Counter | Meaning |
| --- | --- |
| `considered` | `pull_request` runs examined: queued ones, plus running ones with `cancel_running`, that name exactly one pull request or (with `closed_pr_workflows`) none. |
| `superseded` | Of those, runs whose head is no longer the pull request's head. |
| `closed_pr` | Runs of `closed_pr_workflows` whose pull request is closed or merged. |
| `superseded_push` | Queued push runs on `superseded_push_branches` with a newer run of the same workflow. |
| `cancelled` | Queued runs cancelled for any of the three reasons above. |
| `cancelled_running` | Running runs cancelled as superseded or closed-PR (`cancel_running`). |
| `duplicates` | Runs beyond the one kept in a same-workflow, same-commit group (`dedupe_same_sha`), queued or running. Running duplicates are never cancelled, so they are counted again on every pass while they run. |
| `dedup_cancelled` | Queued duplicates cancelled. |
| `would_cancel` | Runs `dry_run` would have cancelled, of any kind. |
| `deferred` | Runs left for a later pass by `max_cancels_per_pass`. |
| `started_push` | Superseded push runs kept because one of their jobs was running. |
| `raced` | Runs whose status, head or pull request changed between the listing and the re-read just before the cancel; left alone. |
| `uncancellable` | Runs whose cancel GitHub answered with 409. Runs stuck in `queued` for days (for example `workflow_run` runs GitHub never started) answer 409 on every pass and are counted each time. |
| `errors` | Failed GitHub calls: a run listing for a repository, a pulls API read, a run re-read or a cancel. Each is logged as a `janitor:` warning in the queen journal. |

`GET /metrics` has `borg_janitor_<counter>_total` for each counter, summed over
passes since the queen started.

The GitHub App needs organization self-hosted runners: read and write, plus
read access to Actions on the listed repositories. Set
`BORG_APP_PRIVATE_KEY` instead of `private_key_path` to pass the PEM in the
environment. The queen speaks plain HTTP; bind it to a private interface or
put a TLS terminator in front.

| Endpoint | Purpose |
| --- | --- |
| `GET /status` | Per-class demand, slots and leases; drones and heartbeat age; oldest queued age per label; `reserved_for`. |
| `GET /metrics` | The same numbers in Prometheus text format. |
| `POST /v1/heartbeat` | Drone API. Reports capacity; returns new leases until their JIT config is acknowledged. |
| `POST /v1/leases/{id}/result` | Drone API. Reports a lease's exit code, duration and peak RSS. |

Manage drones with `borg queen drone list` and `borg queen drone remove <name>`
(which revokes the token). `borg queen enroll` prints only the token to stdout.

Drone requests carry `Authorization: Bearer <token>`. The queen stores only
the SHA-256 of each token.

### Cloud burst drones

A burst drone is a short-lived cloud VM that joins the fleet to drain a queue
and then deletes itself. Two things decide whether it is cheap and safe: how
its traffic leaves the cloud network, and how it reaches the queen.

A burst image must carry the same tools as the on-prem runners (`gh`, `node`
and the rest of the `required_tools` list). The `tools` check is how the queen
refuses a drone whose image does not.

**Egress: a public IP per VM, no NAT gateway.** Give each VM its own
ephemeral external IPv4 address. Traffic to GitHub, crate registries and
container registries then leaves the VM directly. Do not put burst drones
behind a managed NAT gateway (for example GCP Cloud NAT): the gateway bills
per gateway-hour and per gigabyte processed, on top of normal egress, and a
CI fleet pulls tens of gigabytes per host per day. Inbound transfer into the
cloud is free; you pay normal egress only for what the job uploads
(artifacts, caches, images).

**Reaching the queen: Tailscale, not NAT.** The queen usually lives on a
private network behind a home or office router. The burst drone joins the
tailnet and reaches the queen over it, so the router needs no port forward
and the cloud side needs no VPN gateway. Tailscale does not change how
traffic to the internet is billed; the public IP above is what avoids NAT
charges. Tailscale only carries the drone-to-queen connection.

**Containment.** Burst drones run pull-request code, so treat them as
untrusted:

- Join with an ephemeral, pre-approved, tagged auth key (for example
  `tag:borg-drone`). In the tailnet policy, allow that tag to reach only the
  queen's port. Nothing else on the tailnet should be reachable from it.
- Enroll through [tailnet enrollment](#tailnet-enrollment) with that tag in
  `allowed_tags`, so no token is copied onto the VM by hand.
- Create the VM without cloud credentials: no service account and no API
  scopes, so a job cannot read the metadata server for a cloud token.
- Put the VM on a dedicated network with no inbound firewall rules. Manage it
  through the cloud's identity-aware tunnel or Tailscale SSH.
- Do not serve deploy-trust work (image publishing, promotion, anything that
  signs or holds production credentials) from a burst drone that also runs
  pull-request jobs.

**Lifetime.** Use spot or preemptible capacity with a hard maximum run
duration and delete-on-termination, so a forgotten drone costs at most one
run duration. On GCP:

```sh
gcloud compute instances create burst-drone-1 \
  --machine-type=c2d-highcpu-56 --provisioning-model=SPOT \
  --instance-termination-action=DELETE --max-run-duration=12h \
  --no-service-account --no-scopes \
  --network=borg-burst --subnet=borg-burst \
  --image-family=ubuntu-2404-lts-amd64 --image-project=ubuntu-os-cloud
```

The VM keeps its ephemeral external IP by default; do not pass `--no-address`
unless a NAT gateway exists, which this setup avoids. If the cloud reclaims a
spot VM mid-job, GitHub fails that job when its runner stops responding; rerun
it. Queued jobs are unaffected and go to other drones.

#### Additional estimated dollar ceilings

The `budget` block can also set **both**
`max_estimated_usd_per_day` and `max_estimated_usd_per_month`.
When present they are enforced *in addition to* the VM-hour caps:
both already-accrued VM time and the configured `reserve_hours` for
existing and prospective drones must fit under each dollar ceiling.
A crossed cap drains owned VMs and prevents new creation, preserving
running leases until they complete.

Dollar limits use `burst.hourly_cost_usd`, an **operator estimate**
that must be conservative enough to include the expected Spot compute,
boot disk, public IP and traffic overhead. Unlike cloud billing budgets,
the estimated ceiling **cannot guarantee a maximum invoice**: real prices
vary, network egress is workload-dependent and the simulator forgets its
created VMs on queen restart. Live cloud provisioning remains unsupported
in this build; implement persisted accounting and provider-side spend
controls before enabling it.

If only one dollar limit is configured, or a value is nonfinite, zero,
negative, or the daily limit exceeds the monthly limit, config
validation fails. With neither limit configured, old VM-hour-only
behavior is preserved. The status `burst.budget` object and
`borg_burst_budget_estimated_usd{window="day|month"}` metric expose
these configured dollar estimates without credentials.

#### Burst rule (`burst`, dry-run)

The `burst` block decides when the queen would add or remove burst drones.
This build only simulates it: config validation rejects `dry_run: false`, and
no VM is created or deleted. Each pass it:

- weighs the unmet jobs borg must place in `scale_up.classes` (`main` and `pr`
  weights; deploy jobs never count, because burst drones have shared trust),
  ignoring a class with more than `max_free_slots` free slots;
- would create one drone (`step`) once that weight stays at or above
  `min_weighted_unmet` for `sustained_secs`, unless a drone is still
  provisioning, `max_drones` is reached (drones made by hand with the burst
  prefix count), or the day or month drone-hour budget lacks `reserve_hours`
  of headroom for every live drone plus the new one;
- would drain a drone idle for `idle_secs`, near its maximum run time
  (`retire_margin_secs`) or once a budget is spent, and delete it on a later
  pass only when it holds no lease.

What it would do is logged as `burst_would_create`, `burst_would_drain`,
`burst_would_undrain` and `burst_would_delete` health events under the drone
name `burst`. `/status` `burst` shows the demand weight, the outcome of the
last pass (`no_demand`, `sustaining`, `provisioning`, `at_max_drones`,
`no_headroom`, `over_budget` or `create`) with a count per outcome since the
queen started, simulated spend in drone-hours and dollars, and the simulated
VMs. Metrics: `borg_burst_dry_run`, `borg_burst_drones{kind}`,
`borg_burst_drone_hours{window}`, `borg_burst_budget_drone_hours{window}`,
`borg_burst_weighted_unmet`, `borg_burst_demand_holds`,
`borg_burst_passes{outcome}`.

Simulated drones enroll 12 minutes after creation and never take a job, so
while demand lasts, dry-run spend is the most the rule would allow, not what
real drones would cost. The simulation is in memory: a queen restart forgets it.

### Deploy-trust drone

Deploy-scope jobs (labels ending `-deploy`) run only on a drone whose trust is
`deploy` or `deploy-only`. Without one they queue indefinitely
(`queued_deploy` in `/status`). A deploy-only drone is a separate, on-demand
VM that never takes pull-request or main work. Do not reuse a burst drone.

Create the VM. Standard provisioning, no max run duration, no service account,
the same dedicated network, and an ephemeral external IP:

```sh
gcloud compute networks subnets create borg-burst-usc1 --network=borg-burst \
  --region=us-central1 --range=192.0.2.0/24   # replace with your private subnet range
gcloud compute instances create borg-deploy-1 --zone=us-central1-c \
  --machine-type=n2d-standard-8 --provisioning-model=STANDARD \
  --no-service-account --no-scopes \
  --network=borg-burst --subnet=borg-burst-usc1 \
  --image-family=ubuntu-2204-lts --image-project=ubuntu-os-cloud \
  --boot-disk-size=200GB --boot-disk-type=pd-balanced
```

Use Ubuntu 22.04: iac `scripts/install-owned-runner-toolchain.sh` refuses
24.04. Zones can be out of capacity (`stockout`); try another zone in the
region. Reach the VM with `gcloud compute ssh --tunnel-through-iap`.

Install the toolchain and arm64 emulation:

```sh
sudo useradd -m -s /bin/bash runner          # the script checks as this user
sudo apt-get install -y shellcheck qemu-user-static binfmt-support
sudo bash install-owned-runner-toolchain.sh  # docker, gh, node 22, rust, kani
sudo docker run --rm --platform linux/arm64 alpine uname -m   # aarch64
```

The script ends with `sudo: a password is required` from its
`sudo -n true` check on the `runner` user. Every tool check before it has
passed; the failure is expected and the user stays without sudo.

Join the tailnet with an ephemeral, pre-approved auth key tagged
`tag:borg-deploy` (its ACL reaches the queen on 443 only; mint it with the
Tailscale OAuth client in 1Password). Do not use `tag:borg-drone`, which is for
drones that run pull requests:

```sh
sudo tailscale up --auth-key=file:/tmp/ts.key --hostname=borg-deploy-1 \
  --advertise-tags=tag:borg-deploy
shred -u /tmp/ts.key
```

Leave MagicDNS on: the queen is addressed by name. The queen's
`tailscale serve` publishes its HTTPS URL on port 443, proxying the LAN
listener. Use that URL in place of `https://queen.example.test` below; the drone needs no
`--allow-insecure` and no `--ca-cert`.

Enroll and install. Use the queen's own binary on the VM; it is a glibc 2.35
build and runs on 22.04 and 24.04:

```sh
# on the Proxmox node hosting the queen container (CT 102)
pct exec 102 -- /usr/local/bin/borg queen enroll --config /etc/borg/queen.json borg-deploy-1 > drone.token
# copy the binary and token to the VM, then
sudo ./borg drone install --queen https://queen.example.test \
  --token-file ./drone.token --trust deploy-only --max-vcpu 8 --max-mem-mib 24576
shred -u drone.token
```

Check `/status`: `borg-deploy-1` is `fresh`, `accepting`, `trust:
deploy-only`, and `queued_deploy` falls as leases start. Neither the queen
config nor the queen binary changes.

### Starvation guard

When demand for a class has waited longer than `starvation_after_secs`, the
allocator places it first. If it still does not fit, it holds back the drone
that can free up soonest (fewest in-flight leases, ties by name) and grants no
smaller lease there until the starving lease fits. Held drones are listed in
`/status` under `reserved_for`.

### Preemption

Off unless the queen config has a `preemption` block:

```json
"preemption": { "after_secs": 600, "max_per_pass": 1, "min_victim_age_secs": 60 }
```

`after_secs` is how long the oldest queued job of a `(class, scope)` must have
waited, `max_per_pass` caps cancellations in one queen pass, and
`min_victim_age_secs` is how long a job must have been running before it can be
cancelled. All three must be greater than 0.

When demand the allocator could not place has waited `after_secs`, the queen
cancels one running lease whose scope ranks strictly below the starving
demand's scope (a `pr` lease for `main` demand; the order is the `Scope`
order). It only picks a lease whose drone could hold the starving class once
that lease's resources are freed, and only a lease whose in-progress job GitHub
reports on its runner, since the job's start time sets its age. Among the
candidates it takes the lowest-priority scope, then the smallest class, then the
most recently started lease. A lease that is already being cancelled counts as
covering the starving demand, so one starving job cancels one lease, not one per
pass.

The queen stamps `preempted_at` on the lease, records the job (repo, run id,
job id) in the `preemptions` table, and returns the lease id in `cancel_leases`
on every heartbeat response to its drone until the lease ends. The drone sends
SIGINT to the runner's process group, which makes the Actions runner report the
job as cancelled to GitHub, waits `preempt_grace_secs` (drone config, default
30, must be greater than 0), then kills the group as it does at `max_job_secs`.
The result is reported with `preempted: true`, and the lease ends normally.
Drones that predate `cancel_leases` ignore it, so update drones before turning
preemption on.

`/status` lists the last 20 preemptions under `preemptions` (lease, drone,
class, victim `scope`, starving `for_scope`, repo, run, job, `at`,
`requeued_at`) and the all-time counts under `preemption_counts`. The metric
`borg_preemptions_total{from_scope,to_scope}` counts them by victim scope and
starving scope.

A preempted job is re-run automatically when both `preemption` and
`supersede_janitor` are configured. After each janitor pass the queen reads the
runs of preemptions with `requeued_at` unset and, once a run has completed,
calls `POST /repos/{repo}/actions/jobs/{job_id}/rerun` with the janitor's token
and sets `requeued_at`. It does at most `max_cancels_per_pass` reruns per pass,
marks rows older than 24 hours without re-running them, treats a 403, 404 or 409
answer as final, retries other errors on the next pass, and in the janitor's
`dry_run` only logs `would rerun`. `/status` shows the counters under `requeue`
and `/metrics` as `borg_requeue_<field>_total` (`checked`, `rerun`, `waiting`,
`stale`, `terminal`, `errors`).

### Admission by CPU pressure

Slots count memory, vCPUs and leases, not CPU load, so several drones sharing
one saturated host can all show free slots. Optional and off unless the queen
config has an `admission` block:

```json
"admission": { "psi_soft_pm": 300, "psi_light_only_pm": 500, "psi_hard_pm": 600, "facts_max_age_secs": 180 }
```

Each drone reports Linux PSI `cpu some avg10` in permille on every heartbeat
(the higher of `/proc/pressure/cpu` and the cgroup file
`/sys/fs/cgroup/cpu.pressure`). The queen compares it with three thresholds:

| Pressure | State | New grants |
| --- | --- | --- |
| below `psi_soft_pm` | `open` | all |
| at or above `psi_soft_pm` | `pr_blocked` | no pull-request work |
| at or above `psi_light_only_pm` | `light_only` | no pull-request work and no main medium or heavy work; main light and deploy work still placed |
| at or above `psi_hard_pm` | `closed` | none |

`psi_light_only_pm` is optional and must lie between the other two. Without
it a drone goes from `pr_blocked` straight to `closed`. Leases already granted
are not touched. A reading older than `facts_max_age_secs`, or no reading at
all, puts no restriction on the drone. The starvation guard obeys the same
rule: a starving pull-request lease is not placed on a soft-blocked drone, and
a starving main heavy lease is not placed on a `light_only` drone. Requires
`0 < psi_soft_pm <= psi_hard_pm <= 1000` and a positive age.

**Hosts without PSI.** A kernel without PSI (or booted with `psi=0`) reports
no pressure. Such a drone would never be blocked, however loaded. The drone
therefore also reports `load_per_cpu_pm`, the 1-minute load average divided by
its usable CPUs in permille, and `cpu_busy_pm`, the share of CPUs busy since the
previous heartbeat from the root cgroup's `cpu.stat`. When PSI is missing the
queen estimates pressure as `(load - cpus) / load`, the share of runnable tasks
that wait for a CPU (load 93 on 16 CPUs gives 828 permille). The estimate is 0
when the load is at or below one task per CPU, or when `cpu_busy_pm` is under
900. In that case the CPUs have idle time, so the run queue is I/O wait or an
lxcfs-computed load average, not CPU contention. PSI always wins when present.

**A null `cpu_psi_pm` usually means an old drone binary.** Drones built before
CPU-pressure admission send no pressure at all, so `/status` shows
`cpu_psi_pm: null` and the drone is never blocked. Before trusting admission,
check that every drone runs a current binary (see
[Updating a drone](#updating-a-drone)). Inside an LXC, `/proc/loadavg` is
lxcfs's estimate and over-reports badly, while the container's
`/sys/fs/cgroup/cpu.pressure` is kernel-computed. An LXC drone with a current
binary therefore admits on real pressure.

`/status` shows `admission: {state, cpu_psi_pm, cpu_load_pm}` per drone
(`state` is `open`, `pr_blocked`, `light_only` or `closed`; `cpu_load_pm` is the
load estimate, used only when `cpu_psi_pm` is null). Placement diagnostics name
the rejection `pr_pressure`, `light_only_pressure` or `admission_closed`.
Metrics: `borg_drone_cpu_psi_pm{drone}`, `borg_drone_cpu_load_pm{drone}` and
`borg_drone_admission{drone,state}`. The build environment contract's
`resources` check is separate and still works.

### Standby runners and deploy fail-open

Deploy jobs (image promotion) should not depend on one queen host. Optional and
off unless the queen config has a `standby` block:

```json
"standby": { "classes": ["medium"], "per_drone": 1 }
```

`classes` must not be empty and `per_drone` is 1 to 4. For every fresh drone
with trust `deploy` or `deploy-only`, the queen keeps `per_drone` leases of
each listed class in the state `standby`: a JIT runner minted with the deploy
labels only (plus the drone's extra labels). The queen sends the JIT config in
the heartbeat response (`standby`) and resends it until the drone lists the
lease id in `held_standby`. The drone does not start it. A standby holds
capacity like a pending deploy lease, is exempt from `lease_ttl_secs`, and is
rotated only when `lease_max_secs` passes; its registration is never reaped.

Promotion rule: when deploy jobs are queued for a class that pending deploy
runners do not cover, the queen does not mint another runner while a standby
exists. It lists the standby in `start_standby` and the drone starts it as an
ordinary lease. When the drone reports the lease in `active_leases` the queen
marks it `pending`, then `running` once GitHub shows the runner busy; the next
tick mints a replacement standby. A standby that ended on the queen's side is
listed in `drop_standby` and the drone forgets it.

Fail-open rule: the drone config key `fail_open_after_secs` (default 120, `0`
disables it; `borg drone install --fail-open-after-secs N`) is how long the
queen may go without answering a heartbeat. After that the drone starts all
the standbys it holds, with no instruction from the queen, and keeps retrying
heartbeats. The drone keeps standbys in memory and, when `state_dir` is set, in
`<state_dir>/standby/<lease id>.json` (mode 0600), so a restart while the queen
is down still has them; the file is deleted when the standby starts. When the
queen returns, `held_standby` reports what is unstarted and `active_leases`
what runs, and the queen reconciles.

A standby is a single-use ephemeral runner, so after fail-open consumes it no
further deploy job runs until the queen returns and mints another. Pair it with
`--trust deploy-only` for a dedicated promotion box.

## Drone

```sh
sudo borg drone install --queen https://queen.example.test:8443 --token <token> --ca-cert queen-ca.pem
borg drone install ... --dry-run     # list changes without applying them
borg drone run --config /etc/borg/drone.json
```

`install` writes `drone.json`; a hand-written `drone.toml` with the same keys
works too.

`install` is idempotent. It prints one line per action (`created`, `updated`,
`unchanged`, `ran`, ...) and restarts the service only if something changed.
It sets up:

* **Linux and WSL:** a `borg` system user (added to the `docker` group when
  that group exists) and a systemd unit. The unit sends `SIGTERM` to the drone
  only (`KillMode=mixed`), so in-flight jobs finish while it drains. WSL needs
  systemd enabled: put `[boot]` and `systemd=true` in `/etc/wsl.conf`, run
  `wsl --shutdown` from Windows, and start the distribution again.
* **macOS:** a LaunchDaemon (`org.borg.drone`) that runs as `--run-as <user>`,
  which must already exist.
* The pinned actions-runner (version and SHA-256 are in `src/drone/pins.rs`),
  downloaded and checksum-verified before it is unpacked, a copy of this
  binary, `drone.json` and the token file (mode 0600).

The queen URL must be `https://`; `ca_cert_path` makes that CA the only trust
root. A plain `http://` URL is refused unless `allow_insecure` is set.

### Separate job user (Linux)

Leases run code from pull requests, so they must not run as the account that
holds the enrollment token. `borg drone install` creates two accounts: the
drone daemon (owns `/etc/borg`, mode 0700, with the config and token at 0600)
and an unprivileged job user (`job_user` in the config; default `borg-job`)
that runs every lease and owns the work and cache directories. The daemon
becomes the job user with `setpriv --reuid ... --init-groups`, which drops every
capability before the job starts; the systemd unit grants the daemon only
`CAP_SETUID`, `CAP_SETGID` and `CAP_SETPCAP` for that. File operations on
job-owned directories (cloning the runner, cleanup, cache collection) also run
as the job user. `tests/job_user.rs` checks, as root, that a lease's uid differs
from the drone's, that the token and config directory are unreadable to it, and
that it holds no capability. Without `job_user` (macOS, or `--no-job-user`)
jobs run as the drone's own account and the drone logs a warning.

### Labels, environment, hooks

* `extra_labels` are added to every runner the drone registers (for lanes that
  still route by label). The queen drops any that equal a job class label, so
  they can never change whether a runner serves main or pull-request jobs.
* `runner_env` is a map injected into every lease's environment (keys starting
  `BORG_` or `ACTIONS_RUNNER_`, and a few others, are reserved).
* `job_started_hook` and `job_completed_hook` are absolute paths ending in `.sh`,
  exported as `ACTIONS_RUNNER_HOOK_JOB_STARTED` and `ACTIONS_RUNNER_HOOK_JOB_COMPLETED`.
* `borg drone run --token-file F` and `borg drone install --token-file F` read
  the token from a file instead of the command line.

### Drone trust

`trust` in the drone config is `"shared"` (default), `"deploy"` or
`"deploy-only"`. A `deploy-only` drone serves deploy-scope leases and nothing
else: no main, no pull-request work, no work units. The drone
reports it in every heartbeat and `/status` shows it per drone. The queen
places deploy demand only on `deploy` drones, never places pull-request demand
or work units on them, and may place main demand on either. Set it on the
host, for example `"trust": "deploy"` in `drone.json` or
`borg drone install --trust deploy`; nothing is set on the queen. A deploy
drone should be a host that no pull-request job has touched, since the
invariant only covers what the queen schedules.

### Owner caps

`--max-vcpu`, `--max-mem-mib` and `--max-leases` (config keys of the same
names) are sent with every heartbeat. The queen allocates within the capped
budget, so an owner can say "this PC gives at most 8 vCPUs and 24 GiB".

### What a lease does

1. The pinned runner install is cloned into `<work_root>/slot-<N>` (hard
   links where possible) and `run.sh` starts in its own process group. The JIT
   config is passed in `ACTIONS_RUNNER_INPUT_JITCONFIG`, never on the command
   line, so it is not visible in `ps`; the runner masks it and removes it from its
   environment. Jobs see `BORG_LEASE_ID`, `BORG_DOCKER_PREFIX`,
   `COMPOSE_PROJECT_NAME` (both `borg-<lease_id>`) and `BORG_CACHE_ROOT`.
2. When the runner exits, or `max_job_secs` passes, the process group is
   killed, and the work directory (including `_diag`) is deleted. If the queen
   preempts the lease (see [Preemption](#preemption)), the group gets SIGINT
   first and is killed after `preempt_grace_secs` (default 30).
3. Docker containers, networks and volumes are removed only if `docker
   inspect` shows they carry the label `borg.lease=<lease_id>`, the compose
   project `borg-<lease_id>`, or a name that is exactly that prefix or starts
   with it plus `-` or `_`. There is no global prune. Jobs that start their own
   containers should name them with `$BORG_DOCKER_PREFIX` (docker compose does
   this through `COMPOSE_PROJECT_NAME`).
4. Per-job cleanup leaves tool caches under `cache_root` intact.
5. The exit code, duration and peak RSS of the runner's process group (Linux)
   are reported to the queen.

### Host caches

The drone owns one stable directory per cache kind under `cache_root`. The
path is identical for every lease on the host, is exported to every job, is
never touched by per-job cleanup, and is size-capped by least-recently-used
garbage collection that runs only while no lease is running. `cache_root` and
`work_root` must not contain one another. Everything is configured in the
`caches` section of the drone config; every kind except `git` is on by default.

| Kind | Environment for jobs | Default cap | Collected |
| --- | --- | --- | --- |
| `sccache` | `SCCACHE_DIR`, `SCCACHE_CACHE_SIZE` (and `SCCACHE_SERVER_PORT` with the optional host server) | 25% of free disk at start, at most 100G | never by the drone; sccache enforces its own size |
| `cargo` | `CARGO_HOME` (registry and git), `CARGO_INSTALL_ROOT` per lease, `BORG_RUSTFLAGS` and `RUSTFLAGS` | 20G | crates, unpacked sources, git db and checkouts |
| `bazel` | `BAZELISK_HOME`; `--disk_cache` and `--repository_cache` via a managed block in `~/.bazelrc` | 50G | `disk/cas`, `disk/ac`, repository cache entries |
| `pnpm` | `PNPM_STORE_DIR`, `npm_config_store_dir` | 20G | store files (the index is kept) |
| `go` | `GOMODCACHE`, `GOCACHE` | 20G | build cache only; the module cache is read-only trees and is left alone |
| `uv` | `UV_CACHE_DIR` | 10G | entries of each versioned bucket |
| `mise` | `MISE_DATA_DIR` | 20G | installed tool versions and downloads; shims are kept |
| `git` | `BORG_GIT_MIRROR_<OWNER>_<NAME>` per mirror | none | never (objects only accumulate) |

```json
{
  "caches": {
    "git": { "mirrors": ["example-org/app"], "remote_base": "https://github.com" },
    "sccache": { "max_size": "50G" },
    "bazel": { "max_size": "40G", "bazelrc": "/var/lib/borg/.bazelrc" },
    "uv": { "enabled": false }
  }
}
```

Notes on each kind:

* **Garbage collection** removes whole entries (a crate, a content-addressed
  blob, an installed tool version), oldest use first, until the collectable data
  is at most 90% of the cap. It takes a lock that every lease holds for its whole
  life, so it never overlaps a job; under constant load it simply waits for an
  idle moment, and caps are soft in the meantime.
* **cargo** shares only the registry and git data. `cargo install` is pointed at
  a per-lease root so no installed binary ever leaks between leases.
* **bazel**: the drone writes a marked block into the service user's `~/.bazelrc`
  (or `caches.bazel.bazelrc`) and leaves the rest of the file alone. The user rc
  is read after the workspace rc, so these two flags win. Bazel's disk cache is
  safe for concurrent builds: entries are written to a temporary file and renamed
  into place, and a collision between processes is harmless. A collected entry is
  a cache miss.
* **go**: the module cache is never collected; the build cache is.
* **uv** and **mise** collection is coarse: it removes entries those tools
  re-create on demand. mise serializes installs of one tool version with a lock
  file.
* **git mirrors**: for each `caches.git.mirrors` entry (`owner/name`) the drone
  keeps a bare mirror of the branches and tags at
  `<cache_root>/git/<owner>/<name>.git`, refreshed every
  `caches.git.interval_secs` (default 600) and before a lease starts (at most
  once per 15 seconds). The variable name is upper-cased with every other
  character replaced by `_`, and it is set only while the mirror exists, so a job
  can test it and fall back to a normal clone:

  ```
  if [ -n "${BORG_GIT_MIRROR_EXAMPLE_ORG_APP:-}" ]; then
    git clone --reference "$BORG_GIT_MIRROR_EXAMPLE_ORG_APP" <url> src
  else
    git clone <url> src
  fi
  ```

  Mirrors fetch from `remote_base` (default `https://github.com`) plus
  `<owner>/<name>.git`. The drone holds no long-lived GitHub credentials and no
  App key. For a private repository either let the queen hand it a short-lived
  read-only token (see Mirror credentials below), give the service user a
  credential helper, or set `remote_base` to an ssh base such as `git@host:`
  with a read-only deploy key. A failed refresh keeps the previous mirror and
  never blocks a job.

  **Mirror credentials.** When deploy keys are disabled, a second GitHub App
  (installed on the organization with repository permission Contents: read)
  can refresh private mirrors. Its key stays on the queen. Add to `queen.toml`:

  ```toml
  [mirror_auth]
  app_id = "12345"
  private_key_path = "/etc/borg/mirror-app.pem"
  repositories = ["app"]
  ```

  `repositories` are bare names in `github.org` (no slashes). Set
  `BORG_MIRROR_APP_PRIVATE_KEY` to the PEM text instead of `private_key_path`
  to pass the key in the environment. Without a `mirror_auth` block nothing
  changes. The queen mints an installation token limited to `contents: read` on
  those repositories, which GitHub expires after one hour, keeps it, and mints a
  new one when under ten minutes of life remain. Every heartbeat response then
  carries `mirror_credential` (`token`, `expires_at`, and the `owner/name`
  repositories it covers), so resending is the refresh. The drone keeps it in
  memory only, replaces it on each heartbeat and drops it at `expires_at`. For a
  repository in the credential, and only when `caches.git.remote_base` is an
  `https://` base, the drone fetches with `git -c http.extraheader=...`: the
  token is never written to the mirror's config, the stored remote URL stays
  credential-free, and an ssh base ignores the credential. While a fetch runs
  the header is visible in that drone's process list. The queen speaks plain
  HTTP, so run the drone-to-queen channel over TLS or a trusted LAN. `GET
  /status` shows `mirror_auth` as `{"configured": true, "expires_at": <unix
  seconds>}` and never the token; the queen never logs it. The first
  credential after a drone starts refreshes its mirrors at once. Drones need a
  borg build that understands `mirror_credential`; older drones ignore it.

#### Slots, path remapping and compile-cache keys

A lease runs in `<work_root>/slot-<N>` and takes the lowest free slot
(`slots` in the drone config, default `max_leases`, else 16; it is also the most
leases that run at once). The path never contains the lease id, concurrent
leases always get distinct slots, and a slot is fully cleaned before it is
reused, so the next lease in a slot sees exactly the same absolute paths. The
lease-to-slot mapping is logged (`lease X -> slot N`) and kept in drone state.
Jobs see `BORG_SLOT`.

Jobs also get `BORG_RUSTFLAGS` and `RUSTFLAGS`: `--remap-path-prefix` flags that
map **every** slot directory onto `/borg/work`, and `CARGO_HOME`, its registry
and its git data onto fixed virtual prefixes. The string lists all slots, so it
is byte-identical for every lease (a per-slot flag would itself differ between
slots and change the cache key). The same flags are written to
`$CARGO_HOME/config.toml` as `[build] rustflags`, as a fallback. Two cautions:

* Cargo ignores `build.rustflags` and `target.*.rustflags` from config files
  whenever `RUSTFLAGS` is set. A workflow that sets its own `RUSTFLAGS` replaces
  ours; it should append the remap instead, e.g.
  `RUSTFLAGS: "-D warnings ${BORG_RUSTFLAGS}"`. Repositories that depend on
  config-file rustflags can turn the export off with
  `caches.cargo.export_rustflags: false`.
* The remap does **not** make workspace crates hit the compile cache across
  different slots. sccache hashes the compile's working directory and every
  `CARGO_*` variable, including `CARGO_MANIFEST_DIR` (read from sccache's
  `generate_hash_key`, v0.16), and no rustc flag changes those. What slots buy is
  that a *reused* slot recompiles at identical paths and hits, and that
  registry dependencies, whose paths are in the shared `CARGO_HOME`, hit from any
  slot. `tests/sccache_slots.rs` compiles one crate in slot 0, again in a cleaned
  slot 0, and in slot 1, and prints the hit counts for each.

sccache keeps hit and miss counters only in the server's memory, so after a
lease's own server exits there is nothing on disk to read. Set
`caches.sccache.server: {"port": 4226}` to have the drone run one long-lived
sccache server per host (its configuration then applies to all jobs, and a job
whose sccache version differs from the host binary's restarts it); the drone
reads its totals with `sccache --show-stats --stats-format json` and reports
hits and misses for `sccache` in heartbeats, `/status` and `borg_cache_hits`.
Without it, `hits` and `misses` are null and each cache's `counters` says
`unknown`. mono's `setup-sccache` action starts one server per job on its own
socket, so the real counts for those builds are in each job's log, in the
`sccache --show-stats` step. `counters` is `reported` when the numbers are
real, and `not_tracked` for kinds that have no hit counters.

`size_status` says whether `size_bytes` covers the whole cache. `measured`
means every directory was listed. `partial` means some directories could not
be listed by the drone's user, so the size is a lower bound and the drone logs
a warning. With a separate job user, sccache creates its directories `0750`
and owned by the job user. `borg drone install` therefore adds the drone's
user to the job user's group. On a drone installed before that change, run
`usermod -aG borg-job borg` and restart the drone between jobs (a self-update
restarts it too). Until then, `/status` shows `partial` and a size far below
the real one. On a GCP burst drone on Oct 16, `du` as root measured 4.3G in
`sccache` while the drone reported 852K. `unknown` means the drone is older
than these fields.

Every heartbeat carries each cache's size, cap, evicted bytes and last
collection time (and, for git, how many leases were given a mirror and how many
ran without one). The queen shows them under each drone in `/status` and as
`borg_cache_*` metrics. Sizes are re-measured every
`caches.maintenance_interval_secs` (default 600) and after leases.

`borg drone install` takes `--git-mirror owner/name` (repeatable) and
`--git-remote-base`; edit `/etc/borg/drone.json` for the rest.

On start the drone deletes leftover lease directories and borg-labelled docker
resources from a previous run. On `SIGTERM` it stops accepting leases and waits
up to `drain_timeout_secs` for running jobs before stopping them.

## Updating a drone

A drone follows the queen's `desired_version` (see [Quorum rollout](#quorum-rollout)) between
jobs, in either direction. When the version differs it stops accepting leases,
finishes the jobs it holds, and then asks the updater: it writes `update-request`
into its state directory (`state_dir` in `drone.json`). The drone itself cannot
replace its binary, because it runs unprivileged and stopping it is what drains
it. A root-owned path unit (`borg-update.path` on Linux, the `org.borg.update`
launchd daemon on macOS) starts `borg update --from-request`, which:

1. fetches the release manifest and downloads every artifact it needs, checking
   each SHA-256 against the manifest **before** anything is stopped;
2. stops the drone service (which drains running jobs), swaps the binary
   atomically and keeps the previous one as `borg.prev`;
3. starts the service and waits for a fresh `health.json`, which the new drone
   writes after each heartbeat the queen answered;
4. rolls back to `borg.prev` if the service does not start or does not report
   healthy within `health_timeout_secs`.

The outcome is left in `update-result.json` and sent to the queen in the next
heartbeat as `update_report`. A target that failed or rolled back on a host is
not retried there until the queen asks for a different version. A queen that is
down is retried with growing delays, and job results are retried for several
minutes after a queen outage.

The updater reads `/etc/borg-update/update.json`, which only root can write, so
a compromised drone cannot point it at another release. The manifest is a JSON
document (`schema`, `version`, per-platform `url` and `sha256` for
`linux-x64-musl`, `linux-arm64-musl` and `darwin-arm64`, and an optional pinned
actions-runner). `https://` is required unless the config sets `allow_insecure`.
There are no signatures: the trust anchor is HTTPS to the configured manifest
URL plus the pinned digests.

```text
borg update --check          # what would change
borg update --version 0.5.0  # install exactly this version
borg update --runner-only    # replace just the pinned actions-runner, no restart
```

`borg drone install` writes the updater configuration and units;
`--update-manifest-url` points them at another manifest and `--no-self-update`
skips them. Drones installed before this feature have to be updated once by
rerunning `borg drone install` with the new binary.
