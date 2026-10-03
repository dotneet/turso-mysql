# Findings of the first framework run

First run of this harness against turso-mysql at `2ebe064df` and MySQL 8.4.11,
2026-09-28. Every statement listed here is refused, or answered differently, by
turso while MySQL accepts it for the same app. Rerun with `./run.sh` to refresh.

Every app passes every step against MySQL 8.4.11.

## 1. The server stopped on a panic (fixed after this run)

`core/schema.rs:2075: all automatic indexes parsed from sqlite_schema should have
been consumed, but 1 remain`, on the first connection after Django's migrations.
`ALTER TABLE auth_group MODIFY name varchar(150) NOT NULL` over a column declared
`UNIQUE` rewrote the table's stored SQL without the key but kept its automatic
index, so the database could not be opened again. Fixed by
`core: keep a column's own UNIQUE or PRIMARY KEY across ALTER COLUMN`. A database
already written by the bug still cannot be opened.

Because the server stopped, Laravel, TypeORM, Rails and SQLAlchemy have no turso
results yet, and Django's steps after `introspect` failed only on the refused
connection.

## 2. Refused before anything else

| App | Statement | Answer |
|---|---|---|
| Prisma (mysql_async), every connection | `SELECT @@socket, @@max_allowed_packet, @@wait_timeout` | 1193 unknown system variable |
| mysqldump, first statement | `SET SESSION NET_READ_TIMEOUT= 86400, SESSION NET_WRITE_TIMEOUT= 86400` | 1046 / 1235 |
| mysql CLI `status` | `select DATABASE(), USER() limit 1` | 1064 |
| mysql CLI with a latin1 locale | handshake asking for a non-utf8mb4 character set | socket closed without an error packet (`validate_ssl_character_set`) |
| Django `connect` | `SELECT VERSION(), @@character_set_client, DATABASE()` | 1235 |

## 3. Protocol

- A command payload over 16 KiB closes the connection with no error packet
  (`MAX_COMMAND_PAYLOAD_LENGTH`, `@@max_allowed_packet` = 16384). MySQL takes
  64 MiB. Hits large text or JSON values, batch inserts and every dump replay.
- GORM measured a median of 55.9 ms per statement against 0.9 ms on MySQL, many
  at about 40 or 80 ms: probably Nagle / delayed ACK on the server's socket
  (not yet checked).

## 4. Statements refused, by kind

- **Comparison as a result column (1064):** `SELECT 2 > 1`,
  `SELECT created_at > DATE_SUB(NOW(), INTERVAL 1 DAY) FROM users`,
  `SELECT (SELECT COUNT(*) FROM posts) > 0`.
- **`?` inside an expression, prepared (GORM, 1235 at prepare):**
  `VALUES (?,?,?,?,CAST(? AS JSON),?,?)`, `HAVING COUNT(*) > ?`,
  ``UPDATE `users` SET `balance`=balance - ? WHERE email = ?``,
  ``WHERE JSON_EXTRACT(`profile`,?) = ?``.
- **information_schema with a bound schema (GORM, 1235 at execute):**
  `SELECT TABLE_NAME FROM information_schema.tables where TABLE_SCHEMA=?`,
  `information_schema.STATISTICS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?`.
- **Django introspection (1235):** `SELECT column_name, data_type, ..., CASE WHEN
  collation_name = 'utf8mb4_0900_ai_ci' THEN NULL ELSE collation_name END AS
  collation_name, CASE WHEN column_type LIKE '% unsigned' THEN 1 ELSE 0 END AS
  is_unsigned, column_comment FROM information_schema.columns WHERE table_name =
  'users' AND table_schema = DATABASE()`.
- **Upsert (GORM):** ``INSERT INTO `tags` (`name`) VALUES (?) ON DUPLICATE KEY UPDATE `id`=`id` ``.
- **DDL:** ``DROP TABLE IF EXISTS `users` CASCADE`` (1064),
  `ALTER TABLE ... ADD COLUMN ... AFTER col` (1235).
- **GROUP BY a primary key, selecting its other columns (1064):**
  `SELECT u.name FROM users u GROUP BY u.id`.
- **Functions (1064):** `USER()`, `CURRENT_USER()`, `CONNECTION_ID()`,
  `ROW_COUNT()`, `FOUND_ROWS()`, `SQL_CALC_FOUND_ROWS`, `CAST(x AS CHAR)`,
  `GROUP_CONCAT(name ORDER BY name)`, `= 'x' COLLATE utf8mb4_unicode_ci`.
- **Other 1235:** `INSERT ... VALUES (..., NOW(6))`, `WHERE EXISTS (SELECT 1 FROM
  posts p WHERE p.user_id=u.id)`, `IFNULL(json_col,'{}')`, `COALESCE` of a
  `DATETIME(6)` and a `DATETIME`, `SHOW GRANTS`, `SHOW PROCESSLIST`,
  `SHOW STATUS LIKE 'Uptime'`, `SHOW FUNCTION STATUS`, `SHOW PROCEDURE STATUS`,
  `SHOW EVENTS`; `SELECT * FROM information_schema.TRIGGERS WHERE ...` (1064).
