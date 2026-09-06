//! Tests for the checked MySQL parser.
//!
//! Kept in their own file because the crate root is long enough without
//! them; they still reach the parser's private items through `super`.

use super::*;
use turso_parser::ast::{AlterTable as TursoAlterTable, AlterTableBody as TursoAlterTableBody};

#[test]
fn translates_the_checked_sqlite_subset() {
    let translated = parse_create_table(
        "CREATE TABLE app.users (id INTEGER NOT NULL UNIQUE, name TEXT NOT NULL UNIQUE DEFAULT 'guest', data BLOB, CHECK (id >= 0), FOREIGN KEY (id) REFERENCES accounts (id) ON DELETE CASCADE)",
        SessionSqlMode::default(),
    )
    .unwrap();

    assert_eq!(
        translated.as_sql(),
        "CREATE TABLE \"app\".\"users\" (\"id\" INTEGER NOT NULL UNIQUE, \"name\" TEXT NOT NULL UNIQUE DEFAULT 'guest', \"data\" BLOB, CHECK (id >= 0), FOREIGN KEY (\"id\") REFERENCES \"accounts\" (\"id\") ON DELETE CASCADE)"
    );
}

#[test]
fn accepts_ansi_quoted_identifiers_only_when_enabled() {
    let sql = "CREATE TABLE \"users\" (\"id\" INTEGER)";
    let translated = parse_create_table(
        sql,
        SessionSqlMode {
            ansi_quotes: true,
            no_backslash_escapes: false,
        },
    )
    .unwrap();

    assert_eq!(
        translated.as_sql(),
        "CREATE TABLE \"users\" (\"id\" INTEGER)"
    );
    assert!(parse_create_table(sql, SessionSqlMode::default()).is_err());
}

#[test]
fn no_backslash_escapes_preserves_default_string_bytes() {
    let sql = r"CREATE TABLE t (value TEXT DEFAULT 'a\nb')";
    let translated = parse_create_table(
        sql,
        SessionSqlMode {
            ansi_quotes: false,
            no_backslash_escapes: true,
        },
    )
    .unwrap();

    assert_eq!(
        translated.as_sql(),
        r#"CREATE TABLE "t" ("value" TEXT DEFAULT 'a\nb')"#
    );
}

#[test]
fn signed_integer_defaults_are_normalized_with_i64_bounds() {
    for (sql, normalized, rendered) in [
        (
            "CREATE TABLE t (value INT DEFAULT -1)",
            "CREATE TABLE \"t\" (\"value\" INT DEFAULT -1)",
            "CREATE TABLE `t` (`value` INT DEFAULT -1)",
        ),
        (
            "CREATE TABLE t (value INT DEFAULT +1)",
            "CREATE TABLE \"t\" (\"value\" INT DEFAULT +1)",
            "CREATE TABLE `t` (`value` INT DEFAULT +1)",
        ),
        (
            "CREATE TABLE t (value BIGINT DEFAULT -9223372036854775808)",
            "CREATE TABLE \"t\" (\"value\" BIGINT DEFAULT -9223372036854775808)",
            "CREATE TABLE `t` (`value` BIGINT DEFAULT -9223372036854775808)",
        ),
        (
            "CREATE TABLE t (value BIGINT DEFAULT 9223372036854775807)",
            "CREATE TABLE \"t\" (\"value\" BIGINT DEFAULT 9223372036854775807)",
            "CREATE TABLE `t` (`value` BIGINT DEFAULT 9223372036854775807)",
        ),
    ] {
        assert_eq!(
            parse_create_table(sql, SessionSqlMode::default())
                .unwrap()
                .as_sql(),
            normalized
        );
        let statement = parse_create_table_ast(sql, SessionSqlMode::default()).unwrap();
        assert_eq!(
            render_create_table_mysql_with_mode(&statement, SessionSqlMode::default()).unwrap(),
            rendered
        );
    }

    for sql in [
        "CREATE TABLE t (value BIGINT DEFAULT -9223372036854775809)",
        "CREATE TABLE t (value BIGINT DEFAULT +9223372036854775808)",
        "CREATE TABLE t (value BIGINT DEFAULT -1.0)",
    ] {
        assert!(matches!(
            parse_create_table(sql, SessionSqlMode::default()),
            Err(ParseError::Unsupported { .. })
        ));
    }
}

#[test]
fn rejects_unrecognized_backslash_escapes_when_enabled() {
    for escape in ["a", "f"] {
        let sql = format!(r"CREATE TABLE t (value TEXT DEFAULT '\{escape}')");
        assert!(matches!(
            parse_create_table(&sql, SessionSqlMode::default()),
            Err(ParseError::Unsupported { .. })
        ));
        let auto_increment_sql = format!(
            r"CREATE TABLE t (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, value TEXT DEFAULT '\{escape}')"
        );
        assert!(matches!(
            parse_auto_increment_create_table(&auto_increment_sql, SessionSqlMode::default()),
            Err(ParseError::Unsupported { .. })
        ));
        let no_backslash_escapes = SessionSqlMode {
            ansi_quotes: false,
            no_backslash_escapes: true,
        };
        assert!(parse_create_table(&sql, no_backslash_escapes).is_ok());
    }
}

#[test]
fn parses_a_checked_create_table_into_a_turso_statement() {
    let statement = parse_create_table_ast(
        "CREATE TABLE app.users (id INTEGER NOT NULL UNIQUE, name TEXT NOT NULL UNIQUE DEFAULT 'guest', data BLOB, CHECK (id >= 0), FOREIGN KEY (id) REFERENCES accounts (id) ON DELETE CASCADE)",
        SessionSqlMode::default(),
    )
    .unwrap();

    assert!(matches!(statement, Stmt::CreateTable { .. }));
}

#[test]
fn renders_a_checked_turso_ast_as_normalized_mysql() {
    let statement = parse_create_table_ast(
        "CREATE TABLE app.users (id INTEGER NOT NULL UNIQUE, name TEXT NOT NULL UNIQUE DEFAULT 'guest', data BLOB, CHECK (id >= 0), FOREIGN KEY (id) REFERENCES accounts (id) ON DELETE CASCADE)",
        SessionSqlMode::default(),
    )
    .unwrap();

    let mysql = render_create_table_mysql(&statement).unwrap();
    assert_eq!(
        mysql,
        "CREATE TABLE `app`.`users` (`id` INTEGER NOT NULL UNIQUE, `name` TEXT NOT NULL UNIQUE DEFAULT 'guest', `data` BLOB, CHECK (`id` >= 0), FOREIGN KEY (`id`) REFERENCES `accounts` (`id`) ON DELETE CASCADE)"
    );
    let reparsed = parse_create_table_ast(&mysql, SessionSqlMode::default()).unwrap();
    assert_eq!(render_create_table_mysql(&reparsed).unwrap(), mysql);
}

#[test]
fn renderer_preserves_trailing_backslash_under_both_string_modes() {
    let cases = [
        (
            SessionSqlMode::default(),
            r"CREATE TABLE t (v TEXT DEFAULT '\\')",
        ),
        (
            SessionSqlMode {
                ansi_quotes: false,
                no_backslash_escapes: true,
            },
            r"CREATE TABLE t (v TEXT DEFAULT '\')",
        ),
    ];

    for (mode, sql) in cases {
        let statement = parse_create_table_ast(sql, mode).unwrap();
        let rendered = render_create_table_mysql_with_mode(&statement, mode).unwrap();
        let reparsed = parse_create_table_ast(&rendered, mode).unwrap();
        assert_eq!(
            render_create_table_mysql_with_mode(&reparsed, mode).unwrap(),
            rendered
        );
    }
}

#[test]
fn renderer_rejects_sqlite_ast_fields_outside_the_checked_subset() {
    for sql in [
        "CREATE TABLE t (id INTEGER PRIMARY KEY)",
        "CREATE TABLE t (value REAL)",
        "CREATE TABLE t (id INTEGER, CHECK (id LIKE 'x'))",
    ] {
        let statement = parse_sqlite_create_table(sql);
        assert!(
            matches!(
                render_create_table_mysql(&statement),
                Err(ParseError::Unsupported { .. })
            ),
            "expected unsupported error for {sql}"
        );
    }
}

#[test]
fn rejects_multiple_or_non_create_statements() {
    assert_eq!(
        parse_create_table(
            "CREATE TABLE t (id INTEGER); SELECT 1",
            SessionSqlMode::default()
        ),
        Err(ParseError::ExpectedOneStatement { actual: 2 })
    );
    assert_eq!(
        parse_create_table("SELECT 1", SessionSqlMode::default()),
        Err(ParseError::ExpectedCreateTable)
    );
}

#[test]
fn translates_a_conservative_select_subset() {
    let translated = parse_select(
        "SELECT u.`name` AS `display name`, ? AS marker FROM `users` u WHERE u.`name` IS NOT NULL AND TRUE",
        SessionSqlMode::default(),
    )
    .unwrap();

    assert_eq!(
        translated.as_sql(),
        "SELECT \"u\".\"name\" AS \"display name\", ? AS \"marker\" FROM \"users\" AS \"u\" WHERE ((\"u\".\"name\" IS NOT NULL) AND TRUE)"
    );
    assert!(translated.reads_table());
    assert_eq!(translated.source_table(), Some("users"));
    assert!(matches!(
        parse_select_ast(
            translated.as_sql(),
            SessionSqlMode {
                ansi_quotes: true,
                no_backslash_escapes: false
            }
        )
        .unwrap(),
        Stmt::Select(_)
    ));
}

#[test]
fn records_select_comparison_requirements_in_parameter_order() {
    let translated = parse_select(
        "SELECT ?, id FROM users WHERE ? IS NULL AND NOT (id = ?) OR id = NULL",
        SessionSqlMode::default(),
    )
    .unwrap();

    assert_eq!(translated.parameter_count(), 3);
    assert_eq!(translated.checked_comparisons().len(), 2);
    assert_eq!(translated.checked_comparisons()[0].column_name(), "id");
    assert_eq!(
        translated.checked_comparisons()[0].operator(),
        CheckedSelectComparisonOperator::Equal
    );
    assert_eq!(
        translated.checked_comparisons()[0].rhs(),
        &CheckedSelectComparisonRhs::Placeholder { ordinal: 2 }
    );
    assert_eq!(translated.checked_comparisons()[1].column_name(), "id");
    assert_eq!(
        translated.checked_comparisons()[1].rhs(),
        &CheckedSelectComparisonRhs::Null
    );
    assert_eq!(
        translated.as_sql(),
        "SELECT ?, \"id\" FROM \"users\" WHERE (((? IS NULL) AND (NOT ((\"id\" = ?)))) OR (\"id\" = NULL))"
    );
}

#[test]
fn accepts_exact_signed_integer_comparison_without_column_width_limits() {
    for (sql, expected, rendered_rhs) in [
        (
            "SELECT id FROM users WHERE id = 1000",
            CheckedSelectComparisonRhs::SignedInteger(1000),
            "1000",
        ),
        (
            "SELECT id FROM users WHERE id = 0001",
            CheckedSelectComparisonRhs::SignedInteger(1),
            "1",
        ),
        (
            "SELECT id FROM users WHERE id = -0001",
            CheckedSelectComparisonRhs::SignedInteger(-1),
            "(-1)",
        ),
        (
            "SELECT id FROM users WHERE id = 0000",
            CheckedSelectComparisonRhs::SignedInteger(0),
            "0",
        ),
        (
            "SELECT id FROM users WHERE id = -9223372036854775808",
            CheckedSelectComparisonRhs::SignedInteger(i64::MIN),
            "(-9223372036854775808)",
        ),
        (
            "SELECT id FROM users WHERE id = 9223372036854775807",
            CheckedSelectComparisonRhs::SignedInteger(i64::MAX),
            "9223372036854775807",
        ),
    ] {
        let translated = parse_select(sql, SessionSqlMode::default()).unwrap();
        assert_eq!(
            translated.checked_comparisons()[0].rhs(),
            &expected,
            "{sql}"
        );
        assert_eq!(
            translated.as_sql(),
            format!("SELECT \"id\" FROM \"users\" WHERE (\"id\" = {rendered_rhs})")
        );
    }
}

#[test]
fn accepts_all_strict_signed_integer_comparison_operators() {
    for (sql, operator, normalized) in [
        (
            "SELECT id FROM users WHERE id = 7",
            CheckedSelectComparisonOperator::Equal,
            "= 7",
        ),
        (
            "SELECT id FROM users WHERE id <> 7",
            CheckedSelectComparisonOperator::NotEqual,
            "<> 7",
        ),
        (
            "SELECT id FROM users WHERE id != 7",
            CheckedSelectComparisonOperator::NotEqual,
            "<> 7",
        ),
        (
            "SELECT id FROM users WHERE id < 7",
            CheckedSelectComparisonOperator::LessThan,
            "< 7",
        ),
        (
            "SELECT id FROM users WHERE id <= 7",
            CheckedSelectComparisonOperator::LessThanOrEqual,
            "<= 7",
        ),
        (
            "SELECT id FROM users WHERE id > 7",
            CheckedSelectComparisonOperator::GreaterThan,
            "> 7",
        ),
        (
            "SELECT id FROM users WHERE id >= 7",
            CheckedSelectComparisonOperator::GreaterThanOrEqual,
            ">= 7",
        ),
    ] {
        let translated = parse_select(sql, SessionSqlMode::default()).unwrap();
        assert_eq!(translated.checked_comparisons().len(), 1, "{sql}");
        assert_eq!(
            translated.checked_comparisons()[0].operator(),
            operator,
            "{sql}"
        );
        assert_eq!(
            translated.as_sql(),
            format!("SELECT \"id\" FROM \"users\" WHERE (\"id\" {normalized})"),
            "{sql}"
        );
    }
}

#[test]
fn preserves_comparison_three_valued_logic_and_parameter_order() {
    let translated = parse_select(
        "SELECT ?, id FROM users WHERE (? IS NULL AND id < ?) OR NOT (id >= NULL)",
        SessionSqlMode::default(),
    )
    .unwrap();

    assert_eq!(translated.parameter_count(), 3);
    assert_eq!(translated.checked_comparisons().len(), 2);
    assert_eq!(
        translated.checked_comparisons()[0].operator(),
        CheckedSelectComparisonOperator::LessThan
    );
    assert_eq!(
        translated.checked_comparisons()[0].rhs(),
        &CheckedSelectComparisonRhs::Placeholder { ordinal: 2 }
    );
    assert_eq!(
        translated.checked_comparisons()[1].operator(),
        CheckedSelectComparisonOperator::GreaterThanOrEqual
    );
    assert_eq!(
        translated.checked_comparisons()[1].rhs(),
        &CheckedSelectComparisonRhs::Null
    );
    assert_eq!(
        translated.as_sql(),
        "SELECT ?, \"id\" FROM \"users\" WHERE ((((? IS NULL) AND (\"id\" < ?))) OR (NOT ((\"id\" >= NULL))))"
    );
}

