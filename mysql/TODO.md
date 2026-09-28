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
| A number bound against a column of words — `name = ?` executed with a `LONGLONG` | refused, prepared `SELECT`, `UPDATE` and `DELETE` alike; measured, MySQL compares each word as the number it begins with — `name = 0` finds `'abc'`, `name = 5` finds `'5x'` — where the engine compares the number as the word it spells. A bound word is taken |
| A `WHERE` testing a column on its own in an `UPDATE` or a `DELETE` | refused; neither has a second rendering pass to learn whether the column is one of words |
| `IFNULL` / `COALESCE` falling a written word back onto a `TEXT` column | refused; measured, MySQL reports four times the column's own width there, a rule of its own |
| `IFNULL` / `COALESCE` falling a written number back onto a day, a moment or a `JSON` column | refused; measured, MySQL answers a `VAR_STRING` four bytes to each character of the column's text (40 for a `DATE`, 76 for a `DATETIME`) and a `LONG_BLOB` for a `JSON` |
| `NULLIF` over two columns of words, or a number against a word | refused; MySQL reads a word as a number and compares two words without regard to case, where the engine compares them by their kinds |
| `GREATEST`, `LEAST`, `NULLIF` and `MD5` over written values alone, naming no column | refused; each answers a shape read off the column it names, and there is none |
| `CAST(col AS CHAR)` over a `DECIMAL`, a `FLOAT` or a `DOUBLE` | refused; conversion through the engine's generic text cast has not been verified against MySQL's numeric formatting |
| `CAST(col AS CHAR)`, `SUBSTRING`, `SUBSTRING_INDEX`, `REPEAT` and `HEX` over a `TEXT` | refused; MySQL answers a `MEDIUM_BLOB` for each, and only `CONCAT`, `CONCAT_WS`, `LOWER`, `UPPER`, `REVERSE`, `REPLACE` and `TRIM` have had its width measured |
| `ADDTIME`, `SUBTIME`, `TIME_FORMAT`, `PERIOD_ADD` | refused; measured shapes — `ADDTIME` over a `DATETIME` a `DATETIME` of 19 and over a `TIME` a `TIME` of 10, `TIME_FORMAT` a `VAR_STRING` as wide as its format — but no reading written for them |
| `MAKEDATE`, `MAKETIME`, `FROM_DAYS`, `PERIOD_DIFF` over a column | refused; each is worked out over written values alone |
| `CONV`, `FROM_BASE64`, `EXP`, `LN`, `LOG`, `LOG2`, `LOG10` | refused; `CONV` reads the digits it can and stops, `FROM_BASE64` answers a binary string of a width not worked out, and the logarithms and `EXP` come from a maths library whose last digit MySQL's need not share |
| `NATURAL JOIN` | refused; it joins on every column name the two tables share, which only the frontend can list, and a wildcard over it puts the shared columns first |
| `INET6_ATON`, `INET6_NTOA`, `IS_IPV6` | refused; each reads or answers a 16-byte binary string, unmeasured |
| `CHARSET(col)` and `COLLATION(col)` | refused; each answers the column's character set or collation name, which the rendered statement does not carry |
| A text call over a `MEDIUMTEXT` or a `LONGTEXT` | refused; measured, MySQL answers a `LONG_BLOB` — `LOWER(mt)` reports 268435440 and `CONCAT(mt, 'a')` 67108864, `max_allowed_packet` — by a rule not worked out |
| `HOUR()` / `MINUTE()` / `SECOND()` over a `TIME` | refused; a `TIME` holds a span running to 838 hours, which MySQL reads out whole and the engine has no reader for |
| `YEAR()` / `MONTH()` / `DAY()` over anything but a plain date column | refused; measured, `YEAR` over a `TIME` answers the current year, which is a coercion rather than a reading |
| `GROUP_CONCAT` with a `DISTINCT`, or with an `ORDER BY` over anything but one bare column of whole numbers or words under `utf8mb4_0900_ai_ci` in a statement reading one table, or the joined column the call reads | refused, but for Laravel's two catalog reads, which are answered whole (see COMPAT.md); the engine's planner refuses an `ORDER BY` inside an aggregate (`core/translate/planner.rs`), so the order is worked out when the parts are joined, and only over a column whose kind the statement is read knowing. Values tying in the order with different values are refused when joined: measured, MySQL joins `a` before `A` under `utf8mb4_0900_ai_ci`, which is not the order they were read in. Measured for `DISTINCT`: `DISTINCT` drops values equal under the column's collation, keeping the first read, and joins the rest in that collation's order — `b,B,é,e,A` answers `A,b,é`, and over whole numbers `-1,2,9,10,100` — where the engine's drops only identical values and keeps the order it read them. A cut `DISTINCT` names the row by its place in that order, counted across groups |
| `GROUP_CONCAT` over a `DOUBLE`, a `FLOAT`, a `BLOB`, a binary string or a `JSON` column | refused; measured, MySQL writes `1e20` and `0.1` where the engine writes `1.0e+20` and `0.100000001490116`, and a binary argument answers a binary result — up to 512 a `VAR_STRING` one byte to each with the binary collation and flag, past it a `LONG_BLOB` as wide as the limit — and a `JSON` one the text shape with the binary flag |
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
result, not in a subquery, a derived table or a `UNION` branch. A word in quotes
in a `UNION` branch is still named with its quotes and reported as a nullable
`VAR_STRING` of 4096; measured, MySQL names it after the first branch's word
and reports four bytes to each character of the widest branch's word, 0
decimals, NOT NULL — `'abcdefgh' UNION 'de'` a `VAR_STRING` of 32. A number with
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
or not measured. A `?` in their value places takes a word or NULL and refuses
a number when it binds: measured, MySQL writes a bound number as a JSON number
and then reads every word bound there as a number, refusing it with 1292,
which this does not follow. `JSON_ARRAYAGG` over a built document takes
`JSON_OBJECT` and `JSON_ARRAY` and no other call.

---

## SQL syntax

### `SELECT`

