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

The account-store root must be an absolute path. The client walks it from `/`
one component at a time with no-follow descriptor opens; every ancestor must
be owned by root or the client UID and must not be group- or other-writable.
The final directory must be owned by the client UID and have exact `0700`
mode. Supplying a relative path, a duplicate, `.` or `..` component, a
symlink, a writable ancestor, or a different final mode fails closed.

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

## Automated privileged Linux gate

The required Linux CI gate runs the production
`turso-mysql-checkpoint-authority` and `turso-mysql-offline-provision`
binaries in a pinned Docker image. Inside the container it starts the service,
the authorized CLI/client, and a foreign client as separate numeric UIDs. The
authorized and foreign clients share the socket group, so the test proves that
kernel `SO_PEERCRED`, rather than socket-group access, accepts the configured
client and rejects the foreign peer. It also verifies the authority state root
and account root are `0700`, the socket directory is `0710`, the endpoint is
`0660`, a real CLI `initialize` and `reconcile` complete, and `SIGTERM`
removes the service endpoint. The fixture is
`scripts/test-checkpoint-authority-cross-uid.sh`; it requires Linux ELF
artifacts and a working Docker daemon.

## Initialize or reconcile an account store

On Linux and macOS, `turso-mysql-offline-provision` is the standalone client
for the first account generation and for recovery of its retained initialization
journal. It is not a MySQL server command and it must run as the client UID,
not as the authority UID. `--authority-service-uid` must name a different UID;
the client verifies that UID through kernel peer credentials.

All common options are required and have no defaults:

```text
--account-store-root PATH
--authority-id ID
--authority-socket PATH
--authority-service-uid UID
--authority-rpc-timeout-ms MILLISECONDS
--coordination-timeout-ms MILLISECONDS
```

`initialize` also requires every account option below. The three password
source options are mutually exclusive; exactly one is required.

```text
initialize --username NAME --global-connect true|false --global-list true|false \
  --disabled true|false \
  (--password-tty | --password-stdin | --password-fd N) \
  --password-input-timeout-ms MILLISECONDS \
  [--allow-empty-password]
```

`--authority-id` is validated independently as the server checkpoint ID and
the authority wire ID. `--authority-socket` must be an absolute Linux/macOS
safe pathname (at most 103 bytes). The authority RPC timeout bounds one GET or
CAS request/response. The coordination timeout bounds waits for the
provisioning lock and a reconciliation GET under one absolute deadline. It is
not a total wall-clock timeout and cannot interrupt a filesystem sync, rename,
or directory sync that has already begun.

Initialize one enabled or disabled account with only global `Connect` and
`List` permissions:

```bash
cargo run -q -p turso_mysql_offline_provisioner \
  --bin turso-mysql-offline-provision -- \
  --account-store-root /var/lib/turso-mysql/accounts \
  --authority-id account-store \
  --authority-socket /run/turso-mysql-checkpoint/authority.sock \
  --authority-service-uid 991 \
  --authority-rpc-timeout-ms 1000 \
  --coordination-timeout-ms 1000 \
  initialize \
  --username admin \
  --global-connect true \
  --global-list false \
  --disabled false \
  --password-tty \
  --password-input-timeout-ms 30000
```

Use the same common options to reconcile after an interrupted initialization:

```bash
cargo run -q -p turso_mysql_offline_provisioner \
  --bin turso-mysql-offline-provision -- \
  --account-store-root /var/lib/turso-mysql/accounts \
  --authority-id account-store \
  --authority-socket /run/turso-mysql-checkpoint/authority.sock \
  --authority-service-uid 991 \
  --authority-rpc-timeout-ms 1000 \
  --coordination-timeout-ms 1000 \
  reconcile
```

Do not pass `--password-input-timeout-ms` to `reconcile`: it has no password
input and therefore no password-input deadline.

