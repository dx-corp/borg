# borg

borg is a self-organizing CI fleet agent for self-hosted GitHub Actions
runners. Every machine that joins the fleet runs a drone. A single queen
watches the queue, decides how much of each machine to lend to which kind of
job, and hands drones one-shot (ephemeral, JIT) runner configurations.

The crate contains the pure core and the queen:

| Module | Does |
| --- | --- |
| `probe` | Reports vCPU, memory, free disk, architecture, OS, host kind (WSL, LXC, VM, bare metal), and whether Docker, mise and git are usable. |
| `classes` | Defines the `light`, `medium` and `heavy` runner classes, their resource requirements, and the label scheme. All of it is configuration with defaults. |
| `allocator` | A pure function from queued demand, drone capacity, in-flight leases, warm floors and per-host caps to lease grants with exact label sets. |

| `queen`, `github`, `store`, `api` | The coordinator: polls queued jobs, runs the allocator, mints single-use JIT runner configs, keeps lease bookkeeping in SQLite and serves the drone API plus `/status` and `/metrics`. |

| `drone` | The agent on every machine: heartbeats to the queen, runs each lease as an ephemeral runner in a private directory, cleans up after every job, and installs itself. |

## Build

```
cargo build --locked
cargo test --locked
```

## Probe

```
borg probe            # pretty JSON
borg probe --compact  # one line
```

## Labels

A job asks for a class with a runner label: `<prefix><class>` for jobs from
the default branch or a merge queue, and `<prefix><class>-pr` for pull
requests. The default prefix is `borg-ci-`; set `label_prefix` in the classes
config to change it (for example `example-ci-`). A runner minted for main
demand carries only the bare label, so GitHub can hand it nothing but main
jobs. A runner minted for pull-request demand carries both labels.

## Classes config

`Classes::from_json` accepts partial JSON; omitted fields keep their defaults:

```json
{
  "label_prefix": "example-ci-",
  "heavy": { "mem_mib": 20480, "vcpu": 6, "docker": true }
}
```

## Queen

```
borg queen enroll --config queen.json host-a   # prints a one-time token
borg queen run --config queen.json
```

Example `queen.json` (every field except `github`, `repos` and `db_path` is
optional):

```json
{
  "github": { "org": "example-org", "app_id": "12345", "private_key_path": "/etc/borg/app.pem" },
  "repos": ["example-org/app"],
  "db_path": "/var/lib/borg/queen.db",
  "listen": "127.0.0.1:8080",
  "classes": { "label_prefix": "example-ci-" },
  "starvation_after_secs": 120
}
```

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

Drone requests carry `Authorization: Bearer <token>`. The queen stores only
the SHA-256 of each token.

### Starvation guard

When demand for a class has waited longer than `starvation_after_secs`, the
allocator places it first. If it still does not fit, it holds back the drone
that can free up soonest (fewest in-flight leases, ties by name) and grants no
smaller lease there until the starving lease fits. Held drones are listed in
`/status` under `reserved_for`.

## Drone

```
sudo borg drone install --queen https://queen.example.test:8443 --token <token> --ca-cert queen-ca.pem
borg drone install ... --dry-run     # list changes without applying them
borg drone run --config /etc/borg/drone.json
```

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

### Owner caps

`--max-vcpu`, `--max-mem-mib` and `--max-leases` (config keys of the same
names) are sent with every heartbeat. The queen allocates within the capped
budget, so an owner can say "this PC gives at most 8 cores and 24 GiB".

### What a lease does

1. The pinned runner install is cloned into `<work_root>/<lease_id>` (hard
   links where possible) and `run.sh` starts in its own process group. The JIT
   config is passed in `ACTIONS_RUNNER_INPUT_JITCONFIG`, never on the command
   line, so it is not visible in `ps`; the runner masks it and removes it from its
   environment. Jobs see `BORG_LEASE_ID`, `BORG_DOCKER_PREFIX`,
   `COMPOSE_PROJECT_NAME` (both `borg-<lease_id>`) and `BORG_CACHE_ROOT`.
2. When the runner exits, or `max_job_secs` passes, the process group is
   killed, and the work directory (including `_diag`) is deleted.
3. Docker containers, networks and volumes are removed only if `docker
   inspect` shows they carry the label `borg.lease=<lease_id>`, the compose
   project `borg-<lease_id>`, or a name that is exactly that prefix or starts
   with it plus `-` or `_`. There is no global prune. Jobs that start their own
   containers should name them with `$BORG_DOCKER_PREFIX` (docker compose does
   this through `COMPOSE_PROJECT_NAME`).
4. Tool caches under `cache_root` are never touched.
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
| `sccache` | `SCCACHE_DIR`, `SCCACHE_CACHE_SIZE` | 25% of free disk at start, at most 100G | never by the drone; sccache enforces its own size |
| `cargo` | `CARGO_HOME` (registry and git), `CARGO_INSTALL_ROOT` per lease | 20G | crates, unpacked sources, git db and checkouts |
| `bazel` | `BAZELISK_HOME`; `--disk_cache` and `--repository_cache` via a managed block in `~/.bazelrc` | 50G | `disk/cas`, `disk/ac`, repository cache entries |
| `pnpm` | `PNPM_STORE_DIR`, `npm_config_store_dir` | 20G | store files (the index is kept) |
| `go` | `GOMODCACHE`, `GOCACHE` | 20G | build cache only; the module cache is read-only trees and is left alone |
| `uv` | `UV_CACHE_DIR` | 10G | entries of each versioned bucket |
| `mise` | `MISE_DATA_DIR` | 20G | installed tool versions and downloads; shims are kept |
| `git` | `BORG_GIT_MIRROR_<OWNER>_<NAME>` per mirror | none | never (objects only accumulate) |

```json
"caches": {
  "git": { "mirrors": ["example-org/app"], "remote_base": "https://github.com" },
  "sccache": { "max_size": "50G" },
  "bazel": { "max_size": "40G", "bazelrc": "/var/lib/borg/.bazelrc" },
  "uv": { "enabled": false }
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
  `<owner>/<name>.git`. The drone holds no GitHub credentials: for a private
  repository give the service user a credential helper, or set `remote_base` to
  an ssh base such as `git@host:` with a read-only deploy key. A failed refresh
  keeps the previous mirror and never blocks a job.

What the drone does not do: it does not inject `RUSTFLAGS`. A
`--remap-path-prefix` can only make paths stable if the path it names is stable,
and a lease's work directory contains the lease id; workspace crates therefore
do not share compile-cache entries across leases, while dependencies built from
the shared registry do. It does not collect sccache hit rates, because sccache
servers are job-scoped.

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

## Contributing

Issues are welcome here. Code changes land in the source-of-truth repository
and are projected into this one; pull requests opened directly against this
repository will be overwritten by the next projection.

## License

Business Source License 1.1 (BUSL-1.1). See [LICENSE](LICENSE).
