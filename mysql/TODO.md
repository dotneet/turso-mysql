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

`CASE`, `IF`, `IFNULL` and `COALESCE` work over written words and whole
numbers and over columns of one kind, and `COUNT`, `SUM`, `AVG`, `MIN` and
`MAX` over a `CASE`. What is left:

| Form | State |
|---|---|
| `CASE col WHEN v THEN ...` over values of two kinds, or an operand that is not a column | refused; MySQL compares by one rule chosen over every value together |
| A word beside a number, or an unsigned column beside anything | refused; measured, each answers a type of its own (`COALESCE(name, age)` a `VAR_STRING`, `utiny`/`tiny` a `SHORT` of 4) |
| A `TEXT`, `FLOAT` or temporal column in a branch | refused; unmeasured |
| A written number with a point in a branch | refused; `THEN 1.5 ELSE 0` answers a `NEWDECIMAL` by a rule of its own |
| `MIN`/`MAX` over a `CASE` answering a `DECIMAL` or words, ordering by `AVG` over a `CASE` | refused; the engine would compare the written values as words |
| A `CASE` over columns in a join | refused; its column types are read off one table |
| Grouped `SUM`/`COUNT`/`MIN`/`MAX` over a `CASE` | taken, but reports the binary flag MySQL drops under a `GROUP BY` — the same as over a plain column |
| A `CASE` whose every branch is `NULL` | refused; there is no width left |

### Waiting on a column type

| Function | Blocked by |
|---|---|
| A binary date or datetime parameter naming the zero date | refused; the sql_mode this server runs in refuses it. Date, datetime and time parameters with valid microseconds are read |
| A number bound against a column holding a day or a moment | refused; MySQL reads a bound number as a moment — 20260101000000 names the first of January — and what this reads is a word |
| MySQL's warning 1292 for a bound word that reads as no moment | not raised; the row it finds is the row MySQL finds, none, and the warning beside it is not |
| A written day compared against a column holding a moment in an `UPDATE` or a `DELETE` | refused; neither has a second rendering pass to learn that the column holds a moment, which is what says to read the day as its midnight |
| A `WHERE` testing a column of words on its own — `WHERE name` | refused; MySQL reads a word as the number it begins with, where the engine compares a word against a number by their kinds |
| A `WHERE` testing a column on its own in an `UPDATE` or a `DELETE` | refused; neither has a second rendering pass to learn whether the column is one of words |
| `IFNULL` / `COALESCE` falling a written word back onto a `TEXT` column | refused; measured, MySQL reports four times the column's own width there, a rule of its own |
| `NULLIF` over two columns of words, or a number against a word | refused; MySQL reads a word as a number and compares two words without regard to case, where the engine compares them by their kinds |
| `GREATEST`, `LEAST`, `NULLIF` and `MD5` over written values alone, naming no column | refused; each answers a shape read off the column it names, and there is none |
| `CAST(col AS CHAR)` over a `DECIMAL`, a `FLOAT` or a `DOUBLE` | refused; conversion through the engine's generic text cast has not been verified against MySQL's numeric formatting |
| `CAST(col AS CHAR)`, `SUBSTRING`, `SUBSTRING_INDEX`, `REPEAT` and `HEX` over a `TEXT` | refused; MySQL answers a `MEDIUM_BLOB` for each, and only `CONCAT`, `CONCAT_WS`, `LOWER`, `UPPER`, `REVERSE`, `REPLACE` and `TRIM` have had its width measured |
| `CHARSET(col)` and `COLLATION(col)` | refused; each answers the column's character set or collation name, which the rendered statement does not carry |
| A text call over a `MEDIUMTEXT` or a `LONGTEXT` | refused; measured, MySQL answers a `LONG_BLOB` — `LOWER(mt)` reports 268435440 and `CONCAT(mt, 'a')` 67108864, `max_allowed_packet` — by a rule not worked out |
| `HOUR()` / `MINUTE()` / `SECOND()` over a `TIME` | refused; a `TIME` holds a span running to 838 hours, which MySQL reads out whole and the engine has no reader for |
| `YEAR()` / `MONTH()` / `DAY()` over anything but a plain date column | refused; measured, `YEAR` over a `TIME` answers the current year, which is a coercion rather than a reading |
| `GROUP_CONCAT` with an `ORDER BY`, or with `DISTINCT` beside a `SEPARATOR` | refused; MySQL orders the parts it joins and the engine's planner refuses an `ORDER BY` inside an aggregate (`core/translate/planner.rs`), and the engine takes `DISTINCT` only over a single argument, which the separator occupies |
| `JSON_ARRAYAGG` over a `DOUBLE`, a `DECIMAL`, a moment or a JSON column, or with `DISTINCT` | refused; MySQL writes each of those into the array by a rule of its own |
| `JSON_OBJECTAGG` | refused; measured, MySQL answers 3158 for a NULL key, and the engine's `json_group_object` has not been held to that or to anything else about keys |
| `BIT_OR`, `BIT_AND`, `BIT_XOR` | refused; the engine has no bit aggregate. Measured: each answers an unsigned `LONGLONG` of 21, NOT NULL, reading a negative as its sixty-four bits — `BIT_OR` over 3 and -4 is 18446744073709551615 — and over no rows `BIT_AND` answers 18446744073709551615 and the others 0 |
| `STDDEV`, `STD`, `STDDEV_POP`, `VAR_POP`, `VAR_SAMP`, `VARIANCE` | refused; the engine has one deviation aggregate and it is the sample form, so `STDDEV_SAMP` is taken and the population ones would answer a different number — measured over 2,4,4,4,5,5,7,9 the sample form is 2.138089935299395 and the population one is 2. A variance is the square of a deviation, and squaring a rounded square root would answer different last digits than MySQL's own |
| `DATE_ADD` / `DATE_SUB` counting a number worked out from a row — `INTERVAL n DAY` | refused; a week and a quarter are counted in the unit each is made of, which only a written number can be multiplied for |
| `DATE_ADD` / `DATE_SUB` over a moment written any way but `YYYY-MM-DD hh:mm:ss` — `'20260101' + INTERVAL 1 DAY` | refused; MySQL reads each of those, and which it takes for a day alone, and so answers as one, has not been measured |
| `DATE_ADD` / `DATE_SUB` counting a bound number — `NOW() - INTERVAL ? DAY` | refused; MySQL reads the bound value by the type the client sent it as, which has not been measured |
| Two shifts in a row — `created_at + INTERVAL 1 DAY + INTERVAL 1 HOUR` | refused; the second shifts a call rather than a column or a clock reading |
| A shift landing before the year zero | answers NULL, where MySQL answers `0000-00-00` — a value `NO_ZERO_DATE` would not store |

### Not looked at

Every window function MySQL has is taken, with a `ROWS` or `RANGE` frame and a
`WINDOW` clause. What is refused around them: a `GROUPS` frame, which MySQL
answers 1235 for, a frame bound that is not a plain non-negative number, a
window term that is not a plain column, a named window standing for another
name or built on one, a windowed aggregate over anything but one plain column,
and a `LAG` or `LEAD` whose offset is not written or whose default is of
another kind than its column — a number over a word, a word over a day, a
number with a point.
Numbers: a seeded `RAND(n)` is refused: the engine has no seeded random, so
answering one would answer a different sequence.
Written values: a number with a point or an exponent, a hexadecimal or bit
literal, and a cast of a written value are taken only in the statement's own
result, not in a subquery, a derived table or a `UNION` branch. A number with
redundant leading zeroes (`007.5`), a word or a `DOUBLE` cast to `DECIMAL`, and
a cast a warning comes with are refused.
Temporal: `FROM_UNIXTIME` over a fraction or a word, which MySQL carries into
the moment or reads as the number it begins with.
JSON: `JSON_SEARCH` with a path to search inside. `JSON_SET`,
`JSON_INSERT`, `JSON_REPLACE` and `JSON_REMOVE` take a path naming one member
of the top-level object and refuse a wider one, which is the range the engine
and MySQL agree on. `JSON_ARRAY` and `JSON_OBJECT` refuse a nested call and a
boolean literal, and they and `JSON_SET` refuse a `DECIMAL` with places, a
number written with a trailing zero past its first place (`10.00`), a `TIME`, a `TIMESTAMP`, a `FLOAT`, a `SET`, a
`BIT` and binary strings, each written into a document by a rule not followed
or not measured. `JSON_ARRAYAGG` over a built document takes `JSON_OBJECT` and
`JSON_ARRAY` and no other call.

---

## SQL syntax

### `SELECT`

