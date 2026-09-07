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

## What this frontend is for

It stands in for MySQL while a system's **integration tests** run, so the
work worth doing is the SQL a test suite and the code under test actually
write: statement syntax, column types, and functions. Three things are
therefore **out of scope** rather than pending — `XA` transactions,
partitioning, and stored programs (procedures, functions, events, triggers
written as programs). A test suite does not reach for them, and each is a
feature area of its own. They stay listed below so that a client meeting
one gets a refusal rather than a wrong answer, and they are not work to
pick up.

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
| `HOUR()` / `MINUTE()` / `SECOND()` over a `TIME` | refused; a `TIME` holds a span running to 838 hours, which MySQL reads out whole and the engine has no reader for |
| `YEAR()` / `MONTH()` / `DAY()` over anything but a plain date column | refused; measured, `YEAR` over a `TIME` answers the current year, which is a coercion rather than a reading |
| `GROUP_CONCAT` with an `ORDER BY`, or with `DISTINCT` beside a `SEPARATOR` | refused; MySQL orders the parts it joins and the engine's `group_concat` has no way to say in what order, and the engine takes `DISTINCT` only over a single argument, which the separator occupies |
| `STDDEV`, `STD`, `STDDEV_POP`, `VAR_POP`, `VAR_SAMP`, `VARIANCE` | refused; the engine has one deviation aggregate and it is the sample form, so `STDDEV_SAMP` is taken and the population ones would answer a different number — measured over 2,4,4,4,5,5,7,9 the sample form is 2.138089935299395 and the population one is 2. A variance is the square of a deviation, and squaring a rounded square root would answer different last digits than MySQL's own |
| `DATE_ADD` / `DATE_SUB` over an interval of weeks or quarters | refused; the engine has no modifier for either, and answering with a shift of a different size would be worse than refusing |

### Not looked at

Every window function MySQL has is taken, with a `ROWS` or `RANGE` frame and a
`WINDOW` clause. What is refused around them: a `GROUPS` frame, which MySQL
answers 1235 for, a frame bound that is not a plain non-negative number, a
window term that is not a plain column, a named window standing for another
name or built on one, a windowed aggregate over anything but one plain column,
and a `LAG` or `LEAD` carrying an offset or a default.
Numbers: a seeded `RAND(n)` is refused: the engine has no seeded random, so
answering one would answer a different sequence.
Temporal: `FROM_UNIXTIME(n, format)`, whose width is a rule of its own.
JSON: `JSON_SEARCH` with a path to search inside. `JSON_SET`,
`JSON_INSERT`, `JSON_REPLACE` and `JSON_REMOVE` take a path naming one member
of the top-level object and refuse a wider one, which is the range the engine
and MySQL agree on. `JSON_ARRAY` and `JSON_OBJECT` refuse a nested call and a
boolean literal.

---

## SQL syntax

### `SELECT`

| Form | State |
|---|---|
| Scalar subquery in a projection answering a column rather than an aggregate — `SELECT (SELECT n FROM t)` | refused; measured, MySQL answers 1242 for a subquery returning more than one row where the engine answers the first row it finds. Taking it needs the inner statement to prove it answers at most one row — an `ORDER BY ... LIMIT 1`, which the subquery reader refuses today |
| Subquery anywhere but a `WHERE` | not started |
| `ORDER BY` an ordinal over a mixed wildcard projection — `SELECT t.*, id FROM t ORDER BY 2` | refused; requires expanding each wildcard to count through |
| `WITH ROLLUP` | refused |
| `HAVING` with no `GROUP BY` over an unaggregated statement — `SELECT id FROM t HAVING id > 1` | refused; MySQL answers it as a second `WHERE`, the aggregated form is taken |
| `EXCEPT ALL`, `INTERSECT ALL` | refused; they keep duplicates the plain forms collapse, and the engine has no spelling for them |
| A `UNION` branch with its own `ORDER BY` or `LIMIT` | refused |
| `WITH RECURSIVE` | refused |
| A wildcard projection in a CTE body | refused; no name to resolve an ordinal through |
| `DISTINCT ON` | refused, and no part of MySQL |

### DDL

