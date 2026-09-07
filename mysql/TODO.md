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

### Written with syntax rather than an argument list

| Form | State |
|---|---|
| `TRIM` with more than one character to remove — `TRIM(LEADING 'ax' FROM col)` | refused; MySQL removes whole copies of it and the engine removes any of its characters, so they agree only at one character |
| `TRIM(LEADING FROM col)` — a bare side | refused; `sqlparser` does not read the spelling, and the one it does read is not MySQL |

### Conditional expressions

`CASE WHEN ... THEN ... ELSE ... END` and `IF(cond, a, b)` work over string
literal branches. What is left:

| Form | State |
|---|---|
| `CASE col WHEN v THEN ...` | refused; it compares its operand, which raises the coercion question a `WHERE` comparison raises, unmeasured |
| Branches that are not string literals or `NULL` | refused; no width to answer with |
| A `CASE` whose every branch is `NULL` | refused; the same, there is no width left |

### Waiting on a column type

| Function | Blocked by |
|---|---|
| `CURTIME()` | there is no `TIME` column type yet; measured shape is `TIME`, length 8, NOT NULL |
| `YEAR()` / `MONTH()` / `DAY()` | not started; measured, `YEAR(NOW())` answers a `YEAR` of length 4 with the unsigned and numeric flags and no zerofill, while `MONTH` and `DAY` each answer a `LONGLONG` of length 3 |
| `DATE_ADD` / `DATE_SUB` / `DATEDIFF` | not started; `INTERVAL` is a node of sqlparser's own, so `DATE_ADD` does not arrive as a call |

### Not looked at

Aggregates: `GROUP_CONCAT` with `SEPARATOR`, `ORDER BY` or `DISTINCT`,
`STDDEV`, `VARIANCE`.
Every window function MySQL has is taken, with a `ROWS` or `RANGE` frame and a
`WINDOW` clause. What is refused around them: a `GROUPS` frame, which MySQL
answers 1235 for, a frame bound that is not a plain non-negative number, a
window term that is not a plain column, a named window standing for another
name or built on one, a windowed aggregate over anything but one plain column,
and a `LAG` or `LEAD` carrying an offset or a default.
Strings: `LOCATE` with three arguments, `HEX` over a numeric column,
`FORMAT`, `MD5`, `UUID`.
Numbers: `TRUNCATE`, `RAND`.
Temporal: `DATE_FORMAT`, `STR_TO_DATE`, `YEAR`, `MONTH`, `DAY`, `UNIX_TIMESTAMP`.
JSON: the whole `JSON_*` family.

---

## SQL syntax

### `SELECT`

| Form | State |
|---|---|
| Scalar subquery in a projection answering a column rather than an aggregate — `SELECT (SELECT n FROM t)` | refused; measured, MySQL answers 1242 for a subquery returning more than one row where the engine answers the first row it finds. Taking it needs the inner statement to prove it answers at most one row — an `ORDER BY ... LIMIT 1`, which the subquery reader refuses today |
| Correlated subquery | not started |
| Subquery anywhere but a `WHERE` | not started |
| `ORDER BY` an ordinal over a mixed wildcard projection — `SELECT t.*, id FROM t ORDER BY 2` | refused; requires expanding each wildcard to count through |
| `WITH ROLLUP` | refused |
| `HAVING` with no `GROUP BY` over an unaggregated statement — `SELECT id FROM t HAVING id > 1` | refused; MySQL answers it as a second `WHERE`, the aggregated form is taken |
| `EXCEPT ALL`, `INTERSECT ALL` | refused; they keep duplicates the plain forms collapse, and the engine has no spelling for them |
| A `UNION` branch with its own `ORDER BY` or `LIMIT` | refused |
| The comma join | refused |
| An unqualified name in a joined projection that no `USING` merges | refused; ambiguous whenever both tables carry it |
| A `WHERE` comparison in a joined statement | refused; the checked path validates against one table |
| `WITH RECURSIVE` | refused |
| A wildcard projection in a CTE body | refused; no name to resolve an ordinal through |
| `DISTINCT ON` | refused, and no part of MySQL |

### DDL