| Form | State |
|---|---|
| Scalar subquery in a projection answering a column rather than an aggregate — `SELECT (SELECT n FROM t)` | refused; measured, MySQL answers 1242 for a subquery returning more than one row where the engine answers the first row it finds. Taking it needs the inner statement to prove it answers at most one row — an `ORDER BY ... LIMIT 1`, which the subquery reader refuses today |
| Subquery anywhere but a `WHERE`, apart from `EXISTS` and an aggregate's scalar subquery in a projection | not started |
| `ORDER BY` an ordinal over a mixed wildcard projection — `SELECT t.*, id FROM t ORDER BY 2` | refused; requires expanding each wildcard to count through |
| A comparison as a result column over anything but a column against a written number or word, or a `COUNT` against a written whole number — `SELECT a > b`, `SELECT n > ?`, `SELECT SUM(n) > 0` | refused; a column against a column and a bound value raise the coercion question a `WHERE` raises, and an aggregate other than a count may be NULL by rules not measured here |
| `NOT`, `IS TRUE`, `IS FALSE`, `IS NOT TRUE` and `IS NOT FALSE` over a word, a `DECIMAL`, or anything but a column in a projection | refused; measured, MySQL reads a word as the number it begins with — `NOT 'apple'` is 1 — and the engine does not. In a `WHERE` they take whatever the `WHERE` reader takes |
| `WITH ROLLUP` | refused |
| `BINARY col = ?`, `x = BINARY col`, `BINARY col LIKE ...` | refused; only `BINARY` before a column compared with a written word is taken |
| A hexadecimal literal compared with a column — `email = X'616E6E'` | refused; measured, MySQL compares it as a word under a text column's collation and as a number against a number column |
| A word with the `_utf8mb4` introducer in an `INSERT`, an `UPDATE` or a `DELETE` | refused; only a `SELECT` reads it without the introducer |
| Renaming what the offered row carries — `VALUES (...) AS offered (a, b)` | refused; the plain row alias is taken and the column list has not been measured |
| A `CASE` or `IF` branch written as a number with a point — `THEN 1.5 ELSE 0` | refused; MySQL answers a NEWDECIMAL there by a rule of its own. A `DECIMAL` column branch is taken |
| A `CASE` or `IF` mixing a word branch and a number branch | refused; that is a coercion, and what MySQL answers for it has not been measured |
| A `CASE` or `IF` branch holding an aggregate or arithmetic | refused; only a written number and a column have been measured |
| `SET n = DEFAULT` on an `UPDATE` | refused; MySQL writes the column's own default and this cannot work out what that is from the statement alone |
| `DEFAULT` in one row of an `INSERT` and a value in another | supported for the `AUTO_INCREMENT` column in a checked `VALUES` insert; other columns remain refused because leaving the column out would take the default for every row |
| `DEFAULT` beside `ON DUPLICATE KEY UPDATE` | refused; what the offered row carries for a column left out has not been measured |
| `DEFAULT(col)` naming some other column | refused; that writes another column's default, which leaving the column out cannot say |
| Several rows of defaults on an `AUTO_INCREMENT` table — `VALUES (DEFAULT), (DEFAULT)` | refused; which of the numbers the statement reports depends on the rows, and one row leaves no question |
| An `UPDATE ... SET` dividing by a number read from the row, or by a written zero | refused; dividing by zero answers NULL in the engine where MySQL raises 1365 for a write, and only a written divisor says which of the two a statement would get |
| An `UPDATE ... SET` writing a fraction into a whole-number column — `SET n = n / 3` | refused; MySQL rounds the fraction into the column and the engine will not store it. One that divides evenly writes the number MySQL writes |
| An `UPDATE ... SET` value reading a column the same `SET` has written | refused; MySQL takes the assignments left to right and the engine reads the row as it stood |
| An `UPDATE ... SET` value taken from `(SELECT COUNT(*) FROM ...)` | refused; a count says nothing about the kind of the column written, and the pair is held to one kind |
| An `UPDATE ... SET` value whose subquery reads a column its own table does not hold | refused; that is a correlated read of the row being changed, which has not been measured |
| A collation over a `LIKE` or a membership test — `name LIKE 'a' COLLATE utf8mb4_bin`, `name IN ('a' COLLATE utf8mb4_bin)` | refused; the checked LIKE matcher uses the column's default UCA9 rule, while explicit byte collation and membership-test overrides need separate checked paths |
| `%`, `DIV` or a negation over a `DOUBLE` or a `DECIMAL`, and `MOD` over a `DOUBLE` | refused; measured, MySQL answers a `DOUBLE` of 23 or the column's own `DECIMAL`, and the engine's `%` reads a real number as a whole one |
| `%` or `DIV` by zero, `DIV -1`, and a negated `BIGINT` | refused; MySQL answers NULL with warning 1365 for the first, and 1690 for the smallest `BIGINT` in the others, where the engine answers a real number |
| `%`, `DIV` or a negation in a `WHERE` | refused; the comparison reader takes a call there, and the column's kind is checked only for a projection |
| `ROUND` of a `DECIMAL` or a `BIGINT` left of the point, and `FLOOR` or `CEIL` of a `DECIMAL` | refused; measured, `ROUND` of a `BIGINT` past the largest one is 1690, and the others answer shapes of their own |
| `ROUND` naming its places with anything but a written whole number | refused; the answer's width is worked out from them |
| `SIN`, `COS`, `TAN`, `ASIN`, `ACOS`, `ATAN`, `EXP`, `LN`, `LOG`, `LOG2`, `LOG10` | refused; measured, `ATAN(10)` and `TAN(10)` differ from MySQL in the last place, and the rest come from the same maths library, so agreeing at the points tried is not a promise |
| `BIN` or `OCT` over a word — `BIN(name)` | refused; MySQL reads the word as the number it names, which is 0 for a word that names none |
| `CONCAT` over a `DECIMAL`, a `FLOAT` or a `DOUBLE` | refused; the generic text conversion of exact decimal blobs and floating values has not been verified against MySQL's numeric formatting |
| `SUBSTRING`, `SUBSTRING_INDEX` and `CONCAT_WS` over a `TEXT` | refused; measured, MySQL answers a `MEDIUM_BLOB` of 1048560 for each, a shape this does not write |
| `SUBSTRING_INDEX`, `SUBSTRING`, `MD5`, `SHA1` and `SHA2` over a number | refused; MySQL writes the number out first, and the engine would answer NULL or cut something else |
| `CONCAT_WS` with a `NULL` or bound separator, or a `NULL` or bound part | refused; the width of each has not been measured |
| `SHA2` naming a size it does not have — `SHA2(col, 1)` | refused; measured, MySQL answers NULL with a warning this does not raise |
| `MID` | refused; not measured, though it is MySQL's other spelling of `SUBSTRING` |
| A text-answering call compared against a `?` — `SUBSTRING_INDEX(email, '@', -1) = ?`, `LOWER(name) = ?` | refused; a parameter carries no kind until it binds, and MySQL would read a bound number against a word by a rule of its own |
| Pinning a `DOUBLE` result to a golden | the conformance harness reads one back a bit narrower on verify than on record — `RADIANS(7)` records as ...309 and verifies as ...307 — so a double's value is held in a Rust test instead |
| A `REGEXP` pattern looking ahead or naming a group again — `a(?=b)`, `(a)\\1` | refused; MySQL reads those through ICU and the matching here does not |
| A `REGEXP` whose subject or pattern contains non-ASCII text | refused; MySQL ICU expands some characters during case folding, such as `ß` to `ss`, which Rust regex does not |
| A bound `REGEXP` pattern — `name REGEXP ?` | refused; what a written pattern is checked for has no equivalent at bind time |
| `DATE_FORMAT` over a call that is not a clock reading — `DATE_FORMAT(DATE(m), ...)`, `DATE_FORMAT(CURTIME(), ...)` | refused; the three shapes taken are a column, a clock reading and a word, and what MySQL writes for the rest has not been measured |
| A `?` or a word naming no moment in `TIMESTAMPDIFF` or `DATEDIFF` — `DATEDIFF(created_at, ?)`, `DATEDIFF('2024-02-30', d)` | refused; MySQL reads a bound value by the type the client sent, and answers NULL for a word naming no day |
| An index hint on an `UPDATE` or `DELETE` target | refused; the hint is dropped for a `SELECT` but that shape has not been measured |
| `EXTRACT(WEEK FROM ...)` and `EXTRACT(QUARTER FROM ...)` | refused; MySQL counts a week by rules of its own and the engine has no quarter, and neither has been measured |
| A calendar reading over something that is not a column — `QUARTER(NOW())` | refused; every reading here but `WEEK`, `DAYNAME` and `MONTHNAME` names a column, which is what its reported shape is worked out from |
| `WEEK` with a mode that is not a written number from 0 through 7 — `WEEK(d, 8)`, `WEEK(d, ?)` | refused; MySQL counts by the mode's last three bits and reads a word or a bound value by rules of its own |
| `YEARWEEK`, `TIME_FORMAT`, `CONVERT_TZ` in a projection | refused; not measured beyond `YEARWEEK`'s shape, a LONGLONG of 7 |
| The bare `UTC_TIMESTAMP`, `UTC_DATE` and `UTC_TIME`, with no parentheses | read as a column name, where MySQL reads the clock |
| A `LIKE` `ESCAPE` naming more than one character — `ESCAPE '!!'` | refused; MySQL takes one character there |
| A `LIKE` pattern with a piece holding nothing — `LIKE CONCAT('%', NULL, '%')` | refused; MySQL answers no rows for it, but written this way it is not a pattern at all |
| A `LIKE` pattern binding more than one piece — `LIKE CONCAT(?, ?)` | refused; a checked comparison records one bound value |
| A comparison against a `SUM` or `AVG` subquery — `WHERE n > (SELECT AVG(n) FROM t)` | refused; MySQL rounds `AVG` to four decimal places and the engine keeps the whole fraction, so the two can land on either side of a row |
| A comparison against a subquery projecting a plain column — `WHERE id = (SELECT c FROM t)` | refused; MySQL answers 1242 once it finds more than one row and the engine takes the first |
| A `HAVING` over an aggregate with a fallback — `HAVING IFNULL(SUM(n), 0) > 1` | refused; the HAVING renderer records an aggregate's own argument column to check a literal against, and has no rule for one inside a call |
| A ranking over a window naming neither a partition nor an order — `ROW_NUMBER() OVER ()` | refused; the rows are numbered in whatever order they were read and the two need not read them alike |
| A wildcard qualified by a schema — `SELECT db.t.*` | refused; the plain `t.*` form is taken, this one names a source across databases |
| `ORDER BY` over an expression this does not know the shape of, or over a random number — `ORDER BY RAND()` | refused; a random number orders the rows by nothing a client can hold this to, and whether each engine reads it once or once a row is a rule of its own |
| `HAVING` naming an alias for something other than an aggregate or a column — `SELECT LOWER(name) AS l FROM t GROUP BY name HAVING l > 'a'` | refused; the name resolves, but what it stands for is a shape the `HAVING` renderer does not take |
| `EXCEPT ALL`, `INTERSECT ALL` | refused; they keep duplicates the plain forms collapse, and the engine has no spelling for them |
| A `UNION` branch with its own `ORDER BY` or `LIMIT` | refused |
| A `UNION` of three or more branches | refused; the reader takes two |
| A `UNION` column pairing a number with a word, a whole number with a `DOUBLE` or `DECIMAL`, a `DATE` with a `DATETIME`, or a column with a written value | refused; measured, MySQL converts each pair by a rule of its own — `INT` with `VARCHAR(10)` a `VAR_STRING` of 44, `INT` with `DECIMAL(10,2)` a `NEWDECIMAL` of 14, `INT` with `DOUBLE` a `DOUBLE`, `INT` with a written 1 a `LONGLONG` of 11 — and the engine keeps two kinds apart |
| A `UNION` over `FLOAT`, `MEDIUMINT`, unsigned, `TIME`, `TIMESTAMP`, `YEAR`, `ENUM`, `SET`, `JSON`, `BLOB`, a fractional `DATETIME` or two different `DECIMAL`s | refused; their pairs have not been measured |
| A `UNION`, `EXCEPT` or `INTERSECT` dropping repeated rows over words compared without regard to case | refused; measured, MySQL keeps the first of `'aa'` and `'AA'` and the engine the last. `UNION ALL` and `utf8mb4_bin` columns are taken |
| A `UNION` naming a `DECIMAL` column, or reaching one through a wildcard or an expression | refused; only the first branch's table is read for column types |
| A `UNION` branch projecting a wildcard or an expression | taken with the first branch's shape, which is what the engine reports; MySQL's shape for the mixture has not been measured |
| `WITH RECURSIVE` | refused |
| A wildcard projection in a CTE body, or in a derived table's | refused; no name to resolve an ordinal through |
| A derived table whose body projects an expression or an aggregate — `(SELECT SUM(n) AS total FROM t) x` | refused; the same, there is no table column for the ordinal to land on |
| A derived table whose body reads more than one table, or that is written `LATERAL` or names its own columns | refused |
| A derived table in an `UPDATE` or a `DELETE` | refused; each reads its own table |
| `DISTINCT ON` | refused, and no part of MySQL |