| Form | State |
|---|---|
| `ALTER TABLE` beyond `ADD COLUMN` / `DROP COLUMN` / `RENAME` / `MODIFY COLUMN` / `CHANGE COLUMN` / the index operations | refused |
| `ALTER TABLE ... MODIFY/CHANGE COLUMN ... FIRST` or `AFTER x` | refused; the engine cannot move a column |
| `ALTER TABLE ... MODIFY/CHANGE COLUMN` on the primary-key column | refused; MySQL keeps the key through one and replacing the column would drop it |
| `ALTER TABLE` mixing index and column operations | refused; two kinds of change would have to apply together |
| `ALTER TABLE ... ADD/DROP INDEX \`PRIMARY\`` | refused; adding or dropping a primary key is a different operation |
| `CREATE TABLE ... AS SELECT` over a division, an aggregate, or an unaliased expression | refused; integer `+`, `-` and `*` work. A division makes a `decimal(14,4)` on a rule of its own, and an unaliased expression column takes its name from the expression's own text — measured, `SELECT a + 1` makes a column called `a + 1` |
| `CREATE TABLE ... AS SELECT` over a column with a string `DEFAULT` | refused; the escaping is undecided, the same reason `SHOW CREATE TABLE` refuses to print one |
| `CREATE TABLE ... (columns) AS SELECT`, `IF NOT EXISTS`, `TEMPORARY` | refused |
| `CREATE TEMPORARY TABLE` with `AUTO_INCREMENT` | refused; the allocator is keyed on a durable table |
| `FOREIGN KEY` | works, and enforced |
| The index MySQL creates beside a `FOREIGN KEY` | not created; measured, InnoDB adds `` KEY `a` (`a`) `` for the child column and prints it, and this does not, so `SHOW CREATE TABLE` differs by that one line |
| `ALTER TABLE ... ADD FOREIGN KEY` without a `CONSTRAINT` name | refused; MySQL names it `t_ibfk_N` counting the keys the table already carries, which is naming this does not do |
| The index MySQL creates beside a `FOREIGN KEY` an `ALTER TABLE` adds | not created, the same as the one a `CREATE TABLE` declares |
| Column `COMMENT` | refused |
| Column `CHARACTER SET` / `COLLATE` naming anything but this server's own | refused; another collation is a claim about ordering and case this cannot keep |
| Generated columns | refused |
| Partitioning | refused, and out of scope — see what this frontend is for |

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
| `XA` transactions | refused, and out of scope — see what this frontend is for |

---

## Statements and administration