- **A missing table answers 1235, not 1146** (`SELECT id FROM no_such_table`):
  frameworks that detect a table by catching 1146 break.

## Per app, first run

| App | turso | MySQL |
|---|---|---|
| mysql CLI | 16/23 | 23/23 |
| mysqldump | 0 (first statement refused) | all |
| Prisma | 0/22 (`@@socket`) | 22/22 |
| GORM | 9/21 | 21/21 |
| Django | up to `introspect`, then the panic | all |
| Laravel, TypeORM, Rails, SQLAlchemy | not run (server stopped) | all |

# Second run

At local main `e818cdb60`'s parent, 2026-09-28, after the first run's fixes.
The server did not stop. Django and SQLAlchemy did not build (their pinned
requirements were missing from the repository; now committed), and Prisma
still failed on `@@socket` answered as NULL (now answered as an empty path).

| App | turso, first run | turso, second run |
|---|---|---|
| mysql CLI | 16/23 | 19/23 |
| mysqldump | 0 | 5/9 (dump works; replay does not) |
| GORM | 9/21 | 9/21 |
| Rails | not run | 16/22 |
| TypeORM | not run | 6/18 |
| Laravel | not run | fails at `connect` (see below) |
| Prisma | 0/22 | 2/22 (`@@socket` NULL; fixed after this run) |

GORM's median statement time went from 55.9 ms to 1.08 ms (MySQL: 0.54 ms)
with TCP_NODELAY, confirming the delayed-acknowledgement wait.

Refused only by turso, by kind:

- **Prepared statements Laravel sends for everything:**
  `select version() as version, database() as db`;
  `select @@character_set_client as client, ...` (answered 1054, wrongly);
  `select exists(select * from `t` where `c` = ?) as `exists``;
  ``update `users` set `balance` = ?, `users`.`updated_at` = ? where `email` = ?``;
  ``update `cache` set `value` = ? where `key` = ?``;
  ``delete from `migrations` where `migration` = ?``.
- **`?` inside an expression, prepared (GORM):** `CAST(? AS JSON)`,
  `HAVING COUNT(*) > ?`, `SET balance = balance - ?`, `JSON_EXTRACT(profile, ?) = ?`.
- **Upserts:** `ON DUPLICATE KEY UPDATE id = id` (GORM),
  `VALUES (DEFAULT, 'x') ... ON DUPLICATE KEY UPDATE name = VALUES(name)`
  (TypeORM), Rails 8's `INSERT ... VALUES (...) AS users_values` with
  `CURRENT_TIMESTAMP(6)`.
- **Counting and grouping:** `SELECT COUNT(1) AS cnt FROM posts Post` (TypeORM),
  `SELECT COUNT(*) FROM (SELECT 1 AS one FROM posts LIMIT 3 OFFSET 0)
  subquery_for_count` (Rails), `HAVING (COUNT(*) > '1')` (Rails),
  `GROUP_CONCAT(... ORDER BY ...)`, `SQL_CALC_FOUND_ROWS`.
- **Comparisons as result columns:** `DATE_SUB(NOW(), INTERVAL 1 DAY) < created_at`.
- **Names qualified by the database:** ``SELECT * FROM `typeorm`.`migrations` ``,
  ``drop table `laravel`.`cache`, ...``.
- **Catalog reads:** `information_schema.tables where TABLE_SCHEMA=?`
  (prepared), `STATISTICS ... = ?` (prepared), `group_concat(column_name order
  by seq_in_index)` over STATISTICS (Laravel), `performance_schema.session_status`,
  `concat(...) FROM INFORMATION_SCHEMA.VIEWS` (TypeORM).