### DDL

| Form | State |
|---|---|
| `ALTER TABLE` beyond `ADD COLUMN` / `DROP COLUMN` / `RENAME` / `MODIFY COLUMN` / `CHANGE COLUMN` / `ALTER COLUMN ... SET DEFAULT` / `DROP DEFAULT` / `COMMENT` / the index operations, `RENAME INDEX` among them | refused |
| `RENAME INDEX` or a table `COMMENT` beside any other operation in one `ALTER TABLE` | refused; `sqlparser` reads neither, so each is read by its own words and only on its own. Measured, MySQL takes both beside anything else |
| `ALTER TABLE t COMMENT = '...'` while the database holds a view or a trigger | refused; the comment is written by an engine `ALTER TABLE`, which is refused then as every other `ALTER TABLE` is |
| `ALTER COLUMN ... SET DEFAULT` / `DROP DEFAULT` beside any other operation | refused; each is written as a `MODIFY COLUMN` of the column it names, and only a statement made of them alone is |
| `ALTER COLUMN c DROP DEFAULT` on a column that may hold NULL | refused; measured, MySQL takes it and then prints the column with no `DEFAULT` at all — `` `a` int, `` — where a nullable column with no default of its own prints `DEFAULT NULL` here |
| A `MODIFY` or `CHANGE` giving a `DECIMAL` column another size or sign, or turning a column into or out of a `DECIMAL` | refused; measured, MySQL writes every stored value again in the new form — a 1.5 in a `DECIMAL(8,2)` reads `1.500` after `MODIFY d DECIMAL(12,3)` — and the engine keeps a `DECIMAL` as the text it was written as. The same `DECIMAL` restated with another default or nullability is taken |
| `RENAME INDEX` of the key a column declares for itself — `email VARCHAR(255) UNIQUE` | refused; the engine keeps that one with no statement of its own to write again under the new name |
| `ALTER COLUMN ... SET DEFAULT` on a column with a `COMMENT` or `ON UPDATE CURRENT_TIMESTAMP`, or on the primary-key column | refused, for the reasons a `MODIFY COLUMN` of one is |
| Moving the column a table counts on, or the one its key is over | refused; the counted column stands for the engine's rowid and the key is what the rows are found by, and neither survives being written somewhere else |
| `ALTER TABLE ... ADD COLUMN ... FIRST`/`AFTER` on a table carrying a trigger | refused; the table is written again and a trigger is not the table's own row, where MySQL leaves one where it stood |
| `ALTER TABLE ... MODIFY/CHANGE COLUMN` on the primary-key column | refused; MySQL keeps the key through one and replacing the column would drop it |
| `ALTER TABLE` taking an `AUTO_INCREMENT` table's counted column away | refused; `DROP COLUMN`, `RENAME COLUMN` and `MODIFY COLUMN` of that column would write back a table counting on a column that is not there, where MySQL drops it and leaves an ordinary table |
| A `CHARSET` or `COLLATE` naming anything but `utf8mb4` and `utf8mb4_0900_ai_ci`, or an `ENGINE` that is not InnoDB | refused; measured, MySQL prints each back, and this prints one trailer whatever a table holds |
| `CREATE DATABASE` naming a character set, collation or encryption but `utf8mb4`, `utf8mb4_0900_ai_ci` and `'N'` — Prisma's and Laravel's `utf8mb4_unicode_ci` among them | refused; MySQL gives every table made in such a database that collation, and a database here keeps none of its own to give |
| `ALTER TABLE v DISABLE KEYS` / `ENABLE KEYS` over a view | refused, where MySQL answers 1347 |
| A view over more than one table, or with a `WHERE` — what a dump of a view over a join writes | refused; a view here reads one table's columns as they stand |
| A trigger whose values are anything but a `NEW` column or a literal — `CONCAT('post ', NEW.title)` | refused |
| A view or trigger in a dump naming a `DEFINER` other than the account restoring it — `root`@`localhost` | refused, as MySQL refuses it to an account without `SET_ANY_DEFINER`; accounts here are all `'name'@'%'` |
| `ROW_FORMAT` and every other table option but the engine, character set, collation, `AUTO_INCREMENT` and `COMMENT` | refused; measured, MySQL prints `ROW_FORMAT` back, and none of the rest has been measured |
| A table `COMMENT` longer than 2048 characters, or written in double quotes | refused; MySQL answers 1628 for the first and takes the second |
| `AUTO_INCREMENT=<n>` naming a start past what the column holds | refused; measured, MySQL creates the table and answers 1467 for the first row, so this refuses the statement instead of storing a mark no row could take |
| `AUTO_INCREMENT=<n>` naming anything but a plain whole number | refused; measured, `-5` and `'7'` are each 1064 and `1.5` is rounded down, a rule this does not repeat |
| `ALTER TABLE ... AUTO_INCREMENT=<n>` | refused; measured, MySQL raises the counter to it and ignores one below the mark it already has, which is what the allocator's own advance does — the `ALTER TABLE` path has not been given it |
| Uniqueness over a word — a `PRIMARY KEY` or `UNIQUE` key over `VARCHAR`/`CHAR` | works with fixed Unicode 9 weights, folding accents and case in both comparisons and uniqueness; measured, `'ALPHA'` after `'alpha'` is 1062 both here and there |
| A `PRIMARY KEY` over a type that is neither a number nor a sized word — `TEXT`, `BLOB`, `DATE` | refused; measured, MySQL answers 1170 for a `TEXT` key for want of a length, and the rest have not been measured |
| `PRIMARY KEY (a, b)` naming a column without a `NULL`/`NOT NULL` clause | accepted; each key column is stored and reported as `NOT NULL`, as MySQL does. An explicit `NULL` or `DEFAULT NULL` on a key column remains refused |
| `PRIMARY KEY (a, b)` with an `AUTO_INCREMENT` column inside it | refused; the counted column stands for one rowid, which has no way to spread over a pair |
| An `AUTO_INCREMENT` column that is not the table's key, or one with no key at all | refused where MySQL answers 1075 |
| An `AUTO_INCREMENT` column not written `NOT NULL` | accepted when its table-level primary key names that column; the stored definition adds `NOT NULL`, as MySQL does. Other forms are refused |
| `PRIMARY KEY` carrying `USING BTREE` or an index name, or a column written `DESC` | refused; measured, MySQL prints all three back, so dropping them would print a different table |
| `PRIMARY KEY (missing)`, or a table writing two keys | refused where MySQL answers 1072 and 1068 |
| `ALTER TABLE` mixing supported index and column operations | accepted as one transaction; a later failure rolls back every earlier operation, and foreign-key child-index coverage is checked after the full statement so an index can be replaced atomically |
| `ALTER TABLE ... ADD/DROP INDEX \`PRIMARY\`` | refused; adding or dropping a primary key is a different operation |
| `DROP INDEX name` with no table after it | refused; MySQL requires the table, and the engine's own spelling names none |
| `RENAME TABLE` naming a database — `RENAME TABLE db.a TO db.b` — or a view, or any `RENAME TABLE` while the database holds a view | refused; MySQL takes all three. The last is the rule every `ALTER TABLE ... RENAME TO` already keeps |
| `ALTER TABLE a RENAME TO b` onto a name already taken, or of a table that is not there | refused, where MySQL answers 1050 and 1146; the `RENAME TABLE` spelling answers both |
| `CREATE TABLE ... AS SELECT` over a division, an aggregate, or an unaliased expression | refused; integer `+`, `-` and `*` work. A division makes a `decimal(14,4)` on a rule of its own, and an unaliased expression column takes its name from the expression's own text — measured, `SELECT a + 1` makes a column called `a + 1` |
| `CREATE TABLE ... AS SELECT` over a column with a string `DEFAULT` | refused; the escaping is undecided, the same reason `SHOW CREATE TABLE` refuses to print one |
| `CREATE TABLE ... (columns) AS SELECT`, and `IF NOT EXISTS` or `TEMPORARY` beside an `AS SELECT` | refused |
| `CREATE TEMPORARY TABLE` with `AUTO_INCREMENT` | refused; the allocator is keyed on a durable table |
| `FOREIGN KEY` | works, and enforced |
| The index MySQL creates beside a `FOREIGN KEY` | created when no existing key has the child columns as a left prefix; it appears in `SHOW CREATE TABLE` |
| `ALTER TABLE ... ADD FOREIGN KEY` without a `CONSTRAINT` name | works; measured, MySQL names it `t_ibfk_N` counting the keys the table already carries, and so does this — two unnamed keys added one after the other read back as `t_ibfk_1` and `t_ibfk_2` |
| The index MySQL creates beside a `FOREIGN KEY` an `ALTER TABLE` adds | created or reused by the same rule as `CREATE TABLE` |
| Column-position changes on a child table carrying a foreign key | accepted with its foreign key and supporting indexes retained through the rewrite and reopen; position changes on a referenced parent table remain refused |
| `FLOAT(M,D)` and `DOUBLE(M,D)` | refused; MySQL keeps the size and rounds a stored value to it — measured, 1.239 into a `double(10,2)` reads back 1.24 — which is a rounding rule this does not have |
| `FLOAT(p)` naming a precision | refused; MySQL reads `p` up to 24 as a `float` and above it as a `double`, which has not been measured |
| Warning 1681 for an integer display width or a floating-point size | not raised; MySQL raises one per column and this raises none, so a client counting warnings after a `CREATE TABLE` sees zero |
| `ON UPDATE CURRENT_TIMESTAMP` on a table with no key of its own, or in any `ALTER TABLE` | refused; the words live in the stored MySQL DDL, which only the keyed `CREATE TABLE` paths render from the statement as written — every other path rebuilds it from the engine's own definition, where the words are not |
| A joined `UPDATE` on a table carrying an `ON UPDATE` column | refused; a joined one names the table it changes through the columns its `SET` names, so which table's columns are its own is unanswered |
| A `DEFAULT` naming anything but a literal or `CURRENT_TIMESTAMP` | refused; no other expression default has been measured |
| A number written with an exponent as the default of an integer column — `INT DEFAULT 2.5e0` | refused; measured, MySQL reads it as a binary64 and rounds half to even, printing `'2'`, where the word `'2.5e0'` prints `'3'` and is taken |
| A word ending in a bare `e`, or with a tab around it, as the default of an integer column — `INT DEFAULT '1e'`, `INT DEFAULT '\t7'` | refused; measured, MySQL takes both, reading `'1e'` as `1` and skipping a tab where it refuses a newline |
| A default on a `DOUBLE` or `FLOAT` that MySQL prints as some other number — `DOUBLE DEFAULT 1.50`, `'1.50'`, `' 1'`, `'1e2'` | refused; measured, MySQL prints the number the column holds (`'1.5'`, `'1'`, `'100'`) and the engine keeps what was written. A number or word MySQL prints back unchanged, `0`, `'0'` or `'1.5'`, is taken |
| Column `COMMENT` on a table with no key of its own, or in any `ALTER TABLE` | refused; the words live in the stored MySQL DDL, which only the keyed `CREATE TABLE` paths render from the statement as written |
| Column `CHARACTER SET` other than `utf8mb4`, or `COLLATE` other than `utf8mb4_0900_ai_ci` / `utf8mb4_bin` / `utf8mb4_unicode_ci` | refused; other collations have different comparison rules |
| `FIELD`, `GREATEST`, `LEAST` or `NULLIF` over a `utf8mb4_unicode_ci` column, or ordering by or comparing a text-answering call over a `utf8mb4_unicode_ci` or `utf8mb4_bin` column — `ORDER BY LOWER(name)` | refused; each compares under `utf8mb4_0900_ai_ci`'s weights and needs the column's collation passed through |
| An explicit `COLLATE utf8mb4_unicode_ci` inside a query — `ORDER BY name COLLATE utf8mb4_unicode_ci`, `name = 'a' COLLATE utf8mb4_unicode_ci` | refused; the query renderer names only `utf8mb4_0900_ai_ci` and `utf8mb4_bin` |
| `SHOW CREATE TABLE` for a column that named its table's own non-default collation itself | prints ` COLLATE <name>` where MySQL prints ` CHARACTER SET utf8mb4 COLLATE <name>`; which columns named it themselves is not remembered |
| Generated columns | refused |
| Partitioning | refused, and out of scope — see what this frontend is for |