#[test]
fn between_is_rendered_as_checked_bounds() {
    let translated = parse_select(
        "SELECT id FROM users WHERE id BETWEEN 10 AND 20",
        SessionSqlMode::default(),
    )
    .unwrap();

    assert_eq!(
        translated.as_sql(),
        "SELECT \"id\" FROM \"users\" WHERE ((\"id\" >= 10) AND (\"id\" <= 20))"
    );
    assert_eq!(translated.checked_comparisons().len(), 2);
    assert_eq!(
        translated.checked_comparisons()[0].operator(),
        CheckedSelectComparisonOperator::GreaterThanOrEqual
    );
    assert_eq!(
        translated.checked_comparisons()[1].operator(),
        CheckedSelectComparisonOperator::LessThanOrEqual
    );

    let not_between = parse_select(
        "SELECT id FROM users WHERE id NOT BETWEEN ? AND ?",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert_eq!(
        not_between.as_sql(),
        "SELECT \"id\" FROM \"users\" WHERE (NOT ((\"id\" >= ?) AND (\"id\" <= ?)))"
    );
    assert_eq!(not_between.parameter_count(), 2);
}

#[test]
fn a_text_comparison_asks_the_engine_for_a_case_insensitive_collation() {
    // MySQL's default collation ignores case, so the rendered SQL says so.
    // Every other comparison is left alone, because a collation the index
    // does not carry stops the planner from using it.
    let translated = parse_select(
        "SELECT id FROM users WHERE name = 'a''b' AND id = 1",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert_eq!(
        translated.as_sql(),
        "SELECT \"id\" FROM \"users\" WHERE ((\"name\" COLLATE NOCASE = 'a''b') AND (\"id\" = 1))"
    );
    assert_eq!(
        translated.checked_comparisons()[0].rhs(),
        &CheckedSelectComparisonRhs::Text("a'b".to_string())
    );
}

#[test]
fn a_scalar_call_renders_as_the_engine_spells_it() {
    for (sql, rendered) in [
        (
            "SELECT LOWER(v) FROM s",
            "SELECT lower(\"v\") AS \"LOWER(v)\" FROM \"s\"",
        ),
        (
            "SELECT UPPER(v) FROM s",
            "SELECT upper(\"v\") AS \"UPPER(v)\" FROM \"s\"",
        ),
        // MySQL's LENGTH counts bytes and its CHAR_LENGTH counts
        // characters, which the engine spells the other way round.
        (
            "SELECT LENGTH(v) FROM s",
            "SELECT octet_length(\"v\") AS \"LENGTH(v)\" FROM \"s\"",
        ),
        (
            "SELECT CHAR_LENGTH(v) FROM s",
            "SELECT length(\"v\") AS \"CHAR_LENGTH(v)\" FROM \"s\"",
        ),
        (
            "SELECT NOW() FROM s",
            "SELECT datetime('now') AS \"NOW()\" FROM \"s\"",
        ),
        (
            "SELECT lower(v) AS folded FROM s",
            "SELECT lower(\"v\") AS \"folded\" FROM \"s\"",
        ),
        (
            "SELECT ABS(n) FROM s",
            "SELECT abs(\"n\") AS \"ABS(n)\" FROM \"s\"",
        ),
        // The engine answers ROUND as a float where MySQL answers a whole
        // number, and a float where a column promised an integer reads as
        // an overflow.
        (
            "SELECT ROUND(n) FROM s",
            "SELECT CAST(round(\"n\") AS INTEGER) AS \"ROUND(n)\" FROM \"s\"",
        ),
        (
            "SELECT IFNULL(n, 0) FROM s",
            "SELECT ifnull(\"n\", 0) AS \"IFNULL(n, 0)\" FROM \"s\"",
        ),
        (
            "SELECT COALESCE(n, 1) FROM s",
            "SELECT coalesce(\"n\", 1) AS \"COALESCE(n, 1)\" FROM \"s\"",
        ),
        // The engine's own `concat` skips a NULL argument where MySQL
        // answers NULL for the whole call; `||` is the operator that
        // agrees.
        (
            "SELECT CONCAT(v, 'z') FROM s",
            "SELECT (\"v\" || 'z') AS \"CONCAT(v, 'z')\" FROM \"s\"",
        ),
        (
            "SELECT LEFT(v, 2) FROM s",
            "SELECT substr(\"v\", 1, 2) AS \"LEFT(v, 2)\" FROM \"s\"",
        ),
        (
            "SELECT RIGHT(v, 2) FROM s",
            "SELECT substr(\"v\", -2) AS \"RIGHT(v, 2)\" FROM \"s\"",
        ),
        (
            "SELECT REPLACE(v, 'b', 'XY') FROM s",
            "SELECT replace(\"v\", 'b', 'XY') AS \"REPLACE(v, 'b', 'XY')\" FROM \"s\"",
        ),
        (
            "SELECT REVERSE(v) FROM s",
            "SELECT string_reverse(\"v\") AS \"REVERSE(v)\" FROM \"s\"",
        ),
        (
            "SELECT REPEAT(v, 3) FROM s",
            "SELECT repeat(\"v\", 3) AS \"REPEAT(v, 3)\" FROM \"s\"",
        ),
        (
            "SELECT LPAD(v, 6, '*') FROM s",
            "SELECT lpad(\"v\", 6, '*') AS \"LPAD(v, 6, '*')\" FROM \"s\"",
        ),
        (
            "SELECT RPAD(v, 6, '*') FROM s",
            "SELECT rpad(\"v\", 6, '*') AS \"RPAD(v, 6, '*')\" FROM \"s\"",
        ),
        (
            "SELECT INSTR(v, 'b') FROM s",
            "SELECT instr(\"v\", 'b') AS \"INSTR(v, 'b')\" FROM \"s\"",
        ),
        (
            "SELECT LOCATE('b', v) FROM s",
            "SELECT instr(\"v\", 'b') AS \"LOCATE('b', v)\" FROM \"s\"",
        ),
        (
            "SELECT HEX(v) FROM s",
            "SELECT hex(\"v\") AS \"HEX(v)\" FROM \"s\"",
        ),
        // MySQL's `IF` is the call spelling of a two-branch `CASE`, which
        // is the shape the engine reads.
        (
            "SELECT IF(n > 1, 'y', 'n') FROM s",
            concat!(
                "SELECT CASE WHEN (\"n\" > 1) THEN 'y' ELSE 'n' END ",
                "AS \"IF(n > 1, 'y', 'n')\" FROM \"s\""
            ),
        ),
        (
            "SELECT CASE WHEN n > 1 THEN 'y' ELSE 'n' END FROM s",
            concat!(
                "SELECT CASE WHEN (\"n\" > 1) THEN 'y' ELSE 'n' END ",
                "AS \"CASE WHEN n > 1 THEN 'y' ELSE 'n' END\" FROM \"s\""
            ),
        ),
        (
            "SELECT SUBSTRING(v, 1, 2) FROM s",
            "SELECT substr(\"v\", 1, 2) AS \"SUBSTRING(v, 1, 2)\" FROM \"s\"",
        ),
        (
            "SELECT SUBSTRING(v FROM 1 FOR 2) FROM s",
            "SELECT substr(\"v\", 1, 2) AS \"SUBSTRING(v FROM 1 FOR 2)\" FROM \"s\"",
        ),
        (
            "SELECT FLOOR(n) FROM s",
            "SELECT CAST(floor(\"n\") AS INTEGER) AS \"FLOOR(n)\" FROM \"s\"",
        ),
        (
            "SELECT CEIL(n) FROM s",
            "SELECT CAST(ceil(\"n\") AS INTEGER) AS \"CEIL(n)\" FROM \"s\"",
        ),
        (
            "SELECT CEILING(n) FROM s",
            "SELECT CAST(ceil(\"n\") AS INTEGER) AS \"CEILING(n)\" FROM \"s\"",
        ),
    ] {
        assert_eq!(
            parse_select(sql, SessionSqlMode::default())
                .unwrap()
                .as_sql(),
            rendered,
            "{sql}"
        );
    }

    for sql in [
        // An expression argument has no length this could work out, and
        // NOW takes none at all.
        "SELECT LOWER(v || 'z') FROM s",
        "SELECT NOW(3) FROM s",
        // TRIM is its own shape in the AST rather than a call, so it waits
        // for its own reading.
        "SELECT TRIM(v) FROM s",
        // A count this cannot read leaves no width to answer with.
        "SELECT LEFT(v, n) FROM s",
        "SELECT SUBSTRING(v, 1, n) FROM s",
        "SELECT SUBSTRING(v, 1) FROM s",
        // Without an ELSE a row matching nothing answers NULL, and a
        // branch that is not a literal has no width.
        "SELECT CASE WHEN n > 1 THEN 'y' END FROM s",
        "SELECT CASE WHEN n > 1 THEN v ELSE 'n' END FROM s",
        // A `CASE col WHEN` compares its operand, which raises the
        // coercion question a WHERE comparison raises.
        "SELECT CASE n WHEN 1 THEN 'y' ELSE 'n' END FROM s",
        // A fallback that can be null defeats the point of IFNULL.
        "SELECT IFNULL(n, v) FROM s",
        "SELECT IFNULL(n, NULL) FROM s",
        // REPLACE requires a column and two string literals.
        "SELECT REPLACE(v, v, 'XY') FROM s",
        "SELECT REPLACE(v, 'b', v) FROM s",
        "SELECT REPLACE('abc', 'b', 'XY') FROM s",
        "SELECT REPLACE(v, 'b') FROM s",
        // REVERSE takes one column argument.
        "SELECT REVERSE('abc') FROM s",
        "SELECT REVERSE(v, 2) FROM s",
        // REPEAT requires a column and a non-negative integer literal count.
        "SELECT REPEAT(v, n) FROM s",
        "SELECT REPEAT(v, -1) FROM s",
        "SELECT REPEAT('abc', 3) FROM s",
        // LPAD / RPAD require a column, a numeric literal length, and a string literal pad.
        "SELECT LPAD(v, n, '*') FROM s",
        "SELECT LPAD(v, 6, v) FROM s",
        "SELECT LPAD('abc', 6, '*') FROM s",
        "SELECT RPAD(v, n, '*') FROM s",
        "SELECT RPAD(v, 6, v) FROM s",
        "SELECT RPAD('abc', 6, '*') FROM s",
        // Everything else stays refused.
    ] {
        assert!(
            parse_select(sql, SessionSqlMode::default()).is_err(),
            "{sql}"
        );
    }
}

#[test]
fn a_cte_names_the_table_its_body_reads() {
    let translated = parse_select(
        "WITH c AS (SELECT n, id FROM f) SELECT c.n, c.id FROM c",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert_eq!(
        translated.as_sql(),
        concat!(
            "WITH \"c\" AS (SELECT \"n\", \"id\" FROM \"f\") ",
            "SELECT \"c\".\"n\", \"c\".\"id\" FROM \"c\""
        )
    );
    // The statement reads `f` under the name `c`, and carries what `c`
    // projected so a result column's ordinal can be resolved through it.
    let [source] = translated.source_tables() else {
        panic!("one table");
    };
    assert_eq!((source.reference(), source.table().as_str()), ("c", "f"));
    assert_eq!(source.projected_columns(), ["n", "id"]);

    for sql in [
        // A wildcard or an expression leaves no name to resolve through.
        "WITH c AS (SELECT * FROM f) SELECT c.id FROM c",
        "WITH c AS (SELECT id + 1 FROM f) SELECT c.id FROM c",
        // Each body reads one table, and RECURSIVE is its own shape.
        "WITH RECURSIVE c AS (SELECT id FROM f) SELECT c.id FROM c",
        "WITH c AS (SELECT f.id FROM f JOIN g ON f.id = g.id) SELECT c.id FROM c",
    ] {
        assert!(
            parse_select(sql, SessionSqlMode::default()).is_err(),
            "{sql}"
        );
    }
}

#[test]
fn a_subquery_is_read_and_its_tables_kept_apart() {
    let translated = parse_select(
        "SELECT id FROM users WHERE id IN (SELECT user_id FROM accounts)",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert_eq!(
        translated.as_sql(),
        "SELECT \"id\" FROM \"users\" WHERE (\"id\" IN (SELECT \"user_id\" FROM \"accounts\"))"
    );
    // The subquery's table is read, and marked so it names none of the
    // result columns; the statement still has one table of its own.
    assert_eq!(
        translated
            .source_tables()
            .iter()
            .map(|source| (source.table().as_str(), source.subquery()))
            .collect::<Vec<_>>(),
        [("users", false), ("accounts", true)]
    );
    assert_eq!(translated.source_table(), Some("users"));
    let [pair] = translated.checked_subquery_comparisons() else {
        panic!("one membership test");
    };
    assert_eq!(
        (
            pair.column_name(),
            pair.inner_table(),
            pair.inner_column_name()
        ),
        ("id", "accounts", "user_id")
    );

    // EXISTS compares nothing, so it records no pair.
    let exists = parse_select(
        "SELECT id FROM users WHERE NOT EXISTS (SELECT id FROM accounts)",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert_eq!(
        exists.as_sql(),
        "SELECT \"id\" FROM \"users\" WHERE (NOT EXISTS (SELECT \"id\" FROM \"accounts\"))"
    );
    assert!(exists.checked_subquery_comparisons().is_empty());

    for sql in [
        // The subquery has to project one column of one table.
        "SELECT id FROM users WHERE id IN (SELECT id, name FROM accounts)",
        "SELECT id FROM users WHERE id IN (SELECT id FROM accounts LIMIT 1)",
        // The left side has to be one unqualified column.
        "SELECT id FROM users WHERE id + 1 IN (SELECT id FROM accounts)",
    ] {
        assert!(
            parse_select(sql, SessionSqlMode::default()).is_err(),
            "{sql}"
        );
    }
}

#[test]
fn a_union_reads_both_branches_and_marks_the_second() {
    let translated = parse_select(
        "SELECT id FROM users UNION ALL SELECT id FROM accounts ORDER BY id LIMIT 2",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert_eq!(
        translated.as_sql(),
        concat!(
            "SELECT \"id\" FROM \"users\" UNION ALL SELECT \"id\" FROM \"accounts\" ",
            "ORDER BY \"id\" ASC LIMIT 2"
        )
    );
    // Both branches are read, and the second is marked as one, which is
    // what tells the result columns they belong to no table.
    assert_eq!(
        translated
            .source_tables()
            .iter()
            .map(|source| (source.table().as_str(), source.branch()))
            .collect::<Vec<_>>(),
        [("users", 0), ("accounts", 1)]
    );
    assert_eq!(translated.source_table(), None);

    // A branch of its own is a nested query, not a plain SELECT.
    let nested = "SELECT id FROM users UNION (SELECT id FROM accounts LIMIT 1)";
    assert!(parse_select(nested, SessionSqlMode::default()).is_err());
}

/// MySQL's EXCEPT and INTERSECT arrived in 8.0.31, and the engine answers them
/// the same way. Measured on MySQL 8.4.11 over (1),(2),(3) against (2),(3),(4):
/// EXCEPT answers 1 and INTERSECT answers 2 and 3, and the engine answers the
/// same for both.
#[test]
fn select_takes_except_and_intersect() {
    for (sql, normalized) in [
        (
            "SELECT id FROM users EXCEPT SELECT id FROM accounts",
            "SELECT \"id\" FROM \"users\" EXCEPT SELECT \"id\" FROM \"accounts\"",
        ),
        (
            "SELECT id FROM users INTERSECT SELECT id FROM accounts",
            "SELECT \"id\" FROM \"users\" INTERSECT SELECT \"id\" FROM \"accounts\"",
        ),
        (
            "SELECT id FROM users INTERSECT SELECT id FROM accounts ORDER BY id LIMIT 2",
            "SELECT \"id\" FROM \"users\" INTERSECT SELECT \"id\" FROM \"accounts\" ORDER BY \"id\" ASC LIMIT 2",
        ),
    ] {
        let translated = parse_select(sql, SessionSqlMode::default()).unwrap();
        assert_eq!(translated.as_sql(), normalized, "{sql}");
        assert!(translated.parse_ast().is_ok(), "{sql}");
        // Both branches are read, and neither names the result columns on its
        // own, so the statement has no single source table.
        assert_eq!(
            translated
                .source_tables()
                .iter()
                .map(|source| (source.table().as_str(), source.branch()))
                .collect::<Vec<_>>(),
            [("users", 0), ("accounts", 1)],
            "{sql}"
        );
        assert_eq!(translated.source_table(), None, "{sql}");
    }
}

/// `EXCEPT ALL` and `INTERSECT ALL` keep duplicates. Measured on MySQL 8.4.11
/// over rows (1), (1), (2) against (2): `EXCEPT` answers one 1 and
/// `EXCEPT ALL` answers two. The engine has no spelling for the second, so it
/// is refused rather than answered with the first's rows.
#[test]
fn select_refuses_the_all_forms_of_except_and_intersect() {
    for sql in [
        "SELECT id FROM users EXCEPT ALL SELECT id FROM accounts",
        "SELECT id FROM users INTERSECT ALL SELECT id FROM accounts",
    ] {
        assert!(
            parse_select(sql, SessionSqlMode::default()).is_err(),
            "{sql}"
        );
    }
}

#[test]
fn show_warnings_is_read_and_its_neighbours_are_not() {
    let mode = SessionSqlMode::default();
    for sql in ["SHOW WARNINGS", "show warnings", "SHOW WARNINGS;"] {
        assert_eq!(
            parse_optional_show_warnings(sql, mode),
            Ok(Some(MySqlShowWarningsCommand::new(0, None))),
            "{sql}"
        );
    }
    for (sql, offset, row_count) in [
        ("SHOW WARNINGS LIMIT 1", 0, Some(1)),
        ("SHOW WARNINGS LIMIT 10, 5", 10, Some(5)),
        ("SHOW WARNINGS LIMIT 5 OFFSET 10", 10, Some(5)),
        ("SHOW WARNINGS LIMIT 0", 0, Some(0)),
    ] {
        assert_eq!(
            parse_optional_show_warnings(sql, mode),
            Ok(Some(MySqlShowWarningsCommand::new(offset, row_count))),
            "{sql}"
        );
    }
    assert!(parse_optional_show_warnings("SHOW WARNINGS LIMIT", mode).is_err());
    assert!(parse_optional_show_warnings("SHOW WARNINGS LIMIT 1,", mode).is_err());
    // Everything else belongs to its own parser, which refuses the ones
    // MySQL takes and this does not.
    for sql in [
        "SHOW COUNT(*) WARNINGS",
        "SHOW ERRORS",
        "SHOW TABLES",
        "SELECT 1",
        "",
    ] {
        assert_eq!(parse_optional_show_warnings(sql, mode), Ok(None), "{sql}");
    }
}

#[test]
fn show_errors_is_read_and_its_neighbours_are_not() {
    let mode = SessionSqlMode::default();
    for sql in ["SHOW ERRORS", "show errors", "SHOW ERRORS;"] {
        assert_eq!(
            parse_optional_show_errors(sql, mode),
            Ok(Some(MySqlShowErrorsCommand::new(0, None))),
            "{sql}"
        );
    }
    for (sql, offset, row_count) in [
        ("SHOW ERRORS LIMIT 1", 0, Some(1)),
        ("SHOW ERRORS LIMIT 10, 5", 10, Some(5)),
        ("SHOW ERRORS LIMIT 5 OFFSET 10", 10, Some(5)),
        ("SHOW ERRORS LIMIT 0", 0, Some(0)),
    ] {
        assert_eq!(
            parse_optional_show_errors(sql, mode),
            Ok(Some(MySqlShowErrorsCommand::new(offset, row_count))),
            "{sql}"
        );
    }
    assert!(parse_optional_show_errors("SHOW ERRORS LIMIT", mode).is_err());
    assert!(parse_optional_show_errors("SHOW ERRORS LIMIT 1,", mode).is_err());
    for sql in [
        "SHOW COUNT(*) ERRORS",
        "SHOW WARNINGS",
        "SHOW TABLES",
        "SELECT 1",
        "",
    ] {
        assert_eq!(parse_optional_show_errors(sql, mode), Ok(None), "{sql}");
    }
}

#[test]
fn replace_into_renders_the_engines_own_or_replace() {
    assert_eq!(
        parse_dml(
            "REPLACE INTO users (id, name) VALUES (1, 'a')",
            SessionSqlMode::default()
        )
        .unwrap()
        .as_sql(),
        "INSERT OR REPLACE INTO \"users\" (\"id\", \"name\") VALUES (1, 'a')"
    );
    // Everything an ordinary INSERT refuses, a REPLACE refuses too.
    for sql in [
        "REPLACE INTO users SET name = 'a'",
        "REPLACE INTO users (name) VALUES ('a') ON DUPLICATE KEY UPDATE name = 'b'",
        "REPLACE IGNORE INTO users (name) VALUES ('a')",
    ] {
        assert!(parse_dml(sql, SessionSqlMode::default()).is_err(), "{sql}");
    }
}

#[test]
fn a_join_names_its_tables_and_equates_whole_columns() {
    let translated = parse_select(
        "SELECT u.id, a.name FROM users AS u JOIN accounts AS a ON u.id = a.user_id",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert_eq!(
        translated.as_sql(),
        concat!(
            "SELECT \"u\".\"id\", \"a\".\"name\" FROM \"users\" AS \"u\" ",
            "JOIN \"accounts\" AS \"a\" ON (\"u\".\"id\" = \"a\".\"user_id\")"
        )
    );
    // A join has several tables and none of them is "the" table, so the
    // single-table accessor answers None while the list answers both, each
    // under the name the engine will report for its columns.
    assert_eq!(translated.source_table(), None);
    assert_eq!(
        translated
            .source_tables()
            .iter()
            .map(|source| (source.reference(), source.table().as_str()))
            .collect::<Vec<_>>(),
        [("u", "users"), ("a", "accounts")]
    );
    // Without an alias the reference is the table's own name.
    let plain = parse_select(
        "SELECT users.id FROM users JOIN accounts ON users.id = accounts.user_id",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert_eq!(plain.source_tables()[1].reference(), "accounts");

    // An outer join marks the side that can go missing, which is what
    // takes the NOT NULL flag off its columns.
    for (sql, outer) in [
        (
            "SELECT u.id FROM users AS u LEFT JOIN accounts AS a ON u.id = a.user_id",
            [false, true],
        ),
        (
            "SELECT u.id FROM users AS u RIGHT JOIN accounts AS a ON u.id = a.user_id",
            [true, false],
        ),
        (
            "SELECT u.id FROM users AS u JOIN accounts AS a ON u.id = a.user_id",
            [false, false],
        ),
    ] {
        let translated = parse_select(sql, SessionSqlMode::default()).unwrap();
        assert_eq!(
            translated
                .source_tables()
                .iter()
                .map(MySqlSelectSource::outer)
                .collect::<Vec<_>>(),
            outer,
            "{sql}"
        );
    }
    assert_eq!(
        parse_select(
            "SELECT u.id FROM users AS u LEFT JOIN accounts AS a ON u.id = a.user_id",
            SessionSqlMode::default()
        )
        .unwrap()
        .as_sql(),
        concat!(
            "SELECT \"u\".\"id\" FROM \"users\" AS \"u\" ",
            "LEFT JOIN \"accounts\" AS \"a\" ON (\"u\".\"id\" = \"a\".\"user_id\")"
        )
    );

    for sql in [
        // An unqualified name in a join is ambiguous whenever both tables
        // carry it, and every metadata lookup here is by name.
        "SELECT id FROM users JOIN accounts ON users.id = accounts.user_id",
        // The ON has to equate whole columns.
        "SELECT users.id FROM users JOIN accounts ON users.id = 1",
        "SELECT users.id FROM users JOIN accounts ON users.id > accounts.user_id",
        // A cross join has no ON to bound it.
        "SELECT users.id FROM users CROSS JOIN accounts",
        "SELECT users.id FROM users JOIN accounts USING (id)",
        // MySQL's comma join is a cross join.
        "SELECT users.id FROM users, accounts",
    ] {
        assert!(
            parse_select(sql, SessionSqlMode::default()).is_err(),
            "{sql}"
        );
    }
}

#[test]
fn having_and_order_by_see_the_aggregates_a_grouped_query_selects() {
    let translated = parse_select(
        "SELECT team, COUNT(*) FROM users GROUP BY team HAVING COUNT(*) > 1 ORDER BY COUNT(*) DESC",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert_eq!(
        translated.as_sql(),
        concat!(
            "SELECT \"team\", COUNT(*) AS \"COUNT(*)\" FROM \"users\" ",
            "GROUP BY \"team\" HAVING (COUNT(*) > 1) ORDER BY COUNT(*) DESC"
        )
    );

    // A comparison on an aggregate records its argument column, which is
    // what makes an integer literal safe to compare against.
    let summed = parse_select(
        "SELECT team FROM users GROUP BY team HAVING SUM(score) > 45",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert_eq!(summed.checked_comparisons()[0].column_name(), "score");
    assert_eq!(
        summed.checked_comparisons()[0].rhs(),
        &CheckedSelectComparisonRhs::SignedInteger(45)
    );

    for sql in [
        // The right side has to be an exact integer, and the left an
        // aggregate or a grouping column.
        "SELECT team FROM users GROUP BY team HAVING COUNT(*) > 'a'",
        "SELECT team FROM users GROUP BY team HAVING 1 > COUNT(*)",
        "SELECT team FROM users GROUP BY team HAVING COUNT(DISTINCT id) > 1",
    ] {
        assert!(
            parse_select(sql, SessionSqlMode::default()).is_err(),
            "{sql}"
        );
    }
}

/// MySQL reads a `HAVING` with no `GROUP BY` over one implicit group of every
/// row. Measured on MySQL 8.4.11 over rows (1,'a',10), (2,'a',30), (3,'b',20):
/// `SELECT COUNT(*) FROM t HAVING COUNT(*) > 1` answers 3, `... > 5` answers
/// no rows, and the engine answers the same for both.
#[test]
fn having_without_a_group_by_filters_the_one_implicit_group() {
    for (sql, normalized) in [
        (
            "SELECT COUNT(*) FROM users HAVING COUNT(*) > 1",
            "SELECT COUNT(*) AS \"COUNT(*)\" FROM \"users\" HAVING (COUNT(*) > 1)",
        ),
        (
            "SELECT SUM(score) FROM users HAVING SUM(score) > 45",
            "SELECT SUM(\"score\") AS \"SUM(score)\" FROM \"users\" HAVING (SUM(\"score\") > 45)",
        ),
        (
            "SELECT MAX(score) FROM users WHERE id > 1 HAVING MAX(score) > 25",
            "SELECT MAX(\"score\") AS \"MAX(score)\" FROM \"users\" WHERE (\"id\" > 1) HAVING (MAX(\"score\") > 25)",
        ),
    ] {
        let translated = parse_select(sql, SessionSqlMode::default()).unwrap();
        assert_eq!(translated.as_sql(), normalized, "{sql}");
        assert!(translated.parse_ast().is_ok(), "{sql}");
    }
}

/// Once a statement is aggregated, a bare column has no single row to come
/// from. Measured on MySQL 8.4.11: one in the projection answers 1140 and one
/// in the `HAVING` answers 1054. Both are refused rather than answered.
#[test]
fn having_without_a_group_by_refuses_an_ungrouped_column() {
    for sql in [
        // 1140: nonaggregated column in the SELECT list.
        "SELECT team FROM users HAVING COUNT(*) > 1",
        "SELECT team, COUNT(*) FROM users HAVING COUNT(*) > 1",
        // 1054: unknown column in the HAVING clause.
        "SELECT COUNT(*) FROM users HAVING team = 'a'",
        // Not an aggregated statement at all. MySQL answers rows here, as a
        // second WHERE would; that shape is refused rather than guessed at.
        "SELECT id FROM users HAVING id > 1",
        // A wildcard hides whether anything is aggregated.
        "SELECT * FROM users HAVING COUNT(*) > 1",
    ] {
        assert!(
            parse_select(sql, SessionSqlMode::default()).is_err(),
            "{sql}"
        );
    }
}

#[test]
fn group_by_takes_whole_columns_and_holds_only_full_group_by() {
    let translated = parse_select(
        "SELECT team, COUNT(*) FROM users GROUP BY team",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert_eq!(
        translated.as_sql(),
        "SELECT \"team\", COUNT(*) AS \"COUNT(*)\" FROM \"users\" GROUP BY \"team\""
    );

    for sql in [
        // Each projection here lands in one row of several, which MySQL
        // answers 1055 for under its own default sql_mode.
        "SELECT team, score FROM users GROUP BY team",
        "SELECT * FROM users GROUP BY team",
        // The grouping key has to be a whole column.
        "SELECT team FROM users GROUP BY team + 1",
        // The modifiers change what a group is.
        "SELECT team FROM users GROUP BY team WITH ROLLUP",
    ] {
        assert!(
            parse_select(sql, SessionSqlMode::default()).is_err(),
            "{sql}"
        );
    }
}

#[test]
fn distinct_crosses_and_its_neighbours_do_not() {
    let translated =
        parse_select("SELECT DISTINCT id FROM users", SessionSqlMode::default()).unwrap();
    assert_eq!(translated.as_sql(), "SELECT DISTINCT \"id\" FROM \"users\"");
    // DISTINCTROW is MySQL's own synonym for it.
    assert_eq!(
        parse_select(
            "SELECT DISTINCTROW id FROM users",
            SessionSqlMode::default()
        )
        .unwrap()
        .as_sql(),
        "SELECT DISTINCT \"id\" FROM \"users\""
    );
    // `DISTINCT ON` is not MySQL's, and neither is a distinct aggregate.
    assert!(parse_select(
        "SELECT DISTINCT ON (id) id FROM users",
        SessionSqlMode::default()
    )
    .is_err());
}

#[test]
fn arithmetic_keeps_the_name_the_client_wrote() {
    for (sql, rendered) in [
        ("SELECT 1+1", "SELECT (1 + 1) AS \"1+1\""),
        (
            "SELECT  id  *  2  FROM users",
            "SELECT (\"id\" * 2) AS \"id  *  2\" FROM \"users\"",
        ),
        (
            "SELECT (id + 1) FROM users",
            "SELECT ((\"id\" + 1)) AS \"(id + 1)\" FROM \"users\"",
        ),
        // MySQL's division is decimal where the engine's is integer, so
        // `3/2` has to answer 1.5 rather than 1.
        ("SELECT 3/2", "SELECT (CAST(3 AS REAL) / 2) AS \"3/2\""),
        (
            "SELECT id + 1 AS next FROM users",
            "SELECT (\"id\" + 1) AS \"next\" FROM \"users\"",
        ),
    ] {
        let translated = parse_select(sql, SessionSqlMode::default()).unwrap();
        assert_eq!(translated.as_sql(), rendered, "{sql}");
    }

    // A nested division makes everything above it decimal arithmetic, whose
    // precision and scale rules have not been measured.
    assert!(parse_select("SELECT 1 + 3/2", SessionSqlMode::default()).is_err());
}

#[test]
fn an_aggregate_carries_the_column_whose_type_it_answers() {
    let translated = parse_select(
        "SELECT min(id), MAX(n) AS top FROM users",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert_eq!(
        translated.as_sql(),
        "SELECT min(\"id\") AS \"min(id)\", MAX(\"n\") AS \"top\" FROM \"users\""
    );
    assert_eq!(
        translated.static_result_metadata(),
        [
            StaticSelectProjectionMetadata::Literal(StaticSelectMetadata::ColumnAggregate {
                column_name: "id".to_owned(),
                kind: ColumnAggregateKind::MinMax,
            }),
            StaticSelectProjectionMetadata::Literal(StaticSelectMetadata::ColumnAggregate {
                column_name: "n".to_owned(),
                kind: ColumnAggregateKind::MinMax,
            }),
        ]
    );

    let summed = parse_select(
        "SELECT SUM(id), AVG(id) FROM users",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert_eq!(
        summed
            .static_result_metadata()
            .iter()
            .map(|projection| match projection {
                StaticSelectProjectionMetadata::Literal(
                    StaticSelectMetadata::ColumnAggregate { kind, .. },
                ) => *kind,
                other => panic!("{other:?}"),
            })
            .collect::<Vec<_>>(),
        [ColumnAggregateKind::Sum, ColumnAggregateKind::Avg]
    );
}

#[test]
fn a_like_crosses_without_a_collation_and_refuses_a_backslash() {
    let translated = parse_select(
        "SELECT id FROM users WHERE name LIKE 'a%' AND name NOT LIKE '_b'",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert_eq!(
        translated.as_sql(),
        "SELECT \"id\" FROM \"users\" WHERE ((\"name\" LIKE 'a%') AND (\"name\" NOT LIKE '_b'))"
    );
    assert_eq!(
        translated.checked_comparisons()[1].operator(),
        CheckedSelectComparisonOperator::NotLike
    );

    for sql in [
        "SELECT id FROM users WHERE name LIKE 'a\\%'",
        "SELECT id FROM users WHERE name LIKE 'a%' ESCAPE '!'",
        "SELECT id FROM users WHERE users.name LIKE 'a%'",
        "SELECT id FROM users WHERE name LIKE ?",
    ] {
        assert!(
            parse_select(sql, SessionSqlMode::default()).is_err(),
            "expected unsupported LIKE form for {sql}"
        );
    }
}

#[test]
fn rejects_select_comparison_coercions_and_non_column_operands() {
    // Every operator has to refuse the same shapes; checking only `=`
    // would let a new operator through with no coercion rules at all.
    for operator in ["=", "<", "<=", ">", ">=", "<>", "!="] {
        // A string is not here: the parser cannot know whether the column
        // is text, so a string against an integer column is refused by the
        // frontend, which can see the column's type.
        for rhs in [
            "1.0",
            "id",
            "CAST(1 AS SIGNED)",
            "9223372036854775808",
            "-9223372036854775809",
        ] {
            let sql = format!("SELECT id FROM users WHERE id {operator} {rhs}");
            assert!(
                parse_select(&sql, SessionSqlMode::default()).is_err(),
                "expected unsupported comparison form for {sql}"
            );
        }
        for sql in [
            format!("SELECT id FROM users WHERE users.id {operator} ?"),
            format!("SELECT id FROM users WHERE id + 1 {operator} ?"),
            format!("SELECT id FROM users WHERE id {operator} 1 {operator} 0"),
        ] {
            assert!(
                parse_select(&sql, SessionSqlMode::default()).is_err(),
                "expected unsupported comparison form for {sql}"
            );
        }
    }
    // NULL-safe equality is a different operator and stays out.
    assert!(parse_select(
        "SELECT id FROM users WHERE id <=> 1",
        SessionSqlMode::default()
    )
    .is_err());
}

#[test]
fn reversed_comparison_is_normalized_to_column_comparison() {
    for (sql, expected_sql, expected_op) in [
        (
            "SELECT id FROM users WHERE 1 = id",
            "SELECT \"id\" FROM \"users\" WHERE (\"id\" = 1)",
            CheckedSelectComparisonOperator::Equal,
        ),
        (
            "SELECT id FROM users WHERE 1 != id",
            "SELECT \"id\" FROM \"users\" WHERE (\"id\" <> 1)",
            CheckedSelectComparisonOperator::NotEqual,
        ),
        (
            "SELECT id FROM users WHERE 1 <> id",
            "SELECT \"id\" FROM \"users\" WHERE (\"id\" <> 1)",
            CheckedSelectComparisonOperator::NotEqual,
        ),
        (
            "SELECT id FROM users WHERE 1 < id",
            "SELECT \"id\" FROM \"users\" WHERE (\"id\" > 1)",
            CheckedSelectComparisonOperator::GreaterThan,
        ),
        (
            "SELECT id FROM users WHERE 1 <= id",
            "SELECT \"id\" FROM \"users\" WHERE (\"id\" >= 1)",
            CheckedSelectComparisonOperator::GreaterThanOrEqual,
        ),
        (
            "SELECT id FROM users WHERE 1 > id",
            "SELECT \"id\" FROM \"users\" WHERE (\"id\" < 1)",
            CheckedSelectComparisonOperator::LessThan,
        ),
        (
            "SELECT id FROM users WHERE 1 >= id",
            "SELECT \"id\" FROM \"users\" WHERE (\"id\" <= 1)",
            CheckedSelectComparisonOperator::LessThanOrEqual,
        ),
        (
            "SELECT id FROM users WHERE 'admin' = id",
            "SELECT \"id\" FROM \"users\" WHERE (\"id\" COLLATE NOCASE = 'admin')",
            CheckedSelectComparisonOperator::Equal,
        ),
        (
            "SELECT id FROM users WHERE ? = id",
            "SELECT \"id\" FROM \"users\" WHERE (\"id\" = ?)",
            CheckedSelectComparisonOperator::Equal,
        ),
    ] {
        let translated = parse_select(sql, SessionSqlMode::default()).unwrap();
        assert_eq!(translated.as_sql(), expected_sql, "{sql}");
        assert_eq!(translated.checked_comparisons().len(), 1, "{sql}");
        assert_eq!(
            translated.checked_comparisons()[0].column_name(),
            "id",
            "{sql}"
        );
        assert_eq!(
            translated.checked_comparisons()[0].operator(),
            expected_op,
            "{sql}"
        );
    }
}

#[test]
fn select_order_and_limit_preserve_source_and_normalize_mysql_forms() {
    for (suffix, normalized) in [
        ("LIMIT 2", "LIMIT 2"),
        ("LIMIT 2 OFFSET 1", "LIMIT 2 OFFSET 1"),
        ("LIMIT 1, 2", "LIMIT 2 OFFSET 1"),
        ("LIMIT 0", "LIMIT 0"),
        ("LIMIT 9223372036854775807", "LIMIT 9223372036854775807"),
        (
            "LIMIT 1 OFFSET 9223372036854775807",
            "LIMIT 1 OFFSET 9223372036854775807",
        ),
    ] {
        let sql = format!("SELECT u.id AS ranked FROM Users u ORDER BY ranked DESC, u.id {suffix}");
        let translated = parse_select(&sql, SessionSqlMode::default()).unwrap();
        assert_eq!(translated.source_table(), Some("users"));
        assert_eq!(
            translated.as_sql(),
            format!("SELECT \"u\".\"id\" AS \"ranked\" FROM \"Users\" AS \"u\" ORDER BY \"ranked\" DESC, \"u\".\"id\" ASC {normalized}")
        );
        assert!(translated.parse_ast().is_ok());
    }
}

/// MySQL compares an `IN` list member by member under the column's own
/// collation. Measured on MySQL 8.4.11 over rows (1,'b'), (2,'A'), (3,'c'):
/// `name IN ('a','C')` answers 2 and 3, and `id NOT IN (1, NULL)` answers
/// nothing.
#[test]
fn select_in_list_compares_each_member_under_the_column_collation() {
    let translated = parse_select(
        "SELECT id FROM users WHERE id IN (1, 2)",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert_eq!(
        translated.as_sql(),
        "SELECT \"id\" FROM \"users\" WHERE (\"id\" IN (1, 2))"
    );
    // One comparison per member, so the frontend holds every one of them to
    // the column's type.
    assert_eq!(translated.checked_comparisons().len(), 2);
    assert_eq!(
        translated.checked_comparisons()[0].operator(),
        CheckedSelectComparisonOperator::In
    );
    assert_eq!(
        translated.checked_comparisons()[1].rhs(),
        &CheckedSelectComparisonRhs::SignedInteger(2)
    );
    assert!(!translated.checked_comparisons()[0].collated());

    let negated = parse_select(
        "SELECT id FROM users WHERE id NOT IN (1, NULL)",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert_eq!(
        negated.as_sql(),
        "SELECT \"id\" FROM \"users\" WHERE (\"id\" NOT IN (1, NULL))"
    );
    assert_eq!(
        negated.checked_comparisons()[0].operator(),
        CheckedSelectComparisonOperator::NotIn
    );
    assert_eq!(
        negated.checked_comparisons()[1].rhs(),
        &CheckedSelectComparisonRhs::Null
    );

    // A text member collates the whole list, the same way a text `=` is
    // collated.
    let text = parse_select(
        "SELECT id FROM users WHERE name IN ('a', 'C')",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert_eq!(
        text.as_sql(),
        "SELECT \"id\" FROM \"users\" WHERE (\"name\" COLLATE NOCASE IN ('a', 'C'))"
    );
    assert!(text.checked_comparisons()[0].collated());
}

/// A `?` in an `IN` list carries no type until it is bound, so the statement
/// has to be rendered a second time once the caller knows which columns are
/// text — exactly as a `?` on the right of a `=` does.
#[test]
fn select_in_list_collates_a_placeholder_over_a_text_column() {
    let translated = parse_select(
        "SELECT id FROM users WHERE name IN (?, ?)",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert!(translated.needs_column_types());
    assert_eq!(translated.parameter_count(), 2);

    let collated = parse_select_with_text_columns(
        "SELECT id FROM users WHERE name IN (?, ?)",
        SessionSqlMode::default(),
        &["name".to_string()],
    )
    .unwrap();
    assert_eq!(
        collated.as_sql(),
        "SELECT \"id\" FROM \"users\" WHERE (\"name\" COLLATE NOCASE IN (?, ?))"
    );
    assert!(collated.checked_comparisons()[1].collated());
}

/// The members follow the same coercion rule a single literal comparison
/// follows, so nothing wider than an exact integer, a string, NULL or `?`
/// reaches the engine.
#[test]
fn select_in_list_refuses_members_a_comparison_would_refuse() {
    for sql in [
        "SELECT id FROM users WHERE id IN (1.5)",
        "SELECT id FROM users WHERE id IN (1, id)",
        "SELECT id FROM users WHERE id IN (9223372036854775808)",
        "SELECT id FROM users WHERE id + 1 IN (1, 2)",
        "SELECT id FROM users WHERE u.id IN (1, 2)",
    ] {
        assert!(
            parse_select(sql, SessionSqlMode::default()).is_err(),
            "{sql}"
        );
    }
}

/// MySQL reads a bare positive integer in `ORDER BY` as the nth projected
/// column, and orders by it exactly as if it had been written out. Measured on
/// MySQL 8.4.11: `SELECT id, name FROM t ORDER BY 2` sorts by `name`, ignoring
/// case, and `ORDER BY -1` and `ORDER BY 1+1` sort nothing because only a bare
/// integer is positional.
#[test]
fn select_order_by_ordinal_names_the_projected_column() {
    for (order_by, normalized) in [
        ("ORDER BY 1", "ORDER BY \"id\" ASC"),
        ("ORDER BY 2", "ORDER BY \"name\" ASC"),
        ("ORDER BY 2 DESC", "ORDER BY \"name\" DESC"),
        ("ORDER BY 2, 1", "ORDER BY \"name\" ASC, \"id\" ASC"),
        ("ORDER BY 1, name", "ORDER BY \"id\" ASC, \"name\" ASC"),
    ] {
        let sql = format!("SELECT id, name FROM users {order_by}");
        let translated = parse_select(&sql, SessionSqlMode::default()).unwrap();
        assert_eq!(
            translated.as_sql(),
            format!("SELECT \"id\", \"name\" FROM \"users\" {normalized}"),
            "{sql}"
        );
        assert!(translated.needs_column_types(), "{sql}");
    }
}

/// An ordinal that lands on a text column has to be collated the way the same
/// column is when it is spelled out, or `ORDER BY 2` and `ORDER BY name` would
/// answer different orders.
#[test]
fn select_order_by_ordinal_collates_a_text_column() {
    let translated = parse_select_with_text_columns(
        "SELECT id, name FROM users ORDER BY 2",
        SessionSqlMode::default(),
        &["name".to_string()],
    )
    .unwrap();
    assert_eq!(
        translated.as_sql(),
        "SELECT \"id\", \"name\" FROM \"users\" ORDER BY \"name\" COLLATE NOCASE ASC"
    );
}

/// An ordinal over an aliased or computed projection names that expression, so
/// it renders the same way the projection did.
#[test]
fn select_order_by_ordinal_reaches_an_alias_and_an_aggregate() {
    for (sql, normalized) in [
        (
            "SELECT id AS ranked FROM users ORDER BY 1",
            "SELECT \"id\" AS \"ranked\" FROM \"users\" ORDER BY \"id\" ASC",
        ),
        (
            "SELECT team, COUNT(*) FROM users GROUP BY team ORDER BY 2 DESC",
            "SELECT \"team\", COUNT(*) AS \"COUNT(*)\" FROM \"users\" GROUP BY \"team\" ORDER BY COUNT(*) DESC",
        ),
    ] {
        let translated = parse_select(sql, SessionSqlMode::default()).unwrap();
        assert_eq!(translated.as_sql(), normalized, "{sql}");
    }
}

/// A wildcard hides the names an ordinal would count through, so it is refused
/// rather than guessed at.
#[test]
fn select_order_by_ordinal_refuses_a_wildcard_projection() {
    assert!(parse_select("SELECT * FROM users ORDER BY 2", SessionSqlMode::default()).is_err());
}

#[test]
fn select_rejects_unchecked_order_and_limit_options() {
    for suffix in [
        // An ordinal past the projection: MySQL answers 1054 here.
        "ORDER BY 2",
        "ORDER BY 0",
        "ORDER BY id COLLATE utf8mb4_bin",
        "ORDER BY id NULLS FIRST",
        "ORDER BY ?",
        "LIMIT -1",
        "LIMIT +1",
        "LIMIT 1.5",
        "LIMIT 1e2",
        "LIMIT ?",
        "LIMIT ALL",
        "LIMIT /* ignored */ ALL",
        "/*! LIMIT ALL */",
        "LIMIT (1)",
        "LIMIT 9223372036854775808",
        "LIMIT 18446744073709551615",
        "LIMIT 1 OFFSET -1",
        "LIMIT 1 OFFSET ?",
        "OFFSET 1",
        "LIMIT 1 OFFSET 1 ROWS",
        "LIMIT 1 BY id",
    ] {
        let sql = format!("SELECT id FROM users {suffix}");
        assert!(
            parse_select(&sql, SessionSqlMode::default()).is_err(),
            "{sql}"
        );
    }
}

#[test]
fn select_without_from_does_not_read_a_table() {
    let translated = parse_select("SELECT 1", SessionSqlMode::default()).unwrap();

    assert!(!translated.reads_table());
    assert_eq!(translated.source_table(), None);
}

#[test]
fn select_source_table_metadata_is_canonical_and_fail_closed() {
    let mode = SessionSqlMode::default();
    let translated = parse_select("SELECT id FROM `Users` AS u", mode).unwrap();
    assert_eq!(translated.source_table(), Some("users"));

    for sql in [
        "SELECT id FROM app.users",
        "SELECT id FROM users JOIN accounts ON users.id = accounts.id",
        "SELECT id FROM users, accounts",
        "SELECT id FROM (SELECT id FROM users) AS rows",
    ] {
        assert!(
            parse_select(sql, mode).is_err(),
            "expected source-table metadata to reject {sql}"
        );
    }
}

#[test]
fn accepts_only_the_zero_argument_last_insert_id_function() {
    let translated = parse_select(
        "SELECT LAST_INSERT_ID() AS generated_id",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert_eq!(
        translated.as_sql(),
        "SELECT last_insert_id() AS \"generated_id\""
    );
    assert!(matches!(translated.parse_ast(), Ok(Stmt::Select(_))));

    for sql in [
        "SELECT LAST_INSERT_ID(1)",
        "SELECT mysql.LAST_INSERT_ID()",
        "SELECT random()",
    ] {
        assert!(matches!(
            parse_select(sql, SessionSqlMode::default()),
            Err(ParseError::Unsupported { .. })
        ));
    }
}

#[test]
fn translates_checked_signed_integer_dml_and_rebuilds_its_spec() {
    let create =
        "CREATE TABLE `numbers` (`tiny` TINYINT, `small` SMALLINT, `wide` INT, `legacy` INTEGER, `big` BIGINT, `label` TEXT)";
    let statement = parse_create_table_ast(create, SessionSqlMode::default()).unwrap();
    assert_eq!(
        render_create_table_mysql(&statement).unwrap(),
        "CREATE TABLE `numbers` (`tiny` TINYINT, `small` SMALLINT, `wide` INT, `legacy` INTEGER, `big` BIGINT, `label` TEXT)"
    );
    let spec = parse_mysql_numeric_spec(create, SessionSqlMode::default()).unwrap();
    assert_eq!(spec.column(0), Some(MySqlSignedInteger::TinyInt));
    assert_eq!(spec.column(1), Some(MySqlSignedInteger::SmallInt));
    assert_eq!(spec.column(2), Some(MySqlSignedInteger::Int));
    assert_eq!(spec.column(3), Some(MySqlSignedInteger::Int));
    assert_eq!(spec.column(4), Some(MySqlSignedInteger::BigInt));
    assert_eq!(spec.column(5), None);
    assert_eq!(MySqlSignedInteger::BigInt.bounds(), (i64::MIN, i64::MAX));

    let insert = parse_dml(
        "INSERT INTO `numbers` (`tiny`, `wide`, `label`) VALUES (?, ?, 'ok')",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert_eq!(
        insert.as_sql(),
        "INSERT INTO \"numbers\" (\"tiny\", \"wide\", \"label\") VALUES (?, ?, 'ok')"
    );
    assert!(matches!(insert.parse_ast(), Ok(Stmt::Insert { .. })));

    let update = parse_dml(
        "UPDATE `numbers` SET `tiny` = ? WHERE TRUE",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert!(matches!(update.parse_ast(), Ok(Stmt::Update(_))));
    let checked = update.checked_update().unwrap();
    assert_eq!(checked.table_name(), "numbers");
    assert_eq!(checked.assignments()[0].column_name(), "tiny");
    assert_eq!(
        checked.assignments()[0].value(),
        CheckedUpdateAssignmentValue::Other
    );
    assert!(!checked.assignments()[0].assigns_column_to_itself());

    let self_assignment = parse_dml(
        "UPDATE numbers SET `tiny` = TINY WHERE TRUE",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert!(self_assignment.checked_update().unwrap().assignments()[0].assigns_column_to_itself());

    for (sql, expected) in [
        (
            "UPDATE numbers SET tiny = 42 WHERE TRUE",
            CheckedUpdateAssignmentValue::SignedInteger(42),
        ),
        (
            "UPDATE numbers SET tiny = +42 WHERE TRUE",
            CheckedUpdateAssignmentValue::SignedInteger(42),
        ),
        (
            "UPDATE numbers SET tiny = -42 WHERE TRUE",
            CheckedUpdateAssignmentValue::SignedInteger(-42),
        ),
        (
            "UPDATE numbers SET tiny = -9223372036854775808 WHERE TRUE",
            CheckedUpdateAssignmentValue::SignedInteger(i64::MIN),
        ),
        (
            "UPDATE numbers SET tiny = 9223372036854775807 WHERE TRUE",
            CheckedUpdateAssignmentValue::SignedInteger(i64::MAX),
        ),
    ] {
        let update = parse_dml(sql, SessionSqlMode::default()).unwrap();
        assert_eq!(
            update.checked_update().unwrap().assignments()[0].value(),
            expected
        );
    }

    for sql in [
        "UPDATE numbers SET tiny = (tiny) WHERE TRUE",
        "UPDATE numbers SET tiny = numbers.tiny WHERE TRUE",
        "UPDATE numbers SET tiny = (42) WHERE TRUE",
        "UPDATE numbers SET tiny = ? WHERE TRUE",
    ] {
        let update = parse_dml(sql, SessionSqlMode::default()).unwrap();
        let assignment = &update.checked_update().unwrap().assignments()[0];
        assert_eq!(assignment.value(), CheckedUpdateAssignmentValue::Other);
        assert!(!assignment.assigns_column_to_itself());
    }

    let delete = parse_dml(
        "DELETE FROM `numbers` WHERE `tiny` IS NOT NULL AND NOT (`wide` IS NULL)",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert_eq!(
        delete.as_sql(),
        "DELETE FROM \"numbers\" WHERE ((\"tiny\" IS NOT NULL) AND (NOT ((\"wide\" IS NULL))))"
    );
    assert!(matches!(delete.parse_ast(), Ok(Stmt::Delete { .. })));

    let delete_all = parse_dml("DELETE FROM `numbers`", SessionSqlMode::default()).unwrap();
    assert_eq!(delete_all.as_sql(), "DELETE FROM \"numbers\"");
}

#[test]
fn translates_signed_mediumint_and_keeps_its_mysql_bounds() {
    let create = "CREATE TABLE `numbers` (`value` MEDIUMINT, `nullable` MEDIUMINT)";
    let statement = parse_create_table_ast(create, SessionSqlMode::default()).unwrap();
    assert_eq!(
        render_create_table_mysql(&statement).unwrap(),
        "CREATE TABLE `numbers` (`value` MEDIUMINT, `nullable` MEDIUMINT)"
    );

    let spec = parse_mysql_numeric_spec(create, SessionSqlMode::default()).unwrap();
    assert_eq!(spec.column(0), Some(MySqlSignedInteger::MediumInt));
    assert_eq!(spec.column(1), Some(MySqlSignedInteger::MediumInt));
    assert_eq!(
        MySqlSignedInteger::MediumInt.bounds(),
        (-8_388_608, 8_388_607)
    );

    for sql in [
        "CREATE TABLE numbers (value MEDIUMINT UNSIGNED)",
        "CREATE TABLE numbers (value MEDIUMINT(8))",
    ] {
        assert!(
            matches!(
                parse_create_table(sql, SessionSqlMode::default()),
                Err(ParseError::Unsupported { .. })
            ),
            "{sql}"
        );
    }
}

#[test]
fn preserves_explicit_nullable_mediumint_through_checked_rendering() {
    let create = "CREATE TABLE `numbers` (`value` MEDIUMINT NULL)";
    let mode = SessionSqlMode::default();
    let translated = parse_create_table(create, mode).unwrap();
    assert_eq!(
        translated.as_sql(),
        "CREATE TABLE \"numbers\" (\"value\" MEDIUMINT NULL)"
    );

    let statement = parse_create_table_ast(create, mode).unwrap();
    let Stmt::CreateTable {
        body:
            TursoCreateTableBody::ColumnsAndConstraints {
                columns,
                constraints,
                options,
            },
        ..
    } = &statement
    else {
        panic!("expected a CREATE TABLE AST");
    };
    assert!(constraints.is_empty());
    assert_eq!(*options, turso_parser::ast::TableOptions::empty());
    assert!(matches!(
        columns[0].constraints.as_slice(),
        [NamedColumnConstraint {
            name: None,
            constraint: TursoColumnConstraint::NotNull {
                nullable: true,
                conflict_clause: None,
            }
        }]
    ));

    let rendered = render_create_table_mysql_with_mode(&statement, mode).unwrap();
    assert_eq!(rendered, "CREATE TABLE `numbers` (`value` MEDIUMINT NULL)");
    assert_eq!(
        render_create_table_mysql_with_mode(
            &parse_create_table_ast(&rendered, mode).unwrap(),
            mode
        )
        .unwrap(),
        rendered
    );

    let spec = parse_mysql_numeric_spec(create, mode).unwrap();
    assert_eq!(spec.column(0), Some(MySqlSignedInteger::MediumInt));
    assert_eq!(
        MySqlSignedInteger::MediumInt.bounds(),
        (-8_388_608, 8_388_607)
    );
}

#[test]
fn keeps_nullable_and_default_null_column_options_distinct() {
    let mode = SessionSqlMode::default();
    for (sql, normalized, rendered, constraint_count) in [
        (
            "CREATE TABLE t (value MEDIUMINT)",
            "CREATE TABLE \"t\" (\"value\" MEDIUMINT)",
            "CREATE TABLE `t` (`value` MEDIUMINT)",
            0,
        ),
        (
            "CREATE TABLE t (value MEDIUMINT NULL)",
            "CREATE TABLE \"t\" (\"value\" MEDIUMINT NULL)",
            "CREATE TABLE `t` (`value` MEDIUMINT NULL)",
            1,
        ),
        (
            "CREATE TABLE t (value MEDIUMINT NULL DEFAULT NULL)",
            "CREATE TABLE \"t\" (\"value\" MEDIUMINT NULL DEFAULT NULL)",
            "CREATE TABLE `t` (`value` MEDIUMINT NULL DEFAULT NULL)",
            2,
        ),
        (
            "CREATE TABLE t (value MEDIUMINT NOT NULL DEFAULT NULL)",
            "CREATE TABLE \"t\" (\"value\" MEDIUMINT NOT NULL DEFAULT NULL)",
            "CREATE TABLE `t` (`value` MEDIUMINT NOT NULL DEFAULT NULL)",
            2,
        ),
    ] {
        assert_eq!(parse_create_table(sql, mode).unwrap().as_sql(), normalized);
        let statement = parse_create_table_ast(sql, mode).unwrap();
        let Stmt::CreateTable {
            body: TursoCreateTableBody::ColumnsAndConstraints { columns, .. },
            ..
        } = &statement
        else {
            panic!("expected a CREATE TABLE AST");
        };
        assert_eq!(columns[0].constraints.len(), constraint_count, "{sql}");
        assert_eq!(
            render_create_table_mysql_with_mode(&statement, mode).unwrap(),
            rendered
        );
    }
}

#[test]
fn rejects_ambiguous_nullable_column_options() {
    let mode = SessionSqlMode::default();
    for sql in [
        "CREATE TABLE t (value MEDIUMINT NULL NULL)",
        "CREATE TABLE t (value MEDIUMINT NULL NOT NULL)",
        "CREATE TABLE t (value MEDIUMINT NOT NULL NULL)",
        "CREATE TABLE t (id INT NULL AUTO_INCREMENT PRIMARY KEY)",
        "CREATE TABLE t (id INT NOT NULL NULL AUTO_INCREMENT PRIMARY KEY)",
    ] {
        assert!(parse_create_table(sql, mode).is_err(), "{sql}");
        assert!(
            parse_auto_increment_create_table(sql, mode).is_err(),
            "{sql}"
        );
    }

    let statement =
        parse_sqlite_create_table("CREATE TABLE t (value TEXT NULL ON CONFLICT IGNORE)");
    assert!(matches!(
        render_create_table_mysql_with_mode(&statement, mode),
        Err(ParseError::Unsupported { .. })
    ));
}

#[test]
fn rejects_named_nullable_column_constraints() {
    let mode = SessionSqlMode::default();
    for sql in [
        "CREATE TABLE t (value MEDIUMINT CONSTRAINT named NULL)",
        "CREATE TABLE t (value MEDIUMINT CONSTRAINT named NOT NULL)",
    ] {
        assert!(matches!(
            parse_create_table(sql, mode),
            Err(ParseError::Unsupported { .. })
        ));
    }

    for sql in [
        "CREATE TABLE t (value TEXT CONSTRAINT named NULL)",
        "CREATE TABLE t (value TEXT CONSTRAINT named NOT NULL)",
    ] {
        let statement = parse_sqlite_create_table(sql);
        assert!(matches!(
            render_create_table_mysql_with_mode(&statement, mode),
            Err(ParseError::Unsupported { .. })
        ));
    }
}

#[test]
fn insert_empty_row_uses_defaults_and_keeps_allocator_path_closed() {
    let mode = SessionSqlMode::default();
    let sql = "INSERT INTO records () VALUES ()";
    let translated = parse_dml(sql, mode).unwrap();
    assert_eq!(
        translated.as_sql(),
        "INSERT INTO \"records\" DEFAULT VALUES"
    );
    assert_eq!(
        parse_dml("INSERT INTO records VALUES ()", mode)
            .unwrap()
            .as_sql(),
        translated.as_sql()
    );
    assert!(matches!(
        translated.parse_ast().unwrap(),
        Stmt::Insert {
            body: turso_parser::ast::InsertBody::DefaultValues,
            ..
        }
    ));
    assert!(parse_auto_increment_insert(sql, mode).is_err());
    assert!(parse_prepared_auto_increment_insert(sql, mode).is_err());
    assert_eq!(
        parse_auto_increment_insert_target(sql, mode).unwrap(),
        Some("records".into())
    );
    for sql in [
        "INSERT INTO records () VALUES (), ()",
        "INSERT INTO records () VALUES (1)",
        "INSERT INTO records (value) VALUES ()",
        "INSERT INTO records () VALUES () RETURNING value",
        "INSERT IGNORE INTO records () VALUES ()",
        "INSERT INTO records () VALUES () ON DUPLICATE KEY UPDATE value = 1",
    ] {
        assert!(parse_dml(sql, mode).is_err(), "{sql}");
    }
}

#[test]
fn rejects_dml_and_numeric_forms_outside_the_strict_signed_slice() {
    for sql in [
        "INSERT IGNORE INTO t (value) VALUES (1)",
        "INSERT INTO t VALUES (1)",
        "INSERT INTO t (value) SELECT 1",
        "UPDATE t SET value = 1 ORDER BY value",
        "UPDATE t SET value = value + 1 WHERE TRUE",
        "UPDATE t SET value = CONCAT('1', '2')",
    ] {
        assert!(matches!(
            parse_dml(sql, SessionSqlMode::default()),
            Err(ParseError::Unsupported { .. })
        ));
    }
    // A comparison in the WHERE of an UPDATE or DELETE goes through the
    // same checked path a SELECT comparison does, so the parser takes the
    // shapes that path takes and refuses the rest. Whether the column it
    // names is one the two engines agree about is the frontend's check.
    for sql in [
        "UPDATE t SET value = 1 WHERE value = 1",
        "DELETE FROM t WHERE value > 1",
        "UPDATE t SET value = 1 WHERE value = 1 AND other <= 2",
        "DELETE FROM t WHERE value IS NULL",
        "DELETE FROM t WHERE value LIKE 'a%'",
        "UPDATE t SET value = 1 WHERE value NOT LIKE 'a%'",
        "UPDATE t SET value = 1 WHERE 1 = value",
        "DELETE FROM t WHERE value BETWEEN 1 AND 2",
        "UPDATE t SET value = 1 WHERE value NOT BETWEEN 1 AND 2",
    ] {
        assert!(parse_dml(sql, SessionSqlMode::default()).is_ok(), "{sql}");
    }
    for sql in [
        "DELETE FROM t WHERE value IN (1, 2)",
        "DELETE FROM t WHERE value <=> 1",
    ] {
        assert!(
            matches!(
                parse_dml(sql, SessionSqlMode::default()),
                Err(ParseError::Unsupported { .. })
            ),
            "{sql}"
        );
    }
    for sql in [
        "WITH doomed AS (SELECT 1) DELETE FROM numbers",
        "DELETE FROM numbers AS n",
        "DELETE FROM numbers, other",
        "DELETE numbers FROM numbers",
        "DELETE FROM numbers USING other",
        "DELETE FROM numbers ORDER BY id",
        "DELETE FROM numbers LIMIT 1",
        "DELETE FROM numbers RETURNING id",
        "DELETE LOW_PRIORITY FROM numbers",
        "DELETE QUICK FROM numbers",
        "DELETE IGNORE FROM numbers",
        "DELETE /*+ NO_INDEX(numbers) */ FROM numbers",
    ] {
        assert!(parse_dml(sql, SessionSqlMode::default()).is_err(), "{sql}");
    }
    for sql in [
        "CREATE TABLE t (value TINYINT UNSIGNED)",
        "CREATE TABLE t (value SMALLINT UNSIGNED)",
        "CREATE TABLE t (value INT UNSIGNED)",
        "CREATE TABLE t (value TINYINT(3))",
        "CREATE TABLE t (value SMALLINT(5))",
        "CREATE TABLE t (value BIGINT UNSIGNED)",
        "CREATE TABLE t (value BIGINT(20))",
        // DECIMAL is taken, but MySQL's own bounds still hold.
        "CREATE TABLE t (value DECIMAL(66,2))",
        "CREATE TABLE t (value DECIMAL(10,31))",
        "CREATE TABLE t (value DECIMAL(2,5))",
        "CREATE TABLE t (value DECIMAL(0,0))",
    ] {
        assert!(
            matches!(
                parse_create_table(sql, SessionSqlMode::default()),
                Err(ParseError::Unsupported { .. })
            ),
            "{sql}"
        );
    }
    assert!(parse_create_table(
        "CREATE TABLE t (value BIGINT ZEROFILL)",
        SessionSqlMode::default()
    )
    .is_err());
}

#[test]
fn select_string_values_are_normalized_after_mysql_lexing() {
    let translated = parse_select(r"SELECT 'a\nb' AS value", SessionSqlMode::default()).unwrap();
    assert_eq!(translated.as_sql(), "SELECT 'a\nb' AS \"value\"");

    let translated = parse_select(
        r"SELECT 'a\nb' AS value",
        SessionSqlMode {
            ansi_quotes: false,
            no_backslash_escapes: true,
        },
    )
    .unwrap();
    assert_eq!(translated.as_sql(), "SELECT 'a\\nb' AS \"value\"");
}

#[test]
fn select_double_quotes_follow_ansi_quotes_mode() {
    let literal = parse_select(r#"SELECT "value" AS result"#, SessionSqlMode::default()).unwrap();
    assert_eq!(literal.as_sql(), "SELECT 'value' AS \"result\"");

    let identifier = parse_select(
        r#"SELECT "value" FROM "records""#,
        SessionSqlMode {
            ansi_quotes: true,
            no_backslash_escapes: false,
        },
    )
    .unwrap();
    assert_eq!(identifier.as_sql(), "SELECT \"value\" FROM \"records\"");
}

#[test]
fn count_is_rendered_with_the_name_mysql_gives_it() {
    let mode = SessionSqlMode::default();
    // The engine names a result column after the expression text and quotes
    // an identifier there, so an unnamed count carries MySQL's own name as
    // an alias. Measured on 8.4.11: `COUNT(n)`, unquoted, case kept.
    for (sql, rendered) in [
        (
            "SELECT COUNT(*) FROM users",
            "SELECT COUNT(*) AS \"COUNT(*)\" FROM \"users\"",
        ),
        (
            "SELECT count(*) FROM users",
            "SELECT count(*) AS \"count(*)\" FROM \"users\"",
        ),
        (
            "SELECT COUNT(name) FROM users",
            "SELECT COUNT(\"name\") AS \"COUNT(name)\" FROM \"users\"",
        ),
        (
            "SELECT COUNT(*) AS total FROM users",
            "SELECT COUNT(*) AS \"total\" FROM \"users\"",
        ),
    ] {
        assert_eq!(
            parse_select(sql, mode).map(|select| select.as_sql().to_owned()),
            Ok(rendered.to_owned()),
            "{sql}"
        );
    }
}

#[test]
fn rejects_select_features_with_unproven_mysql_semantics() {
    for sql in [
        // Integer arithmetic is taken; a decimal or float operand, a
        // modulo and a comparison in a projection are not.
        "SELECT 1.5 + 1",
        "SELECT 1 % 2",
        "SELECT 1 = 1",
        "SELECT id FROM users JOIN accounts ON users.id = accounts.id",
        "SELECT id FROM app.users",
        "SELECT id FROM users WHERE id = 1.0",
        "SELECT 9223372036854775808",
        "SELECT -9223372036854775809",
        "SELECT id <=> NULL FROM users",
        "SELECT LOCATE('b', name, 3) FROM users",
        // COUNT is taken, but only the plain call: DISTINCT, a window, a
        // filter and the other aggregates each mean something this has not
        // measured, and SUM and AVG answer DECIMAL.
        "SELECT COUNT(DISTINCT id) FROM users",
        "SELECT COUNT(*) OVER () FROM users",
        "SELECT COUNT(id, name) FROM users",
        "SELECT COUNT(id + 1) FROM users",
        // The checked aggregates take one plain column: each answers a
        // type worked out from it, and an expression argument has none
        // this can work out.
        "SELECT MIN(id + 1) FROM users",
        "SELECT SUM(id + 1) FROM users",
        "SELECT MAX(DISTINCT id) FROM users",
        "SELECT MIN(id) OVER () FROM users",
        "SELECT MIN(users.id) FROM users",
    ] {
        assert!(
            matches!(
                parse_select_ast(sql, SessionSqlMode::default()),
                Err(ParseError::Unsupported { .. })
            ),
            "expected unsupported error for {sql}"
        );
    }
}

#[test]
fn accepts_the_complete_signed_i64_literal_range() {
    assert_eq!(
        parse_select("SELECT 9223372036854775807", SessionSqlMode::default())
            .unwrap()
            .as_sql(),
        "SELECT 9223372036854775807"
    );
    let minimum = parse_select("SELECT -9223372036854775808", SessionSqlMode::default()).unwrap();
    assert_eq!(minimum.as_sql(), "SELECT (-9223372036854775808)");
    assert!(matches!(minimum.parse_ast().unwrap(), Stmt::Select(_)));
}

#[test]
fn preserves_static_select_literal_spelling_for_metadata() {
    let translated = parse_select(
        "SELECT 0001, -0001, +0001, TRUE, FALSE, NULL",
        SessionSqlMode::default(),
    )
    .unwrap();
    let metadata = translated.static_result_metadata();
    assert_eq!(metadata.len(), 6);
    assert!(matches!(
        &metadata[0],
        StaticSelectProjectionMetadata::Literal(StaticSelectMetadata::Integer {
            digit_count,
            sign: StaticIntegerSign::None
        }) if *digit_count == 4
    ));
    assert!(matches!(
        &metadata[1],
        StaticSelectProjectionMetadata::Literal(StaticSelectMetadata::Integer {
            digit_count,
            sign: StaticIntegerSign::Negative
        }) if *digit_count == 4
    ));
    assert!(matches!(
        &metadata[2],
        StaticSelectProjectionMetadata::Literal(StaticSelectMetadata::Integer {
            digit_count,
            sign: StaticIntegerSign::Positive
        }) if *digit_count == 4
    ));
    assert_eq!(
        metadata[3],
        StaticSelectProjectionMetadata::Literal(StaticSelectMetadata::Boolean(true))
    );
    assert_eq!(
        metadata[4],
        StaticSelectProjectionMetadata::Literal(StaticSelectMetadata::Boolean(false))
    );
    assert_eq!(
        metadata[5],
        StaticSelectProjectionMetadata::Literal(StaticSelectMetadata::Null)
    );

    let wildcard = parse_select(
        "SELECT *, 0001 AS literal FROM users",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert!(matches!(
        wildcard.static_result_metadata(),
        [StaticSelectProjectionMetadata::Wildcard, StaticSelectProjectionMetadata::Literal(
            StaticSelectMetadata::Integer { digit_count, .. }
        )] if *digit_count == 4
    ));
}

#[test]
fn rejects_mysql_attributes_instead_of_dropping_them() {
    for sql in [
        "CREATE TABLE t (id INTEGER AUTO_INCREMENT)",
        "CREATE TABLE t (id INTEGER UNSIGNED)",
        "CREATE TABLE t (id INTEGER DEFAULT CURRENT_TIMESTAMP)",
        "CREATE TABLE t (id INTEGER) ENGINE=InnoDB",
        "CREATE TABLE t (id INTEGER, UNIQUE KEY uq_id (id))",
        "CREATE TABLE t (id INTEGER, CHECK (RAND() > 0))",
        "CREATE TABLE t (id INTEGER REFERENCES parent (id))",
        "CREATE TABLE t (id INTEGER, PRIMARY KEY (id))",
        "CREATE TABLE t (value REAL)",
        "CREATE TABLE t (id INTEGER, parent_id INTEGER, FOREIGN KEY (parent_id) REFERENCES app.parent (id))",
        "CREATE TABLE t (id INTEGER, CHECK (3 / 2 = 1))",
        "CREATE TABLE t (id INTEGER, CHECK (NOT id BETWEEN 0 AND 1))",
    ] {
        assert!(
            matches!(
                parse_create_table_ast(sql, SessionSqlMode::default()),
                Err(ParseError::Unsupported { .. })
            ),
            "expected unsupported error for {sql}"
        );
    }
}

#[test]
fn checks_one_canonical_auto_increment_column_without_changing_the_general_path() {
    let sql =
        "CREATE TABLE users (label TEXT NOT NULL, id INTEGER NOT NULL AUTO_INCREMENT PRIMARY KEY)";
    let checked = parse_auto_increment_create_table(sql, SessionSqlMode::default()).unwrap();

    assert_eq!(checked.allocator_column_ordinal, 1);
    assert_eq!(checked.table_name, "users");
    assert_eq!(checked.allocator_column_name, "id");
    assert_eq!(
        checked.normalized_mysql_ddl,
        "CREATE TABLE `users` (`label` TEXT NOT NULL, `id` INTEGER NOT NULL AUTO_INCREMENT PRIMARY KEY)"
    );
    let Stmt::CreateTable {
        body:
            TursoCreateTableBody::ColumnsAndConstraints {
                columns,
                constraints,
                options,
            },
        ..
    } = &checked.sqlite_statement
    else {
        panic!("expected a CREATE TABLE AST");
    };
    assert!(constraints.is_empty());
    assert_eq!(*options, turso_parser::ast::TableOptions::empty());
    let allocator_column = &columns[checked.allocator_column_ordinal];
    assert_eq!(allocator_column.col_type.as_ref().unwrap().name, "INTEGER");
    assert_eq!(allocator_column.constraints.len(), 1);
    assert!(matches!(
        allocator_column.constraints[0].constraint,
        TursoColumnConstraint::PrimaryKey {
            order: None,
            conflict_clause: None,
            auto_increment: false,
        }
    ));

    assert!(matches!(
        parse_create_table_ast(sql, SessionSqlMode::default()),
        Err(ParseError::Unsupported { .. })
    ));
    assert!(matches!(
        parse_schema_ddl_ast(sql, SessionSqlMode::default()),
        Err(ParseError::Unsupported { .. })
    ));

    assert!(parse_auto_increment_create_table(
        "CREATE TABLE t (note TEXT DEFAULT '/*!99999 AUTO_INCREMENT PRIMARY KEY */', id INT NOT NULL AUTO_INCREMENT PRIMARY KEY)",
        SessionSqlMode::default(),
    )
    .is_ok());
}

#[test]
fn rejects_auto_increment_shapes_outside_the_checked_slice() {
    for sql in [
        "CREATE TABLE app.t (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY)",
        "CREATE TEMPORARY TABLE t (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY)",
        "CREATE TABLE t (id INT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY)",
        "CREATE TABLE t (id BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY)",
        "CREATE TABLE t (id INT NOT NULL PRIMARY KEY AUTO_INCREMENT)",
        "CREATE TABLE t (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY DEFAULT 1)",
        "CREATE TABLE t (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, PRIMARY KEY (id))",
        "CREATE TABLE t (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, other INT NOT NULL AUTO_INCREMENT PRIMARY KEY)",
        "CREATE TABLE t (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY AUTOINCREMENT)",
        "CREATE TABLE t (id INT NOT NULL AUTO_INCREMENT KEY AUTOINCREMENT)",
        "CREATE TABLE t (id INT NOT NULL AUTO_INCREMENT KEY)",
        "CREATE TABLE t (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY ASC)",
        "CREATE TABLE t (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY DESC)",
        "CREATE TABLE t (id INT NOT NULL /*!99999 AUTO_INCREMENT PRIMARY KEY */)",
        "CREATE TABLE t (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, ID TEXT)",
        "CREATE TABLE t (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, id TEXT)",
        "CREATE TABLE t (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY) ENGINE=InnoDB",
    ] {
        assert!(
            parse_auto_increment_create_table(sql, SessionSqlMode::default()).is_err(),
            "expected checked AUTO_INCREMENT parser to reject {sql}"
        );
    }
}

#[test]
fn parses_and_injects_a_typed_auto_increment_multirow_insert() {
    let checked = parse_auto_increment_insert(
        "INSERT INTO `users` (`name`, `value`) VALUES ('Ada', 10), ('Grace', -20)",
        SessionSqlMode::default(),
    )
    .unwrap();

    assert_eq!(checked.table_name().as_str(), "users");
    assert_eq!(checked.row_count().get(), 2);
    assert_eq!(
        checked
            .columns()
            .iter()
            .map(TursoName::as_str)
            .collect::<Vec<_>>(),
        ["name", "value"]
    );

    let table = parse_auto_increment_create_table(
        "CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT, value INT)",
        SessionSqlMode::default(),
    )
    .unwrap();
    let bound = checked.bind_allocator_table(&table).unwrap();
    assert_eq!(bound.allocator_column().as_str(), "id");
    let Stmt::Insert { columns, body, .. } = bound.inject_reserved_range(41).unwrap() else {
        panic!("expected an INSERT AST");
    };
    assert_eq!(
        columns.iter().map(TursoName::as_str).collect::<Vec<_>>(),
        ["id", "name", "value"]
    );
    let turso_parser::ast::InsertBody::Select(select, upsert) = body else {
        panic!("expected a VALUES INSERT body");
    };
    assert!(upsert.is_none());
    let OneSelect::Values(rows) = select.body.select else {
        panic!("expected VALUES rows");
    };
    assert_eq!(rows.len(), 2);
    assert!(matches!(
        rows[0][0].as_ref(),
        TursoExpr::Literal(TursoLiteral::Numeric(value)) if value == "41"
    ));
    assert!(matches!(
        rows[1][0].as_ref(),
        TursoExpr::Literal(TursoLiteral::Numeric(value)) if value == "42"
    ));
}

#[test]
fn accepts_only_direct_literal_values_for_typed_auto_increment_inserts() {
    for sql in [
        "INSERT INTO users (name, enabled, missing) VALUES ('Ada', TRUE, NULL)",
        "INSERT INTO users (value) VALUES (-9223372036854775808)",
    ] {
        assert!(
            parse_auto_increment_insert(sql, SessionSqlMode::default()).is_ok(),
            "expected direct literals to be accepted for {sql}"
        );
    }
}

#[test]
fn prepared_auto_increment_insert_accepts_bare_markers_and_preserves_their_order() {
    let sql = "INSERT INTO users (name, value) VALUES (?, ?), (?, ?)";
    assert!(parse_auto_increment_insert(sql, SessionSqlMode::default()).is_err());

    let checked = parse_prepared_auto_increment_insert(sql, SessionSqlMode::default()).unwrap();
    assert_eq!(checked.row_count().get(), 2);
    let table = parse_auto_increment_create_table(
        "CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT, value INT)",
        SessionSqlMode::default(),
    )
    .unwrap();
    let Stmt::Insert { columns, body, .. } = checked
        .bind_allocator_table(&table)
        .unwrap()
        .inject_reserved_range(41)
        .unwrap()
    else {
        panic!("expected an INSERT AST");
    };
    assert_eq!(
        columns.iter().map(TursoName::as_str).collect::<Vec<_>>(),
        ["id", "name", "value"]
    );
    let turso_parser::ast::InsertBody::Select(select, upsert) = body else {
        panic!("expected a VALUES INSERT body");
    };
    assert!(upsert.is_none());
    let OneSelect::Values(rows) = select.body.select else {
        panic!("expected VALUES rows");
    };
    assert!(matches!(
        rows[0][0].as_ref(),
        TursoExpr::Literal(TursoLiteral::Numeric(value)) if value == "41"
    ));
    assert!(matches!(
        rows[1][0].as_ref(),
        TursoExpr::Literal(TursoLiteral::Numeric(value)) if value == "42"
    ));
    let markers = rows
        .iter()
        .flat_map(|row| row.iter().skip(1))
        .map(|value| match value.as_ref() {
            TursoExpr::Variable(variable) => variable.index.get(),
            _ => panic!("expected a prepared marker"),
        })
        .collect::<Vec<_>>();
    assert_eq!(markers, [1, 2, 3, 4]);
}

#[test]
fn prepared_auto_increment_insert_rejects_non_bare_markers_and_unsafe_shapes() {
    for sql in [
        "INSERT INTO users (name) VALUES (?1)",
        "INSERT INTO users (name) VALUES (:name)",
        "INSERT INTO users (name) VALUES ((?))",
        "INSERT INTO users (name) VALUES (LOWER(?))",
        "INSERT INTO users (name) SELECT ?",
    ] {
        assert!(
            parse_prepared_auto_increment_insert(sql, SessionSqlMode::default()).is_err(),
            "expected prepared AUTO_INCREMENT parser to reject {sql}"
        );
    }

    let table = parse_auto_increment_create_table(
        "CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT)",
        SessionSqlMode::default(),
    )
    .unwrap();
    let explicit_allocator = parse_prepared_auto_increment_insert(
        "INSERT INTO users (id, name) VALUES (?, ?)",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert!(explicit_allocator.bind_allocator_table(&table).is_err());
}

#[test]
fn rejects_unsupported_typed_auto_increment_insert_shapes() {
    for sql in [
        "INSERT INTO users VALUES ('Ada')",
        "INSERT INTO users (name) SELECT 'Ada'",
        "INSERT INTO users (name) VALUES (?)",
        "INSERT INTO users (name) VALUES (other)",
        "INSERT INTO users (name) VALUES (LOWER('Ada'))",
        "INSERT INTO users (name) VALUES ((1))",
        "INSERT INTO users (name) VALUES (X'01')",
        "INSERT INTO users (name) VALUES (1), (2, 3)",
        "INSERT INTO users (name, NAME) VALUES ('a', 'b')",
        "INSERT INTO app.users (name) VALUES ('a')",
        "INSERT INTO users (name) VALUE ('a')",
        "INSERT INTO users (name) VALUES ROW ('a')",
        "INSERT IGNORE INTO users (name) VALUES ('a')",
        "INSERT INTO users SET name = 'a'",
        "INSERT INTO users (name) VALUES ('a') ON DUPLICATE KEY UPDATE name = 'b'",
        "INSERT INTO users (name) VALUES ('a') RETURNING name",
        "INSERT INTO users (name) VALUES (/*!99999*/ 'a')",
        "INSERT /* ordinary */ INTO users (name) VALUES ('a')",
        "INSERT INTO users (name) VALUES ('a') -- ordinary",
        "INSERT INTO users (name) VALUES ('a') # ordinary",
        "INSERT INTO users (name) VALUES ('a'); SELECT 1",
    ] {
        assert!(
            parse_auto_increment_insert(sql, SessionSqlMode::default()).is_err(),
            "expected typed AUTO_INCREMENT INSERT parser to reject {sql}"
        );
    }

    // A fractional literal is taken, because it is a DOUBLE column's value.
    // The dialect's assignment validator is what holds a column to its own
    // type, so the parser does not have to refuse the literal outright.
    assert!(parse_auto_increment_insert(
        "INSERT INTO users (name) VALUES (1.5)",
        SessionSqlMode::default()
    )
    .is_ok());
    // Still refused: a literal no MySQL value can be.
    for sql in [
        "INSERT INTO users (name) VALUES (1e400)",
        "INSERT INTO users (name) VALUES (1.5e)",
    ] {
        assert!(
            parse_auto_increment_insert(sql, SessionSqlMode::default()).is_err(),
            "{sql}"
        );
    }
}

#[test]
fn rejects_explicit_allocator_columns_and_invalid_reserved_ranges() {
    let explicit_allocator = parse_auto_increment_insert(
        "INSERT INTO users (id, name) VALUES (1, 'Ada')",
        SessionSqlMode::default(),
    )
    .unwrap();
    let table = parse_auto_increment_create_table(
        "CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT)",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert!(explicit_allocator.bind_allocator_table(&table).is_err());
    let uppercase_allocator = parse_auto_increment_insert(
        "INSERT INTO USERS (ID, name) VALUES (1, 'Ada')",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert!(uppercase_allocator.bind_allocator_table(&table).is_err());

    let other_table = parse_auto_increment_create_table(
        "CREATE TABLE other (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT)",
        SessionSqlMode::default(),
    )
    .unwrap();
    let wrong_target = parse_auto_increment_insert(
        "INSERT INTO users (name) VALUES ('Ada')",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert!(wrong_target.bind_allocator_table(&other_table).is_err());

    let checked = parse_auto_increment_insert(
        "INSERT INTO users (name) VALUES ('Ada'), ('Grace')",
        SessionSqlMode::default(),
    )
    .unwrap()
    .bind_allocator_table(&table)
    .unwrap();
    assert!(checked.inject_reserved_range(0).is_err());
    assert!(checked
        .inject_reserved_range(i64::from(i32::MAX) as u64)
        .is_err());
    assert!(checked.inject_reserved_range(u64::MAX).is_err());

    let one_row = parse_auto_increment_insert(
        "INSERT INTO users (name) VALUES ('Ada')",
        SessionSqlMode::default(),
    )
    .unwrap()
    .bind_allocator_table(&table)
    .unwrap();
    assert!(one_row
        .inject_reserved_range(i64::from(i32::MAX) as u64)
        .is_ok());
    assert!(one_row
        .inject_reserved_range(i64::from(i32::MAX) as u64 + 1)
        .is_err());
}

#[test]
fn translates_the_safe_alter_table_forms() {
    for sql in [
        "ALTER TABLE `users` ADD COLUMN `email` TEXT NOT NULL DEFAULT 'n/a'",
        "ALTER TABLE `users` DROP COLUMN `email`",
        "ALTER TABLE `users` RENAME COLUMN `email` TO `address`",
        "ALTER TABLE `users` RENAME TO `accounts`",
    ] {
        let statement = parse_alter_table_ast(sql, SessionSqlMode::default()).unwrap();
        let Stmt::AlterTable(TursoAlterTable { name, body }) = statement else {
            panic!("expected ALTER TABLE AST for {sql}");
        };
        assert_eq!(name.name.as_str(), "users");
        match sql {
            value if value.contains("ADD COLUMN") => {
                assert!(matches!(body, TursoAlterTableBody::AddColumn(_)));
            }
            value if value.contains("DROP COLUMN") => {
                assert!(matches!(body, TursoAlterTableBody::DropColumn(_)));
            }
            value if value.contains("RENAME COLUMN") => {
                assert!(matches!(body, TursoAlterTableBody::RenameColumn { .. }));
            }
            _ => assert!(matches!(body, TursoAlterTableBody::RenameTo(_))),
        }
    }
}

#[test]
fn rejects_unsafe_alter_table_forms() {
    for sql in [
        "ALTER TABLE users ADD COLUMN email TEXT FIRST",
        "ALTER TABLE users ADD COLUMN email TEXT, DROP COLUMN id",
        "ALTER TABLE users DROP COLUMN email CASCADE",
        "ALTER TABLE users RENAME AS accounts",
        "ALTER TABLE users RENAME TO app.accounts",
        "ALTER TABLE users CHANGE COLUMN email address TEXT",
        "ALTER TABLE users ADD COLUMN email TEXT, ALGORITHM = INSTANT",
    ] {
        assert!(
            matches!(
                parse_alter_table_ast(sql, SessionSqlMode::default()),
                Err(ParseError::Unsupported { .. })
            ),
            "expected unsupported error for {sql}"
        );
    }
}

#[test]
fn translates_and_renders_safe_create_indexes() {
    let statement = parse_create_index_ast(
        "CREATE UNIQUE INDEX `idx_users_name` ON `users` (`name`)",
        SessionSqlMode::default(),
    )
    .unwrap();

    assert!(matches!(statement, Stmt::CreateIndex { unique: true, .. }));
    let rendered = render_create_index_mysql(&statement).unwrap();
    assert_eq!(
        rendered,
        "CREATE UNIQUE INDEX `idx_users_name` ON `users` (`name`)"
    );
    let reparsed = parse_create_index_ast(&rendered, SessionSqlMode::default()).unwrap();
    assert_eq!(render_create_index_mysql(&reparsed).unwrap(), rendered);
}

#[test]
fn rejects_unsafe_create_index_forms() {
    for sql in [
        "CREATE INDEX idx_users_name ON users (name(3))",
        "CREATE INDEX idx_users_name USING BTREE ON users (name)",
        "CREATE INDEX idx_users_name ON users (name) USING BTREE",
        "CREATE INDEX idx_users_name ON users ((lower(name)))",
        "CREATE INDEX idx_users_name ON users (name COLLATE utf8mb4_bin)",
        "CREATE INDEX idx_users_name ON users (name) WITH PARSER ngram",
        "CREATE INDEX idx_users_name ON users (name) COMMENT 'note'",
        "CREATE INDEX idx_users_name ON users (name) INVISIBLE",
        "CREATE INDEX idx_users_name ON users (name) ALGORITHM = INPLACE",
        "CREATE INDEX idx_users_name ON users (name) LOCK = NONE",
    ] {
        assert!(
            parse_create_index_ast(sql, SessionSqlMode::default()).is_err(),
            "expected rejection for {sql}"
        );
    }
}

#[test]
fn translates_and_renders_safe_create_views_with_quoted_names() {
    let statement = parse_create_view_ast(
        "CREATE VIEW `select view` AS SELECT `select`, `name with space` FROM `order table`",
        SessionSqlMode::default(),
    )
    .unwrap();

    assert!(matches!(statement, Stmt::CreateView { .. }));
    let rendered = render_create_view_mysql(&statement).unwrap();
    assert_eq!(
        rendered,
        "CREATE VIEW `select view` AS SELECT `select`, `name with space` FROM `order table`"
    );
    for mode in [
        SessionSqlMode::default(),
        SessionSqlMode {
            ansi_quotes: true,
            no_backslash_escapes: true,
        },
    ] {
        let reparsed = parse_create_view_ast(&rendered, mode).unwrap();
        assert_eq!(
            render_create_view_mysql_with_mode(&reparsed, mode).unwrap(),
            rendered
        );
    }
}

#[test]
fn rejects_unsafe_create_view_forms() {
    for sql in [
        "CREATE OR REPLACE VIEW users_view AS SELECT name FROM users",
        "CREATE ALGORITHM = MERGE VIEW users_view AS SELECT name FROM users",
        "CREATE DEFINER = root@localhost VIEW users_view AS SELECT name FROM users",
        "CREATE SQL SECURITY INVOKER VIEW users_view AS SELECT name FROM users",
        "CREATE VIEW users_view (display_name) AS SELECT name FROM users",
        "CREATE VIEW users_view AS SELECT name FROM users WHERE name = 'Ada'",
        "CREATE VIEW users_view AS SELECT name FROM users WITH CASCADED CHECK OPTION",
    ] {
        assert!(
            parse_create_view_ast(sql, SessionSqlMode::default()).is_err(),
            "expected rejection for {sql}"
        );
    }
}

#[test]
fn translates_and_renders_safe_create_triggers() {
    let statement = parse_create_trigger_ast(
        "CREATE TRIGGER `copy user` AFTER INSERT ON `users` FOR EACH ROW BEGIN INSERT INTO `audit log` (`user name`, `kind`) VALUES (NEW.`name`, 'created'); END",
        SessionSqlMode::default(),
    )
    .unwrap();

    assert!(matches!(statement, Stmt::CreateTrigger { .. }));
    let rendered = render_create_trigger_mysql(&statement).unwrap();
    assert_eq!(
        rendered,
        "CREATE TRIGGER `copy user` AFTER INSERT ON `users` FOR EACH ROW BEGIN INSERT INTO `audit log` (`user name`, `kind`) VALUES (NEW.`name`, 'created'); END"
    );
    for mode in [
        SessionSqlMode::default(),
        SessionSqlMode {
            ansi_quotes: true,
            no_backslash_escapes: true,
        },
    ] {
        let reparsed = parse_create_trigger_ast(&rendered, mode).unwrap();
        assert_eq!(
            render_create_trigger_mysql_with_mode(&reparsed, mode).unwrap(),
            rendered
        );
    }
}

#[test]
fn rejects_unsafe_create_trigger_forms() {
    for sql in [
        "CREATE TRIGGER before_insert BEFORE INSERT ON users FOR EACH ROW BEGIN INSERT INTO audit (name) VALUES (NEW.name); END",
        "CREATE TRIGGER update_insert AFTER UPDATE ON users FOR EACH ROW BEGIN INSERT INTO audit (name) VALUES (NEW.name); END",
        "CREATE TRIGGER delete_insert AFTER DELETE ON users FOR EACH ROW BEGIN INSERT INTO audit (name) VALUES (OLD.name); END",
        "CREATE TRIGGER conditional AFTER INSERT ON users FOR EACH ROW WHEN NEW.name IS NOT NULL BEGIN INSERT INTO audit (name) VALUES (NEW.name); END",
        "CREATE TRIGGER multi AFTER INSERT ON users FOR EACH ROW BEGIN INSERT INTO audit (name) VALUES (NEW.name); INSERT INTO audit (name) VALUES ('again'); END",
        "CREATE TRIGGER expression AFTER INSERT ON users FOR EACH ROW BEGIN INSERT INTO audit (name) VALUES (LOWER(NEW.name)); END",
        "CREATE TRIGGER select_insert AFTER INSERT ON users FOR EACH ROW BEGIN INSERT INTO audit (name) SELECT name FROM users; END",
        "CREATE TRIGGER upsert AFTER INSERT ON users FOR EACH ROW BEGIN INSERT INTO audit (name) VALUES (NEW.name) ON DUPLICATE KEY UPDATE name = NEW.name; END",
        "CREATE TRIGGER ignored AFTER INSERT ON users FOR EACH ROW BEGIN INSERT IGNORE INTO audit (name) VALUES (NEW.name); END",
    ] {
        assert!(
            parse_create_trigger_ast(sql, SessionSqlMode::default()).is_err(),
            "expected unsupported error for {sql}"
        );
    }
}

#[test]
fn parses_strict_database_management_commands_and_canonicalizes_names() {
    assert_eq!(
        parse_admin_command("CREATE DATABASE Reports;", SessionSqlMode::default()).unwrap(),
        MySqlAdminCommand::CreateDatabase {
            name: MySqlDatabaseName::parse("reports").unwrap(),
        }
    );
    assert_eq!(
        parse_admin_command("DROP DATABASE Reports", SessionSqlMode::default()).unwrap(),
        MySqlAdminCommand::DropDatabase {
            name: MySqlDatabaseName::parse("reports").unwrap(),
        }
    );
    let command = parse_admin_command("USE reports", SessionSqlMode::default()).unwrap();
    assert!(matches!(command, MySqlAdminCommand::Use { .. }));
    assert_eq!(command.name().unwrap().as_str(), "reports");
    assert_eq!(
        parse_admin_command("SHOW DATABASES;", SessionSqlMode::default()),
        Ok(MySqlAdminCommand::ListDatabases)
    );
}

#[test]
fn accepts_only_plain_show_tables_on_the_catalog_surface() {
    let mode = SessionSqlMode::default();
    for sql in ["SHOW TABLES", "show\ttables", "SHOW\nTABLES;"] {
        assert_eq!(
            parse_show_tables(sql, mode),
            Ok(MySqlShowCommand::Tables),
            "expected SHOW TABLES to be accepted: {sql}"
        );
    }

    for sql in [
        "SHOW FULL TABLES",
        "SHOW TABLES FROM reports",
        "SHOW TABLES IN reports",
        "SHOW TABLES LIKE 'report%'",
        "SHOW TABLES WHERE Tables_in_reports LIKE 'report%'",
        "SHOW TABLES; SELECT 1",
    ] {
        assert!(
            parse_show_tables(sql, mode).is_err(),
            "expected SHOW TABLES form to be rejected: {sql}"
        );
    }
}

#[test]
fn show_catalog_parser_does_not_claim_other_commands() {
    let mode = SessionSqlMode::default();
    for sql in ["SELECT 1", "SHOW DATABASES", "SHOW COLUMNS FROM reports"] {
        assert_eq!(parse_optional_show_tables(sql, mode), Ok(None), "{sql}");
    }
}

#[test]
fn accepts_only_the_supported_information_schema_tables_query() {
    let mode = SessionSqlMode::default();
    for sql in [
        "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE() ORDER BY TABLE_NAME",
        " select `TABLE_SCHEMA` , `TABLE_NAME` , `TABLE_TYPE` from `information_schema` . `TABLES` where `TABLE_SCHEMA` = database ( ) order by `TABLE_NAME` ; ",
        "SeLeCt TABLE_SCHEMA,TABLE_NAME,TABLE_TYPE FrOm INFORMATION_SCHEMA.TABLES WhErE TABLE_SCHEMA=DATABASE() OrDeR By TABLE_NAME;",
        "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE FROM `information_schema`.`TABLES` WHERE TABLE_SCHEMA = `DATABASE`() ORDER BY TABLE_NAME",
    ] {
        assert_eq!(
            parse_information_schema_tables(sql, mode),
            Ok(MySqlInformationSchemaTablesQuery),
            "expected information_schema.TABLES query to be accepted: {sql}"
        );
        assert_eq!(
            parse_optional_information_schema_tables(sql, mode),
            Ok(Some(MySqlInformationSchemaTablesQuery)),
            "expected optional parser to recognize: {sql}"
        );
    }

    for sql in [
        "SELECT * FROM information_schema.TABLES",
        "SELECT TABLE_SCHEMA AS schema_name, TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE() ORDER BY TABLE_NAME",
        "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES AS tables WHERE TABLE_SCHEMA = DATABASE() ORDER BY TABLE_NAME",
        "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA = ? ORDER BY TABLE_NAME",
        "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE(?) ORDER BY TABLE_NAME",
        "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_NAME = DATABASE() ORDER BY TABLE_NAME",
        "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES WHERE DATABASE() = TABLE_SCHEMA ORDER BY TABLE_NAME",
        "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE()",
        "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE() ORDER BY TABLE_SCHEMA",
        "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE() ORDER BY TABLE_NAME DESC",
        "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES JOIN other_tables ON 1 = 1 WHERE TABLE_SCHEMA = DATABASE() ORDER BY TABLE_NAME",
        "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES, other_tables WHERE TABLE_SCHEMA = DATABASE() ORDER BY TABLE_NAME",
        "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE FROM other.TABLES WHERE TABLE_SCHEMA = DATABASE() ORDER BY TABLE_NAME",
        "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE FROM information_schema.other WHERE TABLE_SCHEMA = DATABASE() ORDER BY TABLE_NAME",
        "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE();;",
        "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE() ORDER BY TABLE_NAME; SELECT 1",
        "/* hidden */ SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE() ORDER BY TABLE_NAME",
        "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES /* hidden */ WHERE TABLE_SCHEMA = DATABASE() ORDER BY TABLE_NAME",
        "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE() ORDER BY TABLE_NAME -- hidden",
    ] {
        assert!(
            parse_information_schema_tables(sql, mode).is_err(),
            "expected information_schema.TABLES query to be rejected: {sql}"
        );
    }
}

#[test]
fn information_schema_tables_parser_does_not_claim_other_selects() {
    let mode = SessionSqlMode::default();
    for sql in [
        "SELECT 1",
        "SELECT TABLE_SCHEMA FROM information_schema.SCHEMATA",
        "SELECT table_name FROM users",
        "SHOW TABLES",
    ] {
        assert_eq!(
            parse_optional_information_schema_tables(sql, mode),
            Ok(None),
            "expected non-information_schema.TABLES SQL to pass through: {sql}"
        );
    }
    assert!(parse_information_schema_tables("SELECT 1", mode).is_err());
}

#[test]
fn accepts_only_the_supported_information_schema_schemata_query() {
    let mode = SessionSqlMode::default();
    for sql in [
        "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA",
        " select `SCHEMA_NAME` from `information_schema` . `SCHEMATA` ; ",
        "SeLeCt SCHEMA_NAME FrOm INFORMATION_SCHEMA.SCHEMATA;",
    ] {
        assert_eq!(
            parse_information_schema_schemata(sql, mode),
            Ok(MySqlInformationSchemaSchemataQuery),
            "expected information_schema.SCHEMATA query to be accepted: {sql}"
        );
        assert_eq!(
            parse_optional_information_schema_schemata(sql, mode),
            Ok(Some(MySqlInformationSchemaSchemataQuery)),
            "expected optional parser to recognize: {sql}"
        );
    }

    for sql in [
        "SELECT * FROM information_schema.SCHEMATA",
        "SELECT SCHEMA_NAME AS schema_name FROM information_schema.SCHEMATA",
        "SELECT SCHEMA_NAME, DEFAULT_CHARACTER_SET_NAME FROM information_schema.SCHEMATA",
        "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA AS schemata",
        "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = 'reports'",
        "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA ORDER BY SCHEMA_NAME",
        "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA LIMIT 1",
        "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA JOIN other_tables ON 1 = 1",
        "SELECT SCHEMA_NAME FROM information_schema.TABLES",
        "SELECT SCHEMA_NAME FROM other.SCHEMATA",
        "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA; SELECT 1",
        "/* hidden */ SELECT SCHEMA_NAME FROM information_schema.SCHEMATA",
    ] {
        assert!(
            parse_information_schema_schemata(sql, mode).is_err(),
            "expected information_schema.SCHEMATA form to be rejected: {sql}"
        );
    }
}

#[test]
fn information_schema_schemata_parser_does_not_claim_other_selects() {
    let mode = SessionSqlMode::default();
    for sql in [
        "SELECT 1",
        "SELECT SCHEMA_NAME FROM information_schema.TABLES",
        "SELECT schema_name FROM SCHEMATA",
        "SHOW DATABASES",
    ] {
        assert_eq!(
            parse_optional_information_schema_schemata(sql, mode),
            Ok(None),
            "expected non-information_schema.SCHEMATA SQL to pass through: {sql}"
        );
    }
    assert!(parse_information_schema_schemata("SELECT 1", mode).is_err());
}

#[test]
fn accepts_only_the_supported_information_schema_columns_query() {
    let mode = SessionSqlMode::default();
    for (sql, table) in [
        (
            "SELECT COLUMN_NAME, ORDINAL_POSITION, COLUMN_DEFAULT, IS_NULLABLE, COLUMN_TYPE, COLUMN_KEY, EXTRA FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'records' ORDER BY ORDINAL_POSITION",
            "records",
        ),
        (
            " select `COLUMN_NAME`, `ORDINAL_POSITION`, `COLUMN_DEFAULT`, `IS_NULLABLE`, `COLUMN_TYPE`, `COLUMN_KEY`, `EXTRA` from `information_schema`.`COLUMNS` where `TABLE_SCHEMA` = database ( ) and `TABLE_NAME` = 'RePoRtS' order by `ORDINAL_POSITION` ; ",
            "reports",
        ),
        (
            "SeLeCt COLUMN_NAME,ORDINAL_POSITION,COLUMN_DEFAULT,IS_NULLABLE,COLUMN_TYPE,COLUMN_KEY,EXTRA FrOm INFORMATION_SCHEMA.COLUMNS WhErE TABLE_SCHEMA=DATABASE() AnD TABLE_NAME='other_table' OrDeR By ORDINAL_POSITION;",
            "other_table",
        ),
    ] {
        let expected_table = MySqlTableName::parse(table).unwrap();
        assert_eq!(
            parse_information_schema_columns(sql, mode).map(|query| query.table().clone()),
            Ok(expected_table.clone()),
            "expected information_schema.COLUMNS query to be accepted: {sql}"
        );
        assert_eq!(
            parse_optional_information_schema_columns(sql, mode)
                .map(|query| query.map(|query| query.table().clone())),
            Ok(Some(expected_table)),
            "expected optional parser to recognize: {sql}"
        );
    }

    for sql in [
        "SELECT * FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'records' ORDER BY ORDINAL_POSITION",
        "SELECT COLUMN_NAME AS name, ORDINAL_POSITION, COLUMN_DEFAULT, IS_NULLABLE, COLUMN_TYPE, COLUMN_KEY, EXTRA FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'records' ORDER BY ORDINAL_POSITION",
        "SELECT COLUMN_NAME, ORDINAL_POSITION, COLUMN_DEFAULT, IS_NULLABLE, COLUMN_TYPE, COLUMN_KEY, EXTRA FROM information_schema.COLUMNS AS columns WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'records' ORDER BY ORDINAL_POSITION",
        "SELECT COLUMN_NAME, ORDINAL_POSITION, COLUMN_DEFAULT, IS_NULLABLE, COLUMN_TYPE, COLUMN_KEY, EXTRA FROM information_schema.COLUMNS JOIN other_tables ON 1 = 1 WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'records' ORDER BY ORDINAL_POSITION",
        "SELECT COLUMN_NAME, ORDINAL_POSITION, COLUMN_DEFAULT, IS_NULLABLE, COLUMN_TYPE, COLUMN_KEY, EXTRA FROM (SELECT COLUMN_NAME, ORDINAL_POSITION, COLUMN_DEFAULT, IS_NULLABLE, COLUMN_TYPE, COLUMN_KEY, EXTRA FROM information_schema.COLUMNS) AS columns WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'records' ORDER BY ORDINAL_POSITION",
        "SELECT COLUMN_NAME, ORDINAL_POSITION, COLUMN_DEFAULT, IS_NULLABLE, COLUMN_TYPE, COLUMN_KEY, EXTRA FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() ORDER BY ORDINAL_POSITION",
        "SELECT COLUMN_NAME, ORDINAL_POSITION, COLUMN_DEFAULT, IS_NULLABLE, COLUMN_TYPE, COLUMN_KEY, EXTRA FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = 'records' ORDER BY ORDINAL_POSITION",
        "SELECT COLUMN_NAME, ORDINAL_POSITION, COLUMN_DEFAULT, IS_NULLABLE, COLUMN_TYPE, COLUMN_KEY, EXTRA FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = ? ORDER BY ORDINAL_POSITION",
        "SELECT COLUMN_NAME, ORDINAL_POSITION, COLUMN_DEFAULT, IS_NULLABLE, COLUMN_TYPE, COLUMN_KEY, EXTRA FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'reports.other' ORDER BY ORDINAL_POSITION",
        "SELECT COLUMN_NAME, ORDINAL_POSITION, COLUMN_DEFAULT, IS_NULLABLE, COLUMN_TYPE, COLUMN_KEY, EXTRA FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'reports other' ORDER BY ORDINAL_POSITION",
        "SELECT COLUMN_NAME, ORDINAL_POSITION, COLUMN_DEFAULT, IS_NULLABLE, COLUMN_TYPE, COLUMN_KEY, EXTRA FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'records'",
        "SELECT COLUMN_NAME, ORDINAL_POSITION, COLUMN_DEFAULT, IS_NULLABLE, COLUMN_TYPE, COLUMN_KEY, EXTRA FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'records' ORDER BY COLUMN_NAME",
        "SELECT COLUMN_NAME, ORDINAL_POSITION, COLUMN_DEFAULT, IS_NULLABLE, COLUMN_TYPE, COLUMN_KEY, EXTRA FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'records' ORDER BY ORDINAL_POSITION DESC",
        "SELECT COLUMN_NAME, ORDINAL_POSITION, COLUMN_DEFAULT, IS_NULLABLE, COLUMN_TYPE, COLUMN_KEY, EXTRA FROM information_schema.COLUMNS WHERE TABLE_NAME = 'records' AND TABLE_SCHEMA = DATABASE() ORDER BY ORDINAL_POSITION",
        "SELECT COLUMN_NAME, ORDINAL_POSITION, COLUMN_DEFAULT, IS_NULLABLE, COLUMN_TYPE, COLUMN_KEY, EXTRA FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() OR TABLE_NAME = 'records' ORDER BY ORDINAL_POSITION",
        "SELECT COLUMN_NAME, ORDINAL_POSITION, COLUMN_DEFAULT, IS_NULLABLE, COLUMN_TYPE, COLUMN_KEY, EXTRA FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'records' ORDER BY ORDINAL_POSITION; SELECT 1",
        "/* hidden */ SELECT COLUMN_NAME, ORDINAL_POSITION, COLUMN_DEFAULT, IS_NULLABLE, COLUMN_TYPE, COLUMN_KEY, EXTRA FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'records' ORDER BY ORDINAL_POSITION",
    ] {
        assert!(
            parse_information_schema_columns(sql, mode).is_err(),
            "expected information_schema.COLUMNS form to be rejected: {sql}"
        );
    }
}

#[test]
fn information_schema_columns_parser_does_not_claim_other_selects() {
    let mode = SessionSqlMode::default();
    for sql in [
        "SELECT 1",
        "SELECT TABLE_NAME FROM information_schema.TABLES",
        "SELECT COLUMN_NAME FROM information_schema.SCHEMATA",
        "SELECT column_name FROM records",
        "SHOW COLUMNS FROM records",
    ] {
        assert_eq!(
            parse_optional_information_schema_columns(sql, mode),
            Ok(None),
            "expected non-information_schema.COLUMNS SQL to pass through: {sql}"
        );
    }
    assert!(parse_information_schema_columns("SELECT 1", mode).is_err());
}

#[test]
fn accepts_only_plain_show_columns_for_one_unqualified_table() {
    let mode = SessionSqlMode::default();
    for (sql, table) in [
        ("SHOW COLUMNS FROM reports", "reports"),
        ("show\tcolumns\nfrom `RePoRtS`;", "reports"),
    ] {
        assert_eq!(
            parse_show_columns(sql, mode).map(|command| command.table().as_str().to_owned()),
            Ok(table.to_owned()),
            "expected SHOW COLUMNS form to be accepted: {sql}"
        );
    }

    for sql in [
        "SHOW COLUMNS reports",
        "SHOW COLUMNS FROM reports IN archive",
        "SHOW COLUMNS FROM `report columns`",
        "SHOW FULL COLUMNS FROM reports",
        "SHOW COLUMNS FROM reports LIKE 'id%'",
        "SHOW COLUMNS FROM reports WHERE Field = 'id'",
        "SHOW COLUMNS FROM reports; SELECT 1",
    ] {
        assert!(
            parse_show_columns(sql, mode).is_err(),
            "expected SHOW COLUMNS form to be rejected: {sql}"
        );
    }
}

#[test]
fn accepts_only_plain_describe_for_one_unqualified_table() {
    let mode = SessionSqlMode::default();
    for (sql, table) in [
        ("DESCRIBE reports", "reports"),
        ("describe\t`RePoRtS`;", "reports"),
        ("DESC reports", "reports"),
        ("desc\n`RePoRtS`", "reports"),
    ] {
        assert_eq!(
            parse_describe(sql, mode).map(|command| command.table().as_str().to_owned()),
            Ok(table.to_owned()),
            "expected DESCRIBE form to be accepted: {sql}"
        );
    }

    for sql in [
        "DESCRIBE",
        "DESCRIBE reports extra",
        "DESCRIBE TABLE reports",
        "DESCRIBE FULL reports",
        "DESCRIBE reports IN archive",
        "DESCRIBE `report columns`",
        "DESCRIBE reports LIKE 'id%'",
        "DESCRIBE reports WHERE Field = 'id'",
        "DESCRIBE reports; SELECT 1",
        "DESCR reports",
        "DESC",
        "DESC reports extra",
    ] {
        assert!(
            parse_describe(sql, mode).is_err(),
            "expected DESCRIBE form to be rejected: {sql}"
        );
    }
}

#[test]
fn show_columns_parser_does_not_claim_other_commands() {
    let mode = SessionSqlMode::default();
    for sql in [
        "SELECT 1",
        "SHOW DATABASES",
        "SHOW TABLES",
        "DESCRIBE reports",
    ] {
        assert_eq!(parse_optional_show_columns(sql, mode), Ok(None), "{sql}");
    }
}

#[test]
fn accepts_only_the_supported_show_create_table_command() {
    let mode = SessionSqlMode::default();
    for sql in [
        "SHOW CREATE TABLE reports",
        "SHOW CREATE TABLE reports;",
        "show create table reports",
        "SHOW CREATE table `reports`",
        "SHOW\nCREATE\nTABLE\nreports",
    ] {
        assert_eq!(
            parse_show_create_table(sql, mode).map(|command| command.table().as_str().to_owned()),
            Ok("reports".to_owned()),
            "{sql}"
        );
    }

    for sql in [
        "SHOW CREATE TABLE",
        "SHOW CREATE TABLE;",
        "SHOW CREATE TABLE reports extra",
        "SHOW CREATE TABLE reports; SHOW CREATE TABLE reports",
        "SHOW CREATE TABLE reports LIKE 'x'",
    ] {
        assert!(
            parse_show_create_table(sql, mode).is_err(),
            "must be rejected: {sql}"
        );
    }
}

#[test]
fn accepts_every_spelling_mysql_takes_for_show_index() {
    let mode = SessionSqlMode::default();
    // Measured on MySQL 8.4.11: all six spellings run.
    for sql in [
        "SHOW INDEX FROM reports",
        "SHOW INDEXES FROM reports",
        "SHOW KEYS FROM reports",
        "SHOW INDEX IN reports",
        "SHOW INDEXES IN reports",
        "SHOW KEYS IN reports",
        "show index from `reports`;;",
        "/* c */ SHOW INDEX FROM reports -- x",
    ] {
        assert_eq!(
            parse_show_index(sql, mode).map(|command| command.table().as_str().to_owned()),
            Ok("reports".to_owned()),
            "{sql}"
        );
    }

    let qualified = parse_show_index("SHOW INDEX FROM Archive.Reports", mode).unwrap();
    assert_eq!(
        qualified.database().map(MySqlDatabaseName::as_str),
        Some("archive")
    );
    assert_eq!(qualified.table().as_str(), "reports");

    for sql in [
        "SHOW INDEX",
        "SHOW INDEX FROM",
        "SHOW INDEX reports",
        "SHOW INDEX FROM reports extra",
        "SHOW INDEX FROM reports WHERE Key_name = 'PRIMARY'",
    ] {
        assert!(parse_show_index(sql, mode).is_err(), "{sql}");
    }
}

#[test]
fn sized_text_types_carry_their_declared_character_count() {
    let mode = SessionSqlMode::default();
    let translated = parse_create_table(
        "CREATE TABLE v (id INTEGER NOT NULL UNIQUE, name VARCHAR(4) NOT NULL)",
        mode,
    )
    .unwrap();
    assert!(
        translated.as_sql().contains("VARCHAR(4)"),
        "{}",
        translated.as_sql()
    );

    // The MySQL rendering keeps the length too, so a stored table reads
    // back the width it was declared with.
    let statement = parse_create_table_ast(
        "CREATE TABLE v (id INTEGER NOT NULL UNIQUE, name VARCHAR(4) NOT NULL)",
        mode,
    )
    .unwrap();
    let rendered = render_create_table_mysql_with_mode(&statement, mode).unwrap();
    assert!(rendered.contains("VARCHAR(4)"), "{rendered}");

    // CHAR carries its length the same way.
    let with_char = parse_create_table(
        "CREATE TABLE v (id INTEGER NOT NULL UNIQUE, tag CHAR(2))",
        mode,
    )
    .unwrap();
    assert!(
        with_char.as_sql().contains("CHAR(2)"),
        "{}",
        with_char.as_sql()
    );

    for sql in [
        // MySQL rejects a bare VARCHAR and a zero length, and bounds the
        // column at 65535 bytes, which is 16383 utf8mb4 characters.
        "CREATE TABLE v (id INTEGER NOT NULL UNIQUE, name VARCHAR)",
        "CREATE TABLE v (id INTEGER NOT NULL UNIQUE, name VARCHAR(0))",
        "CREATE TABLE v (id INTEGER NOT NULL UNIQUE, name VARCHAR(16384))",
        "CREATE TABLE v (id INTEGER NOT NULL UNIQUE, tag CHAR)",
        "CREATE TABLE v (id INTEGER NOT NULL UNIQUE, tag CHAR(0))",
    ] {
        assert!(parse_create_table(sql, mode).is_err(), "{sql}");
    }
}

#[test]
fn show_variables_reads_the_scope_and_the_pattern() {
    let mode = SessionSqlMode::default();

    let all = parse_show_variables("SHOW VARIABLES", mode).unwrap();
    assert_eq!(all.scope(), MySqlVariableScope::Session);
    assert!(all.selects("gtid_mode"));
    assert!(all.selects("anything_at_all"));

    for sql in [
        "SHOW SESSION VARIABLES",
        "SHOW LOCAL VARIABLES",
        "show variables;;",
        "/* c */ SHOW VARIABLES -- x",
    ] {
        assert_eq!(
            parse_show_variables(sql, mode).map(|command| command.scope()),
            Ok(MySqlVariableScope::Session),
            "{sql}"
        );
    }
    assert_eq!(
        parse_show_variables("SHOW GLOBAL VARIABLES", mode).map(|command| command.scope()),
        Ok(MySqlVariableScope::Global)
    );

    // The two statements a real `mysqldump --no-data` opens with.
    let gtid = parse_show_variables("SHOW VARIABLES LIKE 'gtid_mode'", mode).unwrap();
    assert!(gtid.selects("gtid_mode"));
    assert!(!gtid.selects("gtid_mode_extra"));
    let ndbinfo = parse_show_variables(r"SHOW VARIABLES LIKE 'ndbinfo\_version'", mode).unwrap();
    assert!(ndbinfo.selects("ndbinfo_version"));
    assert!(!ndbinfo.selects("ndbinfoXversion"));

    let prefix = parse_show_variables("SHOW VARIABLES LIKE 'character_set%'", mode).unwrap();
    assert!(prefix.selects("character_set_client"));
    assert!(!prefix.selects("collation_connection"));

    // Measured on MySQL 8.4.11: outside ANSI_QUOTES a double-quoted
    // pattern is a string, and `LOCAL` reads the session scope.
    let quoted = parse_show_variables("SHOW LOCAL VARIABLES LIKE \"sql_notes\"", mode).unwrap();
    assert_eq!(quoted.scope(), MySqlVariableScope::Session);
    assert!(quoted.selects("sql_notes"));
    assert!(!quoted.selects("sql_note"));
}

#[test]
fn show_variables_rejects_what_it_does_not_answer() {
    let mode = SessionSqlMode::default();
    for sql in [
        "SHOW VARIABLES LIKE",
        "SHOW VARIABLES LIKE gtid_mode",
        "SHOW VARIABLES LIKE 'gtid_mode' extra",
        "SHOW VARIABLES WHERE Variable_name = 'gtid_mode'",
        "SHOW VARIABLES LIKE 'unterminated",
    ] {
        assert!(parse_show_variables(sql, mode).is_err(), "{sql}");
    }
}

#[test]
fn show_variables_parser_does_not_claim_other_commands() {
    let mode = SessionSqlMode::default();
    for sql in [
        "SELECT 1",
        "SHOW TABLES",
        "SHOW GLOBAL STATUS",
        "SHOW CREATE TABLE reports",
    ] {
        assert_eq!(parse_optional_show_variables(sql, mode), Ok(None), "{sql}");
    }
}

#[test]
fn a_string_literal_is_not_a_catalog_name() {
    let mode = SessionSqlMode::default();
    for sql in [
        "USE 'archive'",
        "CREATE DATABASE 'archive'",
        "DROP DATABASE 'archive'",
    ] {
        assert!(parse_admin_command(sql, mode).is_err(), "{sql}");
    }
    for sql in ["SHOW COLUMNS FROM 'reports'", "SHOW CREATE TABLE 'reports'"] {
        assert!(parse_show_columns(sql, mode).is_err(), "{sql}");
        assert!(parse_show_create_table(sql, mode).is_err(), "{sql}");
    }
}

#[test]
fn show_index_parser_does_not_claim_other_commands() {
    let mode = SessionSqlMode::default();
    for sql in [
        "SELECT 1",
        "SHOW TABLES",
        "SHOW COLUMNS FROM reports",
        "SHOW CREATE TABLE reports",
    ] {
        assert_eq!(parse_optional_show_index(sql, mode), Ok(None), "{sql}");
    }
}

#[test]
fn catalog_commands_read_a_database_qualifier() {
    let mode = SessionSqlMode::default();
    let columns = parse_show_columns("SHOW COLUMNS FROM Archive.Reports", mode).unwrap();
    assert_eq!(
        columns.database().map(MySqlDatabaseName::as_str),
        Some("archive")
    );
    assert_eq!(columns.table().as_str(), "reports");

    let described = parse_describe("DESCRIBE `archive`.`reports`", mode).unwrap();
    assert_eq!(
        described.database().map(MySqlDatabaseName::as_str),
        Some("archive")
    );
    assert_eq!(described.table().as_str(), "reports");

    let created = parse_show_create_table("SHOW CREATE TABLE archive.reports;;", mode).unwrap();
    assert_eq!(
        created.database().map(MySqlDatabaseName::as_str),
        Some("archive")
    );
    assert_eq!(created.table().as_str(), "reports");

    // An unqualified name still reports no qualifier.
    assert_eq!(
        parse_show_create_table("SHOW CREATE TABLE reports", mode)
            .unwrap()
            .database(),
        None
    );

    // A dot with nothing on one side of it is not a qualifier.
    for sql in [
        "SHOW CREATE TABLE archive.",
        "SHOW CREATE TABLE .reports",
        "SHOW CREATE TABLE a.b.c",
    ] {
        assert!(parse_show_create_table(sql, mode).is_err(), "{sql}");
    }
}

#[test]
fn catalog_commands_take_the_comments_and_semicolons_mysql_takes() {
    let mode = SessionSqlMode::default();
    // Measured on MySQL 8.4.11: all of these run.
    for sql in [
        "/* c */ SHOW TABLES",
        "SHOW TABLES;;",
        "SHOW TABLES -- x",
        "SHOW TABLES # x",
        "/* c */ SHOW TABLES /* d */ ;; -- x",
    ] {
        assert_eq!(
            parse_show_tables(sql, mode),
            Ok(MySqlShowCommand::Tables),
            "{sql}"
        );
    }
    for sql in [
        "/* c */ SHOW COLUMNS FROM reports",
        "SHOW COLUMNS FROM reports;;",
        "SHOW COLUMNS FROM reports -- x",
    ] {
        assert_eq!(
            parse_show_columns(sql, mode).map(|command| command.table().as_str().to_owned()),
            Ok("reports".to_owned()),
            "{sql}"
        );
    }
    for sql in [
        "/* c */ DESCRIBE reports",
        "DESCRIBE reports;;",
        "DESC reports -- x",
    ] {
        assert_eq!(
            parse_describe(sql, mode).map(|command| command.table().as_str().to_owned()),
            Ok("reports".to_owned()),
            "{sql}"
        );
    }
    for sql in [
        "/* c */ SHOW CREATE TABLE reports",
        "SHOW CREATE TABLE reports;;",
        "SHOW CREATE TABLE reports # x",
    ] {
        assert_eq!(
            parse_show_create_table(sql, mode).map(|command| command.table().as_str().to_owned()),
            Ok("reports".to_owned()),
            "{sql}"
        );
    }
    // A comment still cannot stand in for the operand, and real trailing
    // junk is still refused.
    for sql in [
        "SHOW CREATE TABLE /* c */",
        "SHOW CREATE TABLE reports;; extra",
        "SHOW COLUMNS FROM reports;; SHOW TABLES",
    ] {
        assert!(
            parse_show_create_table(sql, mode).is_err() && parse_show_columns(sql, mode).is_err(),
            "must be rejected: {sql}"
        );
    }
}

#[test]
fn show_create_table_parser_does_not_claim_other_commands() {
    let mode = SessionSqlMode::default();
    for sql in [
        "SELECT 1",
        "SHOW DATABASES",
        "SHOW TABLES",
        "SHOW COLUMNS FROM reports",
        "SHOW CREATE VIEW reports",
        "SHOW CREATE DATABASE reports",
    ] {
        assert_eq!(
            parse_optional_show_create_table(sql, mode),
            Ok(None),
            "{sql}"
        );
    }
}

#[test]
fn describe_parser_does_not_claim_other_commands() {
    let mode = SessionSqlMode::default();
    for sql in [
        "SELECT 1",
        "SHOW DATABASES",
        "SHOW TABLES",
        "SHOW COLUMNS FROM reports",
    ] {
        assert_eq!(parse_optional_describe(sql, mode), Ok(None), "{sql}");
    }
}

#[test]
fn show_columns_validates_the_initial_table_name_policy() {
    for (name, reason) in [
        ("", "empty"),
        (&"a".repeat(65), "longer than 64 bytes"),
        ("a\0b", "NUL byte"),
        ("日本語", "non-ASCII character"),
        ("archive.reports", "character outside [A-Za-z0-9_$]"),
    ] {
        assert_eq!(
            MySqlTableName::parse(name),
            Err(ParseError::InvalidTableName { reason }),
            "expected invalid table name: {name:?}"
        );
    }
    assert_eq!(
        MySqlTableName::parse("RePoRtS").unwrap().as_str(),
        "reports"
    );
}

#[test]
fn accepts_only_the_configured_identifier_quote_style() {
    assert_eq!(
        parse_admin_command("USE `Reports`", SessionSqlMode::default())
            .unwrap()
            .name()
            .unwrap()
            .as_str(),
        "reports"
    );
    assert_eq!(
        parse_admin_command(
            "USE \"Reports\"",
            SessionSqlMode {
                ansi_quotes: true,
                no_backslash_escapes: false,
            }
        )
        .unwrap()
        .name()
        .unwrap()
        .as_str(),
        "reports"
    );
    assert!(parse_admin_command("USE \"Reports\"", SessionSqlMode::default()).is_err());
    assert!(parse_admin_command("USE 'Reports'", SessionSqlMode::default()).is_err());
}

#[test]
fn rejects_comments_options_qualified_names_and_trailing_junk() {
    for sql in [
        "CREATE/*hidden*/ DATABASE reports",
        "CREATE DATABASE reports -- hidden",
        "CREATE DATABASE reports # hidden",
        "CREATE DATABASE reports /* hidden */",
        "CREATE DATABASE reports CHARACTER SET utf8mb4",
        "DROP DATABASE IF EXISTS reports",
        "CREATE DATABASE IF NOT EXISTS reports",
        "USE tenant.reports",
        "CREATE DATABASE reports; DROP DATABASE other",
        "USE reports garbage",
        "USE reports;;",
    ] {
        assert!(
            parse_admin_command(sql, SessionSqlMode::default()).is_err(),
            "expected strict rejection for {sql}"
        );
    }
    assert_eq!(
        parse_admin_command("USE reports garbage", SessionSqlMode::default()),
        Err(ParseError::TrailingAdminCommandTokens)
    );
}

#[test]
fn rejects_non_database_commands_and_incomplete_commands() {
    for sql in [
        "",
        "SELECT 1",
        "CREATE SCHEMA reports",
        "DROP SCHEMA reports",
        "CREATE DATABASE",
        "DROP DATABASE",
        "USE",
        "CREATE reports",
        "DROP reports",
    ] {
        assert!(
            matches!(
                parse_admin_command(sql, SessionSqlMode::default()),
                Err(ParseError::ExpectedAdminCommand)
                    | Err(ParseError::Sqlparser(_))
                    | Err(ParseError::ExpectedOneStatement { .. })
            ),
            "expected incomplete/non-admin rejection for {sql}"
        );
    }
}

#[test]
fn optionally_parses_only_the_network_admin_surface() {
    let mode = SessionSqlMode::default();
    assert_eq!(
        parse_optional_admin_command("CREATE DATABASE reports", mode),
        Ok(Some(MySqlAdminCommand::CreateDatabase {
            name: MySqlDatabaseName::parse("reports").unwrap(),
        }))
    );
    assert_eq!(
        parse_optional_admin_command("SHOW DATABASES", mode),
        Ok(Some(MySqlAdminCommand::ListDatabases))
    );

    for sql in [
        "SELECT 1 + 1",
        "CREATE TABLE records (id INT)",
        "DROP TABLE records",
        "SHOW SCHEMAS",
    ] {
        assert_eq!(parse_optional_admin_command(sql, mode), Ok(None), "{sql}");
    }
}

#[test]
fn parses_only_plain_transaction_control_commands() {
    let mode = SessionSqlMode::default();
    for (sql, expected) in [
        ("BEGIN", MySqlTransactionCommand::Begin),
        ("begin;", MySqlTransactionCommand::Begin),
        ("START TRANSACTION", MySqlTransactionCommand::Begin),
        ("COMMIT", MySqlTransactionCommand::Commit),
        ("ROLLBACK;", MySqlTransactionCommand::Rollback),
    ] {
        assert_eq!(parse_transaction_command(sql, mode), Ok(expected), "{sql}");
        assert_eq!(
            parse_optional_transaction_command(sql, mode),
            Ok(Some(expected)),
            "{sql}"
        );
    }
}

#[test]
fn parses_only_strict_autocommit_assignments() {
    let mode = SessionSqlMode::default();
    for (sql, enabled) in [
        ("SET autocommit = 0", false),
        ("set session AUTOCOMMIT=1;", true),
    ] {
        assert_eq!(
            parse_optional_autocommit_setting(sql, mode),
            Ok(Some(MySqlAutocommitSetting { enabled })),
            "{sql}"
        );
    }
    assert_eq!(
        parse_optional_autocommit_setting("SELECT 1", mode),
        Ok(None)
    );

    for sql in [
        "SET GLOBAL autocommit = 0",
        "SET autocommit = 2",
        "SET autocommit = ON",
        "SET autocommit = 1, sql_mode = ''",
        "SET @@session.autocommit = 0",
        "/* hidden */ SET autocommit = 0",
        "SET autocommit = 0 -- hidden",
        "SET autocommit = 0; SELECT 1",
    ] {
        assert!(
            parse_optional_autocommit_setting(sql, mode).is_err(),
            "expected strict rejection for {sql}"
        );
    }
}

#[test]
fn parses_only_the_mysql_async_settings_query_bytes() {
    assert_eq!(
        parse_driver_bootstrap_query("SELECT @@max_allowed_packet,@@wait_timeout"),
        Ok(MySqlDriverBootstrapQuery::MaxAllowedPacketAndWaitTimeout)
    );

    for sql in [
        "select @@max_allowed_packet,@@wait_timeout",
        "SELECT  @@max_allowed_packet,@@wait_timeout",
        "SELECT @@max_allowed_packet, @@wait_timeout",
        "SELECT @@max_allowed_packet ,@@wait_timeout",
        "SELECT @@max_allowed_packet,@@wait_timeout ",
        "SELECT @@session.max_allowed_packet,@@wait_timeout",
        "SELECT @@global.max_allowed_packet,@@wait_timeout",
        "SELECT @@max_allowed_packet,@@session.wait_timeout",
        "SELECT @@max_allowed_packet AS packet,@@wait_timeout",
        "SELECT @@max_allowed_packet,@@wait_timeout FROM settings",
        "SELECT @@max_allowed_packet + 1,@@wait_timeout",
        "SELECT @@socket,@@wait_timeout",
        "SELECT @@wait_timeout,@@max_allowed_packet",
        "/* hidden */ SELECT @@max_allowed_packet,@@wait_timeout",
        "SELECT /* hidden */ @@max_allowed_packet,@@wait_timeout",
        "SELECT @@max_allowed_packet,@@wait_timeout -- hidden",
        "SELECT @@max_allowed_packet,@@wait_timeout;",
        "SELECT @@max_allowed_packet,@@wait_timeout; SELECT 1",
    ] {
        assert!(
            parse_driver_bootstrap_query(sql).is_err(),
            "expected strict rejection for {sql}"
        );
    }
}

#[test]
fn optional_transaction_parser_ignores_other_sql() {
    let mode = SessionSqlMode::default();
    for sql in [
        "SELECT 1",
        "INSERT INTO records (value) VALUES (1)",
        "CREATE TABLE records (id INT)",
        "USE reports",
    ] {
        assert_eq!(
            parse_optional_transaction_command(sql, mode),
            Ok(None),
            "{sql}"
        );
        assert_eq!(
            parse_transaction_command(sql, mode),
            Err(ParseError::ExpectedTransactionCommand),
            "{sql}"
        );
    }
}

#[test]
fn rejects_transaction_options_comments_and_multiple_statements() {
    let mode = SessionSqlMode::default();
    for sql in [
        "BEGIN WORK",
        "BEGIN TRANSACTION",
        "START TRANSACTION READ ONLY",
        "START TRANSACTION WITH CONSISTENT SNAPSHOT",
        "COMMIT AND CHAIN",
        "COMMIT AND NO CHAIN",
        "ROLLBACK AND CHAIN",
        "ROLLBACK TO SAVEPOINT before_write",
        "BEGIN; SELECT 1",
        "COMMIT;;",
        "/* hidden */ BEGIN",
        "BEGIN -- hidden",
        "START /* hidden */ TRANSACTION",
    ] {
        assert!(
            parse_optional_transaction_command(sql, mode).is_err(),
            "expected strict rejection for {sql}"
        );
    }
}

#[test]
fn optional_admin_parser_rejects_invalid_recognized_statements() {
    let mode = SessionSqlMode::default();
    for sql in [
        "CREATE DATABASE",
        "DROP DATABASE",
        "USE",
        "SHOW DATABASES LIKE 'tenant%'",
        "SHOW DATABASES WHERE 1",
        "SHOW DATABASES; SELECT 1",
        "SHOW DATABASES -- hidden",
        "/* hidden */ SHOW DATABASES",
    ] {
        assert!(
            parse_optional_admin_command(sql, mode).is_err(),
            "expected strict rejection for {sql}"
        );
    }
}

#[test]
fn rejects_database_names_that_could_escape_the_registry_contract() {
    for sql in [
        "USE ``",
        "USE `a/b`",
        "USE `a\\b`",
        "USE `a.b`",
        "USE `information_schema`",
        "USE `SQLite_Schema`",
        "USE `has space`",
        "USE `日本語`",
        "USE `a-b`",
        "USE `a`",
    ] {
        let result = parse_admin_command(sql, SessionSqlMode::default());
        if sql == "USE `a`" {
            assert!(result.is_ok(), "a is a valid database name");
        } else {
            assert!(result.is_err(), "expected invalid-name rejection for {sql}");
        }
    }
    assert!(MySqlDatabaseName::parse(&"a".repeat(65)).is_err());
    assert_eq!(
        MySqlDatabaseName::parse("RePoRtS").unwrap().as_str(),
        "reports"
    );
    assert_eq!(
        MySqlDatabaseName::parse("reports").unwrap().into_string(),
        "reports"
    );
}

#[test]
fn quoted_identifier_escapes_are_decoded_before_name_validation() {
    assert_eq!(
        parse_admin_command("USE `reports``archive`", SessionSqlMode::default()),
        Err(ParseError::InvalidDatabaseName {
            reason: "character outside [A-Za-z0-9_$]",
        })
    );
    assert_eq!(
        parse_admin_command("USE `reports`", SessionSqlMode::default())
            .unwrap()
            .name()
            .unwrap()
            .as_str(),
        "reports"
    );
    assert!(parse_admin_command("USE `reports", SessionSqlMode::default()).is_err());
}

fn parse_sqlite_create_table(sql: &str) -> Stmt {
    let mut parser = TursoParser::new(sql.as_bytes());
    let Some(TursoCmd::Stmt(statement @ Stmt::CreateTable { .. })) = parser.next_cmd().unwrap()
    else {
        panic!("expected SQLite CREATE TABLE AST");
    };
    statement
}
