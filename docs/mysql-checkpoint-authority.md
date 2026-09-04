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
| MySQL data root | MySQL runtime UID and its private group | `0700` | Database catalog and database files |
| MySQL socket directory | MySQL runtime UID and its private group | `0700` | Holds the MySQL Unix socket and owner lock |
| MySQL socket | MySQL runtime UID and its private group | `0600` | Created and removed by the runtime |

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

The MySQL data root and MySQL socket directory have the same absolute-path,
no-follow, trusted-ancestor, client-UID, and exact-`0700` requirements. The
runtime creates the MySQL socket with `0600` mode after binding it, rejects an
existing entry at the configured name, and removes only the socket inode it
created. The MySQL Unix listener admits only clients with the runtime's
effective UID; it is not a shared-group endpoint.

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
`turso-mysql-checkpoint-authority`, `turso-mysql-offline-provision`, and
`turso-mysql-server` binaries in a pinned Docker image. Inside the container
it starts the service, the authorized CLI/client, and a foreign client as
separate numeric UIDs. The authorized and foreign clients share the socket
group, so the test proves that
kernel `SO_PEERCRED`, rather than socket-group access, accepts the configured
client and rejects the foreign peer. It also verifies the authority state root
and account root are `0700`, the socket directory is `0710`, the endpoint is
`0660`, real CLI `initialize`, `add-account`, and `reconcile` commands complete,
the authority and account store reach exact revision one with both granted
accounts, and `SIGTERM` removes the service endpoint. It also starts the real
runtime as the authorized client UID, authenticates through an external MySQL
driver, executes ordinary and prepared queries, and verifies `SIGTERM` removes
the MySQL socket. This gate does not yet exercise an interrupted operation
against the real service. The fixture is
`scripts/test-checkpoint-authority-cross-uid.sh`; it requires Linux ELF
artifacts and a working Docker daemon. Normal macOS-built Mach-O artifacts
cannot run inside that Linux container.

## Run the MySQL runtime

`turso-mysql-server` is the foreground Linux/macOS runtime for the local Unix
listener. Start it as the MySQL client UID, the same UID that owns the account
store, data root, and MySQL socket directory. It must not run as the checkpoint
authority UID. `--authority-service-uid` must name the distinct authority UID;
the runtime verifies that UID using kernel peer credentials before it accepts a
checkpoint.

Build the executable with:

```bash
cargo build -p turso_mysql_runtime --bin turso-mysql-server
```

All runtime options are required. This example uses the authority started
above: authority UID `991`, runtime/provisioning UID `992`, and authority ID
`account-store`.

```bash
target/debug/turso-mysql-server \
  --data-root /var/lib/turso-mysql/data \
  --account-store-root /var/lib/turso-mysql/accounts \
  --socket-directory /run/turso-mysql \
  --socket-name mysql.sock \
  --authority-id account-store \
  --authority-socket /run/turso-mysql-checkpoint/authority.sock \
  --authority-service-uid 991 \
  --authority-rpc-timeout-ms 1000 \
  --reload-interval-ms 1000 \
  --max-connections 128 \
  --max-admissions 32 \
  --max-write-bytes 8192 \
  --max-write-frames 16 \
  --checkpoint-timeout-ms 1000 \
  --tls-timeout-ms 1000 \
  --authentication-timeout-ms 5000 \
  --idle-timeout-ms 60000 \
  --query-timeout-ms 30000 \
  --write-timeout-ms 5000 \
  --shutdown-timeout-ms 10000
```

The runtime does not create the three configured directories. Prepare the data
root, account-store root, and socket directory with the ownership and exact
modes in the table above before starting it. The MySQL socket path, and the
authority socket path, must each fit the Linux/macOS 103-byte pathname limit.
`--max-write-bytes` must be at least `4100` bytes (one 4096-byte initial
handshake plus its four-byte packet header) and no more than 64 MiB; `8192` is
a valid small deployment value. `--max-write-frames` must be from `1` through
`4096`, and `--max-admissions` cannot exceed `--max-connections`.
The runtime checks the exact authority checkpoint before opening the account
store and before each periodic reload; missing, mismatched, malformed, or
unavailable checkpoint state blocks new authentication.

Send `SIGTERM` or `SIGINT` for normal shutdown. The signal handler only
requests shutdown so it can wake a blocked accept loop safely. The runtime
then stops admission, drains owned work within `--shutdown-timeout-ms`, and
performs identity-checked removal of its own MySQL socket. A shutdown that does
not drain in time exits with failure.