### DML

| Form | State |
|---|---|
| `INSERT` mixing generated and explicit `AUTO_INCREMENT` ids | supported for ordinary `VALUES` rows, including a new explicit high-water mark. A mixed statement with a new high-water mark holds the sidecar lock while it applies rows in order; successful explicit rows and generated reservations remain durable after a later row fails or the transaction rolls back. Mixed `IGNORE` and `ON DUPLICATE KEY UPDATE` with explicit ids remain refused |
| A prepared `INSERT` writing explicit or NULL `AUTO_INCREMENT` ids | supported under the same high-water rule; bound values are checked before reservation |
| `INSERT ... ON DUPLICATE KEY UPDATE` over several rows on an `AUTO_INCREMENT` table | supported when every row generates an id and the update leaves the counted column unchanged; rows are applied in order, and the first inserted id and affected-row count are reported — or, where no row was added but one was changed, the id of the last row matched. A prepared statement is taken the same way, a value bound in the upsert clause included. Explicit ids in this form, and `REPLACE`, remain refused |
| `INSERT IGNORE` writing NULL | supported for an `AUTO_INCREMENT` column, where NULL asks for the next id; other NULL coercions remain refused |
| `INSERT IGNORE` over several rows on an `AUTO_INCREMENT` table | supported when every row generates an id, prepared or not. A skipped row consumes a reserved slot, and later successful rows reuse the next number MySQL assigns. Explicit ids in this form remain refused |
| A reading of the clock in a counted `INSERT` whose rows are written one at a time — several rows with `IGNORE` or `ON DUPLICATE KEY UPDATE`, or rows asking for the next id beside one naming its own id past the counter | refused; MySQL reads the clock once for the statement, and each of these rows is written by a statement of its own |
| `INSERT ... SELECT` into an `AUTO_INCREMENT` table mixing rows that name their own id with rows asking for the next | refused; measured, MySQL takes a new batch of numbers whenever a written id passes the batch it holds — `NULL`, `100`, `NULL` into a table counting at 37 writes 37, 100 and 101 and leaves `AUTO_INCREMENT=103` — which the copy path does not repeat |
| `INSERT IGNORE ... SELECT`, `REPLACE ... SELECT` and `INSERT ... SELECT ... ON DUPLICATE KEY UPDATE` into an `AUTO_INCREMENT` table | refused; what a colliding row spends and reports beside a `SELECT` has not been measured |
| A prepared `INSERT ... SELECT` into an `AUTO_INCREMENT` table, which is how Laravel's `insertUsing` arrives with its bindings | refused; the copy path reads the `SELECT` with no bound values, and a bound value compared in the `WHERE` of any `INSERT ... SELECT` is refused already, needing the column types a second rendering pass would give |
| `INSERT ... SELECT` into an `AUTO_INCREMENT` table in a session whose time zone is not UTC | refused; the copy path does not shift `TIMESTAMP` values between zones |
| `INSERT ... SELECT` into an `AUTO_INCREMENT` table whose batch of numbers would pass the column's highest | refused; measured, MySQL cuts the last batch short there and writes the rows that fit |
| `INSERT IGNORE` coercing a value MySQL would clamp | refused instead; needs the coercion `INSERT` does not have either |
| `INSERT ... SELECT` whose `SELECT` needs a second rendering pass | refused; there is no way to ask for that pass from a DML statement |
| `INSERT ... SELECT` without a column list, carrying `IGNORE` or an upsert clause | refused; those forms are refused wherever they are written |
| `UPDATE` / `DELETE` over more than one table | refused |
| `LIMIT` with no `ORDER BY`, or an `ORDER BY` over a column that is not an integer, on an `UPDATE` / `DELETE` | refused |
| An `INSERT` into a counted table carrying a trigger, or into a table whose trigger writes into a counted table | refused; a restored dump's counted table with a trigger takes no new row. The rows the dump wrote before making its trigger are taken |
| `TRUNCATE TABLE` on a counted table carrying a trigger | refused; that table is written again to restart its counter and a trigger is not the table's own row, where MySQL leaves one where it stood. A table with no counter is emptied in place and keeps its triggers |
| `SET foreign_key_checks` to a value that is neither 0, 1, `OFF` nor `ON` | refused as a syntax error where MySQL answers 1231 |

