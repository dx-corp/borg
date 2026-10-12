# Execution capability admission

The build environment contract can require execution proofs in addition to
reported tools and capacity. Probes run through the same job-user executor used
by jobs. Service-user `docker info` remains inventory and does not prove that a
job can run a container.

The named capabilities are sandbox namespace execution, Docker execution, work
filesystem writability, and cache filesystem writability. Each reported proof
keeps its own observation timestamp. Failed, absent, or stale required proofs
make the affected class ineligible. Classes with no capability requirements
retain their existing admission behavior.

Configure required capabilities in the queen's existing `env_contract` block,
globally or per class. Class lists replace the global list. The `capabilities`
check supports `required`, `advisory`, and `off`, like other contract checks.
Use `/status` to inspect the reason before changing workload routing.

```toml
[env_contract]
enabled = true
required_capabilities = ["work_write", "cache_write"]

[env_contract.classes.heavy]
required_capabilities = ["sandbox", "docker", "work_write", "cache_write"]
```

Retain the other checks and thresholds in the existing queen configuration.

In the drone configuration, select an image already loaded on that host:

```toml
[capability_probe]
timeout_bin = "timeout"
timeout_secs = 5
bwrap_bin = "/usr/bin/bwrap"
docker_image = "example/smoke:local"
```

Drone `capability_probe` settings choose the timeout binary, timeout duration,
bubblewrap binary, and an optional Docker smoke image. The default command
timeout is five seconds. On Unix, Borg starts each probe in a private process
group. A trusted shell supervisor kills that group when timeout sends TERM;
this does not depend on the timeout utility's final KILL reaching descendants.
The utility's two-second KILL escalation remains a fallback. macOS installations
using GNU coreutils can configure `gtimeout`. Executors without private process
group support report unknown without starting a smoke command.

The Docker image must already be present and contain `/bin/true`. Borg uses
`--pull=never`, disables container networking, and removes its uniquely named
smoke container after execution. Without a configured image, Docker execution
is reported as unknown. No probe installs packages or changes AppArmor,
namespace, or host permission settings.

Work and cache probes create, write, read and remove a private temporary child
of the configured root. Smoke commands and cleanup are bounded, discard output,
and reuse observations for a minute. Refreshes run in the drone's existing
background task set; heartbeats send the cached proofs without waiting for a
smoke test. The first heartbeats may report no proof, which keeps required
capabilities ineligible until the refresh completes. Drain starts no refreshes
and joins those already running, including their cleanup.

If a refresh cannot start, a cached proof retains its original observation
time. A completed failing smoke test records a fresh failure. Cleanup is best
effort when the Docker daemon is unresponsive.

Deploy the new drone binary first, verify capability observations, then add
requirements for the classes whose workloads need them. Older drones cannot
satisfy a required proof they do not report.