`initialize` requires exactly one password source. `--password-tty` opens the
controlling terminal, disables echo, and asks twice. On every return it discards
unconfirmed terminal input with `tcflush(TCIFLUSH)`, restores echo, and restores
the prior `SIGINT`, `SIGTERM`, and `SIGHUP` handlers. Those three signals cancel
the prompt only after that cleanup has completed. `--password-stdin` and
`--password-fd N` accept only a FIFO or Unix socket; they reject regular files,
terminals, device files, and other descriptor types. `--password-fd N`
additionally requires an inherited non-terminal descriptor `N >= 3`; the tool
duplicates it and never closes the caller-owned descriptor. It temporarily sets
`O_NONBLOCK` while polling the selected stream to the absolute password-input
deadline, then restores the exact original file-status flags before returning.
Raw input is bounded to 4096 bytes and rejects NUL, CR, and LF. Password bytes
never appear in an argument, output, or diagnostic. An empty password is
rejected unless `--allow-empty-password` is explicitly present.

`--password-input-timeout-ms` is required for `initialize` and creates one
absolute deadline for both terminal entries or the selected stream read. It is
separate from `--coordination-timeout-ms`: password collection does not consume
the provisioning-lock/reconcile-GET deadline. `reconcile` has no password
input and does not accept this option.

On success the tool writes the fixed line `offline provisioning completed` to
standard output. Help and version exit `0`. All operational diagnostics are
fixed and redacted on standard error, with these exit statuses:

| Exit | Meaning |
|---:|---|
| `0` | Completed, including a no-op reconcile or a safely aborted pre-snapshot journal |
| `2` | Invalid command input or password source |
| `3` | Invalid or unavailable local account-store state |
| `4` | Authority read, peer verification, or persistence failure |
| `5` | A retained or conflicting transition needs reconciliation |

Other Unix targets print a fixed unsupported-platform error. This command does
not create account-store, authority-state, or socket directories.

## Runtime and provisioning behavior

The client configuration must use the same authority ID and socket path and
must pin the authority service UID. Kernel peer credentials, not pathname
ownership alone, verify the service and client processes.

Initialization prepares the complete snapshot and its exact checkpoint, then
durably writes a fixed-name, `0600`, checksummed pending journal before it
publishes the snapshot. It publishes the snapshot with temp-file sync, rename,
and directory sync; performs the authority CAS; reopens the exact replacement;
and only then unlinks and directory-syncs the journal. `Durable` installs the
new generation. A definite `Conflict` during first initialization removes only
the unchanged snapshot inode that the attempt published, so a corrected
initialization can retry. A lost reply, timeout, or other ambiguous result
retains the snapshot and the old/new checkpoint pair for explicit
reconciliation. Retrying that exact CAS is idempotent.

A deterministic process-kill test covers the four high-level durable
boundaries: journal publication, snapshot publication, durable authority CAS,
and journal removal. It stops a child at each boundary with `SIGSTOP`, kills it
with `SIGKILL`, and reopens then reconciles the journal. Separately, test-only
one-shot fault injection covers journal and snapshot publication at all sixteen
write, file-sync, rename, and directory-sync points before and after the
syscall. Each case checks the exact journal/snapshot final state, leaves no
publication temporary file, reopens every published checkpoint exactly, and
reconciles to its expected result. These gates do not cover journal
clear/unlink failures or crashes, and do not yet run the real authority service
at every boundary.

If recovery finds the journal but no snapshot, it reads the authority before
deleting evidence. Only authority `Missing` proves the initial CAS did not
advance and permits journal removal. A replacement checkpoint, another
checkpoint, timeout, unavailability, or invalid response leaves the journal in
place and fails closed. A snapshot-publication error also leaves the journal in
place because rename may have succeeded before a directory sync error was
reported.

The current command creates exactly one account and only its global `Connect`
and `List` flags. It does not create database grants and it does not replace an
existing generation. The legacy library `replace` path has no durable pending
journal, so it does not promise process-crash recovery. Do not use that path
as an operational substitute for this initialization/reconcile workflow.

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

- Add test-only fault injection and crash recovery for journal clear/unlink.
- Run real-service end-to-end recovery at each initialization crash boundary;
  the existing `SIGSTOP`/`SIGKILL` gate uses a deterministic library authority.
- Add journal-backed replacement and database-grant provisioning before
  exposing account mutation beyond first initialization.
- Add TCP/TLS, certificate policy, and a standalone MySQL runtime before making
  a network service claim.
