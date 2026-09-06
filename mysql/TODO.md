# What the MySQL frontend does not do yet

A checklist of what is left, kept apart from
[COMPAT.md](COMPAT.md) on purpose: that file explains what this server
*does* and where it differs from MySQL, in prose, and it is long. This one
is the list you read to pick up the next piece of work.

Detailed instructions for the easier entries live in
[todos/](todos/) — one file per task, with what to measure on the oracle,
which functions to change, and which tests to write.

An entry leaves this file when the thing works. A behaviour that works but
differs from MySQL belongs in COMPAT.md instead, under **Known divergences**
below, which points at it.

Measured shapes for several of these are already recorded — see the
"measured" notes — so the work is implementing them, not finding out what
MySQL does.

---

## Functions

### Blocked on one shared piece of work

`sqlparser` gives each of these its own AST node rather than a call, because
MySQL's syntax for them is not a plain argument list. Reading those four
nodes is one job, not four.

| Function | Why it is not just another name |
|---|---|
| `TRIM` | `TRIM(LEADING 'x' FROM col)` and its `TRAILING` / `BOTH` forms |

### Conditional expressions

`CASE WHEN ... THEN ... ELSE ... END` and `IF(cond, a, b)` work over string
literal branches. What is left:

| Form | State |
|---|---|
| `CASE col WHEN v THEN ...` | refused; it compares its operand, which raises the coercion question a `WHERE` comparison raises, unmeasured |
| A `CASE` with no `ELSE` | refused; a row matching nothing answers NULL, and that shape is unmeasured |
| Branches that are not string literals | refused; no width to answer with |
| `NULLIF` | not measured |

### Waiting on a column type

| Function | Blocked by |
|---|---|
| `CURDATE()` / `CURRENT_DATE` | there is no `DATE` column type yet; measured shape is `DATE`, length 10, NOT NULL |
| `DATE_ADD` / `DATE_SUB` / `DATEDIFF` | the same |

### Not looked at

Aggregates: `GROUP_CONCAT`, `COUNT(DISTINCT ...)`, `STDDEV`, `VARIANCE`,
window functions.
Strings: `LPAD`, `RPAD`, `LOCATE`, `INSTR`,
`FORMAT`, `HEX`, `MD5`, `UUID`.
Numbers: `MOD` as a call, `POW`, `SQRT`, `SIGN`, `TRUNCATE`, `RAND`,
`GREATEST`, `LEAST`.
Temporal: `DATE_FORMAT`, `STR_TO_DATE`, `YEAR`, `MONTH`, `DAY`, `UNIX_TIMESTAMP`.
JSON: the whole `JSON_*` family.

---

## SQL syntax

### `SELECT`

| Form | State |
|---|---|
| Scalar subquery in a projection — `SELECT (SELECT MAX(n) FROM t)` | not started; measured: the inner aggregate's own type, no table |
| Correlated subquery | not started |
| Subquery anywhere but a `WHERE` | not started |
| `IN` over a list of values in `UPDATE` / `DELETE` — `DELETE FROM t WHERE id IN (1, 2)` | refused; the `SELECT` path takes it, the DML path has its own predicate renderer |
| `<=>` | refused |
| Comparison against a qualified column — `WHERE t.id = 1` | refused; blocks `WHERE c.id = 1` on a CTE |
| `ORDER BY` an ordinal over a wildcard projection — `SELECT * FROM t ORDER BY 2` | refused; no names written down to count through |
| `WITH ROLLUP` | refused |
| `HAVING` with no `GROUP BY` over an unaggregated statement — `SELECT id FROM t HAVING id > 1` | refused; MySQL answers it as a second `WHERE`, the aggregated form is taken |
| `EXCEPT ALL`, `INTERSECT ALL` | refused; they keep duplicates the plain forms collapse, and the engine has no spelling for them |
| A parenthesised `UNION` branch | refused |
| `CROSS JOIN`, `USING`, the comma join | refused |
| A `WHERE` comparison in a joined statement | refused; the checked path validates against one table |
| `WITH RECURSIVE` | refused |
| A wildcard projection in a CTE body | refused; no name to resolve an ordinal through |
| `DISTINCT ON` | refused, and no part of MySQL |