Runtime diagnostics are fixed, redacted categories. They do not print the
configured filesystem paths, authority ID, authority service UID, account
snapshot, or credentials. Preserve the service manager's stderr capture for
the category, but use filesystem inspection under the documented ownership
rules when an operator needs a path-specific diagnosis.

### Cross-UID runtime gate

The runtime integration test at `mysql/runtime/tests/unix_e2e.rs` is ignored
by default because a normal developer account cannot prove the required
separate authority/runtime UIDs. A privileged Linux fixture must run the real
authority as its service UID and the runtime plus MySQL driver test as the
client UID. The test verifies external MySQL authentication, ordinary and
prepared queries, `SIGTERM` success, and MySQL socket cleanup without a
same-UID test hook.

The Linux CI job builds the runtime binary, then
`scripts/test-checkpoint-authority-cross-uid.sh` invokes this ignored test
alongside the authority and provisioning checks. It requires Linux ELF
artifacts and a working Docker daemon; normal macOS-built Mach-O artifacts
cannot execute inside that fixture. The normal local check only confirms that
the runtime test compiles:

```bash
cargo test -p turso_mysql_runtime --test unix_e2e --no-run
```

## Initialize, add an account, or reconcile an account store

On Linux and macOS, `turso-mysql-offline-provision` is the standalone client
for the first account generation, a journal-backed account addition, and
recovery of a retained provisioning journal. It is not a MySQL server command
and it must run as the client UID, not as the authority UID.
`--authority-service-uid` must name a different UID; the client verifies that
UID through kernel peer credentials.

All common options are required and have no defaults:

```text
--account-store-root PATH
--authority-id ID
--authority-socket PATH
--authority-service-uid UID
--authority-rpc-timeout-ms MILLISECONDS
--coordination-timeout-ms MILLISECONDS
```

`initialize` and `add-account` each require every account option below. The
three password source options are mutually exclusive; exactly one is required.

```text
initialize|add-account --username NAME --global-connect true|false --global-list true|false \
  --disabled true|false \
  [--database-grant DATABASE:PERMISSION[,PERMISSION...]]... \
  (--password-tty | --password-stdin | --password-fd N) \
  --password-input-timeout-ms MILLISECONDS \
  [--allow-empty-password]
```

Each `--database-grant` has one canonical lower-case database name and a
nonempty comma-separated set drawn from `connect`, `query`, `create`, and
`drop`. A database and a permission may each appear only once in the same
account command. Invalid grant syntax is rejected before password input. A
grant is always built for the account being added; library callers that supply
a grant for another account fail before writing a journal.

`--authority-id` is validated independently as the server checkpoint ID and
the authority wire ID. `--authority-socket` must be an absolute Linux/macOS
safe pathname (at most 103 bytes). The authority RPC timeout bounds one GET or
CAS request/response. The coordination timeout bounds waits for the
provisioning lock and a reconciliation GET under one absolute deadline. It is
not a total wall-clock timeout and cannot interrupt a filesystem sync, rename,
or directory sync that has already begun.

Initialize one enabled or disabled account with global `Connect` and `List`
permissions, optionally with database grants:

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
  --database-grant reports:connect,query \
  --password-tty \
  --password-input-timeout-ms 30000
```

Add a subsequent account with the same common and account options. The command
rebuilds from the authority-approved current generation; it does not replace
the generation from an operator-supplied full account list.

```bash
cargo run -q -p turso_mysql_offline_provisioner \
  --bin turso-mysql-offline-provision -- \
  --account-store-root /var/lib/turso-mysql/accounts \
  --authority-id account-store \
  --authority-socket /run/turso-mysql-checkpoint/authority.sock \
  --authority-service-uid 991 \
  --authority-rpc-timeout-ms 1000 \
  --coordination-timeout-ms 1000 \
  add-account \
  --username reader \
  --global-connect true \
  --global-list false \
  --disabled false \
  --database-grant reports:connect,query \
  --password-tty \
  --password-input-timeout-ms 30000