---

## Transactions and locking

| Feature | State |
|---|---|
| `BEGIN` / `START TRANSACTION`, `COMMIT`, `ROLLBACK` | works |
| `SET autocommit = 0 \| 1` | works |
| `LOCK TABLES` / `UNLOCK TABLES` | works, and the lock is held until the unlock. One lock over the whole database rather than one for each table, so it locks more than was asked for — see COMPAT.md |
| Touching a table `LOCK TABLES` did not name | taken; MySQL answers 1100 and holds the session to the tables it locked, where one lock over everything has no reason to |
| `START TRANSACTION` / `COMMIT` / `ROLLBACK` while tables are locked | refused; the lock is held by the transaction they would end, and MySQL keeps the two apart |
| `LOCK TABLES ... READ LOCAL` and `LOW_PRIORITY WRITE` | refused; the first lets other sessions insert while the lock is held and the second changes who waits for whom, and one write lock answers neither |
| A savepoint over an in-memory database | taken and does nothing; the engine needs the pager's sub-journal to undo anything, and over an in-memory database `SAVEPOINT` and `ROLLBACK TO` both answer OK without rolling back. The server runs over files, where it works |
| A savepoint name that is a reserved word — `SAVEPOINT select` | taken; MySQL answers 1064 unless it is quoted |
| `SET TRANSACTION ISOLATION LEVEL` naming `READ UNCOMMITTED` or `SERIALIZABLE`, or `GLOBAL` | refused; `READ COMMITTED` and `REPEATABLE READ` are kept, and saying yes to another would be a guarantee this does not keep |
| `SELECT ... FOR UPDATE` / `FOR SHARE` / `LOCK IN SHARE MODE` | works, and the lock is held. A session kept out by it waits and answers 1205, the way MySQL's does. One lock over the whole database rather than one for each row, so it is stronger than MySQL's — see COMPAT.md |
| `SET innodb_lock_wait_timeout` | works, one to 1073741824 seconds, and the session starts at MySQL's fifty |
| `FOR UPDATE NOWAIT`, `SKIP LOCKED`, `OF <table>` | refused; each asks what to do about a lock on some rows, and there is one lock over the whole database |
| `COMMIT AND RELEASE`, `ROLLBACK AND RELEASE` | refused; MySQL closes the connection after them, which is a protocol behaviour rather than a statement |
| `COMMIT AND NO CHAIN` | refused; it is the default spelled out, but the token check takes only the forms it knows |
| `GET_LOCK`, `RELEASE_LOCK`, `IS_FREE_LOCK`, `RELEASE_ALL_LOCKS` | works, in a `SELECT` of those calls alone with a written name and a whole-number or `NULL` timeout |
| `IS_USED_LOCK` | refused; it answers the holder's connection ID, which this server does not hand out |
| A named-lock call beside anything else, with a bound name, or with a fractional or quoted timeout | refused; MySQL reads a fraction as whole seconds by a rule of its own |
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
| `information_schema.COLUMNS` `CHARACTER_MAXIMUM_LENGTH`, `NUMERIC_PRECISION`, `NUMERIC_SCALE`, `COLLATION_NAME` | works for supported column declarations; values and wire types were measured against MySQL 8.4.11 and are checked in the JDBC/Go E2E |
| An `information_schema` query with no `TABLE_SCHEMA`, or one naming its table with an alias — `SELECT c.COLUMN_NAME FROM information_schema.COLUMNS c` | refused; MySQL reads a table of that name in every database the caller can see, which is a search this does not make |
| `SELECT @@net_write_timeout` | reports the configured write deadline in whole seconds; the default is 60. The deadline applies to each bounded server write |
| `SELECT @@net_read_timeout`, `@@socket` | refused as 1193; this server has no equivalent session setting. The message does not name the variable where MySQL's does |
| A system variable read inside a larger statement — `SELECT @@autocommit + 1`, `SELECT @@autocommit FROM t` | refused as 1235; MySQL reads the variable and answers the row, and an unknown name there is its own 1193 |
| `SHOW WARNINGS`, `SHOW ERRORS` | works |
| `SHOW PROCESSLIST` | not started |
| `SHOW TABLE STATUS` with `WHERE` | refused; the `FROM`/`IN` and `LIKE` forms work, and a `WHERE` is a predicate over the eighteen columns rather than a pattern |
| `SHOW TABLE STATUS` storage figures | answered NULL; InnoDB keeps them and this does not |
| `SHOW ENGINE INNODB STATUS`, `SHOW STORAGE ENGINES` | refused; the first reports InnoDB internals this server does not have |
| `SHOW TABLES` with `WHERE` | refused; the `LIKE` form works, and a `WHERE` is a predicate over the one column rather than a pattern |
| `SHOW TABLES` from another database | refused; the `FROM`/`IN` forms name a database this session has not selected. `SHOW FULL TABLES FROM` the selected database works |
| `SHOW COLUMNS` with `WHERE` | refused; the `LIKE` form works, as does the `DESCRIBE t <name>` spelling of it |
| `SHOW FULL COLUMNS` `Privileges` | reports database or table grants; column-specific grants are not supported |
| `EXPLAIN <statement>` | refused; `EXPLAIN <table>` works, being what MySQL makes it — the same rows `DESCRIBE` prints. The statement form is the optimizer's own plan over twelve columns, and answering it would mean writing down a join order, a key choice and a row estimate this server does not make |
| `FLUSH` of anything but `TABLES`, or `FLUSH TABLES` with a table list or `WITH READ LOCK` | refused; the plain form asks for a closed table cache, which this server has none of, while a read lock held across statements, reloaded grants and rotated logs are each something it cannot deliver |
| `OPTIMIZE TABLE` | refused; MySQL's InnoDB does a recreate and analyze, and the engine's nearest thing is a database-wide `VACUUM` — far more than one table was asked for |
| `CHECK TABLE` with a list, a qualified name, or the `QUICK` / `FOR UPGRADE` / `EXTENDED` options | refused; one unqualified table at a time is taken |
| `ANALYZE TABLE` over several tables, or with `NO_WRITE_TO_BINLOG`, `LOCAL` or a histogram clause | refused; one unqualified table at a time is taken |
| `CREATE USER`, `GRANT`, `REVOKE` beyond the narrow account slice | `CREATE USER 'name'@'%' IDENTIFIED BY 'password'` and table-scoped `GRANT` / `REVOKE SELECT ON db.table TO` / `FROM 'name'@'%'` use a dedicated account-management privilege, the crash-safe journal, external checkpoint CAS, and runtime reload. Other hosts, database/global grants in SQL, multiple privileges, and account alteration/drop remain refused. The offline provisioner may bootstrap an administrator with `--global-manage-accounts true` |
| Stored procedures, functions, events | refused, and out of scope — see what this frontend is for |
| `information_schema` beyond `TABLES`, `COLUMNS`, `SCHEMATA`, `STATISTICS`, `KEY_COLUMN_USAGE`, `TABLE_CONSTRAINTS`, `REFERENTIAL_CONSTRAINTS` | not started; what is left is `VIEWS`, `ROUTINES` and the server-status tables, none of which a test suite reads to find out what the schema is |
| A `CHECK` constraint in `information_schema.TABLE_CONSTRAINTS` | no row; the engine keeps a `CHECK` in the stored DDL rather than in the schema these tables read, and reading it would mean decoding the schema envelope inside a scan |
| `information_schema.STATISTICS.CARDINALITY` | refused; it is an estimate of distinct values and the engine keeps no equivalent, so a made-up number would be worse than none |
| `SELECT *` over an `information_schema` table | refused; it asks for MySQL's columns and this answers a few of them. `KEY_COLUMN_USAGE` answers all of them and is refused anyway, because one rule for all of these tables is worth more than the wildcard |
| A call over an `information_schema` column | refused; its shape has not been measured. A count is taken, since a count does not depend on what the column holds |
| `DATABASE()` outside an `information_schema` query's `FROM`, or in a query reading only user tables — `SELECT DATABASE() FROM t` | refused; the call is written in as the selected database only where a framework filters `information_schema` on it. The answer lives in the session and the renderer has no way to carry it into a statement, and a prepared one would keep the old name across a `USE`. Measured: a `VAR_STRING` of 256, nullable |
| `VERSION()` beside a `FROM` — `SELECT VERSION() FROM t` | refused; the version string lives in the server crate, out of the renderer's reach. `SELECT VERSION()` alone works. Measured: a `VAR_STRING` of 24, NOT NULL |
| `CONNECTION_ID()`, `USER()`, `CURRENT_USER()`, `SESSION_USER()`, `SYSTEM_USER()`, `ROW_COUNT()`, `FOUND_ROWS()` | refused, alone or beside a `FROM`; each answers session state — the connection's ID, the account, the last statement's counts — that is not handed to the statement renderer. Measured: `CONNECTION_ID()` a NOT NULL unsigned `LONGLONG` of 21, `ROW_COUNT()` and `FOUND_ROWS()` a NOT NULL `LONGLONG` of 21, and the user calls a nullable `VAR_STRING` of 1152 |
| `information_schema` columns beyond the eight of `TABLES`, thirteen of `COLUMNS`, seventeen of `STATISTICS`, and all of `KEY_COLUMN_USAGE`, `TABLE_CONSTRAINTS` and `REFERENTIAL_CONSTRAINTS` this answers | refused; the rest are statistics and timestamps this server does not keep, and answering NULL would be a claim of its own |
| An `information_schema.COLUMNS` or `SCHEMATA` `WHERE` beyond the one shape each takes | refused; those two are the last recognized by written shape, and moving them to a table the engine scans is the work that retires the recognizer |
| Multi-statement `COM_QUERY` beyond the bounded slice | negotiates `CLIENT_MULTI_STATEMENTS` and returns sequential results with `SERVER_MORE_RESULTS_EXISTS`. More than 32 statements and SQL whose delimiter changes under the session's backslash mode are refused before execution. An assembled response beyond 512 frames or 1 MiB closes the connection after execution, so preceding side effects may remain. A later statement can fail after earlier statements have run, as in MySQL. The runtime's configured write queue can impose a tighter limit |

