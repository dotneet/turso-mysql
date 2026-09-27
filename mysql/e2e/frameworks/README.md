# Real frameworks against a live turso-mysql server

This harness runs real clients and ORMs against a live `turso-mysql-server`
and against MySQL 8.4.11, each in its own container, and lists every SQL
statement turso refuses that MySQL accepts. Findings from the last run are in
[FINDINGS.md](FINDINGS.md).

## Run it

```bash
mysql/e2e/frameworks/run.sh                   # every app; about N minutes warm, see FINDINGS.md
mysql/e2e/frameworks/run.sh laravel gorm      # some apps
```

The result is `mysql/e2e/frameworks/.run/results/summary.md` (and
`summary.json`). Everything under `.run/` is per run and ignored by git.

While working on one app or one fix, keep the servers up and rerun only that app:

```bash
mysql/e2e/frameworks/run.sh --up              # build turso, start both servers
mysql/e2e/frameworks/run.sh --dev laravel     # rerun one app (E2E_TARGETS=turso for one side)
mysql/e2e/frameworks/run.sh --down            # stop and remove the servers
```

`--up` rebuilds the server from the checkout, so after a server change run
`--up` again before `--dev`.

Requirements: Docker with a Linux engine (Docker Desktop is fine), about 10 GB
of disk for the build cache and images. Nothing is built into the checkout's
`target/`.

## What runs

| Container | What it is |
|---|---|
| `turso-e2e-fw-build-*` | `rust:1.88` debug build of the server, checkpoint authority and offline provisioner into the `turso-e2e-cargo-target` volume |
| `turso-e2e-fw-turso-1` | [server/boot-turso.sh](server/boot-turso.sh): the checkpoint authority as UID 41001, provisioning of the `e2e` account, and `turso-mysql-server --listen 0.0.0.0:3306` with mandatory TLS as UID 41002 (the authority refuses a client with its own UID, which is why this cannot run on a single-user macOS host) |
| `turso-e2e-fw-mysql-1` | MySQL 8.4.11 with the same TLS certificate and `require_secure_transport=ON` |
| `turso-e2e-fw-<app>-<target>` | one app, run once against each server |

Each app gets its own freshly created database (`CREATE DATABASE` through the
`dbadmin` service) on both servers before it runs. The account and TLS
material are generated per run: a random password in `.run/password` and a
throwaway CA in `.run/tls/`. The account has `connect,query,create,drop` on
the app databases on turso and `ALL` on MySQL.

### Capturing the SQL

The server has no statement log, and each framework logs SQL differently, so
every app container starts [sqlproxy](sqlproxy/main.go) on `127.0.0.1:3306`
and the app connects there. The proxy relays the MySQL greeting and the
client's SSL request unchanged, terminates TLS on both sides with the test
certificate, and writes each `COM_QUERY`, `COM_STMT_PREPARE`,
`COM_STMT_EXECUTE` and `COM_INIT_DB` with the server's first answer (OK or
the error code, SQLSTATE and message) to
`.run/results/<app>/<target>/wire.jsonl`. It never changes a packet.

### Apps

| App | Client | Flow |
|---|---|---|
| `mysqlcli` | mysql 8.4.11 CLI | schema and data files, SHOW/DESCRIBE, joins, aggregates, JSON, transactions, ALTER, information_schema |
| `mysqldump` | mysqldump 8.4.11 | default dump, `--no-data`, `--single-transaction --routines --triggers`, replay into a second database, row comparison |
| `laravel` | Laravel 12, PDO mysqlnd | `artisan migrate`, Eloquent, `db:show`, paginate, rollback, `migrate:fresh` |
| `prisma` | Prisma 6 | `migrate deploy`, `db pull`, `migrate dev` with a shadow database, client queries |
| `typeorm` | TypeORM 0.3, mysql2 | `synchronize`, repositories, query builder, migrations |
| `django` | Django 5.2, mysqlclient | `migrate`, `inspectdb`, ORM, `flush` |
| `sqlalchemy` | SQLAlchemy 2.0, Alembic, PyMySQL | `alembic upgrade`, `alembic check`, inspector, ORM |
| `rails` | ActiveRecord 8.0, mysql2 | `db:migrate`, `db:schema:dump`, `db:rollback` |
| `gorm` | GORM, go-sql-driver | `AutoMigrate`, Migrator introspection, associations |

Each app writes one JSON line per step to `steps.jsonl`
(`{"step", "ok", "error"}`) and keeps going after a failed step. A step or
statement is reported as a turso difference only when it fails on turso and
passes on MySQL; statements MySQL refuses too (for example a deliberate
foreign key violation) are left out.

## Adding an app

1. Add `apps/<name>/` with a `Dockerfile` (pinned base image, pinned
   dependencies, app in `/app`, executable `./run.sh`).
2. Add a service to `compose.yaml` like the existing ones, and the name to
   `all_apps` in `run.sh` (and to `E2E_DATABASES` plus `databases_of` if it
   needs more than one database).
3. Inside the container use `E2E_HOST`, `E2E_PORT`, `E2E_USER`,
   `E2E_PASSWORD`, `E2E_CA` (TLS is required), database `E2E_APP`, and append
   steps to `$E2E_OUT/steps.jsonl`.

## Cleaning up

Runs remove their containers and network. Kept for speed:

```bash
docker volume rm turso-e2e-cargo-registry turso-e2e-cargo-git turso-e2e-cargo-target turso-e2e-go-cache
docker image rm $(docker images --format '{{.Repository}}:{{.Tag}}' | grep '^turso-e2e-fw-')
```
