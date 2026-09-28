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