---

## Session variables

| Variable | State |
|---|---|
| `@@version`, `@@version_comment`, `VERSION()` | works |
| `@@max_allowed_packet`, `@@wait_timeout`, `@@sql_notes` | works |
| The variables a driver reads before it sends any work — the `@@character_set_*` and `@@collation_*` names, `@@time_zone`, `@@system_time_zone`, `@@transaction_isolation`, `@@auto_increment_increment`, `@@auto_increment_offset`, `@@interactive_timeout`, `@@performance_schema`, `@@lower_case_table_names`, `@@init_connect`, `@@license` | works; each answers what this server decides for itself, and three read differently from MySQL's own — see COMPAT.md |
| `SET NAMES`, `SET sql_mode`, `SET information_schema_stats_expiry` | taken when they name the state the server is already in; `sql_mode` may be an expression over `@@sql_mode`, and `NO_AUTO_VALUE_ON_ZERO` is kept |
| `SET wait_timeout`, `SET sql_auto_is_null = 0`, `SET sql_safe_updates = 0`, several assignments in one `SET` | works |
| A `SET` of a `GLOBAL` variable, `sql_auto_is_null = 1` or `sql_safe_updates = 1` | refused; nothing here can change another session, and neither rule is one this server has |
| `SET wait_timeout` outside one second through a year | refused, where MySQL clamps it with a warning |
| `SET time_zone` | accepts `UTC`, `SYSTEM` and fixed offsets from `-13:59` through `+14:00`; see the TIMESTAMP boundaries below |
| `SET unique_checks` | works; with it off, a duplicate key is refused as MySQL 8.4 refuses one under its default `innodb_change_buffering=none` |
| `SET character_set_client`, `character_set_results` or `collation_connection` to latin1 | taken for what a dump sets around a view: a statement outside ASCII, a result set, and a prepared statement are refused while it is named, and a view made then records utf8mb4 rather than latin1 |
| Any character set or collation but utf8mb4's three and latin1 | refused |
| `SHOW WARNINGS` with no database selected | refused as 1046, where MySQL answers it |
| Any other `@@name` | refused as 1193 rather than answered with a value the server does not have |
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
| `BIGINT UNSIGNED` | works across 0..18446744073709551615, including prepared binary values, indexed comparisons, and reopening a database; see COMPAT.md for the remaining expression boundaries |
| `VARCHAR`, `CHAR`, `TEXT`, `TINYTEXT`, `MEDIUMTEXT`, `LONGTEXT`, `BLOB`, `TINYBLOB`, `MEDIUMBLOB`, `LONGBLOB` | works |
| `DECIMAL`, `DOUBLE`, `FLOAT`, and the other spellings `DOUBLE PRECISION`, `REAL`, `FLOAT4`, `FLOAT8` | works |
| `DATETIME`, `TIMESTAMP` | works with fractional precision 0 through 6; fixed-offset TIMESTAMP conversions have the boundaries below |
| An unsigned prepared value above `i64::MAX` compared with a signed `BIGINT` | refused with 1235; MySQL finds no matching signed row, but this frontend has not implemented that cross-type comparison |
| `BIGINT UNSIGNED AUTO_INCREMENT` | supported with a separate `mysql_uint64` primary key and a durable `u64` counter, including starts above `i64::MAX`, multirow generated IDs, explicit wide IDs, reopening, and `ON DUPLICATE KEY UPDATE`, whose matched row's id is read back off the row |
| `TINYINT`, `SMALLINT` and `MEDIUMINT AUTO_INCREMENT` | refused; no allocator counts in them, and no schema a migration tool writes asks for one |
| `UNSIGNED` on `DECIMAL`, `DOUBLE`, `FLOAT` | works |
| Arithmetic and aggregates over an unsigned column | not measured; the result's own type and width have not been recorded |
| `DATE` | works |
| `TIME` | works with fractional precision 0 through 6, including signed spans and prepared binary parameters |
| `YEAR` | works |
| A `WHERE` comparison against a temporal value written any way but the one the column holds — `d = '2024-1-1'`, `dt = '2024-01-01'`, `y = 24` | refused; MySQL reads each of those as a value the stored form would not meet, and rewriting the literal into that form is a second rendering pass this does not make |
| An ordering comparison against a `TIME` column — `t > '02:00:00'` | refused; a span runs past a day so its hours outgrow two digits, and it carries a sign, so reading two of them in order is not reading them in time order. `=`, `!=`, `<=>` and `IN` are answered |
| A `?` compared against a `TIME` or `YEAR` column | refused; their bound comparison forms are not implemented. Bound DATE, DATETIME and TIMESTAMP values are normalized to the column's stored precision |
| A call other than `CURDATE()`, `NOW()` or `CURTIME()` on the right of a comparison — `n = ABS(-1)` | refused; the three that are read answer a value in the form a column holds and take no argument, and rendering a call with arguments there is the projection renderer's work rather than the comparison reader's |
| `DATE_ADD` / `DATE_SUB` over a reading of the moment | works, in a projection, as a value to write, and on the right of a comparison — see COMPAT.md |
| `DATE_ADD` / `DATE_SUB` over `CURTIME()` | refused; a `TIME` holds a span rather than a moment, and shifting a span by a month names nothing |
| A shift of anything but a column, a clock reading or a written moment — `DATE_SUB(MAKEDATE(2024, 1), INTERVAL 1 DAY)` | refused; the thing shifted has to say what kind it is |
| A call answering a real number on the left of a comparison — `ABS(ratio) = 1` | refused; what a `DOUBLE` compares equal to is a rule of its own and it has not been measured |
| `TIME(col)` | refused; a `TIME` holds a span running past a day and the engine's reader answers NULL for one, so the two would not agree. `DATE(col)` is taken, being the other spelling of `CAST(col AS DATE)` |
| A call on both sides of a comparison, or one beside a column — `LOWER(a) = LOWER(b)`, `LOWER(a) = b` | refused; one side has to be a value the comparison reader takes |
| `(a, b) IN (SELECT ...)` | refused; the row list is written out as `AND` and `OR`, and a subquery has no rows to write out |
| A join `ON` matching one column against another with anything but equality — `ON a.n > b.n` | refused; matching one column against another is what a join is for and that is equality, and everything else an `ON` says is a comparison against a value |
| A join `ON` naming a value in an `UPDATE` or a `DELETE` | refused; each renders its own `FROM` with no statement to record the comparison in, so the value would go unchecked |
| A subquery in a joined `UPDATE`/`DELETE` `WHERE` comparing a qualified column — `WHERE a.id IN (SELECT ...)` | refused; `IN (SELECT ...)` reads only an unqualified column on its left, which a joined statement cannot write |
| A call as a `LIKE` pattern — `LIKE CONCAT('%', ?)` | refused; the pattern is written or bound, and a client that prepares one can build it before it binds |
| An aggregate other than `COUNT` over a qualified column — `MIN(b.id)`, `AVG(b.id)` | refused; each answers its argument's own type, so the qualifier has to be resolved to the table it names before the result's shape is known. A count does not depend on what it counts, so that one is taken |
| A `LIMIT ?` on an `UPDATE` or `DELETE` | refused; only the `SELECT` limit reads a parameter so far |
| An `UPDATE` or `DELETE` `LIMIT` wider than a signed 64-bit count | refused; a count that wide means every row, and what MySQL does with one written there has not been measured |
| A call other than `CURDATE()`, `NOW()` or `CURTIME()` as a value to insert or assign | refused; the three that are read answer a value in the form a column holds and take no argument |
| A value naming a column the same `SET` has already assigned — `SET a = 100, b = a` | refused; MySQL reads the assigned value there and the engine reads the row as it was. The other order is answered |
| Division in an assignment — `SET b = b / 2` | refused; measured, `101 / 2` answers 50.5 in MySQL and 50 in the engine |
| Arithmetic as a value to insert — `INSERT ... VALUES (n + 1)` | refused; a row being written has no row to read a column out of |
| `NOW()` written into a number column | refused; MySQL runs the moment together into 20260908170430 and reading a moment as a number is a rule of its own |
| A legacy v1/v2 MySQL text table | opening fails closed; its stored NOCASE indexes need an explicit rebuild before UCA9 can be used. Automatic migration is not implemented |
| A `LIKE` against a temporal column — `d LIKE '2024-%'` | refused; the pattern is not a value of the column's type, which is what the canonical-form check reads |
| `CAST(col AS UNSIGNED)` | refused; measured, MySQL wraps a negative into an unsigned 64-bit number and the engine holds an integer as an `i64` |
| `CAST(col AS DECIMAL)` and a `DECIMAL` or real column written out with `CHAR` | refused; their result scale, precision and text conversion need separate MySQL rules |
| `CAST(col AS CHAR(n))`, and a word read as a number or a day | refused; measured, each cuts the value short or answers NULL and warns about it, and the warning is not raised here |
| `CONVERT(col USING <charset>)` and the T-SQL `CONVERT(<type>, col)` / `TRY_CONVERT` | refused; the first names a character set rather than a type and this server speaks one, and the others are not MySQL |
| A `WHERE` comparison against a `JSON` column | Partly supported for one base table in a checked `SELECT`: `=`, `<>`, `<=>`, `<`, `<=`, `>` and `>=` with written strings and signed 64-bit integers, plus `IN` and `NOT IN` with those values and SQL `NULL`. Strings compare as JSON strings by their bytes; integers compare with JSON numbers exactly, preserving distinct values at 2^53 and 2^53+1. Ordering comparisons follow MySQL's JSON type precedence: JSON null, number, string, object, array, boolean. SQL `NULL` retains three-valued logic and `<=> NULL` matches only SQL NULL, not the JSON value `null`. Measured on 8.4.11: `doc = '{"a": 1, "b": 2}'` does not match an object holding those members, while `doc = 'word'` matches the JSON string `"word"`. Bare/qualified `ORDER BY doc`, a fractional or out-of-`i64` numeric right side, a bound `?`, explicit collation, multi-source or view comparisons, and JSON comparisons in checked DML remain refused. A bound `?` is gated because MySQL's JSON comparison can depend on earlier parameter types in the same prepared statement; wider JSON grouping and ordering still need a separate audit |
| A JSON reading in a `WHERE` — `meta->>'$.lang' = 'en'`, `json_contains(...)`, `json_length(...) = 2` | works over a `JSON` column of one table in a `SELECT`, `UPDATE` or `DELETE`; see COMPAT.md. Refused: a reading of a text column, which MySQL parses as a document first; a qualified column, a join, a subquery or a view; `LIKE`, `IN`, `BETWEEN` and an explicit `COLLATE` over unquoted text, whose collation rules were not measured there; `->` or `JSON_EXTRACT` compared with a written boolean — measured, `doc->'$.a' = TRUE` finds the JSON `true` where TRUE is otherwise 1 — with a fraction, or with a `?`; a `?` in a prepared `UPDATE` or `DELETE`; a word bound after a number in the same prepared statement, which MySQL then reads as a number; and paths with wildcards, `last`, ranges, spaces, a bare name MySQL refuses or a quoted name holding a quote or a backslash |
| `->`, `->>`, `JSON_EXTRACT` and `JSON_UNQUOTE` in a projection | answered by the engine's own `->` and `->>`, which differ from MySQL where a condition does not: measured on 8.4.11, `$[0]` over an object is the object in MySQL and nothing here, the JSON null unquotes to the word `null` in MySQL and to NULL here, and `true` unquotes to `true` there and to 1 here. The dialect's `mysql_json_extract` and `mysql_json_unquote`, which a condition uses, read all three MySQL's way |
| A `WHERE` comparison against a `BLOB` column | refused; MySQL compares bytes, and the checked comparison validator currently accepts written text only against declared text columns |
| An `ENUM` or `SET` member spelled any way but the way it was declared — `state = 'ACTIVE'` | refused; MySQL's collation ignores case and finds the row, and comparing the stored spelling against that text would find nothing. Asking the engine for the collation here needs the second rendering pass an `ORDER BY` over one already takes |
| An ordering comparison against an `ENUM` or `SET` column — `state > 'active'` | refused; MySQL reads an `ENUM` by the position its members were declared in, which is not the order their words read in |
| A number compared against an `ENUM` or `SET` column — `state = 2` | refused; MySQL reads it as a member's position and the stored value is the word |
| A `WHERE` comparison against a number written with a fraction and a text column — `label > 1.5` | refused; MySQL reads the text as a number, which is the coercion a string against an integer column is refused for |
| A word against a column holding numbers that is not a whole number spelled out — `age = '1.5'`, `' 30'`, `'3e1'`, `''`, `'30abc'` — or any word against a `DOUBLE`, or one past an `i64` | refused; measured, MySQL reads `'1.5'` against an `INT` as a double, the others without a warning or with warning 1292, and this reads only the words it can spell out as the one number MySQL reads. `id = '1'`, `IN ('1', '2')`, `BETWEEN`, a `DECIMAL` against `'10.5'`, a word bound against a whole-number column and `YEAR(col) = '2026'` are taken |
| A word bound against a call — `YEAR(created_at) = ?` | refused; a bound value against a call is refused whatever it binds |
| A `HAVING` counted against a number written with a fraction — `HAVING COUNT(*) > 1.5` | refused; a count is a whole number |
| A run of digits too long for an `i64` in a comparison against an integer or floating column — `n > 9223372036854775808` | refused; an exact `DECIMAL` column accepts up to 65 written digits after its type has been checked |
| `ENUM` | works |
| `ORDER BY` on a `SET` | orders by the numeric bit value of its members, as measured on MySQL 8.4.11 |
| A `DEFAULT` on an `ENUM`, or one as a key | refused; the column takes its nullability and nothing else yet |
| An `ENUM` member matched by case, trailing space, position or bit | works; the value is rewritten into the members' declared spelling and order the way MySQL rewrites it |
| An `ENUM` member holding a quote or a backslash | refused; the members ride inside a quoted declared type and are themselves quoted, so either would have to survive two escapings |
| `SET` | works |
| `JSON` | works |
| A `JSON` number MySQL reads imprecisely | MySQL 8.4.11's RapidJSON conversion is reproduced, including `1000000000000000.1` becoming `1e15` and `1e-30` becoming `9.999999999999999e-31` |
| A literal `DEFAULT` on a `JSON` column, or one as a direct key | refused with MySQL's measured 1101 and 3152 errors; `DEFAULT NULL` is accepted |
| `BINARY(n)` | refused; MySQL pads a shorter value with NUL bytes to the declared width and the engine has no padding, so taking it would store a different value |
| `BIT` and `BIT(1)` | works: written 0, 1, `TRUE` or `FALSE`, read back as the one byte MySQL sends over both protocols, compared against a written number, and printed `bit(1)` with a `b'0'` or `b'1'` default |
| `BIT(n)` wider than one bit | refused; it holds an n-bit number and crosses as ceil(n/8) bytes, which has not been measured |
| A bit literal or a word written into a `BIT(1)`, or compared against one — `b'1'`, `x'01'`, `''` | refused; measured, MySQL takes `b'1'` and `x'01'` as the bit and `''` as 0, and answers 1406 for any other word |
| A `?` compared against a `BIT(1)` column | refused; nothing says what kind of value it is until it binds, and a bound word would compare by its kind rather than as MySQL compares it |
| A call, an aggregate or arithmetic over a `BIT(1)` column — `c + 0`, `MAX(c)`, `IFNULL(c, 0)` | refused; the shape each answers has not been measured |
| `DEFAULT x'01'` or `DEFAULT ''` on a `BIT(1)` | refused; measured, MySQL takes both and prints `b'1'` and `b'0'` |
| A temporal precision above 6 | refused; precision 0 through 6 is supported for stored values, protocol results, and `DEFAULT` / `ON UPDATE CURRENT_TIMESTAMP(n)` at the column's own precision |
| Non-UTC TIMESTAMP queries beyond direct projection from one unfiltered base table | refused; joins, filters, ordering and expressions need conversion before the engine evaluates them |
| Non-UTC `UPDATE` or `DELETE` on a table with TIMESTAMP, or TIMESTAMP `INSERT ... SELECT` | refused; these paths cannot yet convert every value safely |
| Non-UTC `INSERT` into a table with `DATETIME DEFAULT CURRENT_TIMESTAMP`, or `UPDATE` on a table with `DATETIME ON UPDATE CURRENT_TIMESTAMP` | refused; the engine clock would store UTC rather than the session's wall time |
| Session-local clock functions in a non-UTC session | refused until their values can be evaluated in the session's time zone |

