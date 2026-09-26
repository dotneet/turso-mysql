# Hibernate ORM MySQL E2E

This fixture pins Hibernate ORM 6.6.0.Final and Connector/J 9.6.0. It uses
Hibernate's MySQL dialect auto-detection and Connector/J's
`useInformationSchema=false` metadata path. Connector/J's information schema
path remains outside this fixture's tested scope. TLS verifies the CA and
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

On 2026-09-26, the complete fixture passed against a disposable MySQL 8.4.11
container over `VERIFY_IDENTITY` TLS. MySQL emitted `create table ...
engine=InnoDB`, `alter table ... add constraint ... unique`, `alter table ...
add constraint ... foreign key`, and `alter table ... drop foreign key` for these
mappings. The unique and foreign key checks received MySQL errors 1062 and 1452,
both with SQLSTATE `23000`.

With `useInformationSchema=false`, the measured schema metadata queries use
`SHOW FULL TABLES FROM reports`, `SHOW FULL COLUMNS FROM table FROM reports`,
and `SHOW CREATE TABLE reports.table`. This keeps the ORM's schema update and
validation paths active while using the same Connector/J metadata mode as the
existing JDBC E2E fixture.