| Form | State |
|---|---|
| Scalar subquery in a projection answering a column rather than an aggregate — `SELECT (SELECT n FROM t)` | refused; measured, MySQL answers 1242 for a subquery returning more than one row where the engine answers the first row it finds. Taking it needs the inner statement to prove it answers at most one row — an `ORDER BY ... LIMIT 1`, which the subquery reader refuses today |
| Subquery anywhere but a `WHERE`, apart from `EXISTS` and an aggregate's scalar subquery in a projection | not started |
| An `IN`, a scalar subquery or an `UPDATE`/`DELETE` subquery joining tables — `WHERE id IN (SELECT p.user_id FROM posts p JOIN post_tag pt ON ...)` | refused; only a `SELECT`'s `WHERE EXISTS` joins inside, where no column of the subquery is read |
| A scalar subquery in a projection naming the statement's column without its table — `(SELECT COUNT(*) FROM posts WHERE posts.user_id = email)` where `posts` has no `email` | refused; MySQL reads the name as the statement's column, which takes the `NOT_NULL` flag off that table's result columns, and which of the two tables a bare name belongs to is not worked out here |
| `ORDER BY` an ordinal over a mixed wildcard projection — `SELECT t.*, id FROM t ORDER BY 2` | refused; requires expanding each wildcard to count through |
| A comparison as a result column over anything but a column against a written number, a word or a reading of the clock, a `COUNT`, a subquery's `COUNT`, a `DATEDIFF` or a `TIMESTAMPDIFF` against a written whole number, or two written whole numbers — `SELECT a > b`, `SELECT n > ?`, `SELECT SUM(n) > 0`, `SELECT -1 < 0` | refused; a written negative number first is named without its sign by the span this reads, and a column against a column and a bound value raise the coercion question a `WHERE` raises, and an aggregate other than a count may be NULL by rules not measured here |
| `NOT`, `IS TRUE`, `IS FALSE`, `IS NOT TRUE` and `IS NOT FALSE` over a word, a `DECIMAL`, or anything but a column in a projection | refused; measured, MySQL reads a word as the number it begins with — `NOT 'apple'` is 1 — and the engine does not. In a `WHERE` they take whatever the `WHERE` reader takes |
| `WITH ROLLUP` beside an `ORDER BY`, with `GROUPING()`, over an expression, a moment, `TEXT` or more than three keys, or with a `?` | refused; measured, an `ORDER BY` makes MySQL answer the rollup through a temporary table whose shapes differ, and the other shapes have not been measured |
| `BINARY col = ?`, `x = BINARY col`, `BINARY col LIKE ...` | refused; only `BINARY` before a column compared with a written word is taken |
| A hexadecimal literal compared with a column — `email = X'616E6E'` | refused; measured, MySQL compares it as a word under a text column's collation and as a number against a number column |
| A word with the `_utf8mb4` introducer in an `INSERT`, an `UPDATE` or a `DELETE` | refused; only a `SELECT` reads it without the introducer |
| `GROUP BY` the place of a projected call — `SELECT UPPER(name) ... GROUP BY 1` — or a place with `WITH ROLLUP`, or in a subquery or derived table | refused; measured, MySQL groups by the projected expression it names, which is taken only for a whole column of the statement's own projection |
| `GROUP BY` a call over words the client wrote — `GROUP BY UPPER(title)` | refused; MySQL groups words under the column's collation and the engine groups the call's answer by its bytes |
| A column the grouping keys decide through a `WHERE` match, a comma join, `USING`, a `RIGHT JOIN`, a match of columns that are not both whole numbers, or a derived table or view; or named in the `ORDER BY` or `HAVING`; or in a subquery, a `UNION`, a view, `INSERT ... SELECT` or `CREATE TABLE ... AS SELECT` — `SELECT id FROM users GROUP BY id ORDER BY name` | refused; MySQL lets each through as decided by the keys, which is worked out here only for a projected column over `JOIN` and `LEFT JOIN` matches of whole numbers (see COMPAT.md) |
| A scalar subquery naming a column in a grouped projection | refused; the outer columns it names are not held to the grouping. One naming no column, `(SELECT COUNT(*) FROM t)`, is taken |
| A statement grouping by a call, over any answer but a key call, `COUNT`, `SUM`, `MIN`, `MAX` over a number or words, and `AVG` | refused; MySQL reports the temporary table's column for it, whose shape has not been measured |
| Renaming what the offered row carries — `VALUES (...) AS offered (a, b)` | refused; the plain row alias is taken and the column list has not been measured |
| A `CASE` or `IF` branch written as a number with a point — `THEN 1.5 ELSE 0` | refused; MySQL answers a NEWDECIMAL there by a rule of its own. A `DECIMAL` column branch is taken |
| A `CASE` or `IF` mixing a word branch and a number branch | refused; that is a coercion, and what MySQL answers for it has not been measured |
| A `CASE` or `IF` branch holding an aggregate or arithmetic | refused; only a written number and a column have been measured |
| `SET n = DEFAULT` on an `UPDATE` | refused; MySQL writes the column's own default and this cannot work out what that is from the statement alone |
| `DEFAULT` in one row of an `INSERT` and a value in another | supported for the `AUTO_INCREMENT` column in a checked `VALUES` insert, and for any column in an upsert of several rows on a counted table, whose rows are written one at a time; other columns remain refused because leaving the column out would take the default for every row |
| `DEFAULT` beside `ON DUPLICATE KEY UPDATE` in every column, or an upsert reading a counted column off the offered row — `VALUES(id)` beside a `DEFAULT` id | refused; the engine's row of defaults has no room for the clause, and measured, MySQL's offered row carries the number the colliding row spent, which this is not held to |
| `DEFAULT(col)` naming some other column | refused; that writes another column's default, which leaving the column out cannot say |
| Several rows of defaults on an `AUTO_INCREMENT` table — `VALUES (DEFAULT), (DEFAULT)` | refused; which of the numbers the statement reports depends on the rows, and one row leaves no question |
| An `UPDATE ... SET` dividing by a number read from the row, or by a written zero | refused; dividing by zero answers NULL in the engine where MySQL raises 1365 for a write, and only a written divisor says which of the two a statement would get |
| An `UPDATE ... SET` writing a fraction into a whole-number column — `SET n = n / 3` | refused; MySQL rounds the fraction into the column and the engine will not store it. One that divides evenly writes the number MySQL writes |
| An `UPDATE ... SET` value reading a column the same `SET` has written | refused; MySQL takes the assignments left to right and the engine reads the row as it stood |
| A `<=>` in an `ON DUPLICATE KEY UPDATE` comparing a column of the row offered that is not a word offered for a `VARCHAR` or `TEXT`, a whole number for an integer column or a written number for a `DECIMAL` — a moment, a document, a bound value — and a `CASE` in the clause other than Rails' touch of a timestamp | refused; measured, MySQL puts the offered value into the column's type before it compares (`'2026-01-01'` equals a `DATETIME` holding that midnight) and the engine compares it as it was written |
| `ON DUPLICATE KEY UPDATE` on a table with an `ON UPDATE CURRENT_TIMESTAMP` column the clause does not write, on a table that does not count its ids, as one row naming its own id, in a session not in UTC, or on a table carrying a trigger | refused; measured, MySQL writes the moment there whenever the upsert changes the row. On a counted table an upsert asking for every id, or of several rows, is written a row at a time and a changed row written again with the moment; a statement written as one cannot say which of its rows changed, and a trigger would run twice |
| An `ON DUPLICATE KEY UPDATE` value reading a column of the row already there that the same clause has written, or one column written twice | refused; measured, MySQL takes these assignments left to right too — `a = a + 10, b = a` over 1 leaves `b` at 11 — and the engine reads the row as it stood |
| A double, or a word naming a fraction or nothing, bound into `UPDATE ... SET` arithmetic — `balance - ?` binding 0.5, `views + ?` binding `'2.5'` — or a `?` in arithmetic written into a column that holds no whole number or `DECIMAL` | refused; measured, MySQL does the arithmetic as a double's and rounds it into the column, which the engine's exact decimal arithmetic and its refusal to store a fraction in a whole-number column do not match. A bound whole number, a word naming one, NULL, and a word naming a decimal into a `DECIMAL` are taken |
| An `UPDATE ... SET` value taken from `(SELECT COUNT(*) FROM ...)` | refused; a count says nothing about the kind of the column written, and the pair is held to one kind |
| An `UPDATE ... SET` value whose subquery reads a column its own table does not hold | refused; that is a correlated read of the row being changed, which has not been measured |
| A collation over a `LIKE` or a membership test — `name LIKE 'a' COLLATE utf8mb4_bin`, `name IN ('a' COLLATE utf8mb4_bin)` | refused; the checked LIKE matcher uses the column's default UCA9 rule, while explicit byte collation and membership-test overrides need separate checked paths |
| `%`, `DIV` or a negation over a `DOUBLE` or a `DECIMAL`, and `MOD` over a `DOUBLE` | refused; measured, MySQL answers a `DOUBLE` of 23 or the column's own `DECIMAL`, and the engine's `%` reads a real number as a whole one |
| `%` or `DIV` by zero, `DIV -1`, and a negated `BIGINT` | refused; MySQL answers NULL with warning 1365 for the first, and 1690 for the smallest `BIGINT` in the others, where the engine answers a real number |
| `%`, `DIV` or a negation in a `WHERE` | refused; the comparison reader takes one `+`, `-` or `*` over narrow signed whole numbers, and these have not been measured there |
| Arithmetic in a `WHERE` over a `BIGINT`, an unsigned, a `DOUBLE` or a `DECIMAL` column, over more than one operator, or against a `?` — `big + 1 > 10`, `age + 1 > ?` | refused; measured, MySQL answers 1690 where a `BIGINT` or an unsigned answer leaves its range and the engine goes on in floating point, and the rest have not been measured |
| `COALESCE`/`IFNULL` in a `WHERE` over a `DECIMAL`, a day or a moment, or with more than two arguments | refused; the engine holds each in a form a written fallback is not in |
| `COALESCE`/`IFNULL` in an `UPDATE ... SET` naming its column through its table over anything but a column of whole numbers other than a `BIGINT UNSIGNED`, falling back on a written whole number | refused; MySQL reads a word or a `DECIMAL` by rules of its own there, and the engine hands a `BIGINT UNSIGNED` on in its stored form |
| `ROUND` of a `DECIMAL` or a `BIGINT` left of the point, and `FLOOR` or `CEIL` of a `DECIMAL` with more than eighteen whole digits | refused; measured, `ROUND` of a `BIGINT` past the largest one is 1690, and the others answer shapes of their own — `FLOOR` over a `DECIMAL(19,1)` a `NEWDECIMAL` of 20 |
| `ROUND` over an aggregate left of the point, over `MIN`, `MAX` or `COUNT`, or over a float column | refused; measured, `ROUND(MAX(views), 2)` answers a `LONGLONG` of 21 and `ROUND(SUM(views), -2)` 34 characters, shapes this does not work out |
| `ROUND` naming its places with anything but a written whole number | refused; the answer's width is worked out from them |
| `SIN`, `COS`, `TAN`, `ASIN`, `ACOS`, `ATAN`, `EXP`, `LN`, `LOG`, `LOG2`, `LOG10` | refused; measured, `ATAN(10)` and `TAN(10)` differ from MySQL in the last place, and the rest come from the same maths library, so agreeing at the points tried is not a promise |
| `BIN` or `OCT` over a word — `BIN(name)` | refused; MySQL reads the word as the number it names, which is 0 for a word that names none |
| `CONCAT` over a `DECIMAL` with places, a `FLOAT` or a `DOUBLE` | refused; the generic text conversion of exact decimal blobs and floating values has not been verified against MySQL's numeric formatting. A `DECIMAL` with no places and a `BIGINT UNSIGNED` are taken, in a projection and written by an `UPDATE` into a column of words |
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
| A comparison against a `SUM` subquery, or an `AVG` one over a `DECIMAL`, a float or a correlated argument — `WHERE n > (SELECT SUM(n) FROM t)` | refused; not measured. An `AVG` of whole numbers against a whole-number column is taken |
| A comparison against a subquery projecting a plain column — `WHERE id = (SELECT c FROM t)` | refused; MySQL answers 1242 once it finds more than one row and the engine takes the first |
| A `HAVING` over an aggregate with a fallback — `HAVING IFNULL(SUM(n), 0) > 1` | refused; the HAVING renderer records an aggregate's own argument column to check a literal against, and has no rule for one inside a call |
| A ranking over a window naming neither a partition nor an order — `ROW_NUMBER() OVER ()` | refused; the rows are numbered in whatever order they were read and the two need not read them alike |
| A table or a column qualified by a database other than the selected one — `SELECT * FROM other.t`, `SELECT other.t.*` | refused; the engine reads the selected database alone, and reading another would have to open and authorize it too. The selected database written this way is taken |
| An expression projected without an alias naming a column with its database — `SELECT COUNT(db.t.id)` | refused; MySQL names the result column after the text as written, which leaving the database out would change |
| `ORDER BY` over an expression this does not know the shape of, or over a random number — `ORDER BY RAND()` | refused; a random number orders the rows by nothing a client can hold this to, and whether each engine reads it once or once a row is a rule of its own |
| `HAVING` naming an alias for something other than an aggregate or a column — `SELECT LOWER(name) AS l FROM t GROUP BY name HAVING l > 'a'` | refused; the name resolves, but what it stands for is a shape the `HAVING` renderer does not take |
| `EXCEPT ALL`, `INTERSECT ALL` | refused; they keep duplicates the plain forms collapse, and the engine has no spelling for them |
| A `UNION` branch with its own `ORDER BY` or `LIMIT` | refused |
| A `UNION` of three or more branches | refused, but where every branch reads the same columns of the same `information_schema` table, which is how TypeORM reads one table's catalog rows a branch; which shape MySQL answers for a column three kinds meet in has not been measured, and a chain mixing `UNION` with `UNION ALL` loses MySQL's rule that a `UNION DISTINCT` undoes the `UNION ALL`s left of it |
| A `UNION` column pairing a number with a word, a whole number with a `DOUBLE` or `DECIMAL`, a `DATE` with a `DATETIME`, or a column with a written value | refused; measured, MySQL converts each pair by a rule of its own — `INT` with `VARCHAR(10)` a `VAR_STRING` of 44, `INT` with `DECIMAL(10,2)` a `NEWDECIMAL` of 14, `INT` with `DOUBLE` a `DOUBLE`, `INT` with a written 1 a `LONGLONG` of 11 — and the engine keeps two kinds apart |
| A `UNION` over `FLOAT`, `MEDIUMINT`, unsigned, `TIME`, `TIMESTAMP`, `YEAR`, `ENUM`, `SET`, `JSON`, `BLOB`, a fractional `DATETIME` or two different `DECIMAL`s | refused; their pairs have not been measured |
| A `UNION`, `EXCEPT` or `INTERSECT` dropping repeated rows over words compared without regard to case | refused; measured, MySQL keeps the first of `'aa'` and `'AA'` and the engine the last. `UNION ALL` and `utf8mb4_bin` columns are taken |
| A `UNION` naming a `DECIMAL` column, or reaching one through a wildcard or an expression | refused; only the first branch's table is read for column types |
| A `UNION` branch projecting a wildcard or an expression | taken with the first branch's shape, which is what the engine reports; MySQL's shape for the mixture has not been measured |
| `WITH RECURSIVE` over anything but a counted sequence of whole numbers — walking a tree of parents, a run of days | refused; the engine has no recursion limit, where MySQL answers 3636 past `cte_max_recursion_depth`, so only a depth the statement decides is taken |
| An aggregate other than `COUNT(*)` over a counted sequence, or the sequence read beside a table | refused; measured, `MAX(x)` answers a `LONGLONG` of 20 and `SUM(x)` a `NEWDECIMAL` of 42, rules not worked out here |
| A derived table or CTE whose body projects an expression without aggregating — `(SELECT n + 1 AS m FROM t) x` | refused; MySQL reads that body straight through, and what it reports for the expression there has not been measured. A statement only counting the derived table's rows takes any body |
| A `LIMIT` or an `ORDER BY` in a derived table's body, but a written `LIMIT` in one the statement only counts the rows of | refused; which rows a limit without an order keeps is each engine's own, and ordering them has not been measured |
| A derived table or CTE whose body aggregates with a call other than `DATE`, a `GROUP_CONCAT`, or `DISTINCT` | refused; the shape MySQL stores it in has not been measured |
| `COALESCE` or `IFNULL` over a column a derived table worked out, or over a qualified column — Prisma's `COALESCE(t._aggr_count_posts, 0)` | refused; the fallback reads its column's type by name from a table, which a derived table's answer has none of. Measured: over a count on the outer side of a `LEFT JOIN`, a NOT NULL `LONGLONG` of 21 with the binary flag |
| A comparison or an aggregate over a total or an average a derived table worked out — `WHERE t.s > 1`, `SELECT SUM(c) FROM totals` | refused; measured, `SUM(c)` over a count answers a `NEWDECIMAL` of 43, a rule not worked out here |
| A derived table whose body reads more than one table, or that is written `LATERAL` or names its own columns | refused; a body that is a `UNION` of branches each reading the same columns of one `information_schema` table is taken, which is what TypeORM writes. TypeORM's pagination over a relation, `SELECT DISTINCT distinctAlias.Post_id AS ids_Post_id, distinctAlias.Post_id FROM (SELECT Post.id AS Post_id, ... FROM posts Post LEFT JOIN post_tag ... LEFT JOIN tags ...) distinctAlias ORDER BY ... LIMIT 10`, is one: measured on 8.4.11 it answers each post's id once, and reports each column as a `LONGLONG` of 20 with only `NOT_NULL`, the original table `posts` — the table's own name, not the body's alias — and the original name the result column's own (`ids_Post_id`), MySQL sorting the distinct rows through a table of its own; without `DISTINCT`, or over a body reading one table, the columns keep their key flags and name the body's alias and column as their original table and name |
| A derived table in an `UPDATE` or a `DELETE` | refused; each reads its own table |
| `DISTINCT ON` | refused, and no part of MySQL |

