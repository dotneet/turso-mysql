# Hibernate ORM MySQL E2E

This fixture pins Hibernate ORM 6.6.0.Final and Connector/J 9.6.0. It uses
Hibernate's MySQL dialect auto-detection and Connector/J's
`useInformationSchema=true` metadata path. TLS verifies the CA and
server hostname. Run it only against a disposable
`reports` database: it creates and removes the `hibernate_e2e_account` and
`hibernate_e2e_line` tables.

Build with Java 21 and Maven:

```sh
mvn -f mysql/runtime/tests/fixtures/hibernate-e2e/pom.xml -DskipTests package
```

Run from the repository root:

```sh
TURSO_MYSQL_DRIVER_ENDPOINT=localhost:3306 \
TURSO_MYSQL_DRIVER_CA=/absolute/path/to/ca.pem \
TURSO_MYSQL_DRIVER_PASSWORD='test-password' \
java -jar mysql/runtime/tests/fixtures/hibernate-e2e/target/hibernate-e2e-1.0.0.jar
```

`TURSO_MYSQL_DRIVER_USER` defaults to `gateadmin`. The CA must sign a server
certificate valid for the hostname in `TURSO_MYSQL_DRIVER_ENDPOINT`. The process
prints the current stage so the first unsupported schema operation or query is
visible. Completion requires the final `Hibernate E2E: complete` line.
For a server restart test, run the same command first with `prepare` appended,
restart the database server while preserving its data, then run with `verify`
appended. The no-argument form runs both phases without a server restart.

The fixture runs Hibernate `create-only` with a first entity version, `update`
with a new nullable column and a related table, `validate` after closing and
reopening the connection pool and optionally restarting the server, and `drop`.
It also inspects columns and foreign
keys through `DatabaseMetaData`, round-trips a relation, `NULL`, exact DECIMAL,
and whole-second TIMESTAMP, checks updates and rollback, and requires unique and foreign
key errors to report SQLSTATE `23000`.

On 2026-09-26, the fixture with `useInformationSchema=false` passed against a
disposable MySQL 8.4.11 container over `VERIFY_IDENTITY` TLS. MySQL emitted `create table ...
engine=InnoDB`, `alter table ... add constraint ... unique`, `alter table ...
add constraint ... foreign key`, and `alter table ... drop foreign key` for these
mappings. The unique and foreign key checks received MySQL errors 1062 and 1452,
both with SQLSTATE `23000`.

With `useInformationSchema=true`, Connector/J reads tables, columns, primary
keys, indexes, and foreign keys from `INFORMATION_SCHEMA`. The fixture keeps
Hibernate's schema update and validation paths active in this mode.
The current `getIndexInfo` handler answers empty tables; it rejects nonempty
tables because MySQL's `CARDINALITY` is an index statistic and can stay zero
until `ANALYZE TABLE` runs. The E2E fixture exercises the metadata needed by
its schema operations; it does not claim index statistics parity.
Connector/J also reads 262 reserved words from `INFORMATION_SCHEMA.KEYWORDS`.
This result uses 266 wire frames when EOF packets are enabled, so the TCP
fixture allows 512 queued frames and 8,192 queued bytes. A deployment using
this metadata path needs a frame limit of at least 266; this setting does not
add response streaming.