### DDL

| Form | State |
|---|---|
| `ALTER TABLE` beyond `ADD COLUMN` / `DROP COLUMN` / `RENAME` | refused |
| `CREATE TABLE ... AS SELECT` | refused |
| `CREATE TEMPORARY TABLE` with `AUTO_INCREMENT` | refused; the allocator is keyed on a durable table |
| `FOREIGN KEY` | parsed by the parser and refused by the frontend. **What blocks it is not parsing.** The engine runs with `PRAGMA foreign_keys` off, so a constraint taken here would not be enforced, where MySQL answers 1452 for a child row whose parent does not exist. Taking the syntax first would hand a client a guarantee it does not have. Turning the pragma on is the work, and it has to be weighed against what it does to existing databases. The inline column spelling — `parent_id INT REFERENCES parent(id)` — is a separate case: MySQL parses and **ignores** it, measured, so the faithful answer there is to take it and write no constraint. |
| Column `COMMENT` | refused |
| Column `CHARACTER SET` / `COLLATE` naming anything but this server's own | refused; another collation is a claim about ordering and case this cannot keep |
| Generated columns | refused |
| Partitioning | refused |

### DML

| Form | State |
|---|---|
| `INSERT ... SET` into an `AUTO_INCREMENT` table | refused; the allocator reads only the column-list form |
| `INSERT ... ON DUPLICATE KEY UPDATE` on an `AUTO_INCREMENT` table, on the `SET` form, or beside `REPLACE`/`IGNORE` | refused |
| `INSERT IGNORE` writing NULL, or into an `AUTO_INCREMENT` table | refused; MySQL coerces a NULL where the engine skips the row, and the allocator reserves before IGNORE can skip |
| `INSERT IGNORE` coercing a value MySQL would clamp | refused instead; needs the coercion `INSERT` does not have either |
| `INSERT ... SELECT` without a column list, or one whose `SELECT` needs a second rendering pass | refused |
| `UPDATE` / `DELETE` over more than one table | refused |
| `ORDER BY` or `LIMIT` on an `UPDATE` / `DELETE` | refused |
| `TRUNCATE TABLE` | refused |

---

## Transactions and locking

| Feature | State |
|---|---|
| `BEGIN` / `START TRANSACTION`, `COMMIT`, `ROLLBACK` | works |
| `SET autocommit = 0 \| 1` | works |
| `LOCK TABLES` / `UNLOCK TABLES` | **deliberately refused.** Accepting it would tell a client its locks are held when they are not. A `READ` lock could honestly become a read transaction for the locking session, but that still would not block another session's writes the way MySQL does, so the shape needs deciding before it is written. `mysqldump --single-transaction` needs none of it. |
| `SAVEPOINT`, `ROLLBACK TO SAVEPOINT`, `RELEASE SAVEPOINT` | not started |
| `SET TRANSACTION ISOLATION LEVEL` naming a level other than `REPEATABLE READ`, or `GLOBAL` | refused; the sessions run at `REPEATABLE READ` and saying yes to another would be a guarantee this does not keep |
| `SELECT ... FOR UPDATE` / `LOCK IN SHARE MODE` | not started |
| `COMMIT AND RELEASE`, `ROLLBACK AND RELEASE` | refused; MySQL closes the connection after them, which is a protocol behaviour rather than a statement |
| `COMMIT AND NO CHAIN` | refused; it is the default spelled out, but the token check takes only the forms it knows |
| `GET_LOCK` / `RELEASE_LOCK` | not started |
| `XA` transactions | not started |

---

## Statements and administration