Every hand-built `SHOW` result was first measured through a `mysql` client left
on its latin1 default, which makes MySQL report a text column's collation and
width scaled to latin1. All of them are re-measured now with
`--default-character-set=utf8mb4`, which is the only connection this server
speaks; anything measured here from now on has to pass that flag.

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
| `FLUSH` of anything but `TABLES`, or `FLUSH TABLES` with a table list or `WITH READ LOCK` | refused; the plain form asks for a closed table cache, which this server has none of, while a read lock held across statements, reloaded grants and rotated logs are each something it cannot deliver |
| `OPTIMIZE TABLE` | refused; MySQL's InnoDB does a recreate and analyze, and the engine's nearest thing is a database-wide `VACUUM` — far more than one table was asked for |
| `CHECK TABLE` with a list, a qualified name, or the `QUICK` / `FOR UPGRADE` / `EXTENDED` options | refused; one unqualified table at a time is taken |
| `ANALYZE TABLE` over several tables, or with `NO_WRITE_TO_BINLOG`, `LOCAL` or a histogram clause | refused; one unqualified table at a time is taken |
| `CREATE USER`, `GRANT`, `REVOKE` | not started |
| Stored procedures, functions, events | refused, and out of scope — see what this frontend is for |
| `information_schema` beyond `TABLES`, `COLUMNS`, `SCHEMATA`, `STATISTICS`, `KEY_COLUMN_USAGE`, `TABLE_CONSTRAINTS`, `REFERENTIAL_CONSTRAINTS` | not started; what is left is `VIEWS`, `ROUTINES` and the server-status tables, none of which a test suite reads to find out what the schema is |
| A `CHECK` constraint in `information_schema.TABLE_CONSTRAINTS` | no row; the engine keeps a `CHECK` in the stored DDL rather than in the schema these tables read, and reading it would mean decoding the schema envelope inside a scan |
| `information_schema.STATISTICS.CARDINALITY` | refused; it is an estimate of distinct values and the engine keeps no equivalent, so a made-up number would be worse than none |
| `SELECT *` over an `information_schema` table | refused; it asks for MySQL's columns and this answers a few of them. `KEY_COLUMN_USAGE` answers all of them and is refused anyway, because one rule for all of these tables is worth more than the wildcard |
| A call over an `information_schema` column | refused; its shape has not been measured. A count is taken, since a count does not depend on what the column holds |
| `WHERE TABLE_SCHEMA = DATABASE()` outside the one recognized shape | refused; the checked `SELECT` surface does not read `DATABASE()` in a `WHERE` yet, so that predicate is carried only by the shape this recognized before |
| `information_schema` columns beyond the three of `TABLES`, seven of `COLUMNS`, seventeen of `STATISTICS`, and all of `KEY_COLUMN_USAGE`, `TABLE_CONSTRAINTS` and `REFERENTIAL_CONSTRAINTS` this answers | refused; the rest are statistics and timestamps this server does not keep, and answering NULL would be a claim of its own |
| An `information_schema.COLUMNS` or `SCHEMATA` `WHERE` beyond the one shape each takes | refused; those two are the last recognized by written shape, and moving them to a table the engine scans is the work that retires the recognizer |
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
| `BIGINT UNSIGNED` above `i64::MAX` | refused with 1264; MySQL stores up to 18446744073709551615 and the engine holds an integer as an `i64`. 0 to `i64::MAX` is taken |
| `BIGINT UNSIGNED AUTO_INCREMENT` | refused; the allocator takes the `INT` spellings |
| `UNSIGNED` on `DECIMAL`, `DOUBLE`, `FLOAT` | works |
| Arithmetic and aggregates over an unsigned column | not measured; the result's own type and width have not been recorded |
| `DATE` | works |
| `TIME` | works |
| `YEAR` | works |
| A `WHERE` comparison against a temporal value written any way but the one the column holds — `d = '2024-1-1'`, `dt = '2024-01-01'`, `y = 24` | refused; MySQL reads each of those as a value the stored form would not meet, and rewriting the literal into that form is a second rendering pass this does not make |
| An ordering comparison against a `TIME` column — `t > '02:00:00'` | refused; a span runs past a day so its hours outgrow two digits, and it carries a sign, so reading two of them in order is not reading them in time order. `=`, `!=`, `<=>` and `IN` are answered |
| A `?` compared against a `DATE`, `TIME`, `DATETIME`, `TIMESTAMP` or `YEAR` column | refused; a bound value is not put into the form the column holds |
| A call other than `CURDATE()`, `NOW()` or `CURTIME()` on the right of a comparison — `d > DATE_SUB(NOW(), INTERVAL 1 DAY)`, `n = ABS(-1)` | refused; the three that are read answer a value in the form a column holds and take no argument, and rendering a call with arguments there is the projection renderer's work rather than the comparison reader's |
| A call on the left of a comparison — `LOWER(name) = 'a'` | refused; the checked comparison surface names a column on the left |
| A call other than `CURDATE()`, `NOW()` or `CURTIME()` as a value to insert or assign | refused; the three that are read answer a value in the form a column holds and take no argument |
| A value naming a column the same `SET` has already assigned — `SET a = 100, b = a` | refused; MySQL reads the assigned value there and the engine reads the row as it was. The other order is answered |
| Division in an assignment — `SET b = b / 2` | refused; measured, `101 / 2` answers 50.5 in MySQL and 50 in the engine |
| Arithmetic as a value to insert — `INSERT ... VALUES (n + 1)` | refused; a row being written has no row to read a column out of |
| `NOW()` written into a number column | refused; MySQL runs the moment together into 20260908170430 and reading a moment as a number is a rule of its own |
| A `LIKE` against a temporal column — `d LIKE '2024-%'` | refused; the pattern is not a value of the column's type, which is what the canonical-form check reads |
| A `WHERE` comparison against a `JSON` or `BLOB` column | refused; each compares by a rule of its own that has not been measured |
| An `ENUM` or `SET` member spelled any way but the way it was declared — `state = 'ACTIVE'` | refused; MySQL's collation ignores case and finds the row, and comparing the stored spelling against that text would find nothing. Asking the engine for the collation here needs the second rendering pass an `ORDER BY` over one already takes |
| An ordering comparison against an `ENUM` or `SET` column — `state > 'active'` | refused; MySQL reads an `ENUM` by the position its members were declared in, which is not the order their words read in |
| A number compared against an `ENUM` or `SET` column — `state = 2` | refused; MySQL reads it as a member's position and the stored value is the word |
| A `WHERE` comparison against a number written with a fraction and a text column — `label > 1.5` | refused; MySQL reads the text as a number, which is the coercion a string against an integer column is refused for |
| A `HAVING` counted against a number written with a fraction — `HAVING COUNT(*) > 1.5` | refused; a count is a whole number |
| A run of digits too long for an `i64` in a comparison — `n > 9223372036854775808` | refused; it was written as a whole number and reading it as the nearest number a binary64 names would answer a different one |
| `ENUM` | works |
| `ORDER BY` on a `SET` | orders by the member text, where MySQL orders by the numeric value, one bit for each member — measured, `read, write, exec` come back in that order there and alphabetically here |
| A `DEFAULT` on an `ENUM`, or one as a key | refused; the column takes its nullability and nothing else yet |
| An `ENUM` member matched by case, trailing space, position or bit | works; the value is rewritten into the members' declared spelling and order the way MySQL rewrites it |
| An `ENUM` member holding a quote or a backslash | refused; the members ride inside a quoted declared type and are themselves quoted, so either would have to survive two escapings |
| `SET` | works |
| `JSON` | works |
| A `JSON` number MySQL reads imprecisely | stored more accurately than MySQL stores it: measured on 8.4.11, MySQL answers `1000000000000000.1` with `1e15` and `1e-30` with `9.999999999999999e-31`, both rapidjson's fast path landing on the double next to the right one |
| A `DEFAULT` on a `JSON` column, or one as a key | taken, where MySQL refuses both — measured, a default answers 1101 and a key answers 3152 |
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
- `NOW()` written into a `DATE` keeps the day quietly, where MySQL raises 1292 for the
  time it drops
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