```

Use the same common options to reconcile after an interrupted `initialize` or
`add-account`:

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

Both account commands require exactly one password source. `--password-tty` opens the
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

`--password-input-timeout-ms` is required for both account commands and creates one
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

Initialization prepares the complete snapshot and its exact checkpoint. An
account addition first reads the exact authority checkpoint, opens that exact
current snapshot, rebuilds the full generation, and prepares its replacement.
Both operations durably write the fixed-name, `0600`, checksummed pending
journal before snapshot publication. They publish with temp-file sync, rename,
and directory sync; perform the authority CAS; reopen the exact replacement;
and only then unlink and directory-sync the journal. `Durable` installs the new
generation. A definite `Conflict` during first initialization removes only the
unchanged snapshot inode that the attempt published, so a corrected
initialization can retry. An account-addition conflict, a lost reply, timeout,
or other ambiguous result retains the snapshot and exact old/new checkpoint
pair for explicit reconciliation. Retrying that exact CAS is idempotent.

Every crash-safe initialization, account addition, and reconciliation first
checks that the authority client serves the opaque authority ID in the journal.
The provisioning trait defaults to refusing this check, and the Unix client
accepts only its configured authority ID. A mismatch fails before a journal or
snapshot write; it must not be bypassed by treating the socket endpoint alone
as an authority identity.

Deterministic process-kill tests cover the four high-level durable boundaries
for both initialization and account addition: journal publication, snapshot
publication, durable authority CAS, and journal removal. Each test stops a
child at the boundary with `SIGSTOP`, kills it with `SIGKILL`, and reopens then
reconciles the journal. Test-only one-shot fault injection covers initialization
journal and snapshot publication at all sixteen write, file-sync, rename, and
directory-sync points before and after the syscall, and it covers every
replacement snapshot-publication point during `add-account`. Each replacement
case checks the exact old or replacement snapshot, unchanged authority, retained
journal, temporary cleanup, and safe recovery. Journal removal injection covers
unlink and directory-sync failures before and after each operation; retrying an
uncertain removal re-syncs the directory even when the journal is already
absent. A separate child-process test stops before unlink, after unlink before
directory sync, and after directory sync, then kills the child and verifies
recovery without changing the account snapshot or unrelated files.

A same-effective-UID integration test drives `add-account` with a database
grant through the real Unix authority, reloads the running account store, then
restarts the authority and reopens the exact revision-one account generation.
Another real-service test lets the authority durably accept that replacement
while the caller observes an ambiguous result, then uses a fresh client to
reconcile and remove the retained journal before service restart. The
privileged Linux gate separately runs the CLI and authority under distinct
numeric UIDs through the same revision-one addition.

Two real-service process-kill tests exercise both initialization and account
addition at journal publication, snapshot publication, durable authority CAS,
and journal removal. Each child is observed in `SIGSTOP`, killed with
`SIGKILL`, and recovered only after the authority service is restarted. The
recovered state is then opened again after a second authority restart. The
environment-selected stop hooks are compiled only for tests or the default-off
`test-support` feature. Production builds must not enable that feature.

Recovery never derives authority from the snapshot. For an initialization
journal with no snapshot, only authority `Missing` permits journal removal. For
a replacement journal, the following matrix is implemented:

| Local account snapshot | Authority checkpoint | Reconcile result |
|---|---|---|
| Exact replacement | Exact expected or exact replacement | Retry the exact CAS if needed, reopen the replacement, then clear the journal |
| Exact expected generation, with replacement absent | Exact expected | Clear the journal as an aborted pre-publication replacement |
| Any other local state, or authority missing, different, unavailable, or invalid | Any nonmatching state | Retain the journal and fail closed |

A snapshot-publication error also leaves the journal in place because rename
may have succeeded before a directory sync error was reported. The matrix is
covered with test authorities for expected, replacement, missing, foreign,
wrong-revision, wrong-digest, and unavailable authority states; it is not a
real-service crash test.

The current commands create the first account or append one account, with
global `Connect`/`List` flags and explicit canonical database grants. They do
not support removing or editing accounts or grants. The legacy library
`replace` path has no durable pending journal, so it does not promise
process-crash recovery. Do not use that path as an operational substitute for
this workflow.

Do not downgrade to a build from before `add-account` while a provisioning
journal exists. Older reconciliation is initialization-only and cannot safely
resolve a retained replacement transition. Reconcile it with this version
first, confirm that the journal is gone and that the authority matches the
current snapshot, then plan any downgrade as a separate compatibility change.

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

- Run the same process-kill recovery matrix with the CLI and authority under
  distinct numeric UIDs in the pinned Linux gate. The current cross-UID gate
  covers the normal revision-one addition, while the crash matrix uses the
  real authority under one effective UID.
- Add TCP/TLS, certificate policy, and a standalone MySQL runtime before making
  a network service claim.
