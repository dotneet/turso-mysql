# MySQL 8.4 reference environment

This directory runs the MySQL 8.4 reference server used by the MySQL
conformance runner. It is an oracle for observed behavior, not a Turso server.

The image is the official `mysql:8.4` multi-platform index pinned to
`sha256:b3b90af2a6552ae30c266fdb7d5dd55f3afb72404bb78d37fe8a23eb857fd3fb`.
At the time it was resolved, this index identified MySQL 8.4.11. Docker's
image inspection shows the source as the official Docker Library MySQL image.

The service is published only to `127.0.0.1:${MYSQL_ORACLE_PORT:-3307}`. Its
TCP listener is also available as `mysql:3306` on the dedicated `oracle`
Compose network. It is not reachable from another machine.

## Start and stop

Start the reference server and wait for its health check:

```bash
make -C mysql/conformance up
```

Open the bundled MySQL client inside the container:

```bash
make -C mysql/conformance client
```

Keep the database files, but stop the service:

```bash
make -C mysql/conformance down
```

Remove the service and its database volume:

```bash
make -C mysql/conformance clean
```

`clean` deletes the local reference data. It is safe to recreate because this
environment is only for conformance tests.

## Development credentials

The Compose file has local development defaults only for the database, test
user, and loopback port. Passwords have no defaults and must come from the
environment. Set them in the shell that runs Compose:

```bash
export MYSQL_CONFORMANCE_PASSWORD='<test-user-password>'
export MYSQL_CONFORMANCE_ROOT_PASSWORD='<root-password>'
make -C mysql/conformance up
```

Do not put real credentials in this file, a case file, a golden observation,
or command output.

## Inspect and run cases

Render the Compose configuration without resolving secrets:

```bash
make -C mysql/conformance config
```

This command leaves environment substitutions unresolved, so password values
do not appear in terminal or CI output.

Record a reference observation with an explicit output path:

```bash
MYSQL_ORACLE_DSN='mysql://turso_oracle:<test-user-password>@127.0.0.1:3307/turso_oracle' \
  make -C mysql/conformance record OUTPUT=/tmp/mysql-smoke.json
```

Compare the same case with a checked-in observation:

```bash
MYSQL_ORACLE_DSN='mysql://turso_oracle:<test-user-password>@127.0.0.1:3307/turso_oracle' \
  make -C mysql/conformance verify \
  GOLDEN=mysql/conformance/goldens/mysql-8.4/sha256-b3b90af2a6552ae30c266fdb7d5dd55f3afb72404bb78d37fe8a23eb857fd3fb/smoke.json
```

Verify every checked-in P0 case, including the numeric/coercion and Unicode
collation decision corpora, against the pinned server:

```bash
MYSQL_ORACLE_DSN='mysql://turso_oracle:<test-user-password>@127.0.0.1:3307/turso_oracle' \
  make -C mysql/conformance verify-p0
```

The runner receives its DSN through `MYSQL_ORACLE_DSN` and must not print it or
its password. Replace the password placeholder with the environment-supplied
test-user password, URL-encoded when it contains reserved URL characters. The
runner always uses TCP and disables the driver's automatic Unix-socket switch,
because a socket path reported by the container is not a host path.
It accepts only numeric loopback addresses and verifies that the server reports
the pinned MySQL 8.4.11 version before running a case. Loopback confinement is
required because this local reference container does not enable TLS.

Each case declares its sessions and ordered SQL steps. Observations record
typed rows, protocol column metadata, affected rows, last insert ID, warnings,
structured MySQL errors, and the resulting session and transaction state. The
smoke case removes the table it creates so `verify` is repeatable without
restarting the reference server.
`verify-p0` runs the complete checked-in P0 case/golden manifest and fails when
either a case or its matching observation is missing or differs.
The reference server starts with `max_error_count=65535` so every warning
reported in an OK packet can be captured by `SHOW WARNINGS` instead of silently
truncating the details. This is a server launch option because MySQL 8.4 does
not let the unprivileged conformance user change it per session.

## Parser viability reports

Generate an offline report from a case and its recorded MySQL observation:

```bash
make -C mysql/conformance parser-report \
  CASE=mysql/conformance/cases/p0/parser-quoting.json \
  GOLDEN=mysql/conformance/goldens/mysql-8.4/sha256-b3b90af2a6552ae30c266fdb7d5dd55f3afb72404bb78d37fe8a23eb857fd3fb/parser-quoting.json \
  PARSER_REPORT=/tmp/parser-quoting.json
```

The report compares pinned `sqlparser` 0.62.0 with MySQL syntax acceptance,
checks AST format/reparse equality, and runs the same SQL through a
session-aware MySQL dialect. Steps that intentionally compare SQL modes carry
an explicit `probe.group` and unique `probe.variant`. A probe group must use
identical SQL, parameters, timezone, isolation, and autocommit; only SQL mode
may differ. This prevents setup statements or coincidentally repeated SQL from
being reported as semantic collisions.

Mode probes are an author-reviewed contract and must contain deterministic,
read-only queries. Do not use changing data, current-time/random functions, or
unordered multi-row results. The report compares result values and metadata,
affected rows, IDs, warning/error identities, and non-mode session effects; it
also records exact JSON paths for every observed semantic difference.

The checked-in reports live below `reports/mysql-8.4/<image-digest>/` next to
their pinned parser version. They are evidence for parser selection, not a
claim that Turso executes the corresponding MySQL behavior yet.

## Image provenance

Resolve the pinned image again with:

```bash
docker buildx imagetools inspect \
  mysql@sha256:b3b90af2a6552ae30c266fdb7d5dd55f3afb72404bb78d37fe8a23eb857fd3fb
```

Use the index digest instead of an architecture-specific child digest so the
same MySQL version runs on supported amd64 and arm64 development machines.
