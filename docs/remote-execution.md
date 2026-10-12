# Fleet-only command execution

Use `borg run --remote-only` when the command must run on an approved fleet host.
If Borg cannot complete remote execution, it returns exit code 125 and does not
start the command locally. A command that runs remotely and exits nonzero keeps
its command exit code. Without this option, the existing local fallback remains.

```sh
borg run --remote-only --platform linux-x86_64 --wait-secs 120 \
  --timeout 1200 --result-file ./build-result.json -- cargo test -p example
```

`BORG_RUN_REMOTE_ONLY=true` also enables the mode. `--wait-secs` bounds pickup
waiting. After reading unit metadata, the client bounds polling by the pickup
wait plus the queen-admitted command timeout if the queen stops making progress.
Connection and cancellation requests have their own bounded timeouts. An unpublished tree needs `--publish`
so a drone can fetch its bundle. Without a configured queen or client token,
remote-only execution fails.

The optional `--result-file` writes JSON separately from streamed command output.
Its fields include `schema_version`, `requested_tree_sha`, `argv`, `execution`
(`remote`, `local`, or `none`), `provenance` (`new`, `joined`, `reused`, or
`fallback`), `unit_id`, `host`, `exit_code`, `infrastructure_error`, and `receipt`.
A null tree or host means it could not be established. The requested tree does
not describe the mutable checkout used by local fallback.

Borg validates a reused receipt against the submitted repository, tree, command,
platform and toolchain. The receipt digest checks consistency. Configure
authenticated TLS for the queen endpoint. A reused successful receipt names the
original execution host.

An invocation that joins another unit does not cancel that unit when its own
wait ends. Cancellation of a newly created unit is best effort; a client timeout
does not establish that a disconnected drone has stopped executing it.

An unwritable result destination fails before execution. A later write failure
can return 125 after the command ran. If terminal metadata cannot be verified,
Borg preserves the observed remote exit code and records the metadata error;
host and receipt remain unavailable. Provenance may be null before submission.