### DDL

| Form | State |
|---|---|
| `ALTER TABLE` beyond `ADD COLUMN` / `DROP COLUMN` / `RENAME` / `MODIFY COLUMN` / `CHANGE COLUMN` / `ALTER COLUMN ... SET DEFAULT` / `DROP DEFAULT` / `COMMENT` / `ENGINE=InnoDB` / `AUTO_INCREMENT = n` / `ADD CHECK` / `DROP CHECK` / the index operations, `RENAME INDEX` among them | refused |
| `ALTER TABLE ... CONVERT TO CHARACTER SET utf8mb4 [COLLATE c]` | refused; measured on 8.4.11, every text, `ENUM` and `SET` column takes the new collation (a unique key that then holds two equal values is 1062 and the table stays as it was), but `SHOW CREATE TABLE` afterwards prints `CHARACTER SET utf8mb4` on the columns first written with a character set or collation of their own and not on the others, a mark this server does not keep |
| `ALTER TABLE ... ADD PRIMARY KEY` / `DROP PRIMARY KEY` | refused; not measured, and each writes the table again under a different key |
| `ENGINE=InnoDB` or `AUTO_INCREMENT = n` beside any other operation in one `ALTER TABLE`, or naming a view | refused; each is read by its own words and only on its own. Measured, MySQL answers 1347 for a view |
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
| `CREATE DATABASE` or `ALTER DATABASE` naming a character set but `utf8mb4`, a collation but `utf8mb4_0900_ai_ci` and `utf8mb4_unicode_ci` — `utf8mb4_bin` and `utf8mb4_general_ci` among them — or an encryption but `'N'` | refused; MySQL gives every table made in the database that collation, and a table here can take only those two as its own |
| `CREATE DATABASE IF NOT EXISTS` over a database already there, naming a character set or collation that would be refused | refused; measured, MySQL answers OK with note 1007 and leaves the database as it is |
| `ALTER DATABASE ... READ ONLY` | refused; measured, MySQL then refuses every write to the database |
| `CREATE TABLE ... SELECT` in a database whose collation is not `utf8mb4_0900_ai_ci` | refused; measured, MySQL gives the table the database's collation and each column its source column's, spelled out, which the column copy here does not write |
| `SHOW CREATE DATABASE` of `information_schema` and the other system databases | refused; measured, MySQL prints `information_schema` as `utf8mb3`, and this server lists none of them |
| `ALTER TABLE v DISABLE KEYS` / `ENABLE KEYS` over a view | refused, where MySQL answers 1347 |
| An `INSERT` or `UPDATE` writing a value other than NULL into a column its table's `BEFORE` trigger sets — Laravel's `UPDATE ... SET updated_at = ?` beside a trigger setting `NEW.updated_at` — or any `INSERT ... ON DUPLICATE KEY UPDATE` into such a table | refused. Measured on 8.4.11, MySQL holds the statement's value to its column before the trigger writes over it — one too long is 1406 even then — and here a value is held to its column only as the row is written |
| A `BEFORE` trigger writing another table, or one reading a `NEW` column it sets itself, or a counted one, in a `BEFORE INSERT` trigger | refused; the first runs before the row it fires for is checked, which has not been measured, and MySQL reads a column the statement left out as its implicit default and a counted one as 0 there |
| A `BEFORE UPDATE` trigger on a table carrying an `ON UPDATE CURRENT_TIMESTAMP` column | refused; whether MySQL stamps the column before or after the trigger has not been measured |
| A statement written in a session whose time zone is not UTC while any trigger of the database reads the clock | refused; a trigger reads the clock under the time zone of the session whose statement fires it, and the engine's clock reads UTC |
| A trigger body with `IF ... THEN SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = '...'; END IF` | refused. Measured on 8.4.11, a statement firing it answers 1644 with SQLSTATE 45000 and the message written, which the fixed error messages here have no room for |
| A trigger value beyond `NULL`, a word, a whole number, a column, `CONCAT` of those, a column moved by a whole number and a whole reading of the clock, or over a column that is neither a text nor an integer one — `LOWER(NEW.name)`, `CONCAT('t ', NEW.total)` over a `DECIMAL`, `NOW()` into a `DATETIME(3)` or a `VARCHAR` | refused; the trigger is translated without its columns' types each time the database is opened, and only these read the same whatever the types |
| A trigger `UPDATE` or `DELETE` comparing anything but whole numbers with `=`, or an `UPDATE` of a table carrying an `ON UPDATE` column or of a counted column | refused; a text comparison needs the column's collation, and the `ON UPDATE` rewrite and the counter both live in the frontend, which a trigger's own write goes around |
| A second trigger for one table, event and timing | refused; measured, MySQL runs them in the order they were made, which is not kept |
| A trigger that MySQL makes and then fails when it runs — naming a table or a column that is not there, or writing its own table | refused when it is made, where MySQL answers 1146, 1054 or 1442 when it runs |
| A view or trigger in a dump naming a `DEFINER` other than the account restoring it — `root`@`localhost` | refused, as MySQL refuses it to an account without `SET_ANY_DEFINER`; accounts here are all `'name'@'%'` |
| `ROW_FORMAT` and every other table option but the engine, character set, collation, `AUTO_INCREMENT` and `COMMENT` | refused; measured, MySQL prints `ROW_FORMAT` back, and none of the rest has been measured |
| A table `COMMENT` longer than 2048 characters, or written in double quotes | refused; MySQL answers 1628 for the first and takes the second |
| `AUTO_INCREMENT=<n>` naming a start past what the column holds | refused; measured, MySQL creates the table and answers 1467 for the first row, so this refuses the statement instead of storing a mark no row could take |
| `AUTO_INCREMENT=<n>` naming anything but a plain whole number | refused; measured, `-5` and `'7'` are each 1064 and `1.5` is rounded down, a rule this does not repeat |
| `ALTER TABLE ... AUTO_INCREMENT=<n>` moving the counter back | refused; measured, MySQL sets the next id to `n` or one past the highest id held, whichever is larger, which moves the counter back once the rows above it are deleted — `AUTO_INCREMENT = 1` after emptying a table starts it again — and the allocator only moves forward. A number past the column's type is refused too, where MySQL takes it and answers 1467 at the next row |
| Uniqueness over a word — a `PRIMARY KEY` or `UNIQUE` key over `VARCHAR`/`CHAR` | works with fixed Unicode 9 weights, folding accents and case in both comparisons and uniqueness; measured, `'ALPHA'` after `'alpha'` is 1062 both here and there |
| A `PRIMARY KEY` over a type that is neither a number nor a sized word — `TEXT`, `BLOB`, `DATE` | refused; measured, MySQL answers 1170 for a `TEXT` key for want of a length, and the rest have not been measured |
| `PRIMARY KEY (a, b)` naming a column without a `NULL`/`NOT NULL` clause | accepted; each key column is stored and reported as `NOT NULL`, as MySQL does. An explicit `NULL` or `DEFAULT NULL` on a key column remains refused |
| `PRIMARY KEY (a, b)` with an `AUTO_INCREMENT` column inside it | refused; the counted column stands for one rowid, which has no way to spread over a pair |
| An `AUTO_INCREMENT` column that is not the table's key, or one with no key at all | refused where MySQL answers 1075 |
| An `AUTO_INCREMENT` column not written `NOT NULL` | accepted when its table-level primary key names that column; the stored definition adds `NOT NULL`, as MySQL does. Other forms are refused |
| `PRIMARY KEY` carrying `USING BTREE` or an index name, or a column written `DESC` | refused; measured, MySQL prints all three back, so dropping them would print a different table |
| `PRIMARY KEY (missing)`, or a table writing two keys | refused where MySQL answers 1072 and 1068 |
| `ALTER TABLE` mixing supported index and column operations | accepted as one transaction; a later failure rolls back every earlier operation, and foreign-key child-index coverage is checked after the full statement so an index can be replaced atomically |
| `` ALTER TABLE ... ADD/DROP INDEX `PRIMARY` `` | refused; adding or dropping a primary key is a different operation |
| `DROP INDEX name` with no table after it | refused; MySQL requires the table, and the engine's own spelling names none |
| `RENAME TABLE` naming a database — `RENAME TABLE db.a TO db.b` — or a view, or any `RENAME TABLE` while the database holds a view | refused; MySQL takes all three. The last is the rule every `ALTER TABLE ... RENAME TO` already keeps |
| `ALTER TABLE a RENAME TO b` onto a name already taken, or of a table that is not there | refused, where MySQL answers 1050 and 1146; the `RENAME TABLE` spelling answers both |
| `CREATE TABLE ... AS SELECT` over a division, an aggregate, or an unaliased expression | refused; integer `+`, `-` and `*` work. A division makes a `decimal(14,4)` on a rule of its own, and an unaliased expression column takes its name from the expression's own text — measured, `SELECT a + 1` makes a column called `a + 1` |
| `CREATE TABLE ... AS SELECT` over a column with a string `DEFAULT` | refused; the escaping is undecided, the same reason `SHOW CREATE TABLE` refuses to print one |
| `CREATE TABLE ... (columns) AS SELECT`, and `IF NOT EXISTS` or `TEMPORARY` beside an `AS SELECT` | refused |
| `CREATE TEMPORARY TABLE` with `AUTO_INCREMENT` | refused; the allocator is keyed on a durable table |
| `CREATE TABLE ... LIKE` over a table with a `CHECK`, into a `TEMPORARY` table, or naming a database | refused; the copy is made from what `SHOW CREATE TABLE` prints, which cannot print a `CHECK` yet — MySQL renames each one `<new>_chk_<n>` — and the other two are unmeasured |
| `FOREIGN KEY` | works, and enforced |
| The index MySQL creates beside a `FOREIGN KEY` | created when no existing key has the child columns as a left prefix; it appears in `SHOW CREATE TABLE` |
| `ALTER TABLE ... ADD FOREIGN KEY` without a `CONSTRAINT` name | works; measured, MySQL names it `t_ibfk_N` counting the keys the table already carries, and so does this — two unnamed keys added one after the other read back as `t_ibfk_1` and `t_ibfk_2` |
| The index MySQL creates beside a `FOREIGN KEY` an `ALTER TABLE` adds | created or reused by the same rule as `CREATE TABLE` |
| Column-position changes on a table in a foreign key | accepted on a child and on a referenced parent, each key and supporting index retained through the rewrite and reopen |
| A placed `ADD COLUMN` beside any operation but another `ADD COLUMN` in one `ALTER TABLE` | refused; several `ADD COLUMN`s, placed or not, are taken |
| A column placed, moved, or a `CHECK` added or dropped, in a database holding a view or a trigger written through this server | refused; the table is written again under another name, and the engine refuses to rename a table while such a view or trigger exists |
| `FLOAT(M,D)` and `DOUBLE(M,D)` | refused; MySQL keeps the size and rounds a stored value to it — measured, 1.239 into a `double(10,2)` reads back 1.24 — which is a rounding rule this does not have |
| `FLOAT(p)` naming a precision | refused; MySQL reads `p` up to 24 as a `float` and above it as a `double`, which has not been measured |
| Warning 1681 for an integer display width or a floating-point size | not raised; MySQL raises one per column and this raises none, so a client counting warnings after a `CREATE TABLE` sees zero |
| `ON UPDATE CURRENT_TIMESTAMP` on a table with no key of its own, or in any `ALTER TABLE` | refused; the words live in the stored MySQL DDL, which only the keyed `CREATE TABLE` paths render from the statement as written — every other path rebuilds it from the engine's own definition, where the words are not |
| A joined `UPDATE` on a table carrying an `ON UPDATE` column | refused; a joined one names the table it changes through the columns its `SET` names, so which table's columns are its own is unanswered |
| A `DEFAULT` naming anything but a literal, `CURRENT_TIMESTAMP` or `(now())` over a whole-second moment | refused; no other expression default has been measured. Measured and not yet taken: `datetime(6) DEFAULT (now())` stores whole seconds, `DEFAULT (now(3))` prints as `(now(3))` and `DEFAULT (curdate())` as `(curdate())`, and `DEFAULT(col)` over an expression default is 3773 |
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
| A view summing or averaging — `CREATE VIEW v AS SELECT grp, SUM(n) AS s, AVG(n) AS a FROM t GROUP BY grp` — or with a `HAVING`, an unnamed aggregate, or the least or greatest of anything but a whole number or a `VARCHAR` | refused. Measured on 8.4.11, a `SELECT` from such a view reports `SUM` of an `INT` as a `NEWDECIMAL` of 33 and `AVG` as one of 16 with 4 decimals, each naming the view as its original table, which the value renderer here reads as a stored `DECIMAL`; MySQL prints `HAVING` as `` having (count(0) > 1) `` and names an unnamed aggregate after its text (`` count(0) AS `COUNT(*)` ``) |
| A condition on a view grouping its rows, or `SHOW COLUMNS` of one — `SELECT * FROM v WHERE c > 1` | refused; the comparison reader holds a value to the view's engine column, which carries no type for an aggregate, and the column listing reads only a view projecting its table's columns |
| A view joining tables with a comma, `USING`, `NATURAL`, `CROSS` or `RIGHT JOIN`, or a join nested on the right — `a JOIN (b JOIN c ON ...) ON ...` | refused. Measured on 8.4.11, `SHOW CREATE VIEW` prints a comma as `` (`a` join `b`) `` and `information_schema.VIEWS` as `` `a` join `b` ``, `USING (id)` as `` on((`u`.`id` = `p`.`id`)) ``, and a `RIGHT JOIN` turned around into a `left join` with its tables swapped |
| A `SELECT` from a view joining tables that sorts, groups, drops repeated rows or aggregates its rows — `SELECT * FROM v ORDER BY name` | refused. Measured on 8.4.11, MySQL reports the joined tables as the original tables, without their keys, whenever its plan sorts through a table of its own, and whether it does turns on which table it reads first: over the same view `ORDER BY name` did and `ORDER BY title` did not |
| A view joining tables that groups its rows or aggregates them, or `SELECT *` over a join | refused; not measured |
| A `SELECT` from any view reading a `DECIMAL` column | refused; the `SELECT` translator renders a `DECIMAL` from its table's column types, and a view's own columns carry none it reads |
| A view condition beyond comparisons of a column with a column or a written whole number, word, `TRUE`, `FALSE` or `NULL`, `IS [NOT] NULL`, `AND` and `OR` | refused. Measured, MySQL prints `IN`, `LIKE` and `BETWEEN` as `(c in ('a','b'))`, `(c like 'a%')`, `(c between 1 and 10)` and their negations as `(c not in (1,2))`, `(not((c like 'a%')))`, `(c not between 1 and 2)`, prints `-1` as `-(1)`, and rewrites `NOT (n < 0)` into `(n >= 0)` |
| A view with `SELECT *`, `DISTINCT`, `ORDER BY`, `LIMIT`, a column list or an expression column | refused; not measured |