| Form | State |
|---|---|
| `ALTER TABLE` beyond `ADD COLUMN` / `DROP COLUMN` / `RENAME` / the index operations | refused |
| `ALTER TABLE ... DROP KEY` | refused; MySQL's other spelling for `DROP INDEX`, and `sqlparser` reads only the one |
| `ALTER TABLE` mixing index and column operations | refused; two kinds of change would have to apply together |
| `ALTER TABLE ... ADD/DROP INDEX \`PRIMARY\`` | refused; adding or dropping a primary key is a different operation |
| `CREATE TABLE ... AS SELECT` over a division, an aggregate, or an unaliased expression | refused; integer `+`, `-` and `*` work. A division makes a `decimal(14,4)` on a rule of its own, and an unaliased expression column takes its name from the expression's own text — measured, `SELECT a + 1` makes a column called `a + 1` |
| `CREATE TABLE ... AS SELECT` over a column with a string `DEFAULT` | refused; the escaping is undecided, the same reason `SHOW CREATE TABLE` refuses to print one |
| `CREATE TABLE ... (columns) AS SELECT`, `IF NOT EXISTS`, `TEMPORARY` | refused |
| `CREATE TEMPORARY TABLE` with `AUTO_INCREMENT` | refused; the allocator is keyed on a durable table |
| `FOREIGN KEY` | parsed by the parser and refused by the frontend. **What blocks it is not parsing.** The engine runs with `PRAGMA foreign_keys` off, so a constraint taken here would not be enforced, where MySQL answers 1452 for a child row whose parent does not exist. Taking the syntax first would hand a client a guarantee it does not have. Turning the pragma on is the work, and it has to be weighed against what it does to existing databases. The inline column spelling — `parent_id INT REFERENCES parent(id)` — is a separate case: MySQL parses and **ignores** it, measured, so the faithful answer there is to take it and write no constraint. |
| Column `COMMENT` | refused |
| Column `CHARACTER SET` / `COLLATE` naming anything but this server's own | refused; another collation is a claim about ordering and case this cannot keep |
| Generated columns | refused |
| Partitioning | refused |

### DML

| Form | State |
|---|---|
| `INSERT` naming an `AUTO_INCREMENT` column its own value — `INSERT INTO ai (id, v) VALUES (10, 3)` | refused on both the column-list and the `SET` form; the allocator reserves before the row is written |
| `INSERT ... ON DUPLICATE KEY UPDATE` on an `AUTO_INCREMENT` table, or beside `REPLACE`/`IGNORE` | refused; the `SET` form takes it, being written out as the column-list one |
| `INSERT IGNORE` writing NULL, or into an `AUTO_INCREMENT` table | refused; MySQL coerces a NULL where the engine skips the row, and the allocator reserves before IGNORE can skip |
| `INSERT IGNORE` coercing a value MySQL would clamp | refused instead; needs the coercion `INSERT` does not have either |
| `INSERT ... SELECT` whose `SELECT` needs a second rendering pass | refused; there is no way to ask for that pass from a DML statement |
| `INSERT ... SELECT` without a column list, carrying `IGNORE` or an upsert clause | refused; those forms are refused wherever they are written |
| `UPDATE` / `DELETE` over more than one table | refused |
| `LIMIT` with no `ORDER BY`, or an `ORDER BY` over a column that is not an integer, on an `UPDATE` / `DELETE` | refused |
| `TRUNCATE TABLE` on an `AUTO_INCREMENT` table | refused; MySQL restarts the counter at 1 and the durable allocator only moves its high water forward |

---

## Transactions and locking

| Feature | State |
|---|---|
| `BEGIN` / `START TRANSACTION`, `COMMIT`, `ROLLBACK` | works |
| `SET autocommit = 0 \| 1` | works |
| `LOCK TABLES` / `UNLOCK TABLES` | **deliberately refused.** Accepting it would tell a client its locks are held when they are not. A `READ` lock could honestly become a read transaction for the locking session, but that still would not block another session's writes the way MySQL does, so the shape needs deciding before it is written. `mysqldump --single-transaction` needs none of it. |
| A savepoint over an in-memory database | taken and does nothing; the engine needs the pager's sub-journal to undo anything, and over an in-memory database `SAVEPOINT` and `ROLLBACK TO` both answer OK without rolling back. The server runs over files, where it works |
| A savepoint name that is a reserved word — `SAVEPOINT select` | taken; MySQL answers 1064 unless it is quoted |
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
| `SHOW PROCESSLIST` | not started |
| `SHOW TABLE STATUS` with `WHERE` | refused; the `FROM`/`IN` and `LIKE` forms work, and a `WHERE` is a predicate over the eighteen columns rather than a pattern |
| `SHOW TABLE STATUS` storage figures | answered NULL; InnoDB keeps them and this does not |
| `SHOW ENGINE INNODB STATUS`, `SHOW STORAGE ENGINES` | refused; the first reports InnoDB internals this server does not have |
| `SHOW TABLES` with `WHERE` | refused; the `LIKE` form works, and a `WHERE` is a predicate over the one column rather than a pattern |
| `SHOW TABLES` from another database | refused; the `FROM`/`IN` forms name a database this session has not selected |
| `SHOW COLUMNS` with `WHERE` | refused; the `LIKE` form works, as does the `DESCRIBE t <name>` spelling of it |
| `SHOW FULL COLUMNS` `Privileges` | answered NULL; MySQL reports the user's grants on the column and this server's grants are per database and per table |
| `EXPLAIN <statement>` | refused; `EXPLAIN <table>` works, being what MySQL makes it — the same rows `DESCRIBE` prints. The statement form is the optimizer's own plan over twelve columns, and answering it would mean writing down a join order, a key choice and a row estimate this server does not make |
| `FLUSH TABLES` | not started |
| `OPTIMIZE TABLE` | refused; MySQL's InnoDB does a recreate and analyze, and the engine's nearest thing is a database-wide `VACUUM` — far more than one table was asked for |
| `CHECK TABLE` with a list, a qualified name, or the `QUICK` / `FOR UPGRADE` / `EXTENDED` options | refused; one unqualified table at a time is taken |
| `ANALYZE TABLE` over several tables, or with `NO_WRITE_TO_BINLOG`, `LOCAL` or a histogram clause | refused; one unqualified table at a time is taken |
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
| A user variable set to anything but a literal — `SET @y := @x + 1`, `SET @x = (SELECT ...)` | refused; taking it needs an expression evaluated without a table under it |
| A user variable beside anything else in a projection — `SELECT @x, id FROM t` | refused; the reader answers a projection of variables and nothing else |
| An assignment inside a projection — `SELECT @x := id FROM t` | refused |
| A user variable set to a value wider than an `i64` | refused; MySQL answers it as an unsigned LONGLONG and the engine holds an integer as an `i64` |

