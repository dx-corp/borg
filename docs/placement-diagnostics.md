# Explaining placement

`borg explain` reads decisions captured by the allocator at placement time.
These records retain the considered hosts, remaining owner-capped resources,
admission rejections, reservations, bid and profile scores, and selected host.
A current `/status` snapshot cannot reconstruct that earlier capacity state.

```sh
borg explain borg-ci-medium-pr --queen https://queen.example.com
borg explain unit-id --queen https://queen.example.com --json
borg explain unit-id --config /etc/borg/queen.toml
```

Remote unit queries use the existing client token from `BORG_CLIENT_TOKEN`,
`BORG_CLIENT_TOKEN_FILE`, or the default client-token file. They return only
that client's diagnostic records. Runner-label queries expose runner decisions.
Local `--config` access reads the queen database as an operator.

A selected host is an allocator decision. The record's outcome separately
reports whether the queen successfully minted a runner or persisted the unit
lease. A failed owning operation must not appear as a completed placement.

The existing queen SQLite database retains at most 1,000 decision records and
returns the newest 20 for a query. Each allocator call captures at most 128
decisions and 128 candidates per decision, retaining the selected host when
candidates are truncated. Each stored decision is limited to 128 KiB.
Truncated candidate lists carry the original count. No command
arguments, environment values or tokens enter placement history. Queries with
no retained decision report that state explicitly.

History is diagnostic: insertion failure is logged without changing scheduling.
Old rows are pruned during insertion, and database startup creates the table
for an existing installation. A queen database backup preserves this history.