### DML

| Form | State |
|---|---|
| `INSERT` mixing generated and explicit `AUTO_INCREMENT` ids | supported for ordinary `VALUES` rows, including a new explicit high-water mark, text and prepared, and rows that all name their own ids past the counter, which move it only once written. A mixed statement with a new high-water mark holds the sidecar lock while it applies rows in order; successful explicit rows and generated reservations remain durable after a later row fails or the transaction rolls back. An upsert of several rows naming ids is written a row at a time; one naming an id past the counter beside a row asking for one, and `IGNORE` over several rows naming ids, remain refused |
| A prepared `INSERT` writing explicit or NULL `AUTO_INCREMENT` ids | supported under the same high-water rule; bound values are checked before reservation |
| `INSERT ... ON DUPLICATE KEY UPDATE` over several rows on an `AUTO_INCREMENT` table | supported when every row generates an id and the update leaves the counted column unchanged — writing it to itself, GORM's `id = id`, included; rows are applied in order, and the first inserted id and affected-row count are reported — or, where no row was added but one was changed, the id of the last row matched. A prepared statement is taken the same way, a value bound in the upsert clause included. Explicit ids in this form, and `REPLACE`, remain refused |
| `INSERT IGNORE` writing NULL | supported for an `AUTO_INCREMENT` column, where NULL asks for the next id; other NULL coercions remain refused |
| `INSERT IGNORE` over several rows on an `AUTO_INCREMENT` table | supported when every row generates an id, prepared or not. A skipped row consumes a reserved slot, and later successful rows reuse the next number MySQL assigns. Explicit ids in this form remain refused |
| `INSERT ... SELECT` into an `AUTO_INCREMENT` table mixing rows that name their own id with rows asking for the next | refused; measured, MySQL takes a new batch of numbers whenever a written id passes the batch it holds — `NULL`, `100`, `NULL` into a table counting at 37 writes 37, 100 and 101 and leaves `AUTO_INCREMENT=103` — which the copy path does not repeat |
| `INSERT IGNORE ... SELECT`, `REPLACE ... SELECT` and `INSERT ... SELECT ... ON DUPLICATE KEY UPDATE` into an `AUTO_INCREMENT` table | refused; what a colliding row spends and reports beside a `SELECT` has not been measured |
| `INSERT ... SELECT` into an `AUTO_INCREMENT` table in a session whose time zone is not UTC | refused; the copy path does not shift `TIMESTAMP` values between zones |
| `INSERT ... SELECT` into an `AUTO_INCREMENT` table whose batch of numbers would pass the column's highest | refused; measured, MySQL cuts the last batch short there and writes the rows that fit |
| `INSERT IGNORE` coercing a value MySQL would clamp | refused instead; needs the coercion `INSERT` does not have either |
| `INSERT ... SELECT` without a column list, carrying `IGNORE` or an upsert clause | refused; those forms are refused wherever they are written |
| `INSERT ... SELECT ... ON DUPLICATE KEY UPDATE` | refused; the copy is rendered with no upsert clause, and what a colliding copied row updates and reports has not been measured |
| `INSERT ... SELECT` whose `SELECT` needs its column types (a bound value, an `ORDER BY` over a column) and has a `GROUP_CONCAT` | refused; that `SELECT` is rendered as a bare one, which warns about a cut where MySQL fails a statement writing the cut value with 1260 |
| `UPDATE` / `DELETE` over more than one table | refused |
| `LIMIT` with no `ORDER BY`, or an `ORDER BY` over a column that is not an integer, on an `UPDATE` / `DELETE` | refused |
| A counted row failing a `CHECK`, `NOT NULL` or a value check, written by a path that reserves its numbers before it writes — `INSERT ... SELECT`, several rows with `IGNORE` or `ON DUPLICATE KEY UPDATE`, rows naming their own ids, a table carrying a trigger — or a single row `IGNORE` skips for such a failure | spends the numbers reserved for it; measured on 8.4.11, MySQL takes a row's number only once the row is filled, so a failing first row spends none and `INSERT ... SELECT` spends only the batches the rows before the failing one took. Only a `VALUES` insert whose every row leaves its id to the counter, into a table with no trigger, writes before it takes its numbers |
| The first of several counted rows failing a value check — too long, out of range, of the wrong kind | spends the statement's batch, where MySQL spends none; the engine ends the whole transaction on such a value, so the first row cannot be tried on its own to tell which row failed. A `CHECK` or `NOT NULL` failure is told apart |
| A value the assignment check refuses — too long, out of range, of the wrong kind — inside an open transaction | the engine rolls the whole transaction back, rows written before it included, where MySQL fails only the statement |
| A trigger naming the id it writes into a counted table, or writing a `BIGINT UNSIGNED` counted table | refused; the engine lets the counter choose only a trigger row's row number, and a `BIGINT UNSIGNED` id is a column of its own. MySQL takes the first and moves the counter past the written id |
| A statement whose triggers would write a table it writes or reads — a trigger writing its own table, an `INSERT ... SELECT` reading a table a trigger down the line writes | refused before anything is written, where MySQL answers 1442 — measured on 8.4.11, only once the trigger runs, so it has spent the statement's first number and set `LAST_INSERT_ID()` to it by then |
| A row a trigger writes into a counted table that breaks `NOT NULL` or a `CHECK` | the statement fails as in MySQL, but the row has spent that table's next number; measured on 8.4.11, MySQL checks the row before it takes one, while the engine numbers a row before it checks it |
| Rows asking for the next id beside one naming its own id past the counter, into a table whose trigger writes a counted table | refused; that statement holds the counter's file while it writes its rows one at a time, so the trigger could not take a number |
| A `BEFORE INSERT` trigger | refused by `CREATE TRIGGER`, which takes only `AFTER INSERT`; measured on 8.4.11, one reads 0 for `NEW.id` of a row asking for the next number, the number not being taken yet |
| `TRUNCATE TABLE` on a counted table carrying a trigger | refused; that table is written again to restart its counter and a trigger is not the table's own row, where MySQL leaves one where it stood. A table with no counter is emptied in place and keeps its triggers |
| `SET foreign_key_checks` to a value that is neither 0, 1, `OFF` nor `ON` | refused as a syntax error where MySQL answers 1231 |