---

## Column types

| Type | State |
|---|---|
| `TINYINT`, `SMALLINT`, `MEDIUMINT`, `INT`, `BIGINT`, `BOOLEAN` | works |
| `TINYINT`/`SMALLINT`/`MEDIUMINT`/`INT` `UNSIGNED` | works |
| `VARCHAR`, `CHAR`, `TEXT`, `TINYTEXT`, `MEDIUMTEXT`, `LONGTEXT`, `BLOB`, `TINYBLOB`, `MEDIUMBLOB`, `LONGBLOB` | works |
| `DECIMAL`, `DOUBLE`, `FLOAT` | works |
| `DATETIME`, `TIMESTAMP` | works |
| `BIGINT UNSIGNED` | refused; its top value 18446744073709551615 is more than twice `i64::MAX` and the engine holds an integer as an `i64` |
| `UNSIGNED` on `DECIMAL`, `DOUBLE`, `FLOAT` | works |
| Arithmetic and aggregates over an unsigned column | not measured; the result's own type and width have not been recorded |
| `DATE` | works |
| `TIME`, `YEAR` | not started; `YEAR` reports the zerofill flag and no binary flag, unlike every other temporal column |
| A `DATE` written in a spelling MySQL would normalize — `'2026-9-6'`, `'20260906'`, `'2026-09-06 01:02:03'` | refused; MySQL stores `2026-09-06` for each, and this takes the normalized form only, as it does for a `DATETIME` |
| A `WHERE` comparison against a `DATE` column | refused; the checked comparison path knows integers and text, and what a date compares against is its own rule |
| `ENUM`, `SET` | not started, and blocked on the same seam `JSON` is. A table's durable MySQL DDL is written back out of the **engine's** AST — `prepare` renders it with `render_create_table_mysql_with_mode(&stmt, mode)` — so a type the engine cannot spell is lost at the first step. The engine's declared type takes a word before its arguments and numbers inside them, and `ENUM('a','b')` is neither: measured, `CREATE TABLE e (s ENUM('a','b'))` answers `near "'small'": syntax error` in the engine. Keeping the members would mean keeping the original MySQL DDL durably, the way the v2 `AUTO_INCREMENT` metadata already does for its own facts. There is a second thing to decide after that: measured on 8.4.11, `ORDER BY` on an `ENUM` orders by the member's declared position, so `small, medium, large` come back in that order and not alphabetically |
| `JSON` | not started; the column reports type 245 with length 4294967295 and the blob and binary flags, but the work is the value rather than the type — measured on 8.4.11, MySQL **normalizes** what it stores, sorting an object's keys and respacing an array, so `{"b": 1, "a": 2}` reads back as `{"a": 2, "b": 1}` and `[1,  2,3]` as `[1, 2, 3]`. Storing the text as it was given would be a silent difference, and the engine's own `json()` minifies without sorting, so this needs MySQL's normalizer written before the type is taken. The rules are measured: an object's keys sort by length and then bytewise (`{"ab","ba","b"}` reads back `b, ab, ba`), members are separated by `", "` and a key from its value by `": "`, and a number written without a point or an exponent keeps its integer form while any other becomes a double printed with a point or an exponent — `1.500` reads back `1.5`, `1e2` reads back `100.0` and `99999999999999999999999` reads back `1e23`. Invalid JSON answers 3140. The normalizer also needs somewhere to run: a value is rewritten on the way in, and the write path has no seam for that today — the assignment validator only validates, and the DML renderer does not know the column types, though the two-pass `SELECT` rendering shows the shape such a seam would take |
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
- a windowed statement's other columns report no table and no key flags, because
  the engine answers them out of the window's own sorter
- `SHOW FULL COLUMNS` answers NULL for `Privileges`, where MySQL lists the
  connected user's grants on the column
- an index name is per table in MySQL and database-wide in the engine, so two
  tables cannot carry an index of the same name here. Naming the engine's index
  after the table it belongs to is what this needs, and that changes what
  existing databases already store

---

## Not verified

Nothing below has been run against a real client; the frontend is checked
against a pinned MySQL 8.4.11 oracle and its own tests.

- JDBC, PHP `mysqli`, Go `go-sql-driver`, Python `PyMySQL` / `mysqlclient`
- ORMs
- the `mysql` command-line client end to end, beyond the queries it opens with
- `mysqldump` and restore, end to end