| Statement | State |
|---|---|
| `SHOW WARNINGS`, `SHOW ERRORS` | works |
| `SHOW COUNT(*) WARNINGS`, `SHOW COUNT(*) ERRORS` | refused |
| `SHOW PROCESSLIST` | not started |
| `SHOW TABLE STATUS` with `FROM`, `LIKE` or `WHERE` | refused; the plain form is taken |
| `SHOW TABLE STATUS` storage figures | answered NULL; InnoDB keeps them and this does not |
| `SHOW ENGINE INNODB STATUS`, `SHOW STORAGE ENGINES` | refused; the first reports InnoDB internals this server does not have |
| `SHOW TABLES` / `SHOW COLUMNS` with `LIKE` or `WHERE` | refused |
| `EXPLAIN` | not started |
| `FLUSH TABLES`, `ANALYZE`, `OPTIMIZE`, `CHECK TABLE` | not started |
| `CREATE USER`, `GRANT`, `REVOKE` | not started |
| Stored procedures, functions, events | not started |
| `information_schema` beyond `TABLES`, `COLUMNS`, `SCHEMATA` | not started |
| Multi-statement `COM_QUERY` | refused |

---

## Session variables

| Variable | State |
|---|---|
| `@@version`, `@@version_comment`, `VERSION()` | works |
| `@@max_allowed_packet`, `@@wait_timeout`, `@@sql_notes` | works |
| `SET NAMES`, `SET sql_mode`, `SET time_zone`, `SET information_schema_stats_expiry` | taken when they name the state the server is already in |
| Any other `@@name` | refused rather than answered with a value the server does not have |
| User variables — `SET @x = 1`, `SELECT @x` | not started |

---

## Column types

| Type | State |
|---|---|
| `TINYINT`, `SMALLINT`, `MEDIUMINT`, `INT`, `BIGINT`, `BOOLEAN` | works |
| `TINYINT`/`SMALLINT`/`MEDIUMINT`/`INT` `UNSIGNED` | works |
| `VARCHAR`, `CHAR`, `TEXT`, `BLOB` | works |
| `DECIMAL`, `DOUBLE`, `FLOAT` | works |
| `DATETIME`, `TIMESTAMP` | works |
| `BIGINT UNSIGNED` | refused; its top value 18446744073709551615 is more than twice `i64::MAX` and the engine holds an integer as an `i64` |
| `UNSIGNED` on `DECIMAL`, `DOUBLE`, `FLOAT` | refused |
| Arithmetic and aggregates over an unsigned column | not measured; the result's own type and width have not been recorded |
| `DATE`, `TIME`, `YEAR` | not started; blocks `CURDATE()` and the date functions |
| `ENUM`, `SET` | not started |
| `TINYTEXT` / `MEDIUMTEXT` / `LONGTEXT`, and the `BLOB` sizes | not started |
| `JSON` | not started |
| `BINARY(n)` | refused; MySQL pads a shorter value with NUL bytes to the declared width and the engine has no padding, so taking it would store a different value |
| Fractional seconds — `DATETIME(3)` | refused |

---

## Known divergences

Behaviour that works but does not match MySQL lives in
[COMPAT.md](COMPAT.md), not here. The open ones, each explained there:

- the collation ignores case but not accents, so `'café' = 'cafe'` is true in
  MySQL and false here
- `DECIMAL` is held as a binary64, so it is not rounded to the declared scale
  on the way in and does not sum exactly
- `REPLACE` counts a replaced row once where MySQL counts it twice, because
  the engine does not count the delete
- a `MIN` over a `TEXT` column reports the column's length where MySQL
  reports 1048560
- `TIMESTAMP` stores the text it was given and converts no zone
- `FLOAT` is stored as a binary64 and rounded to binary32 only on the way out
- a compound query's column is always nullable, since the engine reports only
  the first branch's column
- an `ON DUPLICATE KEY UPDATE` that updates a row counts 1 where MySQL counts 2,
  and 1 where MySQL counts 0 for an update that changes nothing

---

## Not verified

Nothing below has been run against a real client; the frontend is checked
against a pinned MySQL 8.4.11 oracle and its own tests.

- JDBC, PHP `mysqli`, Go `go-sql-driver`, Python `PyMySQL` / `mysqlclient`
- ORMs
- the `mysql` command-line client end to end, beyond the queries it opens with
- `mysqldump` and restore, end to end
