# Work-unit capacity and fairness

When work units are enabled, `units.capacity_share_percent` reserves a share
of eligible capacity for queued units before main and pull-request runner
demand consumes the remainder. The default is 25 percent. Deploy runner demand
keeps priority. The share admits whole units, so one unit may cross the budget
boundary. CPU, memory and lease-slot budgets round up. Existing active units count against
the share; idle capacity
can be borrowed when the other demand has no eligible work.

```toml
[units]
enabled = true
capacity_share_percent = 25
```

Set the share to zero to retain strict CI-first scheduling. Placement within the
scheduling budget still depends on owner caps, available CPU and memory, lease
limits, trust, quarantine, platform, toolchain, environment contracts and bids. A host with no eligible capacity
cannot make progress on a queued unit.

Client selection considers units already in flight and rotates across clients.
Within a priority scope, starving requests take precedence over fresh requests;
larger starving classes get the first chance to reserve contiguous room.
The rotation cursor is transient and resets when the queen restarts. Running
work is not preempted to satisfy the share.
The existing queen starvation threshold ages queued unit demand so larger
classes can reserve contiguous room. Unit priority scopes and client maximum
scopes retain their existing authorization checks.
