# MySQL checkpoint authority operations

The checkpoint authority is an experimental local service that prevents an
older account and privilege snapshot from being accepted after a normal
process restart. It is available on Linux and macOS through the
`turso-mysql-checkpoint-authority` foreground binary.

It must run as a dedicated non-root operating-system user. The MySQL runtime
and the offline provisioning tool run as a different user. A service manager
owns process startup, restart, signals, and creation of the users and group;
the binary does not daemonize, change identity, or create its directories.

## Required ownership

Choose one stable authority ID, two UIDs, and one shared GID. The service must
start with the authority UID as its effective UID and the shared GID as its
effective GID.

| Object | Owner | Exact mode | Purpose |
|---|---|---:|---|
| Authority state root | authority UID and private authority group | `0700` | Durable rollback checkpoint |
| Socket directory | authority UID and shared GID | `0710` | Lets the client traverse to the socket without listing the directory |
| Authority socket | authority UID and shared GID | `0660` | Created and removed by the service |
| Authority owner lock | authority UID and shared GID | `0600` | Prevents two owners of one socket directory |
| Account store root | MySQL client UID and its private group | `0700` | Credential and privilege snapshots |

Every ancestor of the state and socket roots must be owned by root or the
authority UID and must not be group- or other-writable. Symlinks and `.` or
`..` path components are rejected. The state root is permanently bound to its
first authority ID.

The shared client UID is allowed to perform both checkpoint reads and CAS
writes. This is intentional for the current runtime and provisioning design:
a process compromised under that UID is inside the trust boundary. Use no
unrelated process under that UID.

## Start the service

All options are required. Values below are examples and must be replaced with
the deployment's stable IDs and paths.

```bash
cargo run -q -p turso_mysql_checkpoint_authority \
  --bin turso-mysql-checkpoint-authority -- \
  --authority-id account-store \
  --state-root /var/lib/turso-mysql-checkpoint \
  --socket-directory /run/turso-mysql-checkpoint \
  --socket-name authority.sock \
  --socket-gid 993 \
  --client-uid 992 \
  --io-timeout-ms 1000
```

The socket pathname may contain at most 103 bytes. The I/O timeout is a
cumulative deadline for one bounded request and response. The service accepts
at most 16 authenticated client connections at once; extra connections are
closed and counted. A partial client request does not block other clients or
shutdown.

Send `SIGTERM` or `SIGINT` for normal shutdown. The service stops admission,
cancels waiting client I/O and lock acquisition, joins every worker, and then
removes only the socket inode created by that run. After an abrupt process
exit, the next owner removes an exact authority-owned, shared-group-owned
`0660` stale socket after acquiring the owner lock. Any other entry at the
socket name stops startup for operator inspection.

## Runtime and provisioning behavior

The client configuration must use the same authority ID and socket path and
must pin the authority service UID. Kernel peer credentials, not pathname
ownership alone, verify the service and client processes.

Provisioning publishes an account snapshot first and then performs an exact
checkpoint CAS. `Durable` installs the new generation. A definite `Conflict`
during first initialization removes only the unchanged snapshot inode that
the attempt published, so a corrected initialization can retry. A lost reply,
timeout, or other ambiguous result retains the snapshot and the old/new
checkpoint pair for explicit reconciliation. Retrying that exact CAS is
idempotent.

The runtime waits for an exact checkpoint before opening the account store and
before each reload. A mismatch, corrupt authority state, missing previously
initialized state, or an unavailable authority fails closed. Existing
sessions may retain their last-good authorization only where the runtime
contract explicitly permits it; new authentication remains blocked until an
exact reload succeeds.

## Backup and restore limits

Do not restore account-store files by themselves and assume they are current.
The authority rejects any snapshot that does not match its durable checkpoint.
Reconcile or reprovision through the checked CAS workflow.

The local authority does not protect against root, kernel, authority-user, or
whole-host compromise. Rolling back both the account root and authority state
to a mutually matching older copy is also outside this guarantee. Protect the
authority state with host controls or add an external rollback-resistant
witness before claiming resistance to that threat.

## Remaining production gates

- Run a privileged cross-UID test with the real service UID, client UID, shared
  group, `0710` directory, `0660` socket, rejected foreign peer, runtime load,
  and provisioning CAS.
- Add deterministic filesystem failure and process-kill tests around file
  sync, rename, directory sync, lost replies, and restart recovery.
- Add service-manager examples only after the deployment platform and package
  layout are chosen.