---

## Transactions and locking

| Feature | State |
|---|---|
| `BEGIN` / `START TRANSACTION`, `COMMIT`, `ROLLBACK` | works |
| `SET autocommit = 0 \| 1` | works |
| A value a column refuses inside a transaction — 1406, 1264, 1366, 1292, 1265, 3140 | works; the statement fails and is undone, and the transaction, its earlier rows and its savepoints stay, as MySQL's do |
| `LOCK TABLES` / `UNLOCK TABLES` | works, and the lock is held until the unlock. One lock over the whole database rather than one for each table, so it locks more than was asked for — see COMPAT.md |
| Touching a table `LOCK TABLES` did not name | taken; MySQL answers 1100 and holds the session to the tables it locked, where one lock over everything has no reason to |
| `START TRANSACTION` / `COMMIT` / `ROLLBACK` while tables are locked | refused; the lock is held by the transaction they would end, and MySQL keeps the two apart |
| `LOCK TABLES ... READ LOCAL` and `LOW_PRIORITY WRITE` | refused; the first lets other sessions insert while the lock is held and the second changes who waits for whom, and one write lock answers neither |
| A savepoint over an in-memory database | taken and does nothing; the engine needs the pager's sub-journal to undo anything, and over an in-memory database `SAVEPOINT` and `ROLLBACK TO` both answer OK without rolling back. The server runs over files, where it works |
| A savepoint name that is a reserved word — `SAVEPOINT select` | taken; MySQL answers 1064 unless it is quoted |
| `SET TRANSACTION ISOLATION LEVEL` naming `READ UNCOMMITTED` or `SERIALIZABLE`, or `GLOBAL` | refused; `READ COMMITTED` and `REPEATABLE READ` are kept, and saying yes to another would be a guarantee this does not keep |
| `SELECT ... FOR UPDATE` / `FOR SHARE` / `LOCK IN SHARE MODE` | works, and the lock is held. A session kept out by it waits and answers 1205, the way MySQL's does. One lock over the whole database rather than one for each row, so it is stronger than MySQL's — see COMPAT.md |
| `SET innodb_lock_wait_timeout` | works, one to 1073741824 seconds, and the session starts at MySQL's fifty |
| `FOR UPDATE NOWAIT`, `OF <table>` | refused; each asks what to do about a lock on some rows, and there is one lock over the whole database. `SKIP LOCKED` is taken and waits for that lock where MySQL would skip the rows another session holds |
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
| An `information_schema` query with no `TABLE_SCHEMA` | answers the selected database alone, where MySQL answers every database the caller can see |
| A system variable read inside a larger statement — `SELECT @@autocommit + 1`, `SELECT @@autocommit FROM t` | refused as 1235; MySQL reads the variable and answers the row, and an unknown name there is its own 1193 |
| `SHOW WARNINGS`, `SHOW ERRORS` | works |
| `information_schema.PROCESSLIST` | refused; `SHOW [FULL] PROCESSLIST` answers the same rows. Measured, the table's columns are shaped differently from the statement's — `INFO` a `BLOB`, `TIME` NOT NULL without the binary flag — and the asking row reads `executing` |
| `SHOW STATUS` counters other than `Threads_connected`, `Uptime` and `Uptime_since_flush_status` | no row; this server keeps none of MySQL's other 330, as a name MySQL's build leaves out answers no row. `SHOW STATUS WHERE ...` is refused |
| `SHOW COLLATION` / `SHOW CHARACTER SET` with a `WHERE` other than `=` and `LIKE` tests joined by `AND` | refused; the listing is filtered here rather than by a query engine |
| The `NUM` flag on a column worked out rather than read from a table — a written `1.5`, `TRUE`, `NULL`, `COUNT(*)`, `@@group_concat_max_len` | sent, where MySQL does not: measured on the wire on 8.4.11, `1.5` and `COUNT(*)` carry `0x81`, `NULL` `0x80` and `@@group_concat_max_len` `0xa0`. `libmysqlclient` sets the flag itself for every numeric type, so the `mysql` client shows it either way |
| `SET group_concat_max_len` below 4, or read from a user variable | refused; measured, MySQL takes anything below 4 as 4 with warning 1292 (`Truncated incorrect group_concat_max_len value: '0'`) |
| `SET @x = @@group_concat_max_len` past the largest `BIGINT` | refused; a user variable here holds no unsigned number |
| A `GROUP_CONCAT` column of a statement sorting its groups by anything but the grouped columns, or of `SELECT DISTINCT` over groups | reports the shape it has unsorted; measured, MySQL reads it out of a table it sorts through — a `VAR_STRING` with 0 decimals up to 512, past it a `BLOB` of 16 bytes to each (16384 at 1024) with the blob flag — and every other aggregate column of the statement loses its binary flag there too |
| A `GROUP_CONCAT` in a `UNION`, `EXCEPT` or `INTERSECT` branch | refused; it used to be answered with a column of type NULL, and measured, MySQL reports a `VAR_STRING` with 0 decimals up to 512 and a `BLOB` of 65536 with the blob flag at 1024. Each branch counts its cuts for itself: `UNION ALL` of two cut calls warns `Row 3` twice |
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
| `SHOW GRANTS`, `SHOW GRANTS FOR CURRENT_USER[()]` | refused; measured on 8.4.11, one row per grant in a `VAR_STRING` of 4096 named `Grants for app@%` — ``GRANT USAGE ON *.* TO `app`@`%` ``, then each database's privileges in MySQL's order (``GRANT SELECT, INSERT, UPDATE, DELETE, CREATE, DROP, REFERENCES, INDEX, ALTER, LOCK TABLES, CREATE VIEW, SHOW VIEW, TRIGGER ON `probe`.* TO `app`@`%` ``) and each table's — and 1142 for another account's. An account here holds connect, query, create and drop per database and select per table, which name no MySQL privileges one to one: query lets every statement in the database run, where MySQL splits that over a dozen privileges, and MySQL's `CREATE` and `DROP` on `db`.* also create and drop the database itself, which this server grants apart. Answering would write down privileges the account does not hold, or leave out ones it does |
| `CREATE USER`, `GRANT`, `REVOKE` beyond the narrow account slice | `CREATE USER 'name'@'%' IDENTIFIED BY 'password'` and table-scoped `GRANT` / `REVOKE SELECT ON db.table TO` / `FROM 'name'@'%'` use a dedicated account-management privilege, the crash-safe journal, external checkpoint CAS, and runtime reload. Other hosts, database/global grants in SQL, multiple privileges, and account alteration/drop remain refused. The offline provisioner may bootstrap an administrator with `--global-manage-accounts true` |
| Stored procedures, functions, events | refused, and out of scope — see what this frontend is for. Their listings — `SHOW EVENTS`, `SHOW FUNCTION STATUS`, `SHOW PROCEDURE STATUS` — answer no row |
| `SHOW FUNCTION STATUS` / `SHOW PROCEDURE STATUS` with a `WHERE` other than `Db = 'name'`, `SHOW EVENTS` with a `WHERE` | refused; MySQL checks the predicate against the listing's columns — an unknown one is 1054 — before it finds no row |
| Switching databases inside a transaction, a `--single-transaction` dump of several databases among them | refused; the transaction belongs to the first database's connection. Selecting the same database again is taken |
| A `DROP DATABASE` of a database another session holds a transaction open on that has read no table yet — `BEGIN; SELECT 1` | waits for the transaction to end; measured, MySQL's waits only for a transaction that read one of the database's tables |
| A statement reading no table — `SELECT 1`, `BEGIN`, `COMMIT`, an `information_schema` read — in a session whose database another session dropped | answered 1049; measured, MySQL runs it, and answers 1049 only for a statement on the database |
| A prepared statement executed after its database was dropped and made again under the same name | answered 1049; measured, MySQL prepares it again over the new database |
| `BINARY <expr>` outside the six catalog reads of Prisma's schema engine, and `CAST(<expr> AS BINARY)` | refused; measured, `BINARY` binds tighter than a comparison and answers a `VAR_STRING` in the binary character set — 192 wide over an `information_schema` name — compared and ordered by its bytes |
| Prisma's column read over a database holding a view | refused, as every read of a view's columns out of `information_schema.COLUMNS` is; measured, MySQL answers each column of the view, the `id` of a `BIGINT` key with default `0` |
| `SET lock_wait_timeout` | refused; a `DROP DATABASE` waits MySQL's default of a year for the sessions using the database |
| `DROP DATABASE IF EXISTS` | refused by the strict admin parser; measured, MySQL answers OK for a database that is not there, with no note — `@@warning_count` reads 0 after it, where `DROP TABLE IF EXISTS` leaves 1 |
| `SHOW FIELDS` / `DESCRIBE` of a view | refused as an internal error; measured, MySQL types each column from the view's query — `id + 1` reads `bigint` with default `0` — and `mysqldump` reads it to write a view's placeholder |
| `information_schema` beyond `TABLES`, `VIEWS`, `COLUMNS`, `SCHEMATA`, `STATISTICS`, `KEY_COLUMN_USAGE`, `TABLE_CONSTRAINTS`, `REFERENTIAL_CONSTRAINTS`, `CHECK_CONSTRAINTS` and `ROUTINES` | not started; what is left is `COLLATION_CHARACTER_SET_APPLICABILITY` and the server-status tables |
| `information_schema.CHECK_CONSTRAINTS.CHECK_CLAUSE` for an expression beyond a comparison, `IS [NOT] NULL`, `AND`, `OR`, `+`, `-` and `*` over columns, numbers and words | refused when read; how MySQL writes the rest back has not been measured |
| A `CREATE TABLE` giving a `CHECK` a name another table's `CHECK` already has | taken, where MySQL answers 3822, a name being the database's; `ALTER TABLE ... ADD CONSTRAINT` answers 3822 as MySQL does |
| A `CREATE TABLE` writing an unnamed table-level `CHECK` before a column carrying an unnamed `CHECK` of its own | refused; MySQL numbers the two in the order written and the table is stored with its columns first |
| A row a `CHECK` or `NOT NULL` refuses in a table that counts its own ids | uses up the id it was given; measured, MySQL's `CHECK` refusal leaves the counter where it stood |
| `ALTER TABLE ... ADD`/`DROP` a `CHECK` beside any other operation, on a table another table's foreign key names, or on one carrying a trigger | refused; each is a rewrite of the table, which those rule out |
| A `CHECK` written `NOT ENFORCED` | refused; MySQL keeps it without checking, and the engine checks every one it keeps |
| `SELECT *` over `information_schema.TABLES` | refused; it answers only some of MySQL's columns — `TABLES` has storage statistics and times this server does not keep |
| A prepared statement over an `information_schema` table executed after a schema change | refused; the statement is prepared again and that path does not carry the catalog source through |
| A call over an `information_schema` column | refused; its shape has not been measured. A count is taken, since a count does not depend on what the column holds, and so is `CONCAT` over the catalog's words, which TypeORM writes its view drops with, and a `CASE` whose branches are one catalog column or NULL, which Django's `get_table_description` writes. A `CASE` mixing a catalog column with a written value or another column is refused |
| A comparison over an `information_schema` name column — `c.TABLE_NAME = t.TABLE_NAME`, `COLUMN_NAME = 'Id'` | taken under the collation the catalog gives the column, some of them `utf8mb4_0900_ai_ci`, where MySQL compares `TABLE_SCHEMA`, `TABLE_NAME` and the `*_CATALOG` columns as `utf8mb3_bin` and `COLUMN_NAME`, `CONSTRAINT_NAME` and `INDEX_NAME` as `utf8mb3_tolower_ci`, measured on 8.4.11. Two names differing only in case or accent can match here where MySQL keeps them apart |
| `DATABASE()` beside a `FROM` other than an `information_schema` query's, or inside an expression — `SELECT DATABASE() FROM t` | refused; the call is written in as the selected database only where a framework filters `information_schema` on it, and answered from the session in a `SELECT` of session calls and variables alone, prepared or not. The answer lives in the session and the renderer has no way to carry it into a statement. Measured: a `VAR_STRING` of 256, nullable |
| `VERSION()` beside a `FROM` — `SELECT VERSION() FROM t` | refused; the version string lives in the server crate, out of the renderer's reach. `SELECT VERSION()` alone works. Measured: a `VAR_STRING` of 24, NOT NULL |
| `CONNECTION_ID()`, `USER()`, `CURRENT_USER()`, `SESSION_USER()`, `SYSTEM_USER()`, `ROW_COUNT()`, `FOUND_ROWS()`, or a system variable, beside a `FROM` or inside an expression | refused; they are answered from the session in a `SELECT` of such calls and variables alone, prepared or not, and the statement renderer is not handed the session's state. A variable beside a column was answered 1054, the engine reading it as a column it has not got; it is 1235 now |
| `FOUND_ROWS()` after an `UPDATE` or `SHOW ERRORS`, and both counts after a prepared-statement command or `COM_RESET_CONNECTION` | refused until a statement sets them again; MySQL makes `FOUND_ROWS()` the rows an `UPDATE` matched, which is not reported here, and the rest were not measured |
| `SQL_CALC_FOUND_ROWS` in a prepared statement, beside a bound value or a `WITH`, or in a `UNION` | refused; the count reads the statement a second time, and a prepared one's count is not read back. On a text statement's own `SELECT` it is taken |
| The warning 1287 MySQL raises for `FOUND_ROWS()` | not raised; measured, `FOUND_ROWS() is deprecated and will be removed in a future release. Consider using COUNT(*) instead.` |
| `information_schema` columns beyond the eight of `TABLES` this answers | refused; the rest are statistics and timestamps this server does not keep, and answering NULL would be a claim of its own |
| An `information_schema.SCHEMATA` query with no database selected, beyond `SELECT SCHEMA_NAME FROM information_schema.SCHEMATA` | refused; every other shape is answered by a table scanned on the selected database's connection |
| A name that is no column in a statement over more than one table, in `ORDER BY`, `GROUP BY` or `HAVING`, or in a subquery | answered with this server's own refusal (1235) or a 1054 without the name, where MySQL answers 1054 naming the column and the clause |
| A table that is not there named by `TABLE t`, in another database, or by a statement this server's reader of the tables a statement names cannot read | answered with this server's own refusal, where MySQL answers 1146 |
| A result longer than the runtime's write queue — `--max-write-bytes`, or more frames than `--max-write-frames`, which a result near the 4096-row limit reaches | answered 1235 in place of its rows; MySQL writes a result out as it reads it and has no such limit |
| A dump of a `BLOB`, `BINARY` or `VARBINARY` column | refused; `mysqldump` writes such a value as `_binary '...'` with its raw bytes, which may not be UTF-8 and which this server's command reader refuses before any parser sees them, or with `--hex-blob` as `0x...`, which an `INSERT` value here does not take. A quoted word written into a `BLOB` is stored as text rather than as bytes |
| `NOW(6)`, `CURRENT_TIMESTAMP(6)` as a value to insert | refused, as any clock call taking an argument is; the framework apps' `data.sql` writes one, so seeding them here leaves out that row and the `post_tag` rows naming it |
| `COM_STATISTICS`, which the `mysql` client's `status` sends | refused; MySQL answers one line of `Uptime`, `Threads`, `Questions`, `Slow queries`, `Opens`, `Flush tables`, `Open tables` and `Queries per second avg`, and this server counts neither questions, slow queries nor table opens. The client prints the rest of `status` without it |
| A statement several MiB long, such as one row of a dump's extended `INSERT` | taken, but the checked parsers read its text many times over: about 2 s a MiB in a debug build, linear in the length |
| Long data for several parameters of a connection's statements, together over 64 MiB | answered 1105 at `COM_STMT_EXECUTE`; MySQL holds each parameter to `max_allowed_packet` on its own |
| Multi-statement `COM_QUERY` beyond the bounded slice | negotiates `CLIENT_MULTI_STATEMENTS` and returns sequential results with `SERVER_MORE_RESULTS_EXISTS`. More than 32 statements and SQL whose delimiter changes under the session's backslash mode are refused before execution. An assembled response beyond 512 frames or 1 MiB closes the connection after execution, so preceding side effects may remain. A later statement can fail after earlier statements have run, as in MySQL. The runtime's configured write queue can impose a tighter limit |

