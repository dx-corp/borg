# Coordination: least running time inside one fleet

How borg and a merge queue should divide work so the fleet spends the least
total machine time per landed change, and so the deploy path never waits
behind pull-request work. This document is the design; the per-feature
mechanics are in the [operations guide](operations.md).

## The problem it solves

A monorepo with many agents produces far more CI demand than any fleet can
run at once. Three things then go wrong at the same time:

1. **Priority inversion.** Jobs that gate a deployment (signing a verdict for
   a landed commit, publishing an image, promoting it) queue behind
   pull-request jobs that will be superseded by the next push.
2. **Waste.** The same tree is tested several times: on the PR head, on the
   merge-queue batch, and again on the landed commit. Runs for stale heads
   keep running. Duplicate runs for one commit start from different events.
3. **False capacity.** Slots count memory and vCPUs, so several drones on one
   saturated host all report room while the host's CPU pressure is near 100%.

Mergify's merge queue attacks the second problem on the Git side (batches,
two-step CI, speculative checks, bisection). Borg attacks all three on the
runner side. The two meet at the runner label.

## Principles

* **Scopes are the priority ladder.** `deploy` before `main` before `pr`
  (`Scope` ordering). Every allocator decision, the starvation guard, the
  pressure signal and preemption read the same ladder.
* **Trust follows scope.** A drone is `shared` (main and pull-request work)
  or `deploy` (deploy and main work). A deploy drone never runs code that a
  pull request produced, and never reads a cache such code wrote.
* **Capacity is CPU pressure, not slots.** Admission reads PSI from each
  drone and sheds the lowest scope first.
* **Work that cannot matter is cancelled.** A run for a head that is no
  longer the PR's head, or a second run of the same workflow for the same
  commit, is stopped before it takes a runner.
* **The queue carries the verdict once.** Heavy suites run once per tested
  tree (the batch), and a landed commit whose tree equals a passing batch
  tree reuses that verdict instead of running again.

## Borg mechanisms

| Mechanism | Where | Status |
| --- | --- | --- |
| Scope ladder `deploy > main > pr`; deploy runners carry only `<prefix><class>-deploy` | `classes.rs`, `allocator.rs` | shipped |
| Drone trust (`shared` / `deploy`); aliases with a scope | `classes.rs`, drone config | shipped |
| Admission by CPU pressure (`admission` block; PSI soft sheds `pr`, hard sheds all) | `allocator.rs`, `queen.rs` | shipped |
| Preemption: starving demand cancels one running lower-scope lease (`preemption` block) | `preempt.rs`, drone `cancel_leases` | shipped, including requeue of the cancelled job |
| Supersede janitor: stale heads (queued, and running with `cancel_running`), same-commit duplicates (`dedupe_same_sha`) | `janitor.rs` | shipped |
| Class right-sizing from measured peak RSS (`leases.peak_rss_mib`) | queen config | per fleet |
| Deploy fail-open: standby deploy runner held by deploy drones while the queen is unreachable | queen mints standby; drone starts it on queen loss | shipped |
| Deploy-only trust: a drone that serves nothing but deploy-scope leases | `Trust::DeployOnly` | shipped |
| Queen armada: several queens, one fenced leader | `lead.rs` (new) | design below |

### Right-sizing classes

A class reserves memory and vCPUs. Reserve what jobs use, not what the
largest job might use: read `peak_rss_mib` per class from the queen's
`leases` table (p90 and max over the last week) and set `mem_mib` just above
the max, `vcpu` at the p50 of what a job keeps busy. Over-reservation shows
up as "free CPU, no free slots" and under-reservation as `failure_class:
host` with OOM in the drone log. Admission by CPU pressure makes the vCPU
reservation a packing hint rather than a hard budget, so it can be set low.

### Preemption and requeue

