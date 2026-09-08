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
        (
            "SELECT id FROM users WHERE id <=> 7",
            CheckedSelectComparisonOperator::NullSafeEqual,
            "IS 7",
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
fn null_safe_equal_translates_to_is() {
    let mode = SessionSqlMode::default();
    let translated = parse_select("SELECT id FROM users WHERE id <=> NULL", mode).unwrap();
    assert_eq!(
        translated.as_sql(),
        "SELECT \"id\" FROM \"users\" WHERE (\"id\" IS NULL)"
    );
    assert_eq!(
        translated.checked_comparisons()[0].operator(),
        CheckedSelectComparisonOperator::NullSafeEqual
    );
    assert_eq!(
        translated.checked_comparisons()[0].rhs(),
        &CheckedSelectComparisonRhs::Null
    );

    let reversed = parse_select("SELECT id FROM users WHERE 7 <=> id", mode).unwrap();
    assert_eq!(
        reversed.as_sql(),
        "SELECT \"id\" FROM \"users\" WHERE (\"id\" IS 7)"
    );
    assert_eq!(
        reversed.checked_comparisons()[0].operator(),
        CheckedSelectComparisonOperator::NullSafeEqual
    );

    let collated = parse_select_with_column_types(
        "SELECT id FROM users WHERE name <=> 'admin'",
        mode,
        &["name".to_string()],
        &[],
        &[],
    )
    .unwrap();
    assert_eq!(
        collated.as_sql(),
        "SELECT \"id\" FROM \"users\" WHERE (\"name\" COLLATE NOCASE IS 'admin')"
    );
    assert_eq!(
        collated.checked_comparisons()[0].operator(),
        CheckedSelectComparisonOperator::NullSafeEqual
    );
}

#[test]
fn qualified_column_comparisons_render_and_validate_qualifiers() {
    let mode = SessionSqlMode::default();

    // Table alias used in WHERE
    let aliased = parse_select("SELECT id FROM users u WHERE u.id = 1", mode).unwrap();
    assert_eq!(
        aliased.as_sql(),
        "SELECT \"id\" FROM \"users\" AS \"u\" WHERE (\"u\".\"id\" = 1)"
    );
    assert_eq!(aliased.checked_comparisons()[0].qualifier(), Some("u"));
    assert_eq!(aliased.checked_comparisons()[0].column_name(), "id");

    // Unaliased table used in WHERE
    let unaliased = parse_select("SELECT id FROM users WHERE users.id = 1", mode).unwrap();
    assert_eq!(
        unaliased.as_sql(),
        "SELECT \"id\" FROM \"users\" WHERE (\"users\".\"id\" = 1)"
    );
    assert_eq!(
        unaliased.checked_comparisons()[0].qualifier(),
        Some("users")
    );
    assert_eq!(unaliased.checked_comparisons()[0].column_name(), "id");

    // Reversed comparison with qualified column
    let reversed = parse_select("SELECT id FROM users WHERE 1 = users.id", mode).unwrap();
    assert_eq!(
        reversed.as_sql(),
        "SELECT \"id\" FROM \"users\" WHERE (\"users\".\"id\" = 1)"
    );
    assert_eq!(reversed.checked_comparisons()[0].qualifier(), Some("users"));

    // Text column with collation
    let collated = parse_select_with_column_types(
        "SELECT id FROM users u WHERE u.name = 'alice'",
        mode,
        &["name".to_string()],
        &[],
        &[],
    )
    .unwrap();
    assert_eq!(
        collated.as_sql(),
        "SELECT \"id\" FROM \"users\" AS \"u\" WHERE (\"u\".\"name\" COLLATE NOCASE = 'alice')"
    );

    // CTE with qualified comparison
    let cte = parse_select(
        "WITH c AS (SELECT id, name FROM users) SELECT c.id FROM c WHERE c.id = 1",
        mode,
    )
    .unwrap();
    assert_eq!(
        cte.as_sql(),
        "WITH \"c\" AS (SELECT \"id\", \"name\" FROM \"users\") SELECT \"c\".\"id\" FROM \"c\" WHERE (\"c\".\"id\" = 1)"
    );
    assert_eq!(cte.checked_comparisons()[0].qualifier(), Some("c"));

    // Qualified BETWEEN, LIKE, IN
    let between = parse_select("SELECT id FROM users u WHERE u.id BETWEEN 1 AND 2", mode).unwrap();
    assert_eq!(
        between.as_sql(),
        "SELECT \"id\" FROM \"users\" AS \"u\" WHERE ((\"u\".\"id\" >= 1) AND (\"u\".\"id\" <= 2))"
    );

    let like = parse_select("SELECT id FROM users u WHERE u.name LIKE 'a%'", mode).unwrap();
    assert_eq!(
        like.as_sql(),
        "SELECT \"id\" FROM \"users\" AS \"u\" WHERE (\"u\".\"name\" LIKE 'a%' ESCAPE '\\')"
    );

    let in_list = parse_select("SELECT id FROM users u WHERE u.id IN (1, 2)", mode).unwrap();
    assert_eq!(
        in_list.as_sql(),
        "SELECT \"id\" FROM \"users\" AS \"u\" WHERE (\"u\".\"id\" IN (1, 2))"
    );

    // Rejected: qualifier does not match table alias
    assert!(parse_select("SELECT id FROM users u WHERE users.id = 1", mode).is_err());

    // Rejected: qualifier references unknown table
    assert!(parse_select("SELECT id FROM users WHERE other.id = 1", mode).is_err());

    // A qualifier in a joined query names which table the comparison's column
    // belongs to, which is what the frontend checks the value's type against.
    assert!(parse_select(
        "SELECT users.id FROM users JOIN accounts ON users.id = accounts.user_id WHERE users.id = 1",
        mode
    )
    .is_ok());
    // Rejected: a qualifier naming no table the statement reads.
    assert!(parse_select(
        "SELECT users.id FROM users JOIN accounts ON users.id = accounts.user_id WHERE teams.id = 1",
        mode
    )
    .is_err());
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
            concat!(
                "SELECT CASE WHEN typeof(\"v\") IN ('integer', 'real') ",
                "THEN printf('%X', CAST(round(\"v\") AS INTEGER)) ELSE hex(\"v\") END ",
                "AS \"HEX(v)\" FROM \"s\""
            ),
        ),
        (
            "SELECT SIGN(n) FROM s",
            "SELECT sign(\"n\") AS \"SIGN(n)\" FROM \"s\"",
        ),
        (
            "SELECT SQRT(n) FROM s",
            "SELECT sqrt(\"n\") AS \"SQRT(n)\" FROM \"s\"",
        ),
        (
            "SELECT POW(n, 2) FROM s",
            "SELECT pow(\"n\", 2) AS \"POW(n, 2)\" FROM \"s\"",
        ),
        (
            "SELECT POWER(n, 2) FROM s",
            "SELECT pow(\"n\", 2) AS \"POWER(n, 2)\" FROM \"s\"",
        ),
        (
            "SELECT MOD(n, 3) FROM s",
            "SELECT CAST(mod(\"n\", 3) AS INTEGER) AS \"MOD(n, 3)\" FROM \"s\"",
        ),
        (
            "SELECT GREATEST(n, 10) FROM s",
            "SELECT max(\"n\", 10) AS \"GREATEST(n, 10)\" FROM \"s\"",
        ),
        (
            "SELECT LEAST(n, 10) FROM s",
            "SELECT min(\"n\", 10) AS \"LEAST(n, 10)\" FROM \"s\"",
        ),
        (
            "SELECT NULLIF(n, 0) FROM s",
            "SELECT nullif(\"n\", 0) AS \"NULLIF(n, 0)\" FROM \"s\"",
        ),
        (
            "SELECT NULLIF(v, 'abc') FROM s",
            "SELECT nullif(\"v\", 'abc') AS \"NULLIF(v, 'abc')\" FROM \"s\"",
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
        // Both engines answer NULL for a row that matches nothing, so a
        // missing ELSE is written as a missing ELSE.
        (
            "SELECT CASE WHEN n > 1 THEN 'y' END FROM s",
            concat!(
                "SELECT CASE WHEN (\"n\" > 1) THEN 'y' END ",
                "AS \"CASE WHEN n > 1 THEN 'y' END\" FROM \"s\""
            ),
        ),
        (
            "SELECT SUBSTRING(v, 1, 2) FROM s",
            "SELECT substr(\"v\", 1, 2) AS \"SUBSTRING(v, 1, 2)\" FROM \"s\"",
        ),
        // Both engines spell the ranking calls the same way, so only the
        // window is rewritten.
        (
            "SELECT ROW_NUMBER() OVER (ORDER BY n) FROM s",
            concat!(
                "SELECT row_number() OVER (ORDER BY \"n\" ASC) ",
                "AS \"ROW_NUMBER() OVER (ORDER BY n)\" FROM \"s\""
            ),
        ),
        (
            "SELECT id, ROW_NUMBER() OVER (ORDER BY n) FROM s ORDER BY id",
            concat!(
                "SELECT \"id\", row_number() OVER (ORDER BY \"n\" ASC) ",
                "AS \"ROW_NUMBER() OVER (ORDER BY n)\" FROM \"s\" ORDER BY \"id\" ASC"
            ),
        ),
        (
            "SELECT NTILE(2) OVER (ORDER BY n) FROM s",
            concat!(
                "SELECT ntile(2) OVER (ORDER BY \"n\" ASC) ",
                "AS \"NTILE(2) OVER (ORDER BY n)\" FROM \"s\""
            ),
        ),
        (
            "SELECT SUM(n) OVER (ORDER BY id) FROM s",
            concat!(
                "SELECT sum(\"n\") OVER (ORDER BY \"id\" ASC) ",
                "AS \"SUM(n) OVER (ORDER BY id)\" FROM \"s\""
            ),
        ),
        // A scalar subquery goes through the same reader a subquery in a
        // `WHERE` does, and the column is named after its own text, the
        // parentheses included.
        (
            "SELECT (SELECT MAX(n) FROM f) FROM s",
            concat!(
                "SELECT (SELECT MAX(\"n\") AS \"MAX(n)\" FROM \"f\") ",
                "AS \"(SELECT MAX(n) FROM f)\" FROM \"s\""
            ),
        ),
        // A named window is written out where each call stands, and the
        // column keeps the name MySQL gives it, `OVER win` and all.
        (
            "SELECT SUM(n) OVER win FROM s WINDOW win AS (ORDER BY id)",
            concat!(
                "SELECT sum(\"n\") OVER (ORDER BY \"id\" ASC) ",
                "AS \"SUM(n) OVER win\" FROM \"s\""
            ),
        ),
        // The shorthand frame is written out, which answers the same rows.
        (
            "SELECT SUM(n) OVER (ORDER BY id ROWS UNBOUNDED PRECEDING) FROM s",
            concat!(
                "SELECT sum(\"n\") OVER (ORDER BY \"id\" ASC ",
                "ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) ",
                "AS \"SUM(n) OVER (ORDER BY id ROWS UNBOUNDED PRECEDING)\" FROM \"s\""
            ),
        ),
        (
            "SELECT SUM(n) OVER (PARTITION BY id RANGE BETWEEN 1 PRECEDING AND 2 FOLLOWING) FROM s",
            concat!(
                "SELECT sum(\"n\") OVER (PARTITION BY \"id\" ",
                "RANGE BETWEEN 1 PRECEDING AND 2 FOLLOWING) ",
                "AS \"SUM(n) OVER (PARTITION BY id RANGE BETWEEN 1 PRECEDING AND 2 FOLLOWING)\" ",
                "FROM \"s\""
            ),
        ),
        (
            "SELECT COUNT(*) OVER (PARTITION BY n) FROM s",
            concat!(
                "SELECT count(*) OVER (PARTITION BY \"n\") ",
                "AS \"COUNT(*) OVER (PARTITION BY n)\" FROM \"s\""
            ),
        ),
        (
            "SELECT LAG(v) OVER (ORDER BY id) FROM s",
            concat!(
                "SELECT lag(\"v\") OVER (ORDER BY \"id\" ASC) ",
                "AS \"LAG(v) OVER (ORDER BY id)\" FROM \"s\""
            ),
        ),
        (
            "SELECT DENSE_RANK() OVER (PARTITION BY n ORDER BY id DESC) FROM s",
            concat!(
                "SELECT dense_rank() OVER (PARTITION BY \"n\" ORDER BY \"id\" DESC) ",
                "AS \"DENSE_RANK() OVER (PARTITION BY n ORDER BY id DESC)\" FROM \"s\""
            ),
        ),
        // The engine's three names are what MySQL's one name with a side is.
        (
            "SELECT TRIM(v) FROM s",
            "SELECT trim(\"v\") AS \"TRIM(v)\" FROM \"s\"",
        ),
        (
            "SELECT TRIM(BOTH 'x' FROM v) FROM s",
            "SELECT trim(\"v\", 'x') AS \"TRIM(BOTH 'x' FROM v)\" FROM \"s\"",
        ),
        (
            "SELECT TRIM(LEADING 'x' FROM v) FROM s",
            "SELECT ltrim(\"v\", 'x') AS \"TRIM(LEADING 'x' FROM v)\" FROM \"s\"",
        ),
        (
            "SELECT TRIM(TRAILING 'x' FROM v) FROM s",
            "SELECT rtrim(\"v\", 'x') AS \"TRIM(TRAILING 'x' FROM v)\" FROM \"s\"",
        ),
        // With no side named, MySQL trims both.
        (
            "SELECT TRIM('x' FROM v) FROM s",
            "SELECT trim(\"v\", 'x') AS \"TRIM('x' FROM v)\" FROM \"s\"",
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
        // A count this cannot read leaves no width to answer with.
        "SELECT LEFT(v, n) FROM s",
        "SELECT SUBSTRING(v, 1, n) FROM s",
        "SELECT SUBSTRING(v, 1) FROM s",
        // MySQL removes whole copies of what TRIM was given where the engine
        // removes any of its characters, and they only agree on one.
        "SELECT TRIM(LEADING 'ax' FROM v) FROM s",
        "SELECT TRIM(LEADING v FROM v) FROM s",
        "SELECT TRIM('abc') FROM s",
        // MySQL's bare side is `TRIM(LEADING FROM v)`, which the parser
        // library does not read; what it does read is not MySQL.
        "SELECT TRIM(LEADING v) FROM s",
        // A branch that is not a string literal or NULL has no width, and a
        // CASE whose every branch is NULL has none left.
        "SELECT CASE WHEN n > 1 THEN NULL ELSE NULL END FROM s",
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
    let sql = "SELECT id FROM users UNION (SELECT id FROM accounts LIMIT 1)";
    assert!(
        parse_select(sql, SessionSqlMode::default()).is_err(),
        "{sql}"
    );
}

#[test]
fn union_accepts_parenthesised_branches() {
    let mode = SessionSqlMode::default();
    for (sql, expected_sql) in [
        (
            "(SELECT id FROM users) UNION (SELECT id FROM accounts)",
            "SELECT \"id\" FROM \"users\" UNION SELECT \"id\" FROM \"accounts\"",
        ),
        (
            "(SELECT id FROM users) UNION ALL (SELECT id FROM accounts)",
            "SELECT \"id\" FROM \"users\" UNION ALL SELECT \"id\" FROM \"accounts\"",
        ),
        (
            "SELECT id FROM users UNION (SELECT id FROM accounts)",
            "SELECT \"id\" FROM \"users\" UNION SELECT \"id\" FROM \"accounts\"",
        ),
        (
            "(SELECT id FROM users) UNION SELECT id FROM accounts",
            "SELECT \"id\" FROM \"users\" UNION SELECT \"id\" FROM \"accounts\"",
        ),
        (
            "((SELECT id FROM users)) UNION (SELECT id FROM accounts)",
            "SELECT \"id\" FROM \"users\" UNION SELECT \"id\" FROM \"accounts\"",
        ),
        (
            "(SELECT id, name FROM users) UNION (SELECT id, name FROM accounts) ORDER BY 2",
            "SELECT \"id\", \"name\" FROM \"users\" UNION SELECT \"id\", \"name\" FROM \"accounts\" ORDER BY \"name\" ASC",
        ),
        (
            "(SELECT id FROM users) EXCEPT (SELECT id FROM accounts)",
            "SELECT \"id\" FROM \"users\" EXCEPT SELECT \"id\" FROM \"accounts\"",
        ),
        (
            "(SELECT id FROM users) INTERSECT (SELECT id FROM accounts)",
            "SELECT \"id\" FROM \"users\" INTERSECT SELECT \"id\" FROM \"accounts\"",
        ),
    ] {
        let translated = parse_select(sql, mode).unwrap();
        assert_eq!(translated.as_sql(), expected_sql, "{sql}");
    }

    // Branches carrying options like ORDER BY, LIMIT, or WITH are refused
    for sql in [
        "(SELECT id FROM users ORDER BY id) UNION (SELECT id FROM accounts)",
        "(SELECT id FROM users LIMIT 1) UNION (SELECT id FROM accounts)",
        "(SELECT id FROM users) UNION (SELECT id FROM accounts ORDER BY id)",
        "(SELECT id FROM users) UNION (SELECT id FROM accounts LIMIT 1)",
        "(WITH cte AS (SELECT id FROM users) SELECT id FROM cte) UNION (SELECT id FROM accounts)",
    ] {
        assert!(parse_select(sql, mode).is_err(), "{sql}");
    }
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
    for sql in [
        "SHOW COUNT(*) WARNINGS",
        "show count(*) warnings",
        "SHOW COUNT (*) WARNINGS",
        "SHOW COUNT ( * ) WARNINGS",
        "SHOW COUNT(*) WARNINGS;",
    ] {
        assert_eq!(
            parse_optional_show_warnings(sql, mode),
            Ok(Some(MySqlShowWarningsCommand::count())),
            "{sql}"
        );
    }
    assert!(parse_optional_show_warnings("SHOW WARNINGS LIMIT", mode).is_err());
    assert!(parse_optional_show_warnings("SHOW WARNINGS LIMIT 1,", mode).is_err());
    assert!(parse_optional_show_warnings("SHOW COUNT(*) WARNINGS LIMIT 1", mode).is_err());
    assert!(parse_optional_show_warnings("SHOW COUNT(1) WARNINGS", mode).is_err());
    assert!(parse_optional_show_warnings("SHOW COUNT(*) WARNINGS extra", mode).is_err());
    // Everything else belongs to its own parser, which refuses the ones
    // MySQL takes and this does not.
    for sql in [
        "SHOW COUNT(*) ERRORS",
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
    for sql in [
        "SHOW COUNT(*) ERRORS",
        "show count(*) errors",
        "SHOW COUNT (*) ERRORS",
        "SHOW COUNT ( * ) ERRORS",
        "SHOW COUNT(*) ERRORS;",
    ] {
        assert_eq!(
            parse_optional_show_errors(sql, mode),
            Ok(Some(MySqlShowErrorsCommand::count())),
            "{sql}"
        );
    }
    assert!(parse_optional_show_errors("SHOW ERRORS LIMIT", mode).is_err());
    assert!(parse_optional_show_errors("SHOW ERRORS LIMIT 1,", mode).is_err());
    assert!(parse_optional_show_errors("SHOW COUNT(*) ERRORS LIMIT 1", mode).is_err());
    assert!(parse_optional_show_errors("SHOW COUNT(1) ERRORS", mode).is_err());
    assert!(parse_optional_show_errors("SHOW COUNT(*) ERRORS extra", mode).is_err());
    for sql in [
        "SHOW COUNT(*) WARNINGS",
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
    // Everything an ordinary INSERT refuses, a REPLACE refuses too — and
    // everything it takes, including the SET form, a REPLACE takes.
    for sql in [
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
        (
            "SELECT u.id, a.id FROM users AS u CROSS JOIN accounts AS a",
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
    assert_eq!(
        parse_select(
            "SELECT u.id, a.id FROM users AS u CROSS JOIN accounts AS a",
            SessionSqlMode::default()
        )
        .unwrap()
        .as_sql(),
        "SELECT \"u\".\"id\", \"a\".\"id\" FROM \"users\" AS \"u\" CROSS JOIN \"accounts\" AS \"a\""
    );

    // A `USING` merges the named column into one result column, so the
    // engine's own `USING` is written and the merged name needs no table.
    let merged = parse_select(
        "SELECT id, users.name FROM users JOIN accounts USING (id)",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert_eq!(
        merged.as_sql(),
        concat!(
            "SELECT \"id\", \"users\".\"name\" FROM \"users\" ",
            "JOIN \"accounts\" USING (\"id\")"
        )
    );

    // An unqualified name in a joined projection is left to be resolved
    // against the tables the statement reads, the way MySQL resolves it: the
    // one column that carries it, or an ambiguity refused.
    assert!(parse_select(
        "SELECT id FROM users JOIN accounts ON users.id = accounts.user_id",
        SessionSqlMode::default()
    )
    .is_ok());

    // An ON narrowing the side it joins to goes through the reader a WHERE
    // comparison goes through, so a value stands there as readily as a column.
    assert!(parse_select(
        "SELECT users.id FROM users JOIN accounts ON users.id = accounts.user_id AND users.id = 1",
        SessionSqlMode::default()
    )
    .is_ok());

    for sql in [
        // Matching one column against another is what a join is for, and that
        // is equality. Anything else the ON says is a comparison against a
        // value, which one column against another is not.
        "SELECT users.id FROM users JOIN accounts ON users.id > accounts.user_id",
        // CROSS JOIN takes no ON or USING.
        "SELECT users.id FROM users CROSS JOIN accounts ON users.id = accounts.user_id",
        "SELECT users.id FROM users CROSS JOIN accounts USING (id)",
    ] {
        assert!(
            parse_select(sql, SessionSqlMode::default()).is_err(),
            "{sql}"
        );
    }
}

/// A subquery naming the outer statement's column is a correlated one, and it
/// is written with the same predicate a join is: a column on each side, each
/// saying which table it came from.
#[test]
fn a_subquery_may_name_the_outer_statements_column() {
    let mode = SessionSqlMode::default();
    let exists = parse_select(
        "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.a_id = a.id)",
        mode,
    )
    .unwrap();
    assert_eq!(
        exists.as_sql(),
        concat!(
            "SELECT \"id\" FROM \"a\" WHERE ",
            "(EXISTS (SELECT 1 FROM \"b\" WHERE (\"b\".\"a_id\" = \"a\".\"id\")))"
        )
    );

    // The subquery's table is authorized like any other and names none of the
    // result columns.
    assert_eq!(exists.source_table(), Some("a"));
    assert_eq!(exists.source_tables().len(), 2);

    // A qualified name is the one column an `IN` subquery projects, which is
    // how a correlated one is written.
    let in_subquery = parse_select(
        "SELECT id FROM a WHERE id IN (SELECT b.a_id FROM b WHERE b.a_id = a.id)",
        mode,
    )
    .unwrap();
    assert_eq!(
        in_subquery.as_sql(),
        concat!(
            "SELECT \"id\" FROM \"a\" WHERE ",
            "(\"id\" IN (SELECT \"b\".\"a_id\" FROM \"b\" WHERE (\"b\".\"a_id\" = \"a\".\"id\")))"
        )
    );

    for sql in [
        "SELECT id FROM a WHERE NOT EXISTS (SELECT b.id FROM b WHERE b.a_id = a.id)",
        "SELECT a.name FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.a_id = a.id AND b.tag = 'y')",
    ] {
        assert!(parse_select(sql, mode).is_ok(), "{sql}");
    }
}

/// MySQL changes the rows an `UPDATE` finds through a join, naming the table
/// to change through the columns the SET names.
#[test]
fn a_joined_update_changes_the_rows_the_join_finds() {
    let mode = SessionSqlMode::default();
    let translated = parse_dml("UPDATE a JOIN b ON a.id = b.a_id SET a.n = 0", mode).unwrap();
    assert_eq!(
        translated.as_sql(),
        concat!(
            "UPDATE \"a\" SET \"n\" = 0 WHERE _rowid_ IN (SELECT \"a\"._rowid_ FROM \"a\" ",
            "JOIN \"b\" ON (\"a\".\"id\" = \"b\".\"a_id\"))"
        )
    );
    assert_eq!(translated.read_tables().len(), 2);

    for sql in [
        "UPDATE a JOIN b ON a.id = b.a_id SET a.n = 0 WHERE b.tag = 'z'",
        "UPDATE a LEFT JOIN b ON a.id = b.a_id SET a.n = 0 WHERE b.id IS NULL",
        "UPDATE a JOIN b ON a.id = b.a_id SET b.m = 0",
    ] {
        assert!(parse_dml(sql, mode).is_ok(), "{sql}");
    }

    for sql in [
        // Measured: MySQL answers 1221 for either of these on a joined UPDATE.
        "UPDATE a JOIN b ON a.id = b.a_id SET a.n = 0 ORDER BY a.id",
        "UPDATE a JOIN b ON a.id = b.a_id SET a.n = 0 LIMIT 1",
        // MySQL changes both tables here; each needs its own statement.
        "UPDATE a JOIN b ON a.id = b.a_id SET a.n = 0, b.m = 0",
        // MySQL resolves an unqualified name against the joined tables and
        // answers ambiguous when both carry it; qualifying it says which.
        "UPDATE a JOIN b ON a.id = b.a_id SET n = 0",
        // A value naming another table takes it from whichever row the join
        // happened to find, which is not a rule this answers.
        "UPDATE a JOIN b ON a.id = b.a_id SET a.n = b.m",
        "UPDATE c JOIN b ON c.id = b.a_id SET d.n = 0",
        // sqlparser reads no comma between an UPDATE's tables, so MySQL's
        // comma spelling of a joined UPDATE is refused where the JOIN one is
        // taken.
        "UPDATE a, b SET a.n = 0 WHERE a.id = b.a_id",
    ] {
        assert!(parse_dml(sql, mode).is_err(), "{sql}");
    }
}

/// MySQL names the rows a `DELETE` removes through a join, and names the table
/// to remove them from either in front of the FROM or after a USING. The rows
/// the join finds are the ones to delete, so the join is written as a subquery
/// answering the target's own rowids.
#[test]
fn a_joined_delete_deletes_the_rows_the_join_finds() {
    let mode = SessionSqlMode::default();
    let translated = parse_dml("DELETE a FROM a JOIN b ON a.id = b.a_id", mode).unwrap();
    assert_eq!(
        translated.as_sql(),
        concat!(
            "DELETE FROM \"a\" WHERE _rowid_ IN (SELECT \"a\"._rowid_ FROM \"a\" ",
            "JOIN \"b\" ON (\"a\".\"id\" = \"b\".\"a_id\"))"
        )
    );
    // Both tables are read, so both are authorized.
    assert_eq!(translated.read_tables().len(), 2);

    let using = parse_dml(
        "DELETE FROM a USING a JOIN b ON a.id = b.a_id WHERE b.tag = 'z'",
        mode,
    )
    .unwrap();
    assert_eq!(
        using.as_sql(),
        concat!(
            "DELETE FROM \"a\" WHERE _rowid_ IN (SELECT \"a\"._rowid_ FROM \"a\" ",
            "JOIN \"b\" ON (\"a\".\"id\" = \"b\".\"a_id\") WHERE (\"b\".\"tag\" COLLATE NOCASE = 'z'))"
        )
    );

    for sql in [
        // The comma join and the outer one are the same shape.
        "DELETE a FROM a, b WHERE a.id = b.a_id",
        "DELETE a FROM a LEFT JOIN b ON a.id = b.a_id WHERE b.id IS NULL",
        "DELETE b FROM a JOIN b ON a.id = b.a_id WHERE a.name = 'one'",
        // The target named in front of a single table is the same statement.
        "DELETE a FROM a WHERE id = 2",
    ] {
        assert!(parse_dml(sql, mode).is_ok(), "{sql}");
    }

    for sql in [
        // Measured: MySQL answers 1109 for a target the join does not read,
        // and a syntax error for an ORDER BY on a joined DELETE.
        "DELETE c FROM a JOIN b ON a.id = b.a_id",
        "DELETE a FROM a JOIN b ON a.id = b.a_id ORDER BY a.id",
        // MySQL deletes from both at once here; each would need its own
        // statement to be answered.
        "DELETE a, b FROM a JOIN b ON a.id = b.a_id",
    ] {
        assert!(parse_dml(sql, mode).is_err(), "{sql}");
    }
}

/// MySQL's comma join is a cross join, and a `WHERE` is what bounds it.
#[test]
fn a_comma_join_is_the_cross_join_mysql_means_by_it() {
    let mode = SessionSqlMode::default();
    let translated = parse_select("SELECT users.id FROM users, accounts", mode).unwrap();
    assert_eq!(
        translated.as_sql(),
        "SELECT \"users\".\"id\" FROM \"users\" CROSS JOIN \"accounts\""
    );
    assert_eq!(translated.source_tables().len(), 2);

    let bounded = parse_select(
        "SELECT users.id FROM users, accounts WHERE users.id = accounts.user_id",
        mode,
    )
    .unwrap();
    assert_eq!(
        bounded.as_sql(),
        concat!(
            "SELECT \"users\".\"id\" FROM \"users\" CROSS JOIN \"accounts\" ",
            "WHERE (\"users\".\"id\" = \"accounts\".\"user_id\")"
        )
    );

    // A comparison against a literal names the table its column belongs to,
    // which is what the frontend checks the value's type against.
    let with_literal = parse_select(
        concat!(
            "SELECT users.id FROM users, accounts ",
            "WHERE users.id = accounts.user_id AND accounts.label = 'two'"
        ),
        mode,
    )
    .unwrap();
    assert_eq!(
        with_literal.checked_comparisons()[0].qualifier(),
        Some("accounts")
    );
    // A qualifier naming no table the statement reads is refused.
    assert!(parse_select(
        "SELECT users.id FROM users, accounts WHERE teams.label = 'two'",
        mode
    )
    .is_err());

    // A third table joins the same way, and a comma beside a written join
    // reads the same.
    assert!(parse_select("SELECT users.id FROM users, accounts, teams", mode).is_ok());
    assert!(parse_select(
        "SELECT users.id FROM users JOIN accounts ON users.id = accounts.user_id, teams",
        mode
    )
    .is_ok());
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
        "SELECT team FROM users GROUP BY team HAVING SUM(DISTINCT id) > 1",
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
///
/// A projection aggregates the statement on its own, with no `HAVING` in
/// sight: measured, `SELECT id, SUM(n) FROM t` answers 1140, and so do a
/// column inside arithmetic over the total and a column inside a call beside a
/// count. A window does not aggregate the statement, and neither does a
/// subquery whose aggregate belongs to the statement inside it.
#[test]
fn an_aggregated_projection_refuses_an_ungrouped_column() {
    for sql in [
        "SELECT id, SUM(n) FROM users",
        "SELECT id, COUNT(*) FROM users",
        "SELECT SUM(score), id FROM users",
        "SELECT id + SUM(score) FROM users",
        "SELECT LOWER(name), COUNT(*) FROM users",
        "SELECT IFNULL(SUM(score), 0), id FROM users",
        "SELECT *, COUNT(*) FROM users",
    ] {
        assert!(
            parse_select(sql, SessionSqlMode::default()).is_err(),
            "{sql}"
        );
    }
    for sql in [
        // A literal has no row to come from either.
        "SELECT SUM(score), 1 FROM users",
        "SELECT SUM(score), 'x' FROM users",
        // A window answers a row per row.
        "SELECT id, ROW_NUMBER() OVER (ORDER BY id) FROM users",
        // A subquery's aggregate belongs to the statement inside it.
        "SELECT id, (SELECT COUNT(*) FROM teams) FROM users",
        // A GROUP BY gives every column a group to come from.
        "SELECT id, SUM(score) FROM users GROUP BY id",
        // An ORDER BY is not a projection.
        "SELECT SUM(score) FROM users ORDER BY id",
    ] {
        assert!(
            parse_select(sql, SessionSqlMode::default()).is_ok(),
            "{sql}"
        );
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
        // A wildcard hides whether anything is aggregated.
        "SELECT * FROM users HAVING COUNT(*) > 1",
        // Nothing is aggregated, so this filters rows — but the column it
        // tests is not one the projection carries, which is 1054 as well.
        "SELECT id FROM users HAVING score > 1",
        "SELECT id FROM users WHERE id > 1 HAVING score > 1",
    ] {
        assert!(
            parse_select(sql, SessionSqlMode::default()).is_err(),
            "{sql}"
        );
    }
}

/// An index hint says which key to plan with, so the renderer drops it and
/// answers the keys it named for the frontend to check against the table.
#[test]
fn an_index_hint_renders_away_and_answers_the_keys_it_named() {
    for (sql, normalized, keys) in [
        (
            "SELECT id FROM users FORCE INDEX (PRIMARY) WHERE id > 1",
            "SELECT \"id\" FROM \"users\" WHERE (\"id\" > 1)",
            vec!["PRIMARY"],
        ),
        (
            "SELECT id FROM users USE INDEX (by_name, PRIMARY)",
            "SELECT \"id\" FROM \"users\"",
            vec!["by_name", "PRIMARY"],
        ),
        (
            "SELECT id FROM users IGNORE KEY (by_name)",
            "SELECT \"id\" FROM \"users\"",
            vec!["by_name"],
        ),
        (
            "SELECT id FROM users USE INDEX FOR ORDER BY (PRIMARY) ORDER BY id",
            "SELECT \"id\" FROM \"users\" ORDER BY \"id\" ASC",
            vec!["PRIMARY"],
        ),
        (
            "SELECT id FROM users t USE INDEX (by_name)",
            "SELECT \"id\" FROM \"users\" AS \"t\"",
            vec!["by_name"],
        ),
    ] {
        let translated = parse_select(sql, SessionSqlMode::default()).unwrap();
        assert_eq!(translated.as_sql(), normalized, "{sql}");
        assert!(translated.parse_ast().is_ok(), "{sql}");
        let [source] = translated.source_tables() else {
            panic!("one source table: {sql}");
        };
        assert_eq!(source.hinted_indexes(), keys.as_slice(), "{sql}");
    }
    // A statement without a hint names no keys at all.
    let plain = parse_select("SELECT id FROM users", SessionSqlMode::default()).unwrap();
    assert!(plain.source_tables()[0].hinted_indexes().is_empty());
}

/// The moment a `DATE_FORMAT` writes out or a `STR_TO_DATE` reads need not be
/// a column: a clock reading and a word written out are moments too, and each
/// is spelled the way the engine spells it.
#[test]
fn a_moment_argument_renders_from_a_column_a_clock_or_a_word() {
    for (sql, normalized) in [
        (
            "SELECT DATE_FORMAT(m, '%Y-%m-%d') FROM mf",
            "SELECT mysql_date_format(\"m\", '%Y-%m-%d') AS \"DATE_FORMAT(m, '%Y-%m-%d')\" FROM \"mf\"",
        ),
        (
            "SELECT DATE_FORMAT(NOW(), '%Y-%m-%d')",
            "SELECT mysql_date_format(datetime('now'), '%Y-%m-%d') AS \"DATE_FORMAT(NOW(), '%Y-%m-%d')\"",
        ),
        (
            "SELECT DATE_FORMAT(CURDATE(), '%Y-%m-%d')",
            "SELECT mysql_date_format(date('now'), '%Y-%m-%d') AS \"DATE_FORMAT(CURDATE(), '%Y-%m-%d')\"",
        ),
        (
            "SELECT DATE_FORMAT('2024-03-05', '%Y-%m-%d')",
            "SELECT mysql_date_format('2024-03-05', '%Y-%m-%d') AS \"DATE_FORMAT('2024-03-05', '%Y-%m-%d')\"",
        ),
        (
            "SELECT STR_TO_DATE('2024-03-05', '%Y-%m-%d')",
            "SELECT mysql_str_to_date('2024-03-05', '%Y-%m-%d') AS \"STR_TO_DATE('2024-03-05', '%Y-%m-%d')\"",
        ),
    ] {
        let translated = parse_select(sql, SessionSqlMode::default()).unwrap();
        assert_eq!(translated.as_sql(), normalized, "{sql}");
        assert!(translated.parse_ast().is_ok(), "{sql}");
    }
    for sql in [
        // A clock reading is a moment, not the text a STR_TO_DATE reads.
        "SELECT STR_TO_DATE(NOW(), '%Y-%m-%d')",
        // A reading that holds no day.
        "SELECT DATE_FORMAT(CURTIME(), '%Y-%m-%d')",
        // A call inside the call is none of the three shapes.
        "SELECT DATE_FORMAT(DATE(m), '%Y-%m-%d') FROM mf",
        // The format still has to be written out.
        "SELECT DATE_FORMAT(NOW(), f) FROM mf",
    ] {
        assert!(
            parse_select(sql, SessionSqlMode::default()).is_err(),
            "{sql}"
        );
    }
}

/// An aggregate stands where a column stands in arithmetic, so `SUM(n) + 1`
/// renders as the engine spells the sum plus one. A division keeps its cast,
/// which is what makes MySQL's decimal division out of the engine's integer
/// one.
#[test]
fn arithmetic_renders_an_aggregate_operand() {
    for (sql, normalized) in [
        (
            "SELECT SUM(n) + 1 FROM ar",
            "SELECT (SUM(\"n\") + 1) AS \"SUM(n) + 1\" FROM \"ar\"",
        ),
        (
            "SELECT COUNT(*) * 2 FROM ar",
            "SELECT (COUNT(*) * 2) AS \"COUNT(*) * 2\" FROM \"ar\"",
        ),
        (
            "SELECT SUM(n) + SUM(m) FROM ar",
            "SELECT (SUM(\"n\") + SUM(\"m\")) AS \"SUM(n) + SUM(m)\" FROM \"ar\"",
        ),
        (
            "SELECT SUM(n) / 2 FROM ar",
            "SELECT (CAST(SUM(\"n\") AS REAL) / 2) AS \"SUM(n) / 2\" FROM \"ar\"",
        ),
    ] {
        let translated = parse_select(sql, SessionSqlMode::default()).unwrap();
        assert_eq!(translated.as_sql(), normalized, "{sql}");
        assert!(translated.parse_ast().is_ok(), "{sql}");
    }
    for sql in [
        // A GROUP_CONCAT is not a number, and neither is a deviation here.
        "SELECT GROUP_CONCAT(name) + 1 FROM ar",
        "SELECT STDDEV_SAMP(n) + 1 FROM ar",
        // A windowed aggregate is a window rather than an aggregate.
        "SELECT SUM(n) OVER () + 1 FROM ar",
    ] {
        assert!(
            parse_select(sql, SessionSqlMode::default()).is_err(),
            "{sql}"
        );
    }
}

/// A window naming neither a partition nor an order is the whole result as one
/// frame. An aggregate over it answers the same value for every row, whatever
/// order the rows came in; a ranking does not, and keeps its refusal.
#[test]
fn a_window_over_the_whole_set_renders_with_nothing_in_it() {
    for (sql, normalized) in [
        (
            "SELECT id, COUNT(*) OVER () FROM wf",
            "SELECT \"id\", count(*) OVER () AS \"COUNT(*) OVER ()\" FROM \"wf\"",
        ),
        (
            "SELECT SUM(n) OVER () FROM wf",
            "SELECT sum(\"n\") OVER () AS \"SUM(n) OVER ()\" FROM \"wf\"",
        ),
        (
            "SELECT AVG(n) OVER () FROM wf",
            "SELECT avg(\"n\") OVER () AS \"AVG(n) OVER ()\" FROM \"wf\"",
        ),
    ] {
        let translated = parse_select(sql, SessionSqlMode::default()).unwrap();
        assert_eq!(translated.as_sql(), normalized, "{sql}");
        assert!(translated.parse_ast().is_ok(), "{sql}");
    }
    for sql in [
        // A ranking over an empty window has no order to rank by.
        "SELECT ROW_NUMBER() OVER () FROM wf",
        "SELECT RANK() OVER () FROM wf",
        "SELECT DENSE_RANK() OVER () FROM wf",
        "SELECT PERCENT_RANK() OVER () FROM wf",
        "SELECT NTILE(2) OVER () FROM wf",
        "SELECT LAG(n) OVER () FROM wf",
    ] {
        assert!(
            parse_select(sql, SessionSqlMode::default()).is_err(),
            "{sql}"
        );
    }
}

/// An `ON DUPLICATE KEY UPDATE` value may join the row already there to the
/// one offered. `VALUES(col)` names the offered one, and so does a name put on
/// it; the engine calls it `excluded.col` either way, and a bare column is the
/// row already there.
#[test]
fn an_upsert_renders_the_row_it_was_offered() {
    for (sql, normalized) in [
        (
            "INSERT INTO up (id, hits) VALUES (2, 3) ON DUPLICATE KEY UPDATE hits = hits + 1",
            "INSERT INTO \"up\" (\"id\", \"hits\") VALUES (2, 3) ON CONFLICT DO UPDATE SET \"hits\" = (\"hits\" + 1)",
        ),
        (
            "INSERT INTO up (id, hits) VALUES (2, 3) ON DUPLICATE KEY UPDATE hits = hits + VALUES(hits)",
            "INSERT INTO \"up\" (\"id\", \"hits\") VALUES (2, 3) ON CONFLICT DO UPDATE SET \"hits\" = (\"hits\" + \"excluded\".\"hits\")",
        ),
        (
            "INSERT INTO up (id, hits) VALUES (2, 3) AS offered ON DUPLICATE KEY UPDATE hits = up.hits + offered.hits",
            "INSERT INTO \"up\" (\"id\", \"hits\") VALUES (2, 3) ON CONFLICT DO UPDATE SET \"hits\" = (\"hits\" + \"excluded\".\"hits\")",
        ),
    ] {
        let translated = parse_dml(sql, SessionSqlMode::default()).unwrap();
        assert_eq!(translated.as_sql(), normalized, "{sql}");
        assert!(translated.parse_ast().is_ok(), "{sql}");
    }
    for sql in [
        // 1052 in MySQL: ambiguous once the offered row carries a name.
        "INSERT INTO up (id, hits) VALUES (2, 3) AS offered ON DUPLICATE KEY UPDATE hits = hits + offered.hits",
        // A qualifier naming neither of the two rows.
        "INSERT INTO up (id, hits) VALUES (2, 3) ON DUPLICATE KEY UPDATE hits = other.hits",
        // A name on the offered row with no upsert to use it.
        "INSERT INTO up (id, hits) VALUES (4, 1) AS offered",
        // Renaming what the offered row carries has not been measured.
        "INSERT INTO up (id, hits) VALUES (2, 3) AS offered (a, b) ON DUPLICATE KEY UPDATE hits = offered.b",
    ] {
        assert!(parse_dml(sql, SessionSqlMode::default()).is_err(), "{sql}");
    }
}

/// An `UPDATE` may write a value worked out from the row. A call and a `CASE`
/// are rendered the way a projection renders them, and a value reading a
/// column the same `SET` has already written is refused, MySQL taking the
/// assignments left to right where the engine reads the row as it stood.
#[test]
fn an_update_renders_a_call_in_its_set() {
    for (sql, normalized) in [
        (
            "UPDATE users SET name = LOWER(name) WHERE id = 1",
            "UPDATE \"users\" SET \"name\" = lower(\"name\") WHERE (\"id\" = 1)",
        ),
        (
            "UPDATE users SET name = CONCAT(name, 'x') WHERE id = 1",
            "UPDATE \"users\" SET \"name\" = (\"name\" || 'x') WHERE (\"id\" = 1)",
        ),
        (
            "UPDATE users SET score = IFNULL(score, 0) WHERE id = 1",
            "UPDATE \"users\" SET \"score\" = ifnull(\"score\", 0) WHERE (\"id\" = 1)",
        ),
        (
            "UPDATE users SET name = TRIM(name) WHERE id = 1",
            "UPDATE \"users\" SET \"name\" = trim(\"name\") WHERE (\"id\" = 1)",
        ),
    ] {
        let translated = parse_dml(sql, SessionSqlMode::default()).unwrap();
        assert_eq!(translated.as_sql(), normalized, "{sql}");
        assert!(translated.parse_ast().is_ok(), "{sql}");
    }
    for sql in [
        // The value reads a column this SET has already written.
        "UPDATE users SET name = 'q', score = CHAR_LENGTH(name) WHERE id = 1",
        "UPDATE users SET name = 'q', team = CONCAT(name, 'x') WHERE id = 1",
        // A call this does not read at all.
        "UPDATE users SET name = SOUNDEX(name) WHERE id = 1",
    ] {
        assert!(parse_dml(sql, SessionSqlMode::default()).is_err(), "{sql}");
    }
}

/// A comparison may name a collation, on the column or on the value.
/// `utf8mb4_bin` compares the bytes, which is what the engine does with no
/// collation asked for; the case-ignoring ones are the NOCASE a text
/// comparison already gets.
#[test]
fn a_comparison_renders_the_collation_it_names() {
    for (sql, normalized) in [
        (
            "SELECT id FROM users WHERE name = 'a' COLLATE utf8mb4_bin",
            "SELECT \"id\" FROM \"users\" WHERE (\"name\" = 'a')",
        ),
        (
            "SELECT id FROM users WHERE name COLLATE utf8mb4_bin = 'a'",
            "SELECT \"id\" FROM \"users\" WHERE (\"name\" = 'a')",
        ),
        (
            "SELECT id FROM users WHERE name = 'a' COLLATE utf8mb4_0900_ai_ci",
            "SELECT \"id\" FROM \"users\" WHERE (\"name\" COLLATE NOCASE = 'a')",
        ),
        (
            "SELECT id FROM users WHERE name < 'a' COLLATE utf8mb4_bin",
            "SELECT \"id\" FROM \"users\" WHERE (\"name\" < 'a')",
        ),
    ] {
        let translated = parse_select(sql, SessionSqlMode::default()).unwrap();
        assert_eq!(translated.as_sql(), normalized, "{sql}");
        assert!(translated.parse_ast().is_ok(), "{sql}");
    }
    for sql in [
        "SELECT id FROM users WHERE name = 'a' COLLATE latin1_swedish_ci",
        "SELECT id FROM users WHERE id = 1 COLLATE utf8mb4_bin",
        "SELECT id FROM users WHERE name = ? COLLATE utf8mb4_bin",
        "SELECT id FROM users WHERE name LIKE 'a' COLLATE utf8mb4_bin",
        "SELECT id FROM users WHERE name IN ('a' COLLATE utf8mb4_bin)",
    ] {
        assert!(
            parse_select(sql, SessionSqlMode::default()).is_err(),
            "{sql}"
        );
    }
}

/// An ordering may name a collation. `utf8mb4_bin` is the engine's own byte
/// order, so the ordering asks for no collation at all; the two case-ignoring
/// collations are what the engine calls NOCASE, which a bare text column
/// already gets.
#[test]
fn an_ordering_renders_the_collation_it_names() {
    for (sql, normalized) in [
        (
            "SELECT id FROM users ORDER BY name COLLATE utf8mb4_bin",
            "SELECT \"id\" FROM \"users\" ORDER BY \"name\" ASC",
        ),
        (
            "SELECT id FROM users ORDER BY name COLLATE utf8mb4_bin DESC",
            "SELECT \"id\" FROM \"users\" ORDER BY \"name\" DESC",
        ),
        (
            "SELECT id FROM users ORDER BY name COLLATE utf8mb4_0900_ai_ci",
            "SELECT \"id\" FROM \"users\" ORDER BY \"name\" ASC",
        ),
    ] {
        let translated = parse_select(sql, SessionSqlMode::default()).unwrap();
        assert_eq!(translated.as_sql(), normalized, "{sql}");
        assert!(translated.parse_ast().is_ok(), "{sql}");
    }
    for sql in [
        // 1253 in MySQL: the collation belongs to another character set.
        "SELECT id FROM users ORDER BY name COLLATE latin1_swedish_ci",
        "SELECT id FROM users ORDER BY name COLLATE utf8mb4_unicode_ci",
        // A collation over something that is not a column.
        "SELECT id FROM users ORDER BY LOWER(name) COLLATE utf8mb4_bin",
    ] {
        assert!(
            parse_select(sql, SessionSqlMode::default()).is_err(),
            "{sql}"
        );
    }
}

/// The engine has no radix writing and no reading by place, so the dialect
/// answers all four. Each holds its arguments to the shapes its answer's width
/// is worked out from.
#[test]
fn the_radix_writings_and_the_readings_by_place_render_to_the_dialect() {
    for (sql, normalized) in [
        (
            "SELECT BIN(n) FROM bo",
            "SELECT mysql_bin(\"n\") AS \"BIN(n)\" FROM \"bo\"",
        ),
        (
            "SELECT OCT(n) FROM bo",
            "SELECT mysql_oct(\"n\") AS \"OCT(n)\" FROM \"bo\"",
        ),
        (
            "SELECT FIELD(name, 'a', 'b') FROM bo",
            "SELECT mysql_field(\"name\", 'a', 'b') AS \"FIELD(name, 'a', 'b')\" FROM \"bo\"",
        ),
        (
            "SELECT ELT(n, 'a', 'bb') FROM bo",
            "SELECT mysql_elt(\"n\", 'a', 'bb') AS \"ELT(n, 'a', 'bb')\" FROM \"bo\"",
        ),
    ] {
        let translated = parse_select(sql, SessionSqlMode::default()).unwrap();
        assert_eq!(translated.as_sql(), normalized, "{sql}");
        assert!(translated.parse_ast().is_ok(), "{sql}");
    }
    for sql in [
        // The choices say how wide the answer can be, so they are written out.
        "SELECT FIELD(name, n) FROM bo",
        "SELECT ELT(n, name) FROM bo",
        "SELECT ELT(n) FROM bo",
        "SELECT FIELD(name) FROM bo",
        // The thing read has to be a column.
        "SELECT BIN(1) FROM bo",
        "SELECT FIELD('a', 'a', 'b') FROM bo",
    ] {
        assert!(
            parse_select(sql, SessionSqlMode::default()).is_err(),
            "{sql}"
        );
    }
}

/// `PI()` reads nothing and answers a constant, rounded to the six places
/// MySQL reports for it. `DEGREES` and `RADIANS` are spelled the same in both.
/// The readings a maths library rounds for itself are refused.
#[test]
fn the_math_readings_render_as_the_engine_spells_them() {
    for (sql, normalized) in [
        ("SELECT PI()", "SELECT round(pi(), 6) AS \"PI()\""),
        (
            "SELECT DEGREES(x) FROM mt",
            "SELECT degrees(\"x\") AS \"DEGREES(x)\" FROM \"mt\"",
        ),
        (
            "SELECT RADIANS(x) FROM mt",
            "SELECT radians(\"x\") AS \"RADIANS(x)\" FROM \"mt\"",
        ),
        (
            "SELECT SQRT(x) FROM mt",
            "SELECT sqrt(\"x\") AS \"SQRT(x)\" FROM \"mt\"",
        ),
    ] {
        let translated = parse_select(sql, SessionSqlMode::default()).unwrap();
        assert_eq!(translated.as_sql(), normalized, "{sql}");
        assert!(translated.parse_ast().is_ok(), "{sql}");
    }
    for sql in [
        "SELECT SIN(x) FROM mt",
        "SELECT ATAN(x) FROM mt",
        "SELECT LOG(x) FROM mt",
        "SELECT EXP(x) FROM mt",
        // `PI` reads nothing, so anything in its parentheses is not it.
        "SELECT PI(x) FROM mt",
    ] {
        assert!(
            parse_select(sql, SessionSqlMode::default()).is_err(),
            "{sql}"
        );
    }
}

/// `TIMESTAMPDIFF` counts whole units by dividing the seconds between the two
/// moments, which truncates the way MySQL truncates. Only the units of fixed
/// length are taken.
#[test]
fn timestampdiff_renders_as_the_seconds_between_divided() {
    for (sql, normalized) in [
        (
            "SELECT TIMESTAMPDIFF(SECOND, a, b) FROM td",
            "SELECT CAST((unixepoch(\"b\") - unixepoch(\"a\")) / 1 AS INTEGER) AS \"TIMESTAMPDIFF(SECOND, a, b)\" FROM \"td\"",
        ),
        (
            "SELECT TIMESTAMPDIFF(DAY, a, b) FROM td",
            "SELECT CAST((unixepoch(\"b\") - unixepoch(\"a\")) / 86400 AS INTEGER) AS \"TIMESTAMPDIFF(DAY, a, b)\" FROM \"td\"",
        ),
        (
            "SELECT TIMESTAMPDIFF(WEEK, a, b) FROM td",
            "SELECT CAST((unixepoch(\"b\") - unixepoch(\"a\")) / 604800 AS INTEGER) AS \"TIMESTAMPDIFF(WEEK, a, b)\" FROM \"td\"",
        ),
    ] {
        let translated = parse_select(sql, SessionSqlMode::default()).unwrap();
        assert_eq!(translated.as_sql(), normalized, "{sql}");
        assert!(translated.parse_ast().is_ok(), "{sql}");
    }
    for sql in [
        // Counted by the calendar rather than by a length.
        "SELECT TIMESTAMPDIFF(MONTH, a, b) FROM td",
        "SELECT TIMESTAMPDIFF(QUARTER, a, b) FROM td",
        "SELECT TIMESTAMPDIFF(YEAR, a, b) FROM td",
        // Finer than either side carries.
        "SELECT TIMESTAMPDIFF(MICROSECOND, a, b) FROM td",
        // Both moments have to be columns.
        "SELECT TIMESTAMPDIFF(DAY, a, NOW()) FROM td",
    ] {
        assert!(
            parse_select(sql, SessionSqlMode::default()).is_err(),
            "{sql}"
        );
    }
}

/// The calendar readings the engine has no name for are counted off what it
/// does have: the quarter off the month, the two weekday numberings off its
/// own, and the day a month ends on by walking to the next month and back.
/// `EXTRACT(<field> FROM ...)` reads what the call spelling of that field
/// reads, and MySQL names the column after the whole of it.
#[test]
fn the_calendar_readings_render_as_the_engine_counts_them() {
    for (sql, normalized) in [
        (
            "SELECT QUARTER(d) FROM cal",
            "SELECT ((CAST(strftime('%m', \"d\") AS INTEGER) + 2) / 3) AS \"QUARTER(d)\" FROM \"cal\"",
        ),
        (
            "SELECT WEEKDAY(d) FROM cal",
            "SELECT ((CAST(strftime('%w', \"d\") AS INTEGER) + 6) % 7) AS \"WEEKDAY(d)\" FROM \"cal\"",
        ),
        (
            "SELECT DAYOFWEEK(d) FROM cal",
            "SELECT (CAST(strftime('%w', \"d\") AS INTEGER) + 1) AS \"DAYOFWEEK(d)\" FROM \"cal\"",
        ),
        (
            "SELECT DAYOFYEAR(d) FROM cal",
            "SELECT CAST(strftime('%j', \"d\") AS INTEGER) AS \"DAYOFYEAR(d)\" FROM \"cal\"",
        ),
        (
            "SELECT DAYOFMONTH(d) FROM cal",
            "SELECT CAST(strftime('%d', \"d\") AS INTEGER) AS \"DAYOFMONTH(d)\" FROM \"cal\"",
        ),
        (
            "SELECT LAST_DAY(d) FROM cal",
            "SELECT date(\"d\", 'start of month', '+1 month', '-1 day') AS \"LAST_DAY(d)\" FROM \"cal\"",
        ),
        (
            "SELECT EXTRACT(YEAR FROM d) FROM cal",
            "SELECT CAST(strftime('%Y', \"d\") AS INTEGER) AS \"EXTRACT(YEAR FROM d)\" FROM \"cal\"",
        ),
        (
            "SELECT EXTRACT(SECOND FROM m) FROM cal",
            "SELECT CAST(strftime('%S', \"m\") AS INTEGER) AS \"EXTRACT(SECOND FROM m)\" FROM \"cal\"",
        ),
    ] {
        let translated = parse_select(sql, SessionSqlMode::default()).unwrap();
        assert_eq!(translated.as_sql(), normalized, "{sql}");
        assert!(translated.parse_ast().is_ok(), "{sql}");
    }
    for sql in [
        // A field the engine counts by rules of its own.
        "SELECT EXTRACT(WEEK FROM d) FROM cal",
        "SELECT EXTRACT(QUARTER FROM d) FROM cal",
        // A reading over something that is not a column.
        "SELECT QUARTER(NOW()) FROM cal",
        "SELECT EXTRACT(YEAR FROM NOW()) FROM cal",
    ] {
        assert!(
            parse_select(sql, SessionSqlMode::default()).is_err(),
            "{sql}"
        );
    }
}

/// `REGEXP` and `RLIKE` are one thing under two spellings, and the dialect
/// answers both: the engine keeps its own matching in an extension this
/// frontend does not register, and MySQL holds the match to a collation rather
/// than to the pattern.
#[test]
fn a_regexp_renders_as_the_dialect_answers_it() {
    for (sql, normalized) in [
        (
            "SELECT id FROM users WHERE name REGEXP 'a.c'",
            "SELECT \"id\" FROM \"users\" WHERE (mysql_regexp(\"name\", 'a.c'))",
        ),
        (
            "SELECT id FROM users WHERE name RLIKE '^a'",
            "SELECT \"id\" FROM \"users\" WHERE (mysql_regexp(\"name\", '^a'))",
        ),
        (
            "SELECT id FROM users WHERE name NOT REGEXP 'a'",
            "SELECT \"id\" FROM \"users\" WHERE (NOT mysql_regexp(\"name\", 'a'))",
        ),
        (
            "SELECT id FROM users u WHERE u.name REGEXP 'a'",
            "SELECT \"id\" FROM \"users\" AS \"u\" WHERE (mysql_regexp(\"u\".\"name\", 'a'))",
        ),
    ] {
        let translated = parse_select(sql, SessionSqlMode::default()).unwrap();
        assert_eq!(translated.as_sql(), normalized, "{sql}");
        assert!(translated.parse_ast().is_ok(), "{sql}");
    }
    for sql in [
        // A bound pattern carries nothing until it binds.
        "SELECT id FROM users WHERE name REGEXP ?",
        // The thing matched has to be a column.
        "SELECT id FROM users WHERE LOWER(name) REGEXP 'a'",
        "SELECT id FROM users WHERE 'a' REGEXP 'a'",
    ] {
        assert!(
            parse_select(sql, SessionSqlMode::default()).is_err(),
            "{sql}"
        );
    }
}

/// A `LIKE` pattern may be written in pieces. Pieces that are all written join
/// into the one pattern they spell, which is the pattern MySQL matches; a
/// bound piece has to stay a piece, so the pieces are joined for the engine
/// instead. A piece naming a column, and more than one bound piece, are
/// refused.
#[test]
fn a_like_pattern_renders_from_the_pieces_it_is_written_in() {
    for (sql, normalized) in [
        (
            "SELECT id FROM users WHERE name LIKE CONCAT('%', 'lph', '%')",
            "SELECT \"id\" FROM \"users\" WHERE (\"name\" LIKE '%lph%' ESCAPE '\\')",
        ),
        (
            "SELECT id FROM users WHERE name LIKE '%lph%'",
            "SELECT \"id\" FROM \"users\" WHERE (\"name\" LIKE '%lph%' ESCAPE '\\')",
        ),
        (
            "SELECT id FROM users WHERE name NOT LIKE CONCAT('al', '%')",
            "SELECT \"id\" FROM \"users\" WHERE (\"name\" NOT LIKE 'al%' ESCAPE '\\')",
        ),
        // A backslash reads the same in either engine once the escape is
        // named, whichever piece it is written in.
        (
            "SELECT id FROM users WHERE name LIKE CONCAT('a\\\\b', '%')",
            "SELECT \"id\" FROM \"users\" WHERE (\"name\" LIKE 'a\\b%' ESCAPE '\\')",
        ),
        (
            "SELECT id FROM users WHERE name LIKE CONCAT('%', ?, '%')",
            "SELECT \"id\" FROM \"users\" WHERE (\"name\" LIKE ('%' || ? || '%') ESCAPE '\\')",
        ),
        // A pattern written as one `?` keeps the spelling it always had.
        (
            "SELECT id FROM users WHERE name LIKE ?",
            "SELECT \"id\" FROM \"users\" WHERE (\"name\" LIKE ? ESCAPE '\\')",
        ),
    ] {
        let translated = parse_select(sql, SessionSqlMode::default()).unwrap();
        assert_eq!(translated.as_sql(), normalized, "{sql}");
        assert!(translated.parse_ast().is_ok(), "{sql}");
    }
    for sql in [
        "SELECT id FROM users WHERE name LIKE CONCAT('%', name, '%')",
        "SELECT id FROM users WHERE name LIKE CONCAT('%', NULL, '%')",
        "SELECT id FROM users WHERE name LIKE CONCAT(?, ?)",
        "SELECT id FROM users WHERE name LIKE LOWER('%a%')",
    ] {
        assert!(
            parse_select(sql, SessionSqlMode::default()).is_err(),
            "{sql}"
        );
    }
}

/// A subquery answering one value stands where a value stands. Only an
/// aggregate over one implicit group answers one row, so that is the only
/// projection taken, and a `MIN` or `MAX` records the two columns for the
/// frontend to hold to the same kind.
#[test]
fn a_comparison_renders_a_subquery_that_answers_one_value() {
    for (sql, normalized) in [
        (
            "SELECT id FROM users WHERE score = (SELECT MAX(score) FROM users)",
            "SELECT \"id\" FROM \"users\" WHERE (\"score\" = (SELECT MAX(\"score\") AS \"MAX(score)\" FROM \"users\"))",
        ),
        (
            "SELECT id FROM users WHERE (SELECT COUNT(*) FROM teams) > 0",
            "SELECT \"id\" FROM \"users\" WHERE ((SELECT COUNT(*) AS \"COUNT(*)\" FROM \"teams\") > 0)",
        ),
        (
            "SELECT id FROM users WHERE 0 < (SELECT COUNT(*) FROM teams)",
            "SELECT \"id\" FROM \"users\" WHERE (0 < (SELECT COUNT(*) AS \"COUNT(*)\" FROM \"teams\"))",
        ),
    ] {
        let translated = parse_select(sql, SessionSqlMode::default()).unwrap();
        assert_eq!(translated.as_sql(), normalized, "{sql}");
        assert!(translated.parse_ast().is_ok(), "{sql}");
    }

    // A MIN or MAX records the pair the frontend holds to the same kind.
    let translated = parse_select(
        "SELECT id FROM users WHERE score = (SELECT MAX(points) FROM teams)",
        SessionSqlMode::default(),
    )
    .unwrap();
    let [pair] = translated.checked_subquery_comparisons() else {
        panic!("one subquery comparison is recorded");
    };
    assert_eq!(pair.column_name(), "score");
    assert_eq!(pair.inner_table(), "teams");
    assert_eq!(pair.inner_column_name(), "points");

    for sql in [
        // 1242 in MySQL: a plain column can answer more than one row.
        "SELECT id FROM users WHERE id = (SELECT owner_id FROM teams)",
        // MySQL rounds AVG to four places; the engine keeps the fraction.
        "SELECT id FROM users WHERE score > (SELECT AVG(score) FROM users)",
        // A grouped subquery answers a row per group.
        "SELECT id FROM users WHERE score = (SELECT MAX(score) FROM users GROUP BY team)",
        // A COUNT meets a whole number and nothing else.
        "SELECT id FROM users WHERE name = (SELECT COUNT(*) FROM teams)",
    ] {
        assert!(
            parse_select(sql, SessionSqlMode::default()).is_err(),
            "{sql}"
        );
    }
}

/// A comparison between two whole numbers names no column, so it is written
/// out as it stands rather than checked against one. That is the opening a
/// statement built up in pieces uses — `WHERE 1 = 1 AND ...` — and it holds in
/// a `SELECT` and in a statement that writes.
#[test]
fn a_comparison_between_whole_numbers_renders_as_written() {
    for (sql, normalized) in [
        (
            "SELECT id FROM users WHERE 1 = 1",
            "SELECT \"id\" FROM \"users\" WHERE (1 = 1)",
        ),
        (
            "SELECT id FROM users WHERE 1 = 1 AND id > 1",
            "SELECT \"id\" FROM \"users\" WHERE ((1 = 1) AND (\"id\" > 1))",
        ),
        (
            "SELECT id FROM users WHERE 1",
            "SELECT \"id\" FROM \"users\" WHERE 1",
        ),
        (
            "SELECT id FROM users WHERE -1 < 0",
            "SELECT \"id\" FROM \"users\" WHERE ((-1) < 0)",
        ),
    ] {
        let translated = parse_select(sql, SessionSqlMode::default()).unwrap();
        assert_eq!(translated.as_sql(), normalized, "{sql}");
        assert!(translated.parse_ast().is_ok(), "{sql}");
    }
    let deleted = parse_dml(
        "DELETE FROM users WHERE 1 = 1 AND id = 3",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert_eq!(
        deleted.as_sql(),
        "DELETE FROM \"users\" WHERE ((1 = 1) AND (\"id\" = 3))"
    );
    assert!(deleted.parse_ast().is_ok());

    for sql in [
        // MySQL compares two words without regard to case and coerces a word
        // to a number; the engine does neither, so these stay refused.
        "SELECT id FROM users WHERE 'a' = 'A'",
        "SELECT id FROM users WHERE 1 = '1'",
        "SELECT id FROM users WHERE NULL = NULL",
        // A number with a fraction has not been measured here.
        "SELECT id FROM users WHERE 1.5 = 1.5",
    ] {
        assert!(
            parse_select(sql, SessionSqlMode::default()).is_err(),
            "{sql}"
        );
    }
}

/// `IFNULL` and `COALESCE` take an aggregate as the thing they default, which
/// is how a report writes a total over rows that may not be there. The engine
/// spells both calls the same way, so the aggregate is written inside as it
/// would be on its own and MySQL's own name for the column is the alias.
#[test]
fn a_defaulted_aggregate_renders_the_aggregate_inside_the_call() {
    for (sql, normalized) in [
        (
            "SELECT IFNULL(SUM(n), 0) FROM users",
            "SELECT ifnull(SUM(\"n\"), 0) AS \"IFNULL(SUM(n), 0)\" FROM \"users\"",
        ),
        (
            "SELECT COALESCE(MAX(score), 0) FROM users",
            "SELECT coalesce(MAX(\"score\"), 0) AS \"COALESCE(MAX(score), 0)\" FROM \"users\"",
        ),
        (
            "SELECT IFNULL(COUNT(*), 0) FROM users",
            "SELECT ifnull(COUNT(*), 0) AS \"IFNULL(COUNT(*), 0)\" FROM \"users\"",
        ),
        (
            "SELECT IFNULL(SUM(n), 0) AS total FROM users",
            "SELECT ifnull(SUM(\"n\"), 0) AS \"total\" FROM \"users\"",
        ),
    ] {
        let translated = parse_select(sql, SessionSqlMode::default()).unwrap();
        assert_eq!(translated.as_sql(), normalized, "{sql}");
        assert!(translated.parse_ast().is_ok(), "{sql}");
    }
    for sql in [
        // The fallback has to be a whole number, which is the rule the column
        // form already follows.
        "SELECT IFNULL(SUM(n), 'none') FROM users",
        // Only an aggregate goes inside; a call over a call is not measured.
        "SELECT IFNULL(LOWER(name), 0) FROM users",
        // NULLIF answers the other way round and is not this shape.
        "SELECT NULLIF(SUM(n), 0) FROM users",
    ] {
        assert!(
            parse_select(sql, SessionSqlMode::default()).is_err(),
            "{sql}"
        );
    }
}

/// An `UPDATE` or `DELETE` may name its rows through a subquery, and the table
/// the subquery reads comes back as a table the statement reads so the caller
/// authorizes it. A subquery reading the table being changed answers 1051 —
/// 1093 in MySQL — and is refused rather than rendered.
#[test]
fn a_dml_subquery_renders_and_answers_the_table_it_reads() {
    let translated = parse_dml(
        "DELETE FROM users WHERE id IN (SELECT owner_id FROM teams)",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert_eq!(
        translated.as_sql(),
        "DELETE FROM \"users\" WHERE (\"id\" IN (SELECT \"owner_id\" FROM \"teams\"))"
    );
    assert_eq!(
        translated
            .read_tables()
            .iter()
            .map(|source| source.table().as_str().to_owned())
            .collect::<Vec<_>>(),
        vec!["teams".to_owned()]
    );
    assert!(translated.parse_ast().is_ok());

    let existed = parse_dml(
        "UPDATE users SET score = 0 WHERE EXISTS (SELECT 1 FROM teams WHERE teams.owner_id = users.id)",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert_eq!(
        existed
            .read_tables()
            .iter()
            .map(|source| source.table().as_str().to_owned())
            .collect::<Vec<_>>(),
        vec!["teams".to_owned()]
    );
    assert!(existed.parse_ast().is_ok());

    for sql in [
        "DELETE FROM users WHERE id IN (SELECT id FROM users WHERE score > 100)",
        "UPDATE users SET score = 0 WHERE id IN (SELECT id FROM users WHERE score > 100)",
    ] {
        assert!(parse_dml(sql, SessionSqlMode::default()).is_err(), "{sql}");
    }
}

/// `a.*` names one source's columns, and the engine spells it the same way.
/// A qualifier that is more than a source name — `db.t.*` — and a wildcard
/// carrying an option are refused rather than rendered.
#[test]
fn a_qualified_wildcard_renders_over_the_source_it_names() {
    for (sql, normalized) in [
        (
            "SELECT a.* FROM users a",
            "SELECT \"a\".* FROM \"users\" AS \"a\"",
        ),
        (
            "SELECT a.*, b.id FROM users a JOIN teams b ON b.id = a.id",
            "SELECT \"a\".*, \"b\".\"id\" FROM \"users\" AS \"a\" JOIN \"teams\" AS \"b\" ON (\"b\".\"id\" = \"a\".\"id\")",
        ),
    ] {
        let translated = parse_select(sql, SessionSqlMode::default()).unwrap();
        assert_eq!(translated.as_sql(), normalized, "{sql}");
        assert!(translated.parse_ast().is_ok(), "{sql}");
    }
    for sql in [
        "SELECT reports.users.* FROM users",
        "SELECT a.* EXCEPT (id) FROM users a",
    ] {
        assert!(
            parse_select(sql, SessionSqlMode::default()).is_err(),
            "{sql}"
        );
    }
}

/// A `HAVING` over a statement that groups nothing and aggregates nothing
/// filters rows, not groups. Measured on MySQL 8.4.11 over rows
/// (1,5), (2,3), (3,9), (4,NULL): `SELECT id FROM t HAVING id > 1` answers
/// 2, 3 and 4. The engine reads a HAVING as one group of every row, so the
/// test is written where the rows are filtered instead.
#[test]
fn having_without_a_group_by_filters_rows_when_nothing_is_aggregated() {
    for (sql, normalized) in [
        (
            "SELECT id FROM users HAVING id > 1",
            "SELECT \"id\" FROM \"users\" WHERE (\"id\" > 1)",
        ),
        (
            "SELECT id, score FROM users WHERE id > 1 HAVING score > 10",
            "SELECT \"id\", \"score\" FROM \"users\" WHERE (\"id\" > 1) AND (\"score\" > 10)",
        ),
        (
            "SELECT score FROM users HAVING score IS NULL",
            "SELECT \"score\" FROM \"users\" WHERE (\"score\" IS NULL)",
        ),
    ] {
        let translated = parse_select(sql, SessionSqlMode::default()).unwrap();
        assert_eq!(translated.as_sql(), normalized, "{sql}");
        assert!(translated.parse_ast().is_ok(), "{sql}");
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
fn a_like_crosses_without_a_collation_and_names_its_escape() {
    let translated = parse_select(
        "SELECT id FROM users WHERE name LIKE 'a%' AND name NOT LIKE '_b'",
        SessionSqlMode::default(),
    )
    .unwrap();
    // MySQL takes a backslash as the pattern's escape where the statement
    // names none, and the engine has no escape of its own, so the clause says
    // what MySQL would have taken.
    assert_eq!(
        translated.as_sql(),
        "SELECT \"id\" FROM \"users\" WHERE ((\"name\" LIKE 'a%' ESCAPE '\\') AND (\"name\" NOT LIKE '_b' ESCAPE '\\'))"
    );
    assert_eq!(
        translated.checked_comparisons()[1].operator(),
        CheckedSelectComparisonOperator::NotLike
    );

    // An escaped wildcard and an escape the statement names both render the
    // clause the engine reads them by.
    for (sql, normalized) in [
        (
            "SELECT id FROM users WHERE name LIKE 'a\\%'",
            "SELECT \"id\" FROM \"users\" WHERE (\"name\" LIKE 'a\\%' ESCAPE '\\')",
        ),
        (
            "SELECT id FROM users WHERE name LIKE 'a%' ESCAPE '!'",
            "SELECT \"id\" FROM \"users\" WHERE (\"name\" LIKE 'a%' ESCAPE '!')",
        ),
    ] {
        assert_eq!(
            parse_select(sql, SessionSqlMode::default())
                .unwrap()
                .as_sql(),
            normalized,
            "{sql}"
        );
    }
    // Under `NO_BACKSLASH_ESCAPES` a pattern has no escape at all, measured,
    // and the engine has none either, so no clause is written.
    assert_eq!(
        parse_select(
            "SELECT id FROM users WHERE name LIKE 'a\\%'",
            SessionSqlMode {
                ansi_quotes: false,
                no_backslash_escapes: true,
            },
        )
        .unwrap()
        .as_sql(),
        "SELECT \"id\" FROM \"users\" WHERE (\"name\" LIKE 'a\\%')"
    );
    // A `LIKE` reads one column, and a qualifier names a table this cannot
    // resolve it against.
    assert!(parse_select(
        "SELECT id FROM users WHERE other.name LIKE 'a%'",
        SessionSqlMode::default()
    )
    .is_err());

    // A pattern is bound as readily as it is written. What a written one is
    // checked for — a backslash, which MySQL reads as an escape and the engine
    // reads as itself — is checked where a bound one arrives instead.
    let bound = parse_select(
        "SELECT id FROM users WHERE name LIKE ?",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert_eq!(
        bound.as_sql(),
        "SELECT \"id\" FROM \"users\" WHERE (\"name\" LIKE ? ESCAPE '\\')"
    );
    assert_eq!(bound.parameter_count(), 1);
    assert_eq!(
        bound.checked_comparisons()[0].rhs(),
        &CheckedSelectComparisonRhs::Placeholder { ordinal: 0 }
    );
    assert!(!bound.checked_comparisons()[0].collated());
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
            format!("SELECT id FROM users WHERE other.id {operator} ?"),
            format!("SELECT id FROM users WHERE id + 1 {operator} ?"),
            format!("SELECT id FROM users WHERE id {operator} 1 {operator} 0"),
        ] {
            assert!(
                parse_select(&sql, SessionSqlMode::default()).is_err(),
                "expected unsupported comparison form for {sql}"
            );
        }
    }
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
        // The engine reads the comma spelling the way MySQL does, so it is
        // left as it was written rather than turned into the other one.
        ("LIMIT 1, 2", "LIMIT 1, 2"),
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

    let collated = parse_select_with_column_types(
        "SELECT id FROM users WHERE name IN (?, ?)",
        SessionSqlMode::default(),
        &["name".to_string()],
        &[],
        &[],
    )
    .unwrap();
    assert_eq!(
        collated.as_sql(),
        "SELECT \"id\" FROM \"users\" WHERE (\"name\" COLLATE NOCASE IN (?, ?))"
    );
    assert!(collated.checked_comparisons()[1].collated());
}

/// A number written with a fraction is carried as it was written, so the
/// engine reads the same number the statement asked for.
#[test]
fn a_comparison_carries_a_fraction_as_it_was_written() {
    for (sql, rendered, expected) in [
        (
            "SELECT id FROM users WHERE n > 1.5",
            "SELECT \"id\" FROM \"users\" WHERE (\"n\" > 1.5)",
            "1.5",
        ),
        (
            "SELECT id FROM users WHERE n >= 1e6",
            "SELECT \"id\" FROM \"users\" WHERE (\"n\" >= 1e6)",
            "1e6",
        ),
        (
            "SELECT id FROM users WHERE n > -1.5",
            "SELECT \"id\" FROM \"users\" WHERE (\"n\" > (-1.5))",
            "-1.5",
        ),
    ] {
        let translated = parse_select(sql, SessionSqlMode::default()).unwrap();
        assert_eq!(translated.as_sql(), rendered, "{sql}");
        assert_eq!(
            translated.checked_comparisons()[0].rhs(),
            &CheckedSelectComparisonRhs::Decimal(expected.to_owned()),
            "{sql}"
        );
    }

    // A run of digits too long for an i64 is still refused rather than read as
    // the nearest number it names.
    assert!(parse_select(
        "SELECT id FROM users WHERE n > 9223372036854775808",
        SessionSqlMode::default()
    )
    .is_err());
}

/// The members follow the same coercion rule a single literal comparison
/// follows, so nothing but an exact integer, a number written with a fraction,
/// a string, NULL or `?` reaches the engine.
#[test]
fn select_in_list_refuses_members_a_comparison_would_refuse() {
    for sql in [
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

/// `JSON_EXTRACT` reads one path out of a document, and the engine's `->` reads
/// the same one. What differs is how the two write a document out — the engine
/// puts no space after a comma or a colon — so the answer is written again.
#[test]
fn select_json_extract_renders_the_engine_reading() {
    let mode = SessionSqlMode::default();
    let translated = parse_select("SELECT JSON_EXTRACT(doc, '$.a') FROM j", mode).unwrap();
    assert_eq!(
        translated.as_sql(),
        concat!(
            "SELECT mysql_json_document(\"doc\" -> '$.a') ",
            "AS \"JSON_EXTRACT(doc, '$.a')\" FROM \"j\""
        )
    );

    // JSON_UNQUOTE over the same reading is what the engine's `->>` answers.
    let unquoted =
        parse_select("SELECT JSON_UNQUOTE(JSON_EXTRACT(doc, '$.s')) FROM j", mode).unwrap();
    assert_eq!(
        unquoted.as_sql(),
        concat!(
            "SELECT \"doc\" ->> '$.s' ",
            "AS \"JSON_UNQUOTE(JSON_EXTRACT(doc, '$.s'))\" FROM \"j\""
        )
    );

    // MySQL spells the same two readings with operators too.
    let arrow = parse_select("SELECT doc -> '$.a' FROM j", mode).unwrap();
    assert_eq!(
        arrow.as_sql(),
        "SELECT mysql_json_document(\"doc\" -> '$.a') AS \"doc -> '$.a'\" FROM \"j\""
    );
    let long_arrow = parse_select("SELECT doc ->> '$.s' FROM j", mode).unwrap();
    assert_eq!(
        long_arrow.as_sql(),
        "SELECT \"doc\" ->> '$.s' AS \"doc ->> '$.s'\" FROM \"j\""
    );

    let valid = parse_select("SELECT JSON_VALID(doc) FROM j", mode).unwrap();
    assert_eq!(
        valid.as_sql(),
        "SELECT json_valid(\"doc\") AS \"JSON_VALID(doc)\" FROM \"j\""
    );
}

/// The paths taken are the plain member-and-element ones both MySQL and the
/// engine read the same way. MySQL's wildcards are not among them.
#[test]
fn select_json_extract_takes_only_a_plain_path() {
    let mode = SessionSqlMode::default();
    for path in [
        "$",
        "$.a",
        "$.a.b",
        "$[0]",
        "$.a[1]",
        "$[0][1]",
        "$.a_1[10].b",
    ] {
        assert!(
            parse_select(&format!("SELECT JSON_EXTRACT(doc, '{path}') FROM j"), mode).is_ok(),
            "{path}"
        );
    }
    for path in ["a", "$.", "$.*", "$**.b", "$[*]", "$[]", "$[a]", "$.a-b"] {
        assert!(
            parse_select(&format!("SELECT JSON_EXTRACT(doc, '{path}') FROM j"), mode).is_err(),
            "{path}"
        );
    }
    // A path has to be a literal, and only one is read.
    assert!(parse_select("SELECT JSON_EXTRACT(doc, doc) FROM j", mode).is_err());
    assert!(parse_select("SELECT JSON_EXTRACT(doc, '$.a', '$.b') FROM j", mode).is_err());
}

/// Measured on MySQL 8.4.11: an `ENUM` orders by the position its members were
/// declared in rather than by their text, so `small, medium, large` come back
/// in that order. The empty error member sorts in front of all of them, and a
/// NULL sorts in front of it, which is where a CASE with no match puts both.
#[test]
fn select_order_by_an_enum_orders_by_the_declared_position() {
    let members = vec![(
        "size".to_string(),
        vec![
            "small".to_string(),
            "medium".to_string(),
            "large".to_string(),
        ],
    )];
    let translated = parse_select_with_column_types(
        "SELECT id, size FROM shirts ORDER BY size",
        SessionSqlMode::default(),
        &[],
        &[],
        &members,
    )
    .unwrap();
    assert_eq!(
        translated.as_sql(),
        concat!(
            "SELECT \"id\", \"size\" FROM \"shirts\" ORDER BY ",
            "CASE \"size\" WHEN '' THEN 0 WHEN 'small' THEN 1 ",
            "WHEN 'medium' THEN 2 WHEN 'large' THEN 3 END ASC"
        )
    );

    // An ordinal naming the same column has to order the same way.
    let by_ordinal = parse_select_with_column_types(
        "SELECT * FROM shirts ORDER BY 2 DESC",
        SessionSqlMode::default(),
        &[],
        &["id".to_string(), "size".to_string()],
        &members,
    )
    .unwrap();
    assert_eq!(
        by_ordinal.as_sql(),
        concat!(
            "SELECT * FROM \"shirts\" ORDER BY ",
            "CASE \"size\" WHEN '' THEN 0 WHEN 'small' THEN 1 ",
            "WHEN 'medium' THEN 2 WHEN 'large' THEN 3 END DESC"
        )
    );
}

/// An ordinal that lands on a text column has to be collated the way the same
/// column is when it is spelled out, or `ORDER BY 2` and `ORDER BY name` would
/// answer different orders.
#[test]
fn select_order_by_ordinal_collates_a_text_column() {
    let translated = parse_select_with_column_types(
        "SELECT id, name FROM users ORDER BY 2",
        SessionSqlMode::default(),
        &["name".to_string()],
        &[],
        &[],
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

/// An ordinal over a wildcard projection requests the table columns from the
/// frontend on the first pass, and resolves to the named column with collation
/// on the second pass.
#[test]
fn select_order_by_ordinal_over_wildcard_projection() {
    let mode = SessionSqlMode::default();
    let initial = parse_select("SELECT * FROM users ORDER BY 2", mode).unwrap();
    assert_eq!(initial.as_sql(), "SELECT * FROM \"users\" ORDER BY 2 ASC");
    assert!(initial.needs_column_types());
    assert!(initial.orders_wildcard_ordinal());

    let columns = ["id".to_string(), "name".to_string()];
    let collated = parse_select_with_column_types(
        "SELECT * FROM users ORDER BY 2",
        mode,
        &["name".to_string()],
        &columns,
        &[],
    )
    .unwrap();
    assert_eq!(
        collated.as_sql(),
        "SELECT * FROM \"users\" ORDER BY \"name\" COLLATE NOCASE ASC"
    );

    let non_text = parse_select_with_column_types(
        "SELECT * FROM users ORDER BY 1",
        mode,
        &["name".to_string()],
        &columns,
        &[],
    )
    .unwrap();
    assert_eq!(
        non_text.as_sql(),
        "SELECT * FROM \"users\" ORDER BY \"id\" ASC"
    );

    let desc = parse_select_with_column_types(
        "SELECT * FROM users ORDER BY 2 DESC, 1",
        mode,
        &["name".to_string()],
        &columns,
        &[],
    )
    .unwrap();
    assert_eq!(
        desc.as_sql(),
        "SELECT * FROM \"users\" ORDER BY \"name\" COLLATE NOCASE DESC, \"id\" ASC"
    );

    // Ordinal 0 is refused immediately in pass 1
    assert!(parse_select("SELECT * FROM users ORDER BY 0", mode).is_err());

    // Ordinal outside projection is refused in pass 2
    assert!(parse_select_with_column_types(
        "SELECT * FROM users ORDER BY 3",
        mode,
        &[],
        &columns,
        &[]
    )
    .is_err());

    // Wildcard mixed with explicit columns is refused
    assert!(parse_select("SELECT *, id FROM users ORDER BY 2", mode).is_err());
    assert!(parse_select("SELECT id, * FROM users ORDER BY 2", mode).is_err());
    assert!(parse_select("SELECT users.*, id FROM users ORDER BY 2", mode).is_err());
}

#[test]
fn select_rejects_unchecked_order_and_limit_options() {
    for suffix in [
        // An ordinal past the projection: MySQL answers 1054 here.
        "ORDER BY 2",
        "ORDER BY 0",
        "ORDER BY id NULLS FIRST",
        "ORDER BY ?",
        "LIMIT -1",
        "LIMIT +1",
        "LIMIT 1.5",
        "LIMIT 1e2",
        "LIMIT ALL",
        "LIMIT /* ignored */ ALL",
        "/*! LIMIT ALL */",
        "LIMIT (1)",
        // A count MySQL itself refuses: one past what a row count holds.
        "LIMIT 18446744073709551616",
        "LIMIT 1 OFFSET -1",
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

/// A row count is written as a parameter as readily as a number, and its
/// ordinal is the place it stands in the statement — which is the order a
/// client binds by.
#[test]
fn a_row_count_is_written_as_a_parameter() {
    for (sql, rendered, ordinals) in [
        (
            "SELECT id FROM users LIMIT ?",
            "SELECT \"id\" FROM \"users\" LIMIT ?",
            vec![0],
        ),
        (
            "SELECT id FROM users LIMIT ? OFFSET ?",
            "SELECT \"id\" FROM \"users\" LIMIT ? OFFSET ?",
            vec![0, 1],
        ),
        // MySQL's other spelling writes the offset first, so it is the
        // parameter a client binds first.
        (
            "SELECT id FROM users LIMIT ?, ?",
            "SELECT \"id\" FROM \"users\" LIMIT ?, ?",
            vec![0, 1],
        ),
        (
            "SELECT id FROM users WHERE id > ? LIMIT ?",
            "SELECT \"id\" FROM \"users\" WHERE (\"id\" > ?) LIMIT ?",
            vec![1],
        ),
        (
            "SELECT id FROM users LIMIT 1 OFFSET ?",
            "SELECT \"id\" FROM \"users\" LIMIT 1 OFFSET ?",
            vec![0],
        ),
    ] {
        let translated = parse_select(sql, SessionSqlMode::default()).unwrap();
        assert_eq!(translated.as_sql(), rendered, "{sql}");
        assert_eq!(translated.row_count_parameters(), ordinals, "{sql}");
        assert_eq!(
            translated.parameter_count(),
            ordinals.iter().max().map_or(0, |ordinal| ordinal + 1),
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

    // A statement reading more than one table has no single source, and the
    // frontend answers a result column's table from the sources instead.
    for sql in [
        "SELECT id FROM users JOIN accounts ON users.id = accounts.id",
        "SELECT id FROM users, accounts",
    ] {
        let translated = parse_select(sql, mode).unwrap();
        assert_eq!(translated.source_table(), None, "{sql}");
        assert_eq!(translated.source_tables().len(), 2, "{sql}");
    }
    // A derived table reads its table under the alias it was given, so the
    // statement's one source is that table under that name.
    let translated = parse_select("SELECT id FROM (SELECT id FROM users) AS rows", mode).unwrap();
    let [source] = translated.source_tables() else {
        panic!("a derived table is one source");
    };
    assert_eq!(source.reference(), "rows");
    assert_eq!(source.table().as_str(), "users");
    assert_eq!(source.projected_columns(), ["id"]);

    for sql in [
        "SELECT id FROM app.users",
        "SELECT id FROM (SELECT * FROM users) AS rows",
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
    assert_eq!(spec.column(0), Some(MySqlIntegerType::TinyInt));
    assert_eq!(spec.column(1), Some(MySqlIntegerType::SmallInt));
    assert_eq!(spec.column(2), Some(MySqlIntegerType::Int));
    assert_eq!(spec.column(3), Some(MySqlIntegerType::Int));
    assert_eq!(spec.column(4), Some(MySqlIntegerType::BigInt));
    assert_eq!(spec.column(5), None);
    assert_eq!(MySqlIntegerType::BigInt.bounds(), (i64::MIN, i64::MAX));

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
    assert_eq!(spec.column(0), Some(MySqlIntegerType::MediumInt));
    assert_eq!(spec.column(1), Some(MySqlIntegerType::MediumInt));
    assert_eq!(
        MySqlIntegerType::MediumInt.bounds(),
        (-8_388_608, 8_388_607)
    );

    // A display width is taken and dropped, so it changes neither the stored
    // type nor its bounds.
    let sql = "CREATE TABLE numbers (value MEDIUMINT(8))";
    assert_eq!(
        parse_create_table(sql, SessionSqlMode::default())
            .unwrap()
            .as_sql(),
        "CREATE TABLE \"numbers\" (\"value\" MEDIUMINT)"
    );
}

/// An unsigned column is the same wire type as its signed counterpart with a
/// different range, so the declared name is kept whole — `INT UNSIGNED`, not
/// `INT` — and that name is what carries the sign to the metadata later.
/// Measured on MySQL 8.4.11: 255, 65535, 16777215 and 4294967295 are the top
/// values, and one past any of them answers 1264. `BIGINT UNSIGNED` is taken
/// on narrower terms — the engine holds an integer as an `i64`, so its top is
/// `i64::MAX` rather than MySQL's 18446744073709551615.
#[test]
fn translates_unsigned_integers_and_keeps_their_mysql_bounds() {
    let create = "CREATE TABLE `numbers` (`a` TINYINT UNSIGNED, `b` SMALLINT UNSIGNED, \
                  `c` MEDIUMINT UNSIGNED, `d` INT UNSIGNED, `e` BIGINT UNSIGNED)";
    let statement = parse_create_table_ast(create, SessionSqlMode::default()).unwrap();
    assert_eq!(
        render_create_table_mysql(&statement).unwrap(),
        "CREATE TABLE `numbers` (`a` TINYINT UNSIGNED, `b` SMALLINT UNSIGNED, \
         `c` MEDIUMINT UNSIGNED, `d` INT UNSIGNED, `e` BIGINT UNSIGNED)"
    );

    let spec = parse_mysql_numeric_spec(create, SessionSqlMode::default()).unwrap();
    for (ordinal, integer_type, bounds) in [
        (0, MySqlIntegerType::TinyIntUnsigned, (0, 255)),
        (1, MySqlIntegerType::SmallIntUnsigned, (0, 65_535)),
        (2, MySqlIntegerType::MediumIntUnsigned, (0, 16_777_215)),
        (3, MySqlIntegerType::IntUnsigned, (0, 4_294_967_295)),
        (4, MySqlIntegerType::BigIntUnsigned, (0, i64::MAX)),
    ] {
        assert_eq!(spec.column(ordinal), Some(integer_type), "{ordinal}");
        assert_eq!(integer_type.bounds(), bounds, "{ordinal}");
        assert!(integer_type.is_unsigned(), "{ordinal}");
    }
    assert!(!MySqlIntegerType::Int.is_unsigned());
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
    assert_eq!(spec.column(0), Some(MySqlIntegerType::MediumInt));
    assert_eq!(
        MySqlIntegerType::MediumInt.bounds(),
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
        "INSERT INTO records () VALUES () ON DUPLICATE KEY UPDATE value = 1",
    ] {
        assert!(parse_dml(sql, mode).is_err(), "{sql}");
    }
}

#[test]
fn rejects_dml_and_numeric_forms_outside_the_strict_signed_slice() {
    for sql in [
        "INSERT INTO t VALUES (1)",
        "UPDATE t SET value = 1 LIMIT 1",
        // Dividing a column is taken when the divisor is a written number that
        // is not zero. Dividing by zero answers NULL in the engine where MySQL
        // raises 1365 for a write, and a divisor read from the row says which
        // of the two a statement would get only when it runs.
        "UPDATE t SET value = value / 0",
        "UPDATE t SET value = value / other",
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
        "DELETE FROM t WHERE value <=> 1",
        "DELETE FROM t WHERE value IN (1, 2)",
        "UPDATE t SET value = 1 WHERE value IN (1, 2)",
    ] {
        assert!(parse_dml(sql, SessionSqlMode::default()).is_ok(), "{sql}");
    }

    let delete_in = parse_dml(
        "DELETE FROM t WHERE value IN (1, 2)",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert_eq!(
        delete_in.as_sql(),
        "DELETE FROM \"t\" WHERE (\"value\" IN (1, 2))"
    );
    assert_eq!(delete_in.checked_comparisons().len(), 2);
    assert_eq!(
        delete_in.checked_comparisons()[0].operator(),
        CheckedSelectComparisonOperator::In
    );

    let update_in = parse_dml(
        "UPDATE t SET value = 1 WHERE value NOT IN (1, 2)",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert_eq!(
        update_in.as_sql(),
        "UPDATE \"t\" SET \"value\" = 1 WHERE (\"value\" NOT IN (1, 2))"
    );
    assert_eq!(update_in.checked_comparisons().len(), 2);
    assert_eq!(
        update_in.checked_comparisons()[0].operator(),
        CheckedSelectComparisonOperator::NotIn
    );
    for sql in [
        "WITH doomed AS (SELECT 1) DELETE FROM numbers",
        "DELETE FROM numbers AS n",
        "DELETE FROM numbers, other",
        "DELETE FROM numbers USING other",
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
        (
            "SELECT COUNT(DISTINCT id) FROM users",
            "SELECT COUNT(DISTINCT \"id\") AS \"COUNT(DISTINCT id)\" FROM \"users\"",
        ),
        (
            "SELECT count(distinct id) FROM users",
            "SELECT count(DISTINCT \"id\") AS \"count(distinct id)\" FROM \"users\"",
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
fn select_count_distinct_collates_text_columns() {
    let mode = SessionSqlMode::default();
    let collated = parse_select_with_column_types(
        "SELECT COUNT(DISTINCT team) FROM users",
        mode,
        &["team".to_string()],
        &[],
        &[],
    )
    .unwrap();
    assert_eq!(
        collated.as_sql(),
        "SELECT COUNT(DISTINCT \"team\" COLLATE NOCASE) AS \"COUNT(DISTINCT team)\" FROM \"users\""
    );
}

#[test]
fn having_accepts_count_distinct() {
    let mode = SessionSqlMode::default();
    let translated = parse_select(
        "SELECT team FROM users GROUP BY team HAVING COUNT(DISTINCT id) > 1",
        mode,
    )
    .unwrap();
    assert_eq!(
        translated.as_sql(),
        "SELECT \"team\" FROM \"users\" GROUP BY \"team\" HAVING (COUNT(DISTINCT \"id\") > 1)"
    );
}

#[test]
fn rejects_select_features_with_unproven_mysql_semantics() {
    for sql in [
        // Integer arithmetic is taken; a decimal or float operand, a
        // modulo and a comparison in a projection are not.
        "SELECT 1.5 + 1",
        "SELECT 1 % 2",
        "SELECT 1 = 1",
        "SELECT id FROM app.users",
        "SELECT 9223372036854775808",
        "SELECT -9223372036854775809",
        "SELECT id <=> NULL FROM users",
        "SELECT MOD(n, 'x') FROM users",
        "SELECT POW(n, 'x') FROM users",
        "SELECT GREATEST(n) FROM users",
        "SELECT GREATEST(1, 'x', n) FROM users",
        // COUNT is taken, but only the plain or distinct call: a window, a
        // filter and the other aggregates each mean something this has not
        // measured, and SUM and AVG answer DECIMAL.
        "SELECT COUNT(DISTINCT *) FROM users",
        "SELECT COUNT(id, name) FROM users",
        "SELECT COUNT(id + 1) FROM users",
        // The checked aggregates take one plain column: each answers a
        // type worked out from it, and an expression argument has none
        // this can work out.
        "SELECT MIN(id + 1) FROM users",
        "SELECT SUM(id + 1) FROM users",
        "SELECT SUM(DISTINCT id) FROM users",
        "SELECT MIN(users.id) FROM users",
        // MySQL orders the parts it joins and the engine's group_concat has no
        // way to say in what order, so the ORDER BY form stays refused.
        "SELECT GROUP_CONCAT(name ORDER BY name DESC) FROM users",
        // The engine takes DISTINCT only over a single argument, and the
        // separator is that second argument, so the two together are refused.
        "SELECT GROUP_CONCAT(DISTINCT name SEPARATOR '-') FROM users",
        "SELECT GROUP_CONCAT(id, name) FROM users",
        // A number written with a fraction is read, but a HAVING is counted
        // against a count, which is a whole number.
        "SELECT id FROM users GROUP BY id HAVING COUNT(*) > 1.5",
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
fn select_group_concat_translates_plain_call() {
    let mode = SessionSqlMode::default();
    let translated = parse_select("SELECT GROUP_CONCAT(name) FROM users", mode).unwrap();
    assert_eq!(
        translated.as_sql(),
        "SELECT GROUP_CONCAT(\"name\") AS \"GROUP_CONCAT(name)\" FROM \"users\""
    );
    assert_eq!(
        translated.static_result_metadata(),
        &[crate::StaticSelectProjectionMetadata::Literal(
            crate::StaticSelectMetadata::ColumnAggregate {
                column_name: "name".to_owned(),
                kind: crate::ColumnAggregateKind::Concatenated,
            },
        )]
    );
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

/// A counted table writes its key as a clause too — the shape MySQL prints and
/// so the shape every dumped schema carries.
///
/// Measured on MySQL 8.4.11: the printed schema is the same whichever way the
/// key was written. A key over a column that is not the counted one and a
/// counted column with no key are answered 1075 there and refused here; a
/// counted column inside a key over several columns is taken there and refused
/// here, one rowid having no way to stand for several columns.
#[test]
fn a_counted_table_reads_a_key_written_as_a_clause() {
    let written = parse_auto_increment_create_table(
        "CREATE TABLE `users` (`id` int NOT NULL AUTO_INCREMENT, `n` int DEFAULT NULL, \
         PRIMARY KEY (`id`))",
        SessionSqlMode::default(),
    )
    .unwrap();
    let declared = parse_auto_increment_create_table(
        "CREATE TABLE `users` (`id` int NOT NULL AUTO_INCREMENT PRIMARY KEY, \
         `n` int DEFAULT NULL)",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert_eq!(written, declared);

    for sql in [
        "CREATE TABLE t (id INT NOT NULL AUTO_INCREMENT, n INT NOT NULL, PRIMARY KEY (id, n))",
        "CREATE TABLE t (id INT NOT NULL AUTO_INCREMENT, n INT, PRIMARY KEY (n))",
        "CREATE TABLE t (id INT NOT NULL AUTO_INCREMENT, n INT)",
        // The counted column still has to say NOT NULL, which every schema a
        // migration tool writes does.
        "CREATE TABLE t (id INT AUTO_INCREMENT, PRIMARY KEY (id))",
    ] {
        assert!(
            parse_auto_increment_create_table(sql, SessionSqlMode::default()).is_err(),
            "{sql}"
        );
    }
}

/// MySQL prints `ENGINE=InnoDB DEFAULT CHARSET=utf8mb4
/// COLLATE=utf8mb4_0900_ai_ci` after every table, so that trailer ends every
/// dumped schema, and this prints the same bytes whatever a table holds.
///
/// Measured on MySQL 8.4.11: a table written with any of those options prints
/// back byte for byte the same as one written with none, so they are taken and
/// left out. `COLLATE=utf8mb4_bin`, `DEFAULT CHARSET=latin1`, `ENGINE=MyISAM`,
/// `ROW_FORMAT=DYNAMIC` and `AUTO_INCREMENT=5` are each printed back or mean
/// something of their own, so each stays refused.
#[test]
fn takes_the_table_options_that_name_what_a_table_is_written_back_as() {
    for sql in [
        "CREATE TABLE t (id INT NOT NULL, PRIMARY KEY (id)) ENGINE=InnoDB",
        "CREATE TABLE t (id INT NOT NULL, PRIMARY KEY (id)) DEFAULT CHARSET=utf8mb4",
        "CREATE TABLE t (id INT NOT NULL, PRIMARY KEY (id)) CHARSET=utf8mb4",
        "CREATE TABLE t (id INT NOT NULL, PRIMARY KEY (id)) DEFAULT CHARACTER SET utf8mb4",
        "CREATE TABLE t (id INT NOT NULL, PRIMARY KEY (id)) CHARACTER SET 'utf8mb4'",
        "CREATE TABLE t (id INT NOT NULL, PRIMARY KEY (id)) COLLATE=utf8mb4_0900_ai_ci",
        "CREATE TABLE t (id INT NOT NULL, PRIMARY KEY (id)) DEFAULT COLLATE = utf8mb4_0900_ai_ci",
        "CREATE TABLE t (id INT NOT NULL, PRIMARY KEY (id))          ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci",
    ] {
        let checked = parse_create_table_ast(sql, SessionSqlMode::default());
        assert!(checked.is_ok(), "{sql}: {checked:?}");
    }

    // A table with no key of its own and one that counts its own ids both take
    // the trailer, being written with it just the same.
    assert!(parse_create_table_ast(
        "CREATE TABLE t (id INT) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4",
        SessionSqlMode::default(),
    )
    .is_ok());
    // Measured on MySQL 8.4.11: a table with no counted column takes
    // `AUTO_INCREMENT=<n>` and prints nothing back for it, so there is nothing
    // to keep and nothing to refuse.
    assert!(parse_create_table_ast(
        "CREATE TABLE t (id INT NOT NULL, PRIMARY KEY (id)) AUTO_INCREMENT=5",
        SessionSqlMode::default(),
    )
    .is_ok());
    assert!(parse_auto_increment_create_table(
        "CREATE TABLE t (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY)          ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci",
        SessionSqlMode::default(),
    )
    .is_ok());

    for sql in [
        "CREATE TABLE t (id INT NOT NULL, PRIMARY KEY (id)) DEFAULT CHARSET=latin1",
        "CREATE TABLE t (id INT NOT NULL, PRIMARY KEY (id)) COLLATE=utf8mb4_bin",
        "CREATE TABLE t (id INT NOT NULL, PRIMARY KEY (id)) ENGINE=MyISAM",
        "CREATE TABLE t (id INT NOT NULL, PRIMARY KEY (id)) ENGINE=InnoDB ROW_FORMAT=DYNAMIC",
        "CREATE TABLE t (id INT NOT NULL, PRIMARY KEY (id)) COMMENT='rows'",
        // The same option twice says nothing more the second time, and MySQL
        // takes the last one written, which this would have to read as well.
        "CREATE TABLE t (id INT NOT NULL, PRIMARY KEY (id)) CHARSET=utf8mb4 CHARSET=utf8mb4",
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
fn rejects_mysql_attributes_instead_of_dropping_them() {
    for sql in [
        "CREATE TABLE t (id INTEGER AUTO_INCREMENT)",
        "CREATE TABLE t (id INTEGER DEFAULT CURRENT_TIMESTAMP)",
        "CREATE TABLE t (id INTEGER, UNIQUE KEY uq_id (id))",
        "CREATE TABLE t (id INTEGER, CHECK (RAND() > 0))",
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
        // The allocator takes the INT spellings, signed and unsigned; neither
        // BIGINT is one of them.
        "CREATE TABLE t (id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY)",
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

/// MySQL's `ON DUPLICATE KEY UPDATE` is the engine's `ON CONFLICT DO UPDATE`,
/// and `VALUES(col)` is its `excluded.col`. Measured on MySQL 8.4.11: a new row
/// counts 1, a row the update changes counts 2, and a row the update leaves
/// identical counts 0.
#[test]
fn translates_on_duplicate_key_update_as_the_engines_upsert() {
    for (sql, normalized) in [
        (
            "INSERT INTO users (id, name) VALUES (1, 'a') ON DUPLICATE KEY UPDATE name = 'b'",
            "INSERT INTO \"users\" (\"id\", \"name\") VALUES (1, 'a') \
             ON CONFLICT DO UPDATE SET \"name\" = 'b'",
        ),
        (
            "INSERT INTO users (id, name) VALUES (1, 'a') ON DUPLICATE KEY UPDATE name = VALUES(name)",
            "INSERT INTO \"users\" (\"id\", \"name\") VALUES (1, 'a') \
             ON CONFLICT DO UPDATE SET \"name\" = \"excluded\".\"name\"",
        ),
        (
            "INSERT INTO users (id, name) VALUES (1, 'a') ON DUPLICATE KEY UPDATE id = 2, name = VALUES(name)",
            "INSERT INTO \"users\" (\"id\", \"name\") VALUES (1, 'a') \
             ON CONFLICT DO UPDATE SET \"id\" = 2, \"name\" = \"excluded\".\"name\"",
        ),
    ] {
        let translated = parse_dml(sql, SessionSqlMode::default()).unwrap();
        assert_eq!(translated.as_sql(), normalized, "{sql}");
        assert!(translated.parse_ast().is_ok(), "{sql}");
    }

    for sql in [
        // REPLACE and IGNORE already decide what a collision does.
        "REPLACE INTO users (id) VALUES (1) ON DUPLICATE KEY UPDATE id = 2",
        "INSERT IGNORE INTO users (id) VALUES (1) ON DUPLICATE KEY UPDATE id = 2",
        // The SET form and the empty-row form leave no room for the clause, so
        // it is refused rather than dropped.
        "INSERT INTO users SET id = 1 ON DUPLICATE KEY UPDATE id = 2",
        "INSERT INTO users () VALUES () ON DUPLICATE KEY UPDATE id = 2",
        // VALUES() names one column of the row that was offered.
        "INSERT INTO users (id) VALUES (1) ON DUPLICATE KEY UPDATE id = VALUES(id, name)",
        "INSERT INTO users (id) VALUES (1) ON DUPLICATE KEY UPDATE id = VALUES(users.id)",
        // The update value goes through the rules a DML value goes through.
        "INSERT INTO users (id) VALUES (1) ON DUPLICATE KEY UPDATE id = LOWER('a')",
    ] {
        assert!(parse_dml(sql, SessionSqlMode::default()).is_err(), "{sql}");
    }
}

/// `INSERT IGNORE` skips a row whose key collides instead of failing the
/// statement, which is the engine's own `OR IGNORE`. Measured on MySQL 8.4.11
/// over a table already holding row 1: `INSERT IGNORE` of row 1 leaves it alone
/// and counts 0, and a two-row one where only the second is new counts 1.
/// `INSERT ... SELECT` reads rows rather than listing them, and names the table
/// it read so the caller can authorize it.
#[test]
fn translates_insert_select_and_names_what_it_reads() {
    let translated = parse_dml(
        "INSERT INTO dst (id, n) SELECT id, n FROM src WHERE n > 15",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert_eq!(
        translated.as_sql(),
        "INSERT INTO \"dst\" (\"id\", \"n\") SELECT \"id\", \"n\" FROM \"src\" WHERE (\"n\" > 15)"
    );
    assert_eq!(
        translated
            .read_tables()
            .iter()
            .map(|source| source.table().as_str())
            .collect::<Vec<_>>(),
        ["src"]
    );
    // The WHERE is checked against the table the SELECT reads, not the one the
    // INSERT writes.
    assert_eq!(translated.source_table(), Some("src"));
    assert_eq!(translated.checked_comparisons().len(), 1);
    assert!(translated.parse_ast().is_ok());

    // A SELECT with no FROM reads no table and is taken; measured on MySQL
    // 8.4.11, `INSERT INTO one (value) SELECT 1` stores one row.
    let literal = parse_dml("INSERT INTO t (value) SELECT 1", SessionSqlMode::default()).unwrap();
    assert_eq!(literal.as_sql(), "INSERT INTO \"t\" (\"value\") SELECT 1");
    assert!(literal.read_tables().is_empty());

    for sql in [
        // The column list is required.
        "INSERT INTO dst SELECT id FROM src",
        // A SELECT needing a second rendering pass has no way to ask for one.
        "INSERT INTO dst (n) SELECT n FROM src ORDER BY n",
        "INSERT INTO dst (n) SELECT n FROM src WHERE n = ?",
    ] {
        assert!(parse_dml(sql, SessionSqlMode::default()).is_err(), "{sql}");
    }
}

#[test]
fn translates_insert_ignore_as_the_engines_or_ignore() {
    for (sql, normalized) in [
        (
            "INSERT IGNORE INTO users (id, name) VALUES (1, 'a')",
            "INSERT OR IGNORE INTO \"users\" (\"id\", \"name\") VALUES (1, 'a')",
        ),
        (
            "INSERT IGNORE INTO users (id, name) VALUES (1, 'a'), (2, 'b')",
            "INSERT OR IGNORE INTO \"users\" (\"id\", \"name\") VALUES (1, 'a'), (2, 'b')",
        ),
        (
            "INSERT IGNORE INTO users SET id = 1, name = 'a'",
            "INSERT OR IGNORE INTO \"users\" (\"id\", \"name\") VALUES (1, 'a')",
        ),
    ] {
        let translated = parse_dml(sql, SessionSqlMode::default()).unwrap();
        assert_eq!(translated.as_sql(), normalized, "{sql}");
        assert!(translated.parse_ast().is_ok(), "{sql}");
    }

    // REPLACE already decides what a collision does, so IGNORE on top of it is
    // not a shape MySQL accepts either.
    assert!(parse_dml(
        "REPLACE IGNORE INTO users (id) VALUES (1)",
        SessionSqlMode::default()
    )
    .is_err());
}

/// MySQL's `INSERT ... SET` names its columns and values in one place instead
/// of two and means what the column-list form means. Measured on MySQL 8.4.11:
/// `INSERT INTO s SET id = 1, a = 2, b = 'x'` stores the row
/// `INSERT INTO s (id, a, b) VALUES (1, 2, 'x')` stores, and a column the SET
/// leaves out takes its default.
#[test]
fn translates_the_insert_set_form_as_the_column_list_form() {
    for (sql, normalized) in [
        (
            "INSERT INTO users SET name = 'a'",
            "INSERT INTO \"users\" (\"name\") VALUES ('a')",
        ),
        (
            "INSERT INTO users SET id = 1, name = 'a'",
            "INSERT INTO \"users\" (\"id\", \"name\") VALUES (1, 'a')",
        ),
        (
            "INSERT INTO users SET name = ?",
            "INSERT INTO \"users\" (\"name\") VALUES (?)",
        ),
        (
            "REPLACE INTO users SET id = 1, name = 'a'",
            "INSERT OR REPLACE INTO \"users\" (\"id\", \"name\") VALUES (1, 'a')",
        ),
    ] {
        let translated = parse_dml(sql, SessionSqlMode::default()).unwrap();
        assert_eq!(translated.as_sql(), normalized, "{sql}");
        assert!(translated.parse_ast().is_ok(), "{sql}");
    }

    for sql in [
        // A value outside the checked kinds is refused exactly as it is on the
        // other form.
        "INSERT INTO users SET name = LOWER('a')",
        // A qualified target names a table this cannot check against.
        "INSERT INTO users SET users.name = 'a'",
        // The two forms do not mix.
        "INSERT INTO users (name) SET name = 'a'",
    ] {
        assert!(parse_dml(sql, SessionSqlMode::default()).is_err(), "{sql}");
    }
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
        // One row's upsert is taken; which of several rows the reported id
        // comes from depends on what each of them did.
        "INSERT INTO users (name) VALUES ('a'), ('b') ON DUPLICATE KEY UPDATE name = 'c'",
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
    // The bound here is the widest number the engine holds; how high the
    // numbering may actually run is the column's own type's to say, and the
    // caller holds the range to that before this is reached.
    assert!(checked.inject_reserved_range(i64::MAX as u64).is_err());
    assert!(checked.inject_reserved_range(u64::MAX).is_err());

    let one_row = parse_auto_increment_insert(
        "INSERT INTO users (name) VALUES ('Ada')",
        SessionSqlMode::default(),
    )
    .unwrap()
    .bind_allocator_table(&table)
    .unwrap();
    assert!(one_row.inject_reserved_range(i64::MAX as u64).is_ok());
    assert!(one_row.inject_reserved_range(i64::MAX as u64 + 1).is_err());
    // A range an INT column could not hold is the caller's to refuse, not
    // this one's.
    assert!(one_row
        .inject_reserved_range(i64::from(i32::MAX) as u64 + 1)
        .is_ok());
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

/// A DATE holds the day alone and a TIME a span, and the clock readings that
/// answer either are spelled with and without their parentheses. Measured on
/// MySQL 8.4.11.
#[test]
fn reads_a_date_column_and_the_calls_that_answer_a_day() {
    let mode = SessionSqlMode::default();
    let translated = parse_create_table("CREATE TABLE d (a DATE, b TIME, c YEAR)", mode).unwrap();
    assert_eq!(
        translated.as_sql(),
        "CREATE TABLE \"d\" (\"a\" DATE, \"b\" TIME, \"c\" YEAR)"
    );
    // sqlparser has no YEAR of its own, so it arrives as a custom type name;
    // every other custom name is still a type this frontend does not know.
    assert!(parse_create_table("CREATE TABLE d (a GEOMETRY)", mode).is_err());

    for (sql, normalized) in [
        (
            "SELECT CURDATE() FROM d",
            "SELECT date('now') AS \"CURDATE()\" FROM \"d\"",
        ),
        (
            "SELECT CURRENT_DATE FROM d",
            "SELECT date('now') AS \"CURRENT_DATE\" FROM \"d\"",
        ),
        (
            "SELECT CURTIME() FROM d",
            "SELECT time('now') AS \"CURTIME()\" FROM \"d\"",
        ),
        (
            "SELECT CURRENT_TIME FROM d",
            "SELECT time('now') AS \"CURRENT_TIME\" FROM \"d\"",
        ),
        // The bare spelling works for the older reading too, which had the
        // same missing argument list standing in its way.
        (
            "SELECT CURRENT_TIMESTAMP FROM d",
            "SELECT datetime('now') AS \"CURRENT_TIMESTAMP\" FROM \"d\"",
        ),
    ] {
        assert_eq!(
            parse_select(sql, mode).map(|select| select.as_sql().to_owned()),
            Ok(normalized.to_owned()),
            "{sql}"
        );
    }

    // The engine reads a part of a date out as text where MySQL answers a
    // number, so the cast is what keeps the two agreeing.
    for (sql, normalized) in [
        (
            "SELECT YEAR(a) FROM d",
            "SELECT CAST(strftime('%Y', \"a\") AS INTEGER) AS \"YEAR(a)\" FROM \"d\"",
        ),
        (
            "SELECT MONTH(a) FROM d",
            "SELECT CAST(strftime('%m', \"a\") AS INTEGER) AS \"MONTH(a)\" FROM \"d\"",
        ),
        (
            "SELECT DAY(a) FROM d",
            "SELECT CAST(strftime('%d', \"a\") AS INTEGER) AS \"DAY(a)\" FROM \"d\"",
        ),
        (
            "SELECT HOUR(a) FROM d",
            "SELECT CAST(strftime('%H', \"a\") AS INTEGER) AS \"HOUR(a)\" FROM \"d\"",
        ),
        (
            "SELECT MINUTE(a) FROM d",
            "SELECT CAST(strftime('%M', \"a\") AS INTEGER) AS \"MINUTE(a)\" FROM \"d\"",
        ),
        (
            "SELECT SECOND(a) FROM d",
            "SELECT CAST(strftime('%S', \"a\") AS INTEGER) AS \"SECOND(a)\" FROM \"d\"",
        ),
        // The shift keeps the column's own kind for an interval of whole
        // days, which the stored width says because this layer does not
        // know the column's type.
        (
            "SELECT DATE_ADD(a, INTERVAL 1 DAY) FROM d",
            "SELECT mysql_shift_moment(\"a\", 1, 'day') AS \"DATE_ADD(a, INTERVAL 1 DAY)\" FROM \"d\"",
        ),
        (
            "SELECT DATE_SUB(a, INTERVAL 2 MONTH) FROM d",
            "SELECT mysql_shift_moment(\"a\", -2, 'month') AS \"DATE_SUB(a, INTERVAL 2 MONTH)\" FROM \"d\"",
        ),
        // An interval carrying a time answers a moment either way.
        (
            "SELECT DATE_ADD(a, INTERVAL 1 HOUR) FROM d",
            "SELECT mysql_shift_moment(\"a\", 1, 'hour') AS \"DATE_ADD(a, INTERVAL 1 HOUR)\" FROM \"d\"",
        ),
        // MySQL counts whole days between the dates alone, dropping any
        // time either carries.
        (
            "SELECT DATEDIFF(a, b) FROM d",
            "SELECT CAST(julianday(date(\"a\")) - julianday(date(\"b\")) AS INTEGER) AS \"DATEDIFF(a, b)\" FROM \"d\"",
        ),
    ] {
        assert_eq!(
            parse_select(sql, mode).map(|select| select.as_sql().to_owned()),
            Ok(normalized.to_owned()),
            "{sql}"
        );
    }

    // A day is not a call that takes something.
    for sql in [
        "SELECT CURDATE(1) FROM d",
        "SELECT CURRENT_DATE(1) FROM d",
        "SELECT CURTIME(1) FROM d",
        "SELECT YEAR() FROM d",
        "SELECT YEAR(a, b) FROM d",
        "SELECT DATEDIFF(a) FROM d",
        "SELECT DATEDIFF(a, b, c) FROM d",
        "SELECT DATE_ADD(a, 1) FROM d",
        // A count worked out from a row cannot be multiplied for a week or a
        // quarter, so a shift counts a written number and nothing else.
        "SELECT DATE_ADD(a, INTERVAL b DAY) FROM d",
    ] {
        assert!(parse_select(sql, mode).is_err(), "{sql}");
    }
}

#[test]
fn accepts_only_plain_show_tables_on_the_catalog_surface() {
    let mode = SessionSqlMode::default();
    for sql in ["SHOW TABLES", "show\ttables", "SHOW\nTABLES;"] {
        let command = parse_show_tables(sql, mode).expect("SHOW TABLES must be accepted");
        assert_eq!(command.pattern(), None, "{sql}");
        assert!(command.covers("reports"), "{sql}");
    }
    for (sql, pattern) in [
        ("SHOW TABLES LIKE 'report%'", "report%"),
        ("show\ttables\tlike\t'a%';", "a%"),
        ("SHOW TABLES LIKE ''", ""),
    ] {
        let command = parse_show_tables(sql, mode).unwrap();
        assert_eq!(command.pattern().unwrap().text(), pattern, "{sql}");
    }

    // Measured on MySQL 8.4.11: a table name is matched by case here, where
    // every other `SHOW ... LIKE` subject is matched whatever its case.
    let filtered = parse_show_tables("SHOW TABLES LIKE 'Report%'", mode).unwrap();
    assert_eq!(filtered.pattern().unwrap().text(), "Report%");
    assert!(filtered.covers("Reports"));
    assert!(!filtered.covers("reports"));

    for sql in [
        "SHOW FULL TABLES",
        "SHOW TABLES FROM reports",
        "SHOW TABLES IN reports",
        "SHOW TABLES LIKE",
        "SHOW TABLES LIKE reports",
        "SHOW TABLES LIKE 123",
        "SHOW TABLES LIKE 'report%' extra",
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
        let columns = [
            MySqlInformationSchemaTablesColumn::TableSchema,
            MySqlInformationSchemaTablesColumn::TableName,
            MySqlInformationSchemaTablesColumn::TableType,
        ];
        assert_eq!(
            parse_information_schema_tables(sql, mode)
                .as_ref()
                .map(MySqlInformationSchemaTablesQuery::columns),
            Ok(columns.as_slice()),
            "expected information_schema.TABLES query to be accepted: {sql}"
        );
        assert!(
            parse_optional_information_schema_tables(sql, mode)
                .unwrap()
                .is_some(),
            "expected optional parser to recognize: {sql}"
        );
    }

    // The columns come back in the order the query named them, so a client
    // that asks for fewer, or for the same ones in another order, is answered
    // the way it asked. An absent ORDER BY is taken as well: the rows come
    // back in table-name order either way.
    for (sql, expected) in [
        (
            "SELECT TABLE_NAME FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE()",
            vec![MySqlInformationSchemaTablesColumn::TableName],
        ),
        (
            "SELECT TABLE_TYPE, TABLE_NAME FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE() ORDER BY TABLE_NAME",
            vec![
                MySqlInformationSchemaTablesColumn::TableType,
                MySqlInformationSchemaTablesColumn::TableName,
            ],
        ),
    ] {
        assert_eq!(
            parse_information_schema_tables(sql, mode)
                .as_ref()
                .map(MySqlInformationSchemaTablesQuery::columns),
            Ok(expected.as_slice()),
            "{sql}"
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
        // A column MySQL has and this does not answer, and the same column
        // named twice.
        "SELECT TABLE_ROWS FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE()",
        "SELECT TABLE_NAME, TABLE_NAME FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE()",
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

    // The columns come back in the order the query named them, and an absent
    // ORDER BY is taken: the rows come back in declaration order either way.
    for (sql, expected) in [
        (
            "SELECT COLUMN_NAME FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'records'",
            vec![MySqlInformationSchemaColumnsColumn::ColumnName],
        ),
        (
            "SELECT IS_NULLABLE, COLUMN_NAME FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'records' ORDER BY ORDINAL_POSITION",
            vec![
                MySqlInformationSchemaColumnsColumn::IsNullable,
                MySqlInformationSchemaColumnsColumn::ColumnName,
            ],
        ),
    ] {
        assert_eq!(
            parse_information_schema_columns(sql, mode)
                .as_ref()
                .map(MySqlInformationSchemaColumnsQuery::columns),
            Ok(expected.as_slice()),
            "{sql}"
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
        // A column MySQL has and this does not answer, and the same column
        // named twice.
        "SELECT COLLATION_NAME FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'records'",
        "SELECT COLUMN_NAME, COLUMN_NAME FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'records'",
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
        // Measured on MySQL 8.4.11: the pattern names the columns to report.
        ("SHOW COLUMNS FROM reports LIKE 'id%'", "reports"),
        ("SHOW FULL COLUMNS FROM reports", "reports"),
        ("SHOW FULL COLUMNS FROM reports LIKE 'id%'", "reports"),
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
        // Measured: MySQL answers 1064 for a name after the pattern, and the
        // pattern has to be a string literal.
        "SHOW COLUMNS FROM reports LIKE 'id%' extra",
        "SHOW COLUMNS FROM reports LIKE id",
        "SHOW COLUMNS FROM reports LIKE",
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
        // Measured on MySQL 8.4.11: `EXPLAIN t` prints what `DESCRIBE t`
        // prints.
        ("EXPLAIN reports", "reports"),
        ("explain\t`RePoRtS`;", "reports"),
        // Measured: a name after the table is the pattern its columns are
        // named with, quoted or not.
        ("DESCRIBE reports 'id%'", "reports"),
        ("DESCRIBE reports id", "reports"),
        ("DESC reports `id`", "reports"),
    ] {
        assert_eq!(
            parse_describe(sql, mode).map(|command| command.table().as_str().to_owned()),
            Ok(table.to_owned()),
            "expected DESCRIBE form to be accepted: {sql}"
        );
    }

    for sql in [
        "DESCRIBE",
        // Measured: MySQL answers 1064 for a second name after the pattern.
        "DESCRIBE reports 'id%' extra",
        "DESCRIBE TABLE reports",
        "DESCRIBE reports IN archive",
        "DESCRIBE `report columns`",
        "DESCRIBE reports LIKE 'id%'",
        "DESCRIBE reports WHERE Field = 'id'",
        "DESCRIBE reports; SELECT 1",
        "DESCR reports",
        "DESC",
        "DESC reports id extra",
        // Anything after EXPLAIN that is not one lone name is the optimizer's
        // plan, which this does not answer.
        "EXPLAIN SELECT 1",
        "EXPLAIN SELECT id FROM reports",
        "EXPLAIN FORMAT = JSON SELECT 1",
        "EXPLAIN ANALYZE SELECT 1",
        "EXPLAIN",
        // The pattern is read only for DESCRIBE and DESC: after EXPLAIN, no
        // shape tells `EXPLAIN t 1` from `EXPLAIN SELECT 1`.
        "EXPLAIN reports 'id%'",
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
            parse_show_tables(sql, mode).map(|command| command.covers("x")),
            Ok(true),
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
        assert_eq!(
            parse_transaction_command(sql, mode),
            Ok(expected.clone()),
            "{sql}"
        );
        assert_eq!(
            parse_optional_transaction_command(sql, mode),
            Ok(Some(expected)),
            "{sql}"
        );
    }
}

/// A savepoint marks a point inside a transaction. Measured on MySQL 8.4.11:
/// the `SAVEPOINT` keyword after `TO` is optional, `RELEASE` needs it, and a
/// name is matched whatever its case.
#[test]
fn reads_the_three_savepoint_statements() {
    let mode = SessionSqlMode::default();
    for (sql, expected) in [
        (
            "SAVEPOINT s1",
            MySqlTransactionCommand::Savepoint("s1".to_owned()),
        ),
        (
            "savepoint `S1`;",
            MySqlTransactionCommand::Savepoint("s1".to_owned()),
        ),
        (
            "ROLLBACK TO s1",
            MySqlTransactionCommand::RollbackToSavepoint("s1".to_owned()),
        ),
        (
            "ROLLBACK TO SAVEPOINT S1;",
            MySqlTransactionCommand::RollbackToSavepoint("s1".to_owned()),
        ),
        (
            "RELEASE SAVEPOINT s1",
            MySqlTransactionCommand::ReleaseSavepoint("s1".to_owned()),
        ),
    ] {
        assert_eq!(parse_transaction_command(sql, mode), Ok(expected), "{sql}");
    }

    // A bare ROLLBACK still ends the transaction. Reading one as the other
    // would end a transaction MySQL keeps open.
    for (sql, expected) in [
        ("ROLLBACK", MySqlTransactionCommand::Rollback),
        ("ROLLBACK;", MySqlTransactionCommand::Rollback),
        (
            "ROLLBACK AND CHAIN",
            MySqlTransactionCommand::RollbackAndChain,
        ),
    ] {
        assert_eq!(parse_transaction_command(sql, mode), Ok(expected), "{sql}");
    }

    for sql in [
        "SAVEPOINT",
        "SAVEPOINT s1 s2",
        "SAVEPOINT 's1'",
        "SAVEPOINT 1",
        // Measured on MySQL 8.4.11: `RELEASE s1` without the keyword is 1064.
        "RELEASE s1",
        "RELEASE",
        "ROLLBACK TO",
        "ROLLBACK TO SAVEPOINT",
        "SAVEPOINT s1; SAVEPOINT s2",
    ] {
        assert!(parse_transaction_command(sql, mode).is_err(), "{sql}");
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

/// `AND CHAIN` ends a transaction and begins another at once. Measured on
/// MySQL 8.4.11: after `ROLLBACK AND CHAIN` a following write is inside a new
/// transaction, and `COMMIT AND CHAIN` leaves the session in one even when
/// autocommit is on and there was none to end.
#[test]
fn reads_the_chaining_transaction_commands() {
    let mode = SessionSqlMode::default();
    for (sql, command) in [
        (
            "START TRANSACTION READ ONLY",
            MySqlTransactionCommand::BeginReadOnly,
        ),
        (
            "start transaction read write",
            MySqlTransactionCommand::Begin,
        ),
        ("COMMIT AND CHAIN", MySqlTransactionCommand::CommitAndChain),
        (
            "rollback and chain;",
            MySqlTransactionCommand::RollbackAndChain,
        ),
    ] {
        assert_eq!(
            parse_optional_transaction_command(sql, mode),
            Ok(Some(command)),
            "{sql}"
        );
    }
}

/// `mysqldump --single-transaction` opens with this, inside the versioned
/// comment MySQL runs. sqlparser 0.62.0 has no `CONSISTENT SNAPSHOT` in its
/// AST, so the token check answers it and never hands it on; the tokenizer
/// expands the comment, so both spellings arrive as the same five words.
#[test]
fn reads_start_transaction_with_consistent_snapshot() {
    let mode = SessionSqlMode::default();
    for sql in [
        "START TRANSACTION WITH CONSISTENT SNAPSHOT",
        "start transaction with consistent snapshot;",
        "START TRANSACTION /*!40100 WITH CONSISTENT SNAPSHOT */",
    ] {
        assert_eq!(
            parse_optional_transaction_command(sql, mode),
            Ok(Some(MySqlTransactionCommand::Begin)),
            "{sql}"
        );
    }

    for sql in [
        "START TRANSACTION WITH SNAPSHOT",
        "START TRANSACTION CONSISTENT SNAPSHOT",
        "START TRANSACTION WITH CONSISTENT",
    ] {
        assert!(
            parse_optional_transaction_command(sql, mode).is_err(),
            "{sql}"
        );
    }
}

/// MySQL takes several operations in one `ALTER TABLE` and the engine takes
/// one, so the statement is split into one MySQL statement per operation. They
/// come back as MySQL rather than SQLite so each can go through the ordinary
/// schema path, which carries the durable DDL a table is remembered by.
/// Measured on MySQL 8.4.11: `ADD COLUMN a, ADD COLUMN b` adds both, and a
/// statement whose second operation fails adds neither.
#[test]
fn splits_a_multi_operation_alter_table() {
    let mode = SessionSqlMode::default();
    assert_eq!(
        split_alter_table_operations("ALTER TABLE t ADD COLUMN a INT, ADD COLUMN b INT", mode)
            .unwrap(),
        vec![
            "ALTER TABLE `t` ADD COLUMN `a` INT".to_owned(),
            "ALTER TABLE `t` ADD COLUMN `b` INT".to_owned(),
        ]
    );
    assert_eq!(
        split_alter_table_operations(
            "ALTER TABLE t DROP COLUMN a, RENAME COLUMN b TO c, RENAME TO u",
            mode
        )
        .unwrap(),
        vec![
            "ALTER TABLE `t` DROP COLUMN `a`".to_owned(),
            "ALTER TABLE `t` RENAME COLUMN `b` TO `c`".to_owned(),
            "ALTER TABLE `t` RENAME TO `u`".to_owned(),
        ]
    );

    // One operation still answers one statement through either entry point.
    assert_eq!(
        split_alter_table_operations("ALTER TABLE t ADD COLUMN a INT", mode)
            .unwrap()
            .len(),
        1
    );
    assert!(parse_alter_table_ast("ALTER TABLE t ADD COLUMN a INT", mode).is_ok());
    assert!(
        parse_alter_table_ast("ALTER TABLE t ADD COLUMN a INT, ADD COLUMN b INT", mode).is_err()
    );

    // An operation outside the checked set is refused wherever it sits, and it
    // is refused before any of them runs.
    assert!(
        split_alter_table_operations("ALTER TABLE t ADD COLUMN a INT, DROP PRIMARY KEY", mode)
            .is_err()
    );
}

/// `MODIFY COLUMN` and `CHANGE COLUMN` restate one column whole. Measured on
/// MySQL 8.4.11: `MODIFY COLUMN n BIGINT` over an `INT NOT NULL DEFAULT 5`
/// leaves a `bigint DEFAULT NULL`, so an attribute the statement does not
/// restate is gone, and `CHANGE` does the same while renaming the column.
#[test]
fn alter_table_restates_a_column_whole() {
    let mode = SessionSqlMode::default();
    assert_eq!(
        split_alter_table_operations(
            "ALTER TABLE t MODIFY COLUMN n BIGINT, CHANGE COLUMN name label VARCHAR(20) NOT NULL",
            mode
        )
        .unwrap(),
        vec![
            "ALTER TABLE `t` MODIFY COLUMN `n` BIGINT".to_owned(),
            "ALTER TABLE `t` CHANGE COLUMN `name` `label` VARCHAR(20) NOT NULL".to_owned(),
        ]
    );

    // The engine's ALTER COLUMN takes the column the old one is to become,
    // which is what carries CHANGE's rename.
    for (sql, old_name, new_name, new_type) in [
        ("ALTER TABLE t MODIFY COLUMN n BIGINT", "n", "n", "BIGINT"),
        (
            "ALTER TABLE t CHANGE name label VARCHAR(20) NOT NULL",
            "name",
            "label",
            "VARCHAR",
        ),
    ] {
        let statement = parse_alter_table_ast(sql, mode).unwrap();
        let Stmt::AlterTable(TursoAlterTable {
            body: TursoAlterTableBody::AlterColumn { old, new },
            ..
        }) = statement
        else {
            panic!("expected ALTER COLUMN for {sql}");
        };
        assert_eq!(old.as_str(), old_name, "{sql}");
        assert_eq!(new.col_name.as_str(), new_name, "{sql}");
        assert_eq!(new.col_type.unwrap().name, new_type, "{sql}");
    }

    // Repositioning moves a column and the engine has no way to, so it is
    // refused rather than quietly ignored.
    for sql in [
        "ALTER TABLE t MODIFY COLUMN n BIGINT FIRST",
        "ALTER TABLE t MODIFY COLUMN n BIGINT AFTER id",
        "ALTER TABLE t CHANGE COLUMN name label VARCHAR(20) AFTER id",
    ] {
        assert!(parse_alter_table_ast(sql, mode).is_err(), "{sql}");
    }
}

#[test]
fn rejects_transaction_options_comments_and_multiple_statements() {
    let mode = SessionSqlMode::default();
    for sql in [
        "BEGIN WORK",
        "BEGIN TRANSACTION",
        "COMMIT AND NO CHAIN",
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

#[test]
fn translates_text_and_blob_size_variants() {
    let mode = SessionSqlMode::default();
    let sql = "CREATE TABLE t (id INT NOT NULL UNIQUE, a TINYTEXT, b TEXT, c MEDIUMTEXT, d LONGTEXT, e TINYBLOB, f BLOB, g MEDIUMBLOB, h LONGBLOB)";
    let translated = parse_create_table(sql, mode).unwrap();
    assert_eq!(
        translated.as_sql(),
        "CREATE TABLE \"t\" (\"id\" INT NOT NULL UNIQUE, \"a\" TINYTEXT, \"b\" TEXT, \"c\" MEDIUMTEXT, \"d\" LONGTEXT, \"e\" TINYBLOB, \"f\" BLOB, \"g\" MEDIUMBLOB, \"h\" LONGBLOB)"
    );

    let statement = parse_create_table_ast(sql, mode).unwrap();
    let rendered = render_create_table_mysql_with_mode(&statement, mode).unwrap();
    assert_eq!(
        rendered,
        "CREATE TABLE `t` (`id` INT NOT NULL UNIQUE, `a` TINYTEXT, `b` TEXT, `c` MEDIUMTEXT, `d` LONGTEXT, `e` TINYBLOB, `f` BLOB, `g` MEDIUMBLOB, `h` LONGBLOB)"
    );

    let pk_sql = "CREATE TABLE t (id INT NOT NULL PRIMARY KEY, a TINYTEXT, b TEXT, c MEDIUMTEXT, d LONGTEXT, e TINYBLOB, f BLOB, g MEDIUMBLOB, h LONGBLOB)";
    let checked = parse_checked_primary_key_create_table(pk_sql, mode).unwrap();
    assert_eq!(
        checked.normalized_mysql_ddl,
        "CREATE TABLE `t` (`id` INT NOT NULL PRIMARY KEY, `a` TINYTEXT, `b` TEXT, `c` MEDIUMTEXT, `d` LONGTEXT, `e` TINYBLOB, `f` BLOB, `g` MEDIUMBLOB, `h` LONGBLOB)"
    );
}

fn parse_sqlite_create_table(sql: &str) -> Stmt {
    let mut parser = TursoParser::new(sql.as_bytes());
    let Some(TursoCmd::Stmt(statement @ Stmt::CreateTable { .. })) = parser.next_cmd().unwrap()
    else {
        panic!("expected SQLite CREATE TABLE AST");
    };
    statement
}

#[test]
fn translates_dml_order_by_and_limit_via_subquery() {
    let mode = SessionSqlMode::default();

    let d1 = parse_dml("DELETE FROM t WHERE n > 5 ORDER BY id LIMIT 2", mode).unwrap();
    assert_eq!(
        d1.as_sql(),
        "DELETE FROM \"t\" WHERE _rowid_ IN (SELECT _rowid_ FROM \"t\" WHERE (\"n\" > 5) ORDER BY \"id\" ASC LIMIT 2)"
    );
    assert_eq!(d1.ordered_columns(), &["id"]);
    assert!(d1.parse_ast().is_ok());

    let d2 = parse_dml("DELETE FROM t ORDER BY id DESC LIMIT 1", mode).unwrap();
    assert_eq!(
        d2.as_sql(),
        "DELETE FROM \"t\" WHERE _rowid_ IN (SELECT _rowid_ FROM \"t\" ORDER BY \"id\" DESC LIMIT 1)"
    );
    assert_eq!(d2.ordered_columns(), &["id"]);
    assert!(d2.parse_ast().is_ok());

    let d3 = parse_dml("DELETE FROM numbers ORDER BY id", mode).unwrap();
    assert_eq!(
        d3.as_sql(),
        "DELETE FROM \"numbers\" WHERE _rowid_ IN (SELECT _rowid_ FROM \"numbers\" ORDER BY \"id\" ASC)"
    );
    assert_eq!(d3.ordered_columns(), &["id"]);
    assert!(d3.parse_ast().is_ok());

    let u1 = parse_dml("UPDATE t SET n = 0 WHERE n > 5 ORDER BY id LIMIT 1", mode).unwrap();
    assert_eq!(
        u1.as_sql(),
        "UPDATE \"t\" SET \"n\" = 0 WHERE _rowid_ IN (SELECT _rowid_ FROM \"t\" WHERE (\"n\" > 5) ORDER BY \"id\" ASC LIMIT 1)"
    );
    assert_eq!(u1.ordered_columns(), &["id"]);
    assert!(u1.parse_ast().is_ok());

    let u2 = parse_dml("UPDATE t SET n = 0 ORDER BY id LIMIT 1", mode).unwrap();
    assert_eq!(
        u2.as_sql(),
        "UPDATE \"t\" SET \"n\" = 0 WHERE _rowid_ IN (SELECT _rowid_ FROM \"t\" ORDER BY \"id\" ASC LIMIT 1)"
    );
    assert_eq!(u2.ordered_columns(), &["id"]);
    assert!(u2.parse_ast().is_ok());

    let u3 = parse_dml("UPDATE t SET value = 1 ORDER BY value", mode).unwrap();
    assert_eq!(
        u3.as_sql(),
        "UPDATE \"t\" SET \"value\" = 1 WHERE _rowid_ IN (SELECT _rowid_ FROM \"t\" ORDER BY \"value\" ASC)"
    );
    assert_eq!(u3.ordered_columns(), &["value"]);
    assert!(u3.parse_ast().is_ok());

    // Qualified column name matching the table is accepted.
    let d_qual = parse_dml("DELETE FROM t ORDER BY t.id LIMIT 1", mode).unwrap();
    assert_eq!(
        d_qual.as_sql(),
        "DELETE FROM \"t\" WHERE _rowid_ IN (SELECT _rowid_ FROM \"t\" ORDER BY \"t\".\"id\" ASC LIMIT 1)"
    );
    assert_eq!(d_qual.ordered_columns(), &["id"]);

    // Rejections:
    for sql in [
        "DELETE FROM t LIMIT 1",
        "UPDATE t SET n = 0 LIMIT 1",
        "DELETE FROM t ORDER BY 1 LIMIT 1",
        "UPDATE t SET n = 0 ORDER BY 1 LIMIT 1",
        "DELETE FROM t ORDER BY other.id LIMIT 1",
        "UPDATE t SET n = 0 ORDER BY other.id LIMIT 1",
    ] {
        assert!(parse_dml(sql, mode).is_err(), "should reject: {sql}");
    }
}

/// `SET n = (SELECT MAX(m) FROM other)` takes one value out of another table.
/// The subquery has to answer exactly one row, which an aggregate over one
/// implicit group does and a plain column does not — MySQL answers 1242 for
/// that one. The pair of columns is recorded so the frontend can hold them to
/// the same kind, as it does for a comparison against a subquery.
#[test]
fn an_update_takes_one_value_out_of_another_table() {
    let mode = SessionSqlMode::default();
    for (sql, normalized) in [
        (
            "UPDATE users SET score = (SELECT MAX(points) FROM teams) WHERE id = 1",
            "UPDATE \"users\" SET \"score\" = (SELECT MAX(\"points\") AS \"MAX(points)\" FROM \"teams\") WHERE (\"id\" = 1)",
        ),
        (
            "UPDATE users SET name = (SELECT MIN(word) FROM teams WHERE id = 1) WHERE id = 2",
            "UPDATE \"users\" SET \"name\" = (SELECT MIN(\"word\") AS \"MIN(word)\" FROM \"teams\" WHERE (\"id\" = 1)) WHERE (\"id\" = 2)",
        ),
    ] {
        let translated = parse_dml(sql, mode).unwrap();
        assert_eq!(translated.as_sql(), normalized, "{sql}");
        assert!(translated.parse_ast().is_ok(), "{sql}");
    }

    let translated = parse_dml(
        "UPDATE users SET score = (SELECT MAX(points) FROM teams) WHERE id = 1",
        mode,
    )
    .unwrap();
    let [pair] = translated.checked_subquery_comparisons() else {
        panic!("one subquery pair is recorded");
    };
    assert_eq!(pair.column_name(), "score");
    assert_eq!(pair.inner_table(), "teams");
    assert_eq!(pair.inner_column_name(), "points");

    for sql in [
        // 1242 in MySQL: a plain column can answer more than one row.
        "UPDATE users SET score = (SELECT points FROM teams) WHERE id = 1",
        // A grouped subquery answers a row per group.
        "UPDATE users SET score = (SELECT MAX(points) FROM teams GROUP BY id) WHERE id = 1",
        // A count says nothing about the kind of the column written.
        "UPDATE users SET score = (SELECT COUNT(*) FROM teams) WHERE id = 1",
    ] {
        assert!(parse_dml(sql, mode).is_err(), "{sql}");
    }
}

/// `DEFAULT` written where a value goes asks for the column's own default.
/// The engine has no spelling for it, and leaving the column out of the
/// statement asks for the same thing: measured on 8.4.11, a column left out
/// and a column given `DEFAULT` both take the column's default, both leave a
/// nullable column with none at NULL, and both answer 1364 when the column is
/// NOT NULL with no default of its own. `DEFAULT(col)` naming that same column
/// is the other spelling and writes the same value.
#[test]
fn an_insert_takes_the_columns_own_default() {
    let mode = SessionSqlMode::default();
    for (sql, normalized) in [
        (
            "INSERT INTO d (id, n) VALUES (1, DEFAULT)",
            "INSERT INTO \"d\" (\"id\") VALUES (1)",
        ),
        (
            "INSERT INTO d (id, n) VALUES (1, DEFAULT(n))",
            "INSERT INTO \"d\" (\"id\") VALUES (1)",
        ),
        // The same column in every row, so leaving it out loses nothing.
        (
            "INSERT INTO d (id, n, word) VALUES (1, DEFAULT, 'x'), (2, DEFAULT, 'y')",
            "INSERT INTO \"d\" (\"id\", \"word\") VALUES (1, 'x'), (2, 'y')",
        ),
        // Every column defaulted is the row MySQL's own empty column list
        // writes.
        (
            "INSERT INTO d (id, n) VALUES (DEFAULT, DEFAULT)",
            "INSERT INTO \"d\" DEFAULT VALUES",
        ),
        (
            "REPLACE INTO d (id, n) VALUES (DEFAULT, DEFAULT)",
            "INSERT OR REPLACE INTO \"d\" DEFAULT VALUES",
        ),
        // A quoted `default` is an ordinary column name, which is what MySQL
        // takes it for.
        (
            "INSERT INTO d (id, `default`) VALUES (1, 2)",
            "INSERT INTO \"d\" (\"id\", \"default\") VALUES (1, 2)",
        ),
    ] {
        let translated = parse_dml(sql, mode).unwrap();
        assert_eq!(translated.as_sql(), normalized, "{sql}");
        assert!(translated.parse_ast().is_ok(), "{sql}");
    }

    for sql in [
        // Leaving the column out would take the default for every row, so the
        // row that wrote a value would lose it.
        "INSERT INTO d (id, n) VALUES (1, DEFAULT), (2, 3)",
        // What the offered row carries for a column left out has not been
        // measured.
        "INSERT INTO d (id, n) VALUES (1, DEFAULT) ON DUPLICATE KEY UPDATE n = 1",
        // A default named after some other column writes that column's default
        // into this one, which leaving the column out cannot say.
        "INSERT INTO d (id, n) VALUES (1, DEFAULT(word))",
        // `SET n = DEFAULT` writes the column's own default, which this cannot
        // work out from the statement alone.
        "UPDATE d SET n = DEFAULT WHERE id = 1",
        "UPDATE d SET n = DEFAULT(n) WHERE id = 1",
    ] {
        assert!(parse_dml(sql, mode).is_err(), "{sql}");
    }
}

/// The counted INSERT sees the column list the engine will run, not the one
/// that was written, so `DEFAULT` for the counted column reads as leaving it
/// out — which is the one shape the allocator can fill in.
#[test]
fn an_auto_increment_insert_takes_a_default_for_the_counted_column() {
    let checked = parse_auto_increment_insert(
        "INSERT INTO `users` (`id`, `name`) VALUES (DEFAULT, 'Ada'), (DEFAULT, 'Grace')",
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
        ["name"]
    );

    for sql in [
        // Every column defaulted renders as `DEFAULT VALUES`, which leaves no
        // row for the allocator to write its number into.
        "INSERT INTO `users` (`id`, `name`) VALUES (DEFAULT, DEFAULT)",
        // A row that wrote a value would lose it.
        "INSERT INTO `users` (`id`, `name`) VALUES (DEFAULT, 'Ada'), (7, 'Grace')",
    ] {
        assert!(
            parse_auto_increment_insert(sql, SessionSqlMode::default()).is_err(),
            "{sql}"
        );
    }
}

/// An integer's display width is taken and dropped, which is what MySQL 8.4
/// does with one — measured, `INT(11)` reads back as `int`. `TINYINT(1)` is
/// the one it keeps, and it is exactly what MySQL stores `BOOLEAN` as, so the
/// two spellings meet on one stored type.
#[test]
fn a_column_type_drops_the_display_width_it_was_written_with() {
    let mode = SessionSqlMode::default();
    for (sql, stored, printed) in [
        (
            "CREATE TABLE t (a INT(11))",
            "CREATE TABLE \"t\" (\"a\" INT)",
            "CREATE TABLE `t` (`a` INT)",
        ),
        (
            "CREATE TABLE t (a INTEGER(11))",
            "CREATE TABLE \"t\" (\"a\" INTEGER)",
            "CREATE TABLE `t` (`a` INTEGER)",
        ),
        (
            "CREATE TABLE t (a TINYINT(4))",
            "CREATE TABLE \"t\" (\"a\" TINYINT)",
            "CREATE TABLE `t` (`a` TINYINT)",
        ),
        (
            "CREATE TABLE t (a SMALLINT(6))",
            "CREATE TABLE \"t\" (\"a\" SMALLINT)",
            "CREATE TABLE `t` (`a` SMALLINT)",
        ),
        (
            "CREATE TABLE t (a MEDIUMINT(9))",
            "CREATE TABLE \"t\" (\"a\" MEDIUMINT)",
            "CREATE TABLE `t` (`a` MEDIUMINT)",
        ),
        (
            "CREATE TABLE t (a BIGINT(20) UNSIGNED)",
            "CREATE TABLE \"t\" (\"a\" BIGINT UNSIGNED)",
            "CREATE TABLE `t` (`a` BIGINT UNSIGNED)",
        ),
        // The one width MySQL keeps, which is what it stores BOOLEAN as.
        (
            "CREATE TABLE t (a TINYINT(1))",
            "CREATE TABLE \"t\" (\"a\" BOOLEAN)",
            "CREATE TABLE `t` (`a` BOOLEAN)",
        ),
        (
            "CREATE TABLE t (a BOOLEAN)",
            "CREATE TABLE \"t\" (\"a\" BOOLEAN)",
            "CREATE TABLE `t` (`a` BOOLEAN)",
        ),
        // Measured: `tinyint(1) unsigned` reads back as `tinyint unsigned`.
        (
            "CREATE TABLE t (a TINYINT(1) UNSIGNED)",
            "CREATE TABLE \"t\" (\"a\" TINYINT UNSIGNED)",
            "CREATE TABLE `t` (`a` TINYINT UNSIGNED)",
        ),
    ] {
        assert_eq!(
            parse_create_table(sql, mode).unwrap().as_sql(),
            stored,
            "{sql}"
        );
        let statement = parse_create_table_ast(sql, mode).unwrap();
        assert_eq!(
            render_create_table_mysql_with_mode(&statement, mode).unwrap(),
            printed,
            "{sql}"
        );
    }

    // The counted column a dump writes carries one too.
    let counted = parse_auto_increment_create_table(
        "CREATE TABLE t (id INT(11) NOT NULL AUTO_INCREMENT PRIMARY KEY, v INT(11))",
        mode,
    )
    .unwrap();
    assert_eq!(counted.allocator_column_name, "id");
}

/// `AUTO_INCREMENT=<n>` after the columns says where the numbering starts,
/// which is the option mysqldump writes on every table that has held a row.
///
/// Measured on MySQL 8.4.11: 0 and 1 both leave the counter where it starts,
/// the value is a plain whole number and nothing else, and the option may be
/// written in any position among the others.
#[test]
fn a_counted_table_reads_where_its_option_starts_the_numbering() {
    let mode = SessionSqlMode::default();
    for (sql, expected) in [
        (
            "CREATE TABLE t (id INT NOT NULL AUTO_INCREMENT, PRIMARY KEY (id)) ENGINE=InnoDB AUTO_INCREMENT=100",
            Some(100),
        ),
        (
            "CREATE TABLE t (id INT NOT NULL AUTO_INCREMENT, PRIMARY KEY (id)) auto_increment = 42 ENGINE=InnoDB DEFAULT CHARSET=utf8mb4",
            Some(42),
        ),
        (
            "CREATE TABLE t (id BIGINT NOT NULL AUTO_INCREMENT, PRIMARY KEY (id)) AUTO_INCREMENT=9223372036854775807",
            Some(9223372036854775807),
        ),
        (
            "CREATE TABLE t (id INT NOT NULL AUTO_INCREMENT, PRIMARY KEY (id)) AUTO_INCREMENT=1",
            None,
        ),
        (
            "CREATE TABLE t (id INT NOT NULL AUTO_INCREMENT, PRIMARY KEY (id)) AUTO_INCREMENT=0",
            None,
        ),
        (
            "CREATE TABLE t (id INT NOT NULL AUTO_INCREMENT, PRIMARY KEY (id))",
            None,
        ),
    ] {
        let checked = parse_auto_increment_create_table(sql, mode)
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
        assert_eq!(checked.starts_the_counter_at, expected, "{sql}");
        // The words say nothing the counter itself does not, so the stored
        // definition does not carry them and `SHOW CREATE TABLE` prints the
        // mark as it stands.
        assert!(
            !checked.normalized_mysql_ddl.contains("AUTO_INCREMENT="),
            "{sql}: {}",
            checked.normalized_mysql_ddl
        );
    }

    for sql in [
        // Measured: MySQL creates the table and answers 1467 for the first
        // row, a number no row of the column could be given.
        "CREATE TABLE t (id INT NOT NULL AUTO_INCREMENT, PRIMARY KEY (id)) AUTO_INCREMENT=99999999999",
        // Measured: `AUTO_INCREMENT=-5` and `AUTO_INCREMENT='7'` are each 1064,
        // and a fraction is rounded down rather than kept.
        "CREATE TABLE t (id INT NOT NULL AUTO_INCREMENT, PRIMARY KEY (id)) AUTO_INCREMENT=1.5",
        "CREATE TABLE t (id INT NOT NULL AUTO_INCREMENT, PRIMARY KEY (id)) AUTO_INCREMENT=100 AUTO_INCREMENT=200",
    ] {
        assert!(
            matches!(
                parse_auto_increment_create_table(sql, mode),
                Err(ParseError::Unsupported { .. })
            ),
            "expected unsupported error for {sql}"
        );
    }
}

/// A column `COMMENT` is what a dumped schema puts beside a column to say what
/// it holds, and it was refused outright.
///
/// The engine has no such attribute, so the words live in the stored MySQL DDL
/// alone, written back the way MySQL's own `SHOW CREATE TABLE` writes them.
/// Measured on 8.4.11 by reading the printed bytes: a quote is doubled, a
/// backslash is written twice, a newline becomes `\n`, a carriage return `\r`
/// and a zero byte `\0`, while a tab, a double quote, a `%` and a `_` each come
/// back raw. An empty comment is dropped, and the words are printed last —
/// after `AUTO_INCREMENT`, after `PRIMARY KEY` and after
/// `ON UPDATE CURRENT_TIMESTAMP`.
#[test]
fn a_column_keeps_the_comment_it_was_written_with() {
    let mode = SessionSqlMode::default();
    for (sql, expected) in [
        (
            "CREATE TABLE t (id INT NOT NULL COMMENT 'the id', PRIMARY KEY (id))",
            "`id` INT NOT NULL PRIMARY KEY COMMENT 'the id'",
        ),
        (
            "CREATE TABLE t (id INT NOT NULL, name VARCHAR(40) COMMENT '名前', PRIMARY KEY (id))",
            "`name` VARCHAR(40) COMMENT '名前'",
        ),
        (
            "CREATE TABLE t (id INT NOT NULL, n INT COMMENT 'it''s here', PRIMARY KEY (id))",
            "`n` INT COMMENT 'it''s here'",
        ),
        (
            r"CREATE TABLE t (id INT NOT NULL, n INT COMMENT 'back \\ slash', PRIMARY KEY (id))",
            r"`n` INT COMMENT 'back \\ slash'",
        ),
        (
            r"CREATE TABLE t (id INT NOT NULL, n INT COMMENT 'new \n line', PRIMARY KEY (id))",
            r"`n` INT COMMENT 'new \n line'",
        ),
        (
            r#"CREATE TABLE t (id INT NOT NULL, n INT COMMENT 'tab \t and " and %', PRIMARY KEY (id))"#,
            "`n` INT COMMENT 'tab \t and \" and %'",
        ),
        (
            // Measured: an empty comment is printed back as none at all.
            "CREATE TABLE t (id INT NOT NULL, n INT COMMENT '', PRIMARY KEY (id))",
            "`n` INT)",
        ),
        (
            // Measured: the words come after `ON UPDATE CURRENT_TIMESTAMP`.
            "CREATE TABLE t (id INT NOT NULL, at DATETIME ON UPDATE CURRENT_TIMESTAMP COMMENT 'when', PRIMARY KEY (id))",
            "`at` DATETIME ON UPDATE CURRENT_TIMESTAMP COMMENT 'when'",
        ),
    ] {
        let checked = parse_checked_primary_key_create_table(sql, mode)
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
        assert!(
            checked.normalized_mysql_ddl.contains(expected),
            "{sql}: {}",
            checked.normalized_mysql_ddl
        );
    }

    // Measured: the words come after `AUTO_INCREMENT PRIMARY KEY`, on the
    // counted column every dumped schema puts a comment on.
    let counted = parse_auto_increment_create_table(
        "CREATE TABLE t (id INT NOT NULL AUTO_INCREMENT COMMENT 'ID', n INT COMMENT 'a count', PRIMARY KEY (id))",
        mode,
    )
    .unwrap();
    assert!(
        counted
            .normalized_mysql_ddl
            .contains("`id` INT NOT NULL AUTO_INCREMENT PRIMARY KEY COMMENT 'ID'"),
        "{}",
        counted.normalized_mysql_ddl
    );

    assert_eq!(
        column_comments(
            "CREATE TABLE t (id INT NOT NULL COMMENT 'the id', n INT, m INT COMMENT '')",
            mode,
        )
        .unwrap(),
        vec![("id".to_owned(), "the id".to_owned())]
    );
}