---

## Session variables

| Variable | State |
|---|---|
| `@@version`, `@@version_comment`, `VERSION()` | works |
| `@@max_allowed_packet`, `@@wait_timeout`, `@@sql_notes` | works; `SET [SESSION] max_allowed_packet` answers 1621, as MySQL does |
| The variables a driver reads before it sends any work — the `@@character_set_*` and `@@collation_*` names, `@@time_zone`, `@@system_time_zone`, `@@transaction_isolation`, `@@auto_increment_increment`, `@@auto_increment_offset`, `@@interactive_timeout`, `@@performance_schema`, `@@lower_case_table_names`, `@@init_connect`, `@@license` | works; each answers what this server decides for itself, and three read differently from MySQL's own — see COMPAT.md |
| `SET NAMES`, `SET sql_mode`, `SET information_schema_stats_expiry` | taken when they name the state the server is already in; `sql_mode` may be an expression over `@@sql_mode`, `NO_AUTO_VALUE_ON_ZERO` is kept, and `TRADITIONAL` is taken, standing for modes of that list alone. `@@sql_mode` always reads back `ONLY_FULL_GROUP_BY`, which this server keeps whatever the session names |
| `SET wait_timeout`, `SET sql_auto_is_null = 0`, `SET sql_safe_updates = 0`, several assignments in one `SET` | works |
| A `SET` of a `GLOBAL` variable, `sql_auto_is_null = 1` or `sql_safe_updates = 1` | refused; nothing here can change another session, and neither rule is one this server has |
| `SET wait_timeout` outside one second through a year | refused, where MySQL clamps it with a warning |
| `SET time_zone` | accepts `UTC`, `SYSTEM` and fixed offsets from `-13:59` through `+14:00`; see the TIMESTAMP boundaries below |
| `SET unique_checks` | works; with it off, a duplicate key is refused as MySQL 8.4 refuses one under its default `innodb_change_buffering=none` |
| `SET character_set_client`, `character_set_results` or `collation_connection` to latin1 | taken for what a dump sets around a view: a statement outside ASCII, a result holding anything outside ASCII, and a prepared statement are refused while it is named, and a view made then records utf8mb4 rather than latin1. A result in ASCII is sent, described in latin1 as MySQL describes it |
| Any character set or collation but utf8mb4's three and latin1 | refused, a client's handshake naming one — utf8mb3, which older drivers send for `utf8`, binary, another latin1 collation — with 1235 in place of the final OK; MySQL takes every character set it has there |
| A client's handshake naming latin1 | taken as `SET NAMES latin1`: a result in ASCII — `select @@version_comment limit 1`, which the `mysql` client sends as it starts, among them — is sent, and one holding any other character is refused until the session names utf8mb4. MySQL converts its results to latin1 |
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
| A `BIGINT UNSIGNED`, `DECIMAL` or word column compared with a bound value or a list, or a whole-number column compared with a word naming a number, in a `SELECT` that also reads a subquery, or over several tables where another column of that name holds another kind | refused; such a statement is read without the column types that say to compare the column's stored form by value |
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
| A call over written values on the right of a comparison, other than `LOWER` or `UPPER` of an ASCII word — `YEAR(d) = YEAR('2024-05-05')`, `n = LENGTH('ab')` | refused; the reader takes a call that reads a column, a clock reading, and a written word in another case, and folding the rest into the value they answer is work of its own |
| `(a, b) IN (SELECT ...)` | refused; the row list is written out as `AND` and `OR`, and a subquery has no rows to write out |
| A joined `UPDATE` or `DELETE` whose `ON` equates two columns MySQL converts one of first — `DELETE p FROM p JOIN q ON p.n = q.label` | taken without looking at the two types; a `SELECT` holds the same `ON` to the pairs listed in COMPAT.md and refuses the rest |
| A column compared with another column MySQL converts first — `WHERE n = label`, `WHERE a = b` under two collations, `WHERE dt = d` | refused; measured, MySQL converts one side to the other's type or answers 1267, where the engine compares the two as stored. See COMPAT.md for the pairs taken |
| A join `ON` naming a value in an `UPDATE` or a `DELETE` | refused; each renders its own `FROM` with no statement to record the comparison in, so the value would go unchecked |
| A subquery in a joined `UPDATE`/`DELETE` `WHERE` comparing a qualified column — `WHERE a.id IN (SELECT ...)` | refused; `IN (SELECT ...)` reads only an unqualified column on its left, which a joined statement cannot write |
| A call as a `LIKE` pattern — `LIKE CONCAT('%', ?)` | refused; the pattern is written or bound, and a client that prepares one can build it before it binds |
| An aggregate other than `COUNT` over a qualified column in a subquery, `AVG` over one in a join, `MIN`, `MAX` or `SUM` over a joined column that is not a signed whole number, or `GROUP_CONCAT` over a joined column it does not write the way MySQL does — `AVG(b.id)`, `SUM(u.balance)`, `MAX(u.name)` | refused; each answers its argument's own type, and the engine keeps a `DECIMAL` and a `BIGINT UNSIGNED` in stored forms of their own and compares words by their bytes. A count does not depend on what it counts, so that one is taken; `MIN`, `MAX` and `SUM` over a joined signed whole number and `GROUP_CONCAT` over a joined column are taken, and so is any of them over a statement reading one table, whose qualifier can only name that table |
| An aggregate over a qualified column in a `HAVING` or an `ORDER BY` — `HAVING SUM(u.n) > 1` | refused; the engine reads a bare name there as one of the projection's aliases when no column has it, so the qualifier is not left out |
| `COUNT(NULL)`, `COUNT('x')`, `COUNT(1.5)` | refused; a count of a written whole number is taken, the rest have not been needed |
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
| A JSON reading in a `WHERE` — `meta->>'$.lang' = 'en'`, `json_contains(...)`, `json_length(...) = 2` | works over a `JSON` column of one table in a `SELECT`, `UPDATE` or `DELETE`; see COMPAT.md. Refused: a reading of a text column, which MySQL parses as a document first; a column qualified in an `UPDATE` or a `DELETE` (a `SELECT` reading one table takes its own qualifier), a join, a subquery or a view; `LIKE`, `BETWEEN` and an explicit `COLLATE` over unquoted text, whose collation rules were not measured there — Django's text lookups under a key (`LOWER((col ->> path)) LIKE LOWER('%x%')` for `icontains`, `(col ->> path) LIKE BINARY 'x%'` for `startswith`) and SQLAlchemy's `CASE ... END LIKE 'x%'` among them; an `IN` list with a bound member; `->` or `JSON_EXTRACT` compared with a written boolean — measured, `doc->'$.a' = TRUE` finds the JSON `true` where TRUE is otherwise 1 — with a fraction, or with a `?` other than through `=` or `<>`; a bound double; a document written through `JSON_EXTRACT('<document>', '$')` other than a word or a whole number compared by anything but `=`; a `?` in a prepared `UPDATE` or `DELETE`; a word bound after a number in the same prepared statement, which MySQL then reads as a number; and paths with wildcards, `last`, ranges, spaces, a bare name MySQL refuses or a quoted name holding a quote or a backslash |
| SQLAlchemy's `as_numeric(p, s)` — `CASE JSON_EXTRACT(col, path) WHEN 'null' THEN NULL ELSE CAST(JSON_EXTRACT(col, path) AS DECIMAL(p, s)) END` — and `as_boolean()` — `CASE JSON_EXTRACT(...) WHEN 'null' THEN NULL WHEN true THEN true ELSE false END` | refused. Measured on 8.4.11: the decimal clamps what does not fit to 99999999.99 with a warning and rounds the decimal a double is written as (1.005 is 1.01, 2.675 2.68), and reads a string as the number it spells; the boolean finds both `true` and `1`, reports a LONGLONG of 1, and answers 0 for a NULL document, which falls through to its `ELSE`. Each needs a dialect reading of its own. `as_integer()` and `as_float()` refuse, when they read one, the values MySQL reads only with a warning |
| `ORDER BY` a JSON reading — Django's `order_by('profile__city')`, `ORDER BY JSON_EXTRACT(col, path)` — or by SQLAlchemy's `CASE` | refused; MySQL orders JSON values by their kind first — measured on 8.4.11, the JSON null before every string — which has not been held to the engine's ordering |
| A `WHERE` comparison against a `BLOB` column | refused; MySQL compares bytes, and the checked comparison validator currently accepts written text only against declared text columns |
| An `ENUM` or `SET` member spelled any way but the way it was declared — `state = 'ACTIVE'` | refused; MySQL's collation ignores case and finds the row, and comparing the stored spelling against that text would find nothing. Asking the engine for the collation here needs the second rendering pass an `ORDER BY` over one already takes |
| An ordering comparison against an `ENUM` or `SET` column — `state > 'active'` | refused; MySQL reads an `ENUM` by the position its members were declared in, which is not the order their words read in |
| A number compared against an `ENUM` or `SET` column — `state = 2` | refused; MySQL reads it as a member's position and the stored value is the word |
| A `WHERE` comparison against a number written with a fraction and a text column — `label > 1.5` | refused; MySQL reads the text as a number, which is the coercion a string against an integer column is refused for |
| A word against a column holding numbers that is not a whole number spelled out — `age = '1.5'`, `' 30'`, `'3e1'`, `''`, `'30abc'` — or any word against a `DOUBLE`, or one past an `i64` | refused; measured, MySQL reads `'1.5'` against an `INT` as a double, the others without a warning or with warning 1292, and this reads only the words it can spell out as the one number MySQL reads. `id = '1'`, `IN ('1', '2')`, `BETWEEN`, a `DECIMAL` against `'10.5'`, a word bound against a whole-number column and `YEAR(col) = '2026'` are taken |
| A word bound against a call — `YEAR(created_at) = ?` | refused; a bound value against a call is refused whatever it binds |
| `CAST(... AS JSON)` anywhere but whole as the value an `INSERT` or `UPDATE` writes into a `JSON` column, over anything but a word or a `?`, or binding a double | refused; what MySQL writes into another kind of column, and how it spells a double as JSON, were not measured. A word that is no document is refused where MySQL answers 3141 |
| A `HAVING` counted against a number written with a fraction — `HAVING COUNT(*) > 1.5` | refused; a count is a whole number |
| A `HAVING` comparing a word with an aggregate other than a count, or a count with a word that is not only a whole number's digits — `HAVING SUM(n) > '1'`, `HAVING COUNT(*) > '1abc'` | refused; measured, MySQL compares them as doubles and warns 1292 for a word it cuts. A count against a word naming a whole number is taken |
| A word bound against a `COUNT` in a `HAVING` that names no whole number — `HAVING COUNT(*) > ?` binding `'abc'` or `'1.5'` — or a `?` against any other aggregate | refused; MySQL reads the word as a whole number by its own conversion, `'abc'` as 0 with warning 1292, and a bound value against `SUM` or `MAX` meets the column's type, which was not measured. A bound whole number, a word naming one, a double and NULL against a `COUNT` are taken |
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