- **Other:** `UPDATE ... SET views = COALESCE(views, 0) + 1` (Rails),
  `SELECT ... FOR UPDATE SKIP LOCKED` (Laravel's queue), a multi-row INSERT
  with quoted quotes in a replayed dump, and a statement that is only a
  `-- comment` line in a replayed dump.

# Third run

At local main `dba538ffb`, 2026-09-28, after Laravel's prepared statements,
bound values in expressions, ORM upserts, SELECT shapes and dump replay.

| App | second run | third run |
|---|---|---|
| Rails | 16/22 | 22/22 |
| mysqldump | 5/9 | 9/9 |
| mysql CLI | 19/23 | 22/23 (`join`) |
| Laravel | fails at `connect` | 24/27 (relations, aggregate, json) |
| SQLAlchemy | 2/20 | 18/20 (aggregate, json) |
| GORM | 9/21 | 17/21 |
| TypeORM | 6/18 | 14/18 |
| Django | 17/22 | 17/22 |
| Prisma | 0/22 | 1/22 (`DROP DATABASE` waits and answers 1205) |

Laravel's count read 23 in the report: its step log wrote PHP namespaces as
invalid JSON escapes, which the report could not read (fixed in steps.sh).

Refused only by turso, by kind:

- **Grouping by a primary key while selecting its other columns:** Django's and
  SQLAlchemy's aggregates over a join (`GROUP BY users.id [, users.name]
  HAVING COUNT(posts.id) >= 1`), the mysql CLI `join` step, Laravel's
  `group by users.email having post_count >= ?`.
- **Correlated subqueries in the select list:** Laravel `withCount`
  (`(select count(*) from posts where users.id = posts.user_id) as posts_count`).
- **Prisma:** `DROP DATABASE` while its own pooled connections have the database
  open answers 1205 after waiting (MySQL drops it); its introspection
  `SELECT DISTINCT BINARY table_info.table_name ... FROM information_schema.tables
  JOIN information_schema.columns ON BINARY ... = BINARY ...`.
- **JSON:** Django `JSON_CONTAINS(JSON_EXTRACT(profile, '$.tags'), '"a"')`,
  SQLAlchemy `CASE JSON_EXTRACT(...) WHEN 'null' THEN NULL ELSE
  JSON_UNQUOTE(...) END AS anon_1` in a projection, Laravel `json_set(profile,
  '$.city', ?)`.
- **Django introspection:** `SELECT column_name, data_type, ... CASE WHEN
  collation_name = 'utf8mb4_0900_ai_ci' THEN NULL ELSE collation_name END ...,
  CASE WHEN column_type LIKE '% unsigned' THEN 1 ELSE 0 END ... FROM
  information_schema.columns WHERE table_name = ...`.
- **TypeORM:** the pagination `SELECT DISTINCT distinctAlias.Post_id ... FROM
  (SELECT Post.id AS Post_id, ...)` with joins, `MAX(Post.views)` over a
  LEFT JOIN, a join `ON (t.tag_id = '1' AND ...)`, a multi-row INSERT with
  `DEFAULT` in some rows.
- **GORM:** `COUNT(DISTINCT(user_id))`, `UPDATE posts SET slug = CONCAT('p-', id)`,
  an upsert mixing an explicit id and `DEFAULT`.

# Fourth run

At local main after the Prisma, grouping, JSON/introspection and GORM/TypeORM
rounds, 2026-09-28. Every step passes on turso for Django (22/22), GORM (21/21),
Laravel (26/26), the mysql CLI (23/23), mysqldump (9/9), Rails (22/22) and
SQLAlchemy (20/20).

Left:

- **Prisma 13/22:** two pooled connections each run `BEGIN` and an INSERT;
  the second INSERT runs after the first transaction commits and still answers
  1213 (deadlock / write conflict), apparently because its transaction's read
  snapshot was taken before that commit. MySQL takes both rows. Behind it, a
  SELECT with Prisma's relation counts (correlated `EXISTS`, `COALESCE` over a
  grouped derived table) is refused.
- **TypeORM 15/18:** the pagination `SELECT DISTINCT distinctAlias.Post_id ...
  FROM (SELECT ... FROM posts Post LEFT JOIN ...) distinctAlias` (a derived
  table over a join), `SELECT post.user_id AS userId, SUM(post.views) AS views
  FROM posts post GROUP BY post.user_id HAVING SUM(post.views) > 5` (a qualified
  aggregate in HAVING), and its rollback-migration step.

# Fifth run

After the concurrent-transaction fix, Prisma's client statements and the
counted-row fix, 2026-09-28. Every step passes on turso for Django (22/22),
GORM (21/21), Laravel (26/26), the mysql CLI (23/23), mysqldump (9/9), Prisma
(22/22), Rails (22/22) and SQLAlchemy (20/20). TypeORM passes 17/18; its
pagination over a derived table that joins tables is still refused.

# Sixth run (new apps)

Six new apps at `df22b69aa` (branch `compat-integration`), 2026-09-28, run as
a second copy of the harness (`COMPOSE_PROJECT_NAME=turso-e2e-more`). Every
step of every new app passes against MySQL 8.4.11. GORM (21/21) and the mysql
CLI (23/23) were rerun alongside as a check of the harness changes and still
pass on turso.

| App | Stack | turso | MySQL |
|---|---|---|---|
| `sequelize` | Sequelize 6.37.8, sequelize-cli 6.6.5, mysql2 3.24.4 | 2/23 | 23/23 |
| `drizzle` | drizzle-orm 0.45.3, drizzle-kit 0.31.11, mysql2 3.24.4 | 0/20 | 20/20 |
| `spring` | Spring Boot 3.5.16, Hibernate 6.6.53, Flyway 11.7.2, Connector/J 9.7.0 | 2/46 | 46/46 |
| `efcore` | EF Core 9.0.20, Pomelo 9.0.0, MySqlConnector 2.4.0 | 0/21 | 21/21 |
| `sqlx` | sqlx 0.8.6 and sqlx-cli 0.8.6 | 0/22 | 22/22 |
| `dbtools` | Connector/J 9.7.0 metadata; DBeaver, Workbench, TablePlus statements | 3/11 | 11/11 |

Drizzle was chosen over Knex (about 83 million npm downloads a month against
19 million), sqlx over diesel and Ecto (sqlx 0.8.6 alone has 73 million crate
downloads). The GUI statements in `dbtools` are modeled on those tools'
general-log output, not captured from the GUIs; its DatabaseMetaData calls are
real Connector/J calls.

## Steps on turso

Every app is stopped by one statement or handshake near the start, so most
steps fail only because an earlier step did:

- **efcore 0/21, sqlx 0/22:** no connection is ever established (see 1.1
  and 1.2).
- **sequelize 2/23:** `connect` and `sync` pass; everything from `migrate` on
  fails because the `SequelizeMeta` table cannot be created, so no table
  exists.
- **drizzle 0/20:** `connect` fails on `select 1 from dual`, `migrate` on the
  `__drizzle_migrations` table, `introspect-push`/`introspect-pull` on the
  STATISTICS query, and the rest because no table exists.
- **spring 2/46:** `connect` passes in both modes; Flyway cannot start, so no
  table exists and `ddl-auto=validate` fails, and every later step needs the
  context. The same happens with client-side and server-side prepared
  statements.
- **dbtools 3/11:** `setup` (tables, a view, rows), `dbeaver-data` and
  `tableplus-connect` pass; the other eight fail on the statements listed
  in 4.

To see past the first blocker, three apps were also run once with the missing
table created by hand first (not part of the harness): Sequelize with
`SequelizeMeta` then passes 15/23, Drizzle with `__drizzle_migrations` 1/20,
and Spring with the V1 schema and `SPRING_JPA_HIBERNATE_DDL_AUTO=none` 12 of
23 steps in each mode. What those runs hit is listed under 3.

## 1. Connection refused before any statement (P0)

1. **MySqlConnector (every .NET app on EF Core/Pomelo, Dapper and plain
   ADO.NET with MySqlConnector):** after TLS its HandshakeResponse41 carries
   capabilities `0x011b8202`, without `CLIENT_SSL`; turso closes the
   connection with no error packet ("post-TLS client response must retain
   CLIENT_SSL", `connection_state.rs`). MySQL accepts it. The proxy logs it as
   an `authenticate` entry.
2. **sqlx (every Rust app on sqlx):** its SSLRequest announces
   `max_packet_size` 1024; turso requires at least 4096
   (`validate_ssl_max_packet_size`, `MIN_SERVER_RESPONSE_PAYLOAD_LENGTH`) and
   closes the connection with no error packet, so rustls reports "peer closed
   connection without sending TLS close_notify". MySQL accepts it. Seen with
   `E2E_PROXY_DUMP=1`: greeting, SSLRequest `0a8a0b0100040000e0...`, then
   nothing. (In one earlier development run a single sqlx connection got as
   far as three statements; this was not reproduced in four later runs.)

Both should answer with an error packet at the least; accepting them is what
MySQL does.

## 2. Migration tools stopped at their first statement (P0)

- **Sequelize** (`sequelize-cli` / umzug's meta table), 1235:
  ``CREATE TABLE IF NOT EXISTS `SequelizeMeta` (`name` VARCHAR(255) NOT NULL UNIQUE , PRIMARY KEY (`name`)) ENGINE=InnoDB DEFAULT CHARSET=utf8 COLLATE utf8_unicode_ci``.
  Two separate refusals, checked with the mysql CLI: a column that is both
  `UNIQUE` and the `PRIMARY KEY` (`... NOT NULL UNIQUE, PRIMARY KEY (name)`,
  also `... AUTO_INCREMENT UNIQUE PRIMARY KEY`), and `DEFAULT CHARSET=utf8` /
  `utf8mb3`.
- **Drizzle** (`drizzle-kit migrate`), 1235:
  `create table if not exists __drizzle_migrations ( id serial primary key, hash text not null, created_at bigint )`:
  the `SERIAL` type (`BIGINT UNSIGNED NOT NULL AUTO_INCREMENT UNIQUE`), alone
  or with `primary key`.
- **Flyway** (Spring Boot's default migrator), before it creates anything:
  - `SELECT SUM(found) FROM ((SELECT 1 as found FROM information_schema.tables WHERE table_schema='spring') UNION ALL (SELECT 1 as found FROM information_schema.views WHERE table_schema='spring' LIMIT 1) UNION ALL ...)` (1064: parenthesized UNION ALL members with LIMIT, over tables, views, table_constraints, triggers, routines);
  - `SELECT COUNT(1) FROM information_schema.schemata WHERE schema_name=? LIMIT 1` (1235 at execute, server-side prepared);
  - `SET foreign_key_checks=?, sql_safe_updates=?` (1235 at prepare);
  - `SELECT SUBSTRING_INDEX(USER(),'@',1)` (1064 text, 1235 prepared);
  - `SELECT event_name FROM information_schema.events WHERE event_schema=...` (1064);
  - probes whose failure Flyway tolerates but that turso answers differently:
    `SELECT @@GLOBAL.ENFORCE_GTID_CONSISTENCY` (1193),
    `select VARIABLE_VALUE from performance_schema.global_variables where variable_name = 'pxc_strict_mode'` and
    `SELECT variable_name FROM performance_schema.user_variables_by_thread WHERE variable_value IS NOT NULL` (1064),
    `SHOW DATABASES LIKE 'RDSAdmin'` (1064).
- **Hibernate `ddl-auto=validate`, and every JDBC schema tool:** Connector/J
  9's `DatabaseMetaData.getColumns` is one information_schema query with long
  `CASE`/`IF(LOCATE(...))` expressions and
  `IS_NULLABLE COLLATE utf8mb3_general_ci = 'NO'` /
  `EXTRA COLLATE utf8mb3_general_ci LIKE '%auto_increment%'`; turso answers
  1064 (1235 in another shape). See 4 for the other metadata calls.

## 3. Statements behind the blockers (P1)

From the hand-primed runs above, and the drizzle/sequelize steps that did run:

- **Hibernate / Spring Data JPA:**
  `update posts p1_0 set views=(p1_0.views+1) where p1_0.views<5` (1235: UPDATE
  with a table alias; every bulk JPQL update) and `delete p1_0 from posts p1_0`
  (1235: multi-table DELETE syntax, `deleteAllInBatch`).
- **Connector/J:** `SET SESSION TRANSACTION READ ONLY` / `READ WRITE` (1235),
  sent for every `@Transactional(readOnly = true)`; Spring ignores the failure,
  the connection is then not read-only, and an UPDATE inside a read-only
  transaction runs on turso (against MySQL, Connector/J itself refuses it:
  "Connection is read-only"). `COM_SET_OPTION` (0x1b, 1235): Connector/J turns
  multi-statements on for a rewritten batch of UPDATEs
  (`rewriteBatchedStatements=true`, Hibernate batched updates), so the batch
  fails.
- **Sequelize:**
  - a join nested in parentheses, which every `belongsToMany` include
    produces (1064):
    ``... LEFT OUTER JOIN ( `post_tags` AS `tags->PostTag` INNER JOIN `tags` AS `tags` ON `tags`.`id` = `tags->PostTag`.`tag_id`) ON `Post`.`id` = `tags->PostTag`.`post_id` ...``;
  - ``SAVEPOINT `45b6574b-4115-421c-92d9-43e3c00dad7b-sp-1` `` and
    `ROLLBACK TO SAVEPOINT` with such a name (1235: backquoted savepoint names
    with `-`; every nested Sequelize transaction);
  - an aggregate without GROUP BY next to joined columns (1064):
    ``SELECT max(`views`) AS `max`, `user`.`id` AS `user.id`, ... FROM `posts` AS `Post` INNER JOIN `users` AS `user` ON `Post`.`user_id` = `user`.`id` AND `user`.`email` = 'alice@example.com'``;
  - `SHOW INDEX FROM users FROM sequelize` (1064; `queryInterface.showIndex`,
    also used by `sync({ alter })` and `sync({ force })`);
  - `SELECT COUNT(*) AS n FROM posts WHERE views >= ? AND title <> ?`
    prepared (bind parameters): prepare succeeds, execute answers 1235;
  - **table-name case:** a table created as `SequelizeMeta` is listed as
    `sequelizemeta` by `information_schema.TABLES` (MySQL on Linux keeps the
    case), so `undo:all` and any case-sensitive check see the wrong name.
- **Drizzle:**
  - `select 1 from dual` (1235);
  - a named composite primary key in CREATE TABLE (1235):
    ``CONSTRAINT `post_tags_post_id_tag_id` PRIMARY KEY(`post_id`,`tag_id`)``;
    the migration's earlier tables stay created, so the migration is left half
    applied (as it would be on MySQL, where DDL is not transactional either);
  - a multi-row INSERT with `default` for some columns of some rows (1235):
    ``insert into `posts` (`id`, `user_id`, ...) values (default, 1, 'Hello', ...), (default, 1, 'Draft', 'Not yet', default, 0), ...``;
  - `JSON_SET` on a table-qualified column (1235):
    ``update `users` set `profile` = JSON_SET(`users`.`profile`, '$.city', 'Kyoto') where `users`.`id` = 2``;
  - `VALUES()` naming a table-qualified column in an upsert (1235):
    ``on duplicate key update `name` = values(`tags`.`name`)``;
  - information_schema columns qualified by the table name, used by
    `drizzle-kit push` and `pull` (1235 at prepare):
    `select * from INFORMATION_SCHEMA.STATISTICS WHERE INFORMATION_SCHEMA.STATISTICS.TABLE_SCHEMA = 'drizzle' and INFORMATION_SCHEMA.STATISTICS.INDEX_NAME != 'PRIMARY'`;
  - relational queries (`db.query.users.findMany({ with: ... })`) send
    `left join lateral (select coalesce(json_arrayagg(json_array(...)), json_array()) as data from (select *, row_number() over (order by ...) from posts ... where user_id = users.id) ...)`;
    not yet reached on turso past the missing tables.
- **sqlx** (from the mysql CLI, since sqlx cannot connect): its session setup
  `SET sql_mode=(SELECT CONCAT(@@sql_mode, ',PIPES_AS_CONCAT,NO_ENGINE_SUBSTITUTION')),time_zone='+00:00',NAMES utf8mb4 COLLATE utf8mb4_unicode_ci`
  answers 1235 and leaves `@@sql_mode` and `@@time_zone` unchanged; with
  `PIPES_AS_CONCAT` unset, `SELECT 'a' || 'b'` is 1064 (MySQL: 0 with a
  warning).

## 4. Schema browsers and JDBC metadata (P1)

Connector/J `DatabaseMetaData` (what DBeaver, IntelliJ, DbVisualizer and
Hibernate call):

- default (information_schema) mode: `getColumns` (1064, the query in 2),
  `getCrossReference` (1064,
  `SELECT DISTINCT A.REFERENCED_TABLE_SCHEMA AS PKTABLE_CAT, ... FROM INFORMATION_SCHEMA.KEY_COLUMN_USAGE A ...`),
  `getIndexInfo` (1235,
  `SELECT TABLE_SCHEMA AS TABLE_CAT, NULL AS TABLE_SCHEM, TABLE_NAME, NON_UNIQUE, ... FROM INFORMATION_SCHEMA.STATISTICS ...`),
  `getBestRowIdentifier` (1064, `SELECT 2 AS SCOPE, COLUMN_NAME, CASE ...`),
  `getTablePrivileges` (1064, `INFORMATION_SCHEMA.TABLE_PRIVILEGES`).
  `getTables`, `getPrimaryKeys`, `getImportedKeys`, `getExportedKeys`,
  `getCatalogs`, `getTypeInfo` and `getSQLKeywords` work.
- `useInformationSchema=false` (SHOW) mode: `SHOW KEYS FROM posts FROM dbtools`
  and `SHOW INDEX FROM users FROM dbtools` (1064, `getPrimaryKeys`,
  `getIndexInfo`), `SHOW FUNCTION STATUS WHERE Db = 'dbtools' AND Name LIKE '%'`
  (1064, `getProcedures`/`getFunctions`),
  `SELECT host, db, table_name, grantor, user, table_priv FROM mysql.tables_priv ...`
  (1064), and `SHOW FULL TABLES FROM <another database>` (1235) while
  `getExportedKeys` scans every database.
- **Result-set metadata differs** (what a result grid and every JDBC
  `ResultSetMetaData` user reads) for
  `SELECT u.id, u.balance, ... FROM users u JOIN posts p ...`:
  `users.id` BIGINT AUTO_INCREMENT reports scale 66 and no auto-increment flag
  (MySQL: scale 0, auto-increment); `users.balance` DECIMAL(10,2) reports
  precision 11, scale 0 (MySQL: 10, 2).

Statements the GUIs send (modeled, see above):

- DBeaver: `SHOW PLUGINS` (1235), `SELECT * FROM information_schema.VIEWS WHERE TABLE_SCHEMA=...`
  (1235), `information_schema.TRIGGERS`, `.EVENTS` and `.PARTITIONS` (1064).
- MySQL Workbench: `SET @@SESSION.autocommit = ON` and `SET SQL_SAFE_UPDATES=1`
  (1235), `SELECT st.* FROM performance_schema.events_statements_current st JOIN performance_schema.threads thr ...`
  (1064), `EXPLAIN SELECT ...` and `EXPLAIN FORMAT=JSON SELECT ...` (1235).
- TablePlus: `information_schema.TABLES` with `ENGINE`, `TABLE_ROWS`,
  `TABLE_COMMENT` (1054 unknown column), and
  `SHOW TABLE STATUS FROM dbtools WHERE Name = 'users'` (1064).

## Suggested priority

1. The two handshakes (1.1, 1.2): each closes a whole language ecosystem, and
   each is a validation rule, not missing SQL.
2. The migration-table DDL (`UNIQUE` + `PRIMARY KEY` on one column, `SERIAL`,
   `CHARSET=utf8`) and Flyway's startup queries: they stop Sequelize, Drizzle
   and every Flyway project before a single table exists.
3. Connector/J `getColumns` and `getIndexInfo` (Hibernate validate, DBeaver,
   IntelliJ), `SET SESSION TRANSACTION READ ONLY` (silently weakens every
   read-only Spring transaction), `COM_SET_OPTION`, UPDATE/DELETE with a table
   alias (Hibernate bulk operations).
4. The Sequelize shapes (nested join in parentheses, savepoint names with `-`,
   `SHOW INDEX ... FROM db`, table-name case) and the Drizzle shapes (`DUAL`,
   named composite PK, `default` in multi-row INSERT, qualified columns in
   `JSON_SET`, `VALUES()` and information_schema).
5. Result-set metadata of BIGINT AUTO_INCREMENT and DECIMAL columns, then the
   GUI-only statements (EXPLAIN, SHOW PLUGINS, information_schema.VIEWS /
   TRIGGERS / EVENTS / PARTITIONS, SHOW TABLE STATUS ... WHERE,
   performance_schema).

Also seen in the check run of the mysql CLI app: `COM_STATISTICS` (0x09, the
CLI's `status`) answers 1235; the step still passes.

## Harness changes in this run

- `COMPOSE_PROJECT_NAME` and `E2E_CARGO_TARGET_VOLUME` let a second copy of
  the harness run beside the main one.
- The proxy logs a handshake the server closes without answering, as an
  `authenticate` entry naming the client's capabilities, collation and plugin,
  and `E2E_PROXY_DUMP=1` writes every packet in hex to `packets.txt`.

# Seventh to ninth runs

All three on 2026-09-28; every app passes every step against MySQL 8.4.11.

The seventh run (at `264217efb`) kept the first nine apps where the fifth left
them, but one Prisma connection was refused with 1045 at sign-in and passed
when Prisma was run again alone. Two of its tries also ended with the turso
container exiting at boot, unable to create `/log/server.log` in the
bind-mounted directory `run.sh` had just emptied; `boot-turso.sh` now waits
for the directory before starting anything.

The eighth run (at `b5f306c4a`) came after the Java, protocol and Node work
and the fix for a key over a value stored rewritten — a `DATETIME(6)` written
without its fraction, which had stopped Spring's `delete p1_0 from posts p1_0`:

| app | steps passing on turso |
|---|---|
| django, gorm, laravel, mysqlcli, mysqldump, rails, sqlalchemy, typeorm | all |
| spring | 46/46 (was 2/46) |
| drizzle | 17/20 (was 0/20) |
| sequelize | 16/23 (was 2/23) |
| prisma | 13/22 |
| dbtools | 4/11 (was 3/11) |
| efcore | 1/21 |
| sqlx | 1/22 |

Prisma's loss was one of its two concurrent `tag.create` calls answering 1205
at once: the table's id counter lets one step in at a time and the frontend
passed its `Busy` straight up. Each counter step now waits its turn for as
long as the session waits for any lock (`c468c2f57`).

The ninth run (at `c468c2f57`) has Prisma back at 22/22 and everything else as
in the eighth, except one Spring step (`server-prep/revert-migration`) whose
new connection was closed by the server before any answer; it passed in the
eighth run. With Prisma's 1045 of the seventh run, that makes two sign-ins
refused once each in a full run and never when an app runs alone, which is
being looked into.

What still fails on turso alone:

- sqlx: `CREATE TABLE IF NOT EXISTS _sqlx_migrations (... installed_on
  TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP, success BOOLEAN NOT NULL,
  checksum BLOB NOT NULL ...)` answers 1235, so no migration runs and every
  later step finds no table.
- EF Core: a derived table over no table (`SELECT s.Value FROM (SELECT
  CONCAT(VERSION(), ' ', DATABASE()) AS Value) AS s LIMIT 2`), the scaffold's
  `information_schema.TABLES` read with `IF(...)`, its include and aggregate
  queries (1064), and `UPDATE Users AS u SET u.Balance = ...` (1235).
- Sequelize: table names read back lowercased (`sequelizemeta`), a join in
  parentheses (`LEFT OUTER JOIN (a INNER JOIN b ON ...) ON ...`, 1064),
  `UPDATE posts SET body = CONCAT(COALESCE(body, ''), '!')` (1235).
- Drizzle: its relational queries and a `count(...)` over a left join grouped
  by the key (1064).
- dbtools: `getTablePrivileges`, `SHOW PLUGINS`, `information_schema.PARTITIONS`,
  Workbench's `performance_schema` join and `EXPLAIN`, all left refused on
  purpose for now: none of them can be answered truthfully yet.

# Tenth and eleventh runs

Both on 2026-09-28; every app passes every step against MySQL 8.4.11.

The tenth run (at `1e569e12f`, after the sqlx and second Node rounds) had
Spring back at 46/46, Sequelize at 20/23 and Drizzle at 18/20, but sqlx at
9/22: the adapter replays had written each bound value's length in the
shortest form, and sqlx writes a JSON parameter's length in the nine-byte form
whatever it is, which the execute decoder refused (1064). MySQL takes it, and so
does this now (`cd73d5b38`); sqlx then passed 22/22. Sequelize's
`sync({ force: true })` dropped its keys by the names MySQL generates,
`post_tags_ibfk_1`, which answered 1235 (`09565f5e4`). Two Sequelize checks
assumed which of two posts written at once over two connections took the lower
id — a race on MySQL too — and now find the post by its title.

The eleventh run (at `0ffef16cb`, after the EF Core round):

| app | steps passing on turso |
|---|---|
| django, gorm, laravel, mysqlcli, mysqldump, prisma, rails, spring, sqlalchemy, sqlx, typeorm | all |
| sequelize | 21/23 |
| drizzle | 18/20 |
| efcore | 13/21, 16/21 after `SERIALIZABLE` was taken |
| dbtools | 4/11 |

EF Core's isolation step begins a `SERIALIZABLE` transaction, which
MySqlConnector sends as the level and `start transaction;` without waiting
between them; the refused level left the client reading every later answer one
behind, and the json and upsert steps failed on that. The level is taken now,
kept the way `REPEATABLE READ` is (see COMPAT.md for why that is serializable
here).

What still fails on turso alone:

- Table names read back lowercased — this server is `lower_case_table_names=1`
  (see COMPAT.md): Sequelize's migrate and undo-all (`SequelizeMeta`), EF
  Core's migrate and rollback-all (`__EFMigrationsHistory`).
- EF Core: the scaffold's catalog reads, the `Include` after `Take(1)`, and
  `Take(2)` with no order, each refused (see TODO.md).
- Drizzle: relational queries built from `LATERAL` joins of JSON aggregates.
  Taken since 2026-10-04: Drizzle passes 20/20.
- dbtools: `getTablePrivileges`, `SHOW PLUGINS`, `information_schema.PARTITIONS`,
  Workbench's `performance_schema` join and `EXPLAIN`, left refused on purpose.

# Gitea's own integration suite

2026-10-04, `E2E_GITEA_RUN='^TestDatabaseCollation$'` after `utf8mb4_general_ci`
databases, tables and `CONVERT TO`: `TestDatabaseCollation` passes on turso
and on MySQL, all four of its subtests among it.

2026-10-04, turso alone (`E2E_TARGETS=turso E2E_GITEA_SHARDS=60`), after
`utf8mb4_bin` databases and `CONVERT TO`: 385 of the 395 tests that pass on
MySQL pass, and every test is reported (485). Gitea now gives its database
`utf8mb4_bin` at startup, so every table is case-sensitive, as on MySQL (where
it is `utf8mb4_0900_as_cs`). `TestDatabaseCollation` passes its first part and
three of its four subtests; the one converting to `utf8mb4_general_ci` fails on
that collation. `TestPackageNuGet` still meets its derived table cut by a
`LIMIT` beside a join. `TestAPIOrg` refuses nothing: its background deletion of
org3's repositories takes about a second a repository in the debug build (each
of some 390 prepared statements about 16 ms), and the test waits two seconds.
Six later tests of the same shard then fail loading their fixtures with 1205,
the deletion still holding the write lock. A first run before orderings and
comparisons of words read through a call followed the column's collation
passed 347: the team, fork, issue and LFS lock tests were refused `ORDER BY
CASE WHEN name LIKE 'Owners' THEN '' ELSE name END`, `LOWER(name) IN (?)` and
`lower(path) = ?` over the now `utf8mb4_bin` tables.

2026-10-03, the same run (`E2E_GITEA_SHARDS=60`): 392 of the 395 tests that pass
on MySQL pass on turso, every test is reported on both (484), and none passes on
MySQL alone but for `TestDatabaseCollation` (`ALTER DATABASE ... COLLATE
utf8mb4_bin`), `TestPackageNuGet` (a derived table cut by a `LIMIT` joined to
`package_version`) and `TestViewIssuesKeyword` (the issue indexer not done
within the test's one second while the debug build fills it). turso took 1534 s,
MySQL 360 s. What changed since the run below: the heatmap's `DIV`, totals over
joins, derived tables inside membership tests and `DELETE`, `MAX` over a joined
column in a membership test and against a written number, `COUNT(DISTINCT(x))`,
`LOWER(col) IN (...)`, the milestone completeness quotient written into an
`INT`, a `VARBINARY` sent in a prepared statement's rows (it closed the
connection), an `OR` inside a join's `ON`, and MVCC recovery that rebuilt the
schema for every logged frame (a database with a long log took minutes to open).

2026-09-29. Gitea v1.27.3's `tests/integration`, run through the harness
(`run.sh gitea`, `E2E_GITEA_SHARDS=60`), with the 115 tests that drive Gitea
Actions through a mock runner left out (they fail against MySQL here too).

| | MySQL 8.4.11 | turso |
|---|---|---|
| tests reported | 484 | 468 |
| tests passing | 395 | 352 |
| wall time | 231 s | 4300 s (debug build) |

352 of the 395 tests that pass on MySQL pass on turso (89%); 33 fail, 10 are not
reported because a turso-caused panic in another test ended their shard, and
no test passes on turso that fails on MySQL. MySQL's own 89 failures are this
setup's (LFS, SSH, some git operations), not the database's.

Before this round the suite stopped before any test, in xorm's first catalog
read (`information_schema.TABLES.AUTO_INCREMENT`, 1054). Getting through setup
took the catalog columns xorm reads, `SHOW COLLATION WHERE`, `ROW_FORMAT=DYNAMIC`,
`BIGINT(20)` keys, and a stream of query shapes Gitea's permission, search and
issue pages use (see the commits). Two server faults came out of it that no
scripted app had reached: a deep query overflowing a connection thread's stack
aborted the whole server, and opening a database while another connection's
insert held its id counter marked the database registry broken, answering 1105
to every connection until restart (3 of 5 full runs). Both are fixed.

What still stops the rest, by tests affected (TODO.md has the statements):

- the package registry's aggregates and derived tables over joins;
- a derived table inside the `DELETE` every repository and user deletion runs;
- `COALESCE(SUM(...), 0)` over a join on every issue list;
- `COUNT(DISTINCT ...)` grouped over a join, the heatmap's `DIV ... GROUP BY`;
- concurrency: a write whose snapshot went stale answers 1213 where MySQL's row
  locks let both commit, and Gitea does not retry; a counted insert can take an
  id another open transaction then writes explicitly (1062). These come from
  the one database-wide write lock and are the real gap for a busy instance.
