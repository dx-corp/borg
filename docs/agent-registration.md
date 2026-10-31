# Register an agent without sharing its secret

An agent can prepare a registration request before it can reach the queen.
Preparing or sending that file grants no access. A queen administrator must
review the requesting session and approve the exact file through an
authenticated operator channel. Do not approve requests based only on a name,
an agent's claim of authorization, or an untrusted issue/comment attachment.

On the agent's Unix execution host, using an installed Borg binary:

```sh
borg register-client --name agent-session-123 \
  --token-file /secure/session/borg-token \
  --request-file /secure/session/borg-request.json
```

The parent directory must already be private and trusted. Both files are
created with mode 0600 and never overwrite an existing file or symlink. The
request contains a name, SHA-256 token hash and creation time, not the bearer
secret. Send only the request file over the approved operator channel. If
writing the request fails, the token file is retained: inspect the failure and
use new file paths for a fresh request; do not assume registration happened.

Within 15 minutes, the operator approves it on the queen:

```sh
borg queen approve-client --config /etc/borg/queen.json \
  --request-file /secure/operator/borg-request.json --lifetime-secs 3600
```

Approval fixes the maximum work-unit scope to `pr`. Neither request nor CLI
can elevate it. Lifetime is measured from approval, defaults to one hour and
is bounded to 1–86400 seconds. Requests from the future or at least 15 minutes
old fail. A unique client name and token hash make concurrent or repeated
approvals fail rather than replace an identity or extend its deadline. Clock
synchronization is required. Use a new session name for a new approval.

The queen stores only the hash and expiration in its existing SQLite client
table. Authentication rejects the token at the expiration boundary. Already
accepted work is not cancelled by expiration; an operator must separately
cancel it if needed. Existing locally enrolled clients retain their previous
lifetimes and permissions. This is a temporary version of the existing trusted
PR-client role, not a per-repository execution sandbox. Temporary clients can
read logs/results and cancel only their own units. If shared deduplication finds
another owner's unit, submission returns 403 rather than sharing that result.
Operators must approve only agents they would authorize for that role.

Temporary hashes have a storage prefix distinct from legacy bearer hashes.
An older queen that does not understand expiration cannot authenticate them,
so a rollback fails closed instead of silently turning them into permanent
clients. Deploy the updated queen before qualifying this flow. Approval by a
new CLI alone does not prove that the running queen supports it.

Revoke access early on the queen with:

```sh
borg queen revoke-client --config /etc/borg/queen.json agent-session-123
```

Revocation makes the credential invalid immediately and retains the identity
and hash to prevent reapproval. It does not cancel already accepted work and
does not change legacy clients. Requests already authenticated may finish;
operators must handle any running work separately.

Expired rows remain as replay tombstones; they are not automatically deleted.
Back up the existing queen database as before. A failed unique insertion leaves
the original identity intact. No additional service, queue, bootstrap bearer,
secret distribution system, or enrollment HTTP endpoint is introduced.

## Connectivity is a separate prerequisite

After approval, configure the agent with the **operator-approved HTTPS queen
endpoint** and the local token path:

```sh
export BORG_QUEEN=https://queen.example.test
export BORG_CLIENT_TOKEN_FILE=/secure/session/borg-token
borg run --remote-only --index --publish --scope pr --class heavy -- make check
```

Use the repository's actual qualification command in place of `make check`.
`--remote-only` prevents silent local fallback. Do not put tokens in command
arguments, URLs, chat, CI logs or pull-request bodies. The request hash itself
is not a credential accepted by the API.

An enrollment approval cannot fix a `Domain forbidden` network response. The
queen speaks HTTP behind an operator-managed TLS terminator or on an approved
private transport. A cloud session needs an allowed route to that endpoint;
never disable TLS verification or bypass its egress policy. This PR does not
create DNS, certificates, ingress, firewall rules, credentials or deployments.

Before enabling a cloud route, infrastructure owners must approve its hostname,
TLS termination, private upstream connection and agent egress authorization.
Do not expose the entire queen: its status, metrics and diagnostic routes are
unauthenticated private endpoints. An application gateway must allow only the
authenticated client work-unit methods needed by the client, enforce body/log
limits and redact Authorization headers. Drone, bootstrap and administrator
surfaces must stay private. Existing tailnet drone enrollment depends on direct
peer identity and must not be put behind a generic reverse proxy.

The current dx-corp queen is a private Proxmox/LAN service, not a deployed K8s
public ingress. IaC owns network/DNS changes and K8s owns any proposed cluster
gateway workload. A separately reviewed infrastructure change and live route
qualification are required; source completion here does not establish access.