---

## Known divergences

Behaviour that works but does not match MySQL lives in
[COMPAT.md](COMPAT.md), not here. The open ones, each explained there:

- Legacy `DECIMAL` tables that stored binary64 values cannot recover their original
  decimal digits. Re-import them into a new exact `DECIMAL` table; opening an old
  table through the MySQL frontend fails with a migration error
- Legacy tables with a foreign key but no child index must be rebuilt or
  re-imported; opening them through the MySQL frontend fails with a migration
  error. Column-position changes on a referenced parent table are refused until
  its foreign key targets can be preserved through a rewrite
- `TIMESTAMP` converts explicit `INSERT ... VALUES` input and direct column
  output for fixed-offset sessions. Wider non-UTC queries and writes are
  refused as listed above
- A `REPEATABLE READ` transaction that writes after another session committed
  since its first read is rolled back with 1213, where MySQL writes and keeps
  reading the old snapshot for the rows it did not touch
- `CURRENT_TIMESTAMP(n)` as a default or on update, and `NOW(n)`,
  `CURTIME(n)` and their spellings as a result column, read the engine's
  clock, which stops at the millisecond, so digits past the third are zeros
- `NOW()` written into a `DATE` keeps the day without MySQL's note 1292 about
  the discarded time
- complex compound projections still have conservative nullable metadata
- complex window projections still have conservative source-column metadata
- `SHOW FULL COLUMNS` derives `Privileges` from database or table grants;
  column-specific grants are not supported

