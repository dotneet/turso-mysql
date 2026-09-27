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
