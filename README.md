# Borg

Borg runs self-hosted GitHub Actions runners across Linux, WSL, and macOS.
A **queen** watches queued jobs and allocates capacity. A **drone** runs on
each host, starts an ephemeral runner for each lease, and cleans up when the
job finishes. Hosts keep their build caches between jobs.

Runner classes (`light`, `medium`, and `heavy`) define CPU, memory, and Docker
requirements. Host owners can cap the resources they lend to the fleet.

## Install from source

Requires Rust 1.95 or newer. Build and install from the public repository:

```sh
git clone https://github.com/dx-corp/borg.git
cd borg
cargo install --path . --locked
borg probe
```

`borg probe` reports host capacity and available tools as JSON. Add `--compact`
for a single line.

## Start a fleet

### 1. Configure the queen

Create a GitHub App with organization self-hosted runners **read and write**
and Actions **read** access to the repositories it will serve. Install the
App on the organization and save its private key on the queen host.

Save this as `queen.toml`, replacing the organization, repository, App ID,
and key path:

```toml
repos = ["example-org/app"]
db_path = "queen.db"
listen = "127.0.0.1:8080"

[github]
org = "example-org"
app_id = "12345"
private_key_path = "/etc/borg/app.pem"
```

Validate the configuration, enroll a host, and start the queen:

```sh
borg config check queen.toml
umask 077
borg queen enroll --config queen.toml host-a > host-a.token
borg queen run --config queen.toml
```

Enrollment prints the token once, to `stdout`; diagnostics go to `stderr`. Keep
`host-a.token` private. The queen stores its hash.

The queen serves plain HTTP. Put its loopback listener behind an HTTPS
endpoint reachable by the drones. The drone requires HTTPS unless explicitly
configured for an insecure connection; use `--ca-cert` to pin a private CA.

### 2. Install a drone

Copy the host's token and CA certificate to that host, then install the service:

```sh
sudo "$(command -v borg)" drone install \
  --queen https://queen.example.test:8443 \
  --token-file host-a.token \
  --ca-cert queen-ca.pem
```

On Linux, the installer creates a daemon account and a separate job account,
installs the pinned GitHub runner, and writes a `systemd` unit. WSL needs
`systemd` enabled. On macOS, add `--run-as <existing-user>` to select the
LaunchDaemon's account. See the [drone guide](docs/operations.md#drone) for
platform setup and isolation details.

Add `--dry-run` to preview installation. Use `--max-vcpu`, `--max-mem-mib`,
and `--max-leases` to limit what a host lends.

### 3. Route a workflow

With the default label prefix, use `borg-ci-<class>` for default-branch and
merge-queue jobs, and `borg-ci-<class>-pr` for pull requests. Replace `main`
with your repository's default branch:

```yaml
name: Fleet check
on:
  push:
    branches: [main]
  pull_request:
  merge_group:

jobs:
  check:
    runs-on: ${{ github.event_name == 'pull_request' && 'borg-ci-light-pr' || 'borg-ci-light' }}
    steps:
      - run: uname -a
```

Class requirements and the label prefix are configurable. See
[labels](docs/operations.md#labels) and
[class configuration](docs/operations.md#classes-config).

## Operate the fleet

```sh
borg queen drone list --config queen.toml
borg queen quarantine --config queen.toml --reason "maintenance" host-a
borg queen release --config queen.toml host-a
borg report --config queen.toml
```

The queen exposes fleet state at `/status`, Prometheus metrics at `/metrics`,
and job telemetry at `/report`.

| Guide | Topics |
| --- | --- |
| [Configuration](docs/operations.md#config-files) | TOML and JSON, validation, defaults, and redacted secrets. |
| [Scheduling](docs/operations.md#drone-bids-and-demand-pressure) | Bids, capacity, starvation protection, host health, and warm floors. |
| [Work units](docs/remote-execution.md) | Remote-only execution, local fallback, and JSON execution results. |
| [Capability admission](docs/capability-admission.md) | Job-user smoke tests and per-class requirements. |
| [Work-unit fairness](docs/work-unit-fairness.md) | Capacity shares, client rotation, and starvation protection. |
| [Placement diagnostics](docs/placement-diagnostics.md) | Explain recorded placement choices with `borg explain`. |
| [Host caches](docs/operations.md#host-caches) | Cache limits, collection, git mirrors, slots, and compile-cache behavior. |
| [Drone updates](docs/operations.md#updating-a-drone) | Canary rollout, update manifests, health checks, and rollback. |
| [Operations reference](docs/operations.md) | APIs, telemetry fields, enrollment, and optional controllers. |

## Development

From the public repository root:

```sh
cargo build --locked
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

The integration tests share fixtures across two suites. Run service-state tests
with `cargo test --locked --test suite units::`, or host tests with
`cargo test --locked --test host_suite drone::`. The largest module, `queen`,
keeps its own target for focused edits: `cargo test --locked --test queen`.
The compiler-cache test has its own target because it invokes itself by exact test name:
`cargo test --locked --test sccache_slots`.

| Source | Responsibility |
| --- | --- |
| `src/probe/` | Host capacity and tool detection. |
| `src/classes.rs`, `src/allocator.rs` | Runner requirements and lease placement. |
| `src/queen.rs`, `src/github.rs`, `src/store.rs`, `src/api.rs` | Coordination, GitHub integration, persistence, and APIs. |
| `src/drone/` | Heartbeats, runner execution, installation, and cleanup. |

Issues are welcome in this repository. Source changes land upstream in
[dx-corp/mono](https://github.com/dx-corp/mono/tree/main/rust/tools/borg)
and are projected here. Maintainers make code and documentation changes
upstream so the next projection preserves them.

## License

[Business Source License 1.1](../../../LICENSE) (BUSL-1.1).