---

## Not verified

The remaining clients below have not been run end to end. The frontend is
also checked against a pinned MySQL 8.4.11 oracle and its own tests.

- PHP `mysqli`, Python `PyMySQL` / `mysqlclient`
- ORM versions and settings beyond the pinned GORM and Hibernate fixtures
- `mysqldump` and restore through a real `mysql` client. A standard
  `mysqldump --single-transaction --databases` of a small schema, replayed the
  way the client splits it, restores whole and reads back as dumped; the
  client itself has not been run against this server

The pinned MySQL 8.0.46 command-line client passed the privileged Linux
cross-UID TLS/TCP E2E locally on 2026-09-26. It covers schema creation and
inspection, writes and reads, rollback, and reconnect. Other CLI commands and
client versions still need coverage.

Pinned GORM 1.31.2 with `gorm.io/driver/mysql` 1.6.0 and
`go-sql-driver/mysql` 1.9.3, and Hibernate ORM 6.6.0.Final with Connector/J
9.6.0, passed the privileged Linux cross-UID TLS/TCP E2E on 2026-09-26.
Both fixtures passed against a disposable MySQL 8.4.11 oracle first. The
fixtures cover schema creation and inspection, migration, CRUD, relations,
NULL, exact `DECIMAL`, whole-second timestamps, unique and foreign-key errors,
transactions, and reconnect or pool reopening. Hibernate also validates its
schema after a server restart. The Hibernate fixture uses Connector/J's
`useInformationSchema=false` metadata path; other settings remain unverified.

Connector/J 9.6.0 and `go-sql-driver/mysql` 1.9.3 passed the same gate with
verified TLS, prepared inserts, CRUD, rollback, an added column, and schema
inspection. JDBC uses `useInformationSchema=false` so `DatabaseMetaData` reads
the supported `SHOW` surface. Other metadata paths and driver settings remain
open.