A lease is only preempted for demand that has waited `after_secs` and that
the allocator could not place. The victim is the lowest-scope, smallest,
most recently started lease on a drone that could then hold the starving
class. The drone sends SIGINT so the Actions runner reports the job as
cancelled, and the queen records (repo, run, job). The requeue step re-runs
that job through the jobs API once its run has completed, with the janitor's
token (it needs `preemption` and `supersede_janitor` both set and respects the
janitor's `dry_run`).

### Deploy fail-open

With the queen `standby` block set, each `deploy` or `deploy-only` drone holds
`per_drone` standby leases per class: a JIT runner minted with the `-deploy`
labels that the drone does not start while heartbeats succeed. The queen
starts one through `start_standby` when deploy jobs are queued for its class.
After `fail_open_after_secs` without a queen answer (default 120) the drone
starts all it holds, so a deploy job can still land. A standby is single use:
after fail-open consumes it, no further deploy job runs until the queen
returns, reconciles it (`held_standby`, `active_leases`) and mints another.
Standby leases are exempt from the pending-lease TTL and are never reaped as
unowned. This is the last line of defence; the armada below is the first.
See [operations](operations.md#standby-runners-and-deploy-fail-open).

### Queen armada (design)

Several queens, exactly one leader at a time.

The lease logic (a record of holder, epoch and expiry in an object store
with generation-match preconditions, renewed every `renew_secs` and taken
over after expiry with `epoch + 1`) is in `lead.rs`, with the decision rules
in its module docs and the two-queen, takeover, split-brain and store-outage
cases in its tests. The epoch is the fencing token: `lead.rs` provides the
check a drone applies to a grant (`fence_ok`) and the runner name format
`<prefix><drone>-e<epoch>-<lease_id>`. A queen that lost the lease stops
minting at once, and the new leader deletes registrations whose name
carries a lower epoch and that no lease it knows owns.

* **Only the leader allocates and mints.** Double minting means duplicate
  runners and double-run jobs, so followers never call the allocator.
  Followers answer `/status` from their last snapshot (marked `follower`),
  accept heartbeats and forward them to the leader (or answer `307` with the
  leader's URL when the drone can reach it).
* **Drones carry a queen list.** `queen_urls: [...]`; a drone tries the next
  URL after `heartbeat_failures` consecutive failures and remembers the one
  that answered. Fail-open for deploy still applies when none answers.
* **State is rebuilt, not replicated.** Everything the leader needs is
  reconstructible: queued jobs and registered runners from GitHub, drone
  capacity and held leases from heartbeats (drones list their active lease
  ids and slots). The takeover tick runs the existing reconcile step against
  GitHub, adopts registrations that heartbeats claim, deletes the rest, and
  only then allocates. SQLite stays per queen; what cannot be rebuilt
  (telemetry history, rollout state, health decay) is best-effort and is
  mirrored to the store on a timer.
* **Tests.** Two leaders with the same store never both mint (the loser's
  CAS fails); a deposed leader's grant with a stale epoch is refused by the
  drone and by the store; failover completes within one lease TTL and the
  new leader adopts every lease the drones still hold.

The design keeps the single-authority property the allocator relies on and
adds nothing a follower has to agree on. The fence is the store's generation
check, which is why the store must offer one.

## Merge-queue side

Borg does not decide what to run; the repository's CI and merge queue do.
The shape that minimizes running time inside the coordination:

1. **Two-step CI.** A pull request runs only a light lane on its head; the
   heavy graph runs on the merge-queue batch (main plus the batch). The
   batch requests main-scope runners, so borg already ranks it above PR work.
2. **Verdict reuse by tree.** A push to the default branch whose git tree
   equals a passing batch run's tree runs nothing and signs the batch's
   verdict for its commit. The squash of the last PR in a batch has that
   tree; the earlier squashes do not. With one pending run per concurrency
   group, those intermediate pushes replace each other and only the newest
   and the one already running cost anything.
3. **Per-job attestation.** Each heavy job signs its own unit on success, so
   a run that is replaced still banks what it finished and the next run tests
   only what is left. Without this, a starved default-branch run grows every
   time it is replaced.
4. **Parallelism sized to the fleet.** `max_parallel_checks` and per-scope
   capacity must not exceed the heavy slots the fleet can give main-scope
   work; speculative batches above that only time-share the same CPUs and
   finish later, while bisection multiplies the waste on failure.
5. **Priority rules.** Red-main fixes and deploy-path fixes enter at the front
   of their scope and never interrupt a running check
   (`allow_checks_interruption: false`), because an interrupted heavy batch is
   pure loss.
6. **Admin merges disable the queue's economics.** A PR merged outside the
   queue while a batch contains it re-cuts the batch, cancels its run and
   leaves the landed commit with no batch verdict to reuse. Either changes
   land through the queue, or the default-branch run is accepted as the one
   heavy run per landed tree and is kept small by 3.

## Measuring

Before and after each change, from the Actions API over a fixed window:
job-minutes by event (`pull_request`, batch PR, `push`, `workflow_dispatch`),
job-minutes cancelled after start, duplicate same-commit runs and their
minutes, queue wait p50/p90 for deploy-path labels, job-minutes per landed
commit. From the queen: `/status` `queue` ages per scope,
`borg_drone_admission`, `borg_preemptions_total`, `borg_janitor_*_total`.