- A transaction begun before any database is selected — how `mysqldump
  --single-transaction` opens — takes its read view when a database is
  selected, not at `START TRANSACTION`, so it sees a row committed between the
  two
- `SET net_read_timeout` is kept and read back but bounds nothing: the rest of
  a command a client has begun sending is waited for until the idle deadline,
  where MySQL gives up after `net_read_timeout`
- Legacy `DECIMAL` tables that stored binary64 values cannot recover their original
  decimal digits. Re-import them into a new exact `DECIMAL` table; opening an old
  table through the MySQL frontend fails with a migration error
- Legacy tables with a foreign key but no child index must be rebuilt or
  re-imported; opening them through the MySQL frontend fails with a migration
  error
- `TIMESTAMP` converts explicit `INSERT ... VALUES` input and direct column
  output for fixed-offset sessions. Wider non-UTC queries and writes are
  refused as listed above
- A `REPEATABLE READ` transaction that writes after another session committed
  since its first read is rolled back with 1213, where MySQL writes and keeps
  reading the old snapshot for the rows it did not touch
- `CURRENT_TIMESTAMP(n)` as a default or on update, `NOW(n)` and its
  spellings as a result column or a value an `INSERT` writes, and
  `CURTIME(n)` as a result column, read the engine's clock, which stops at
  the millisecond, so digits past the third are zeros
- `NOW()` written into a `DATE` keeps the day without MySQL's note 1292 about
  the discarded time
- complex compound projections still have conservative nullable metadata
- complex window projections still have conservative source-column metadata
- `SHOW FULL COLUMNS` derives `Privileges` from database or table grants;
  column-specific grants are not supported
- an `INSERT ... SELECT` failing on a cut `GROUP_CONCAT` answers 1260 with a
  fixed message, where MySQL's names the row

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
