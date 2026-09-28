//! Tests for the checked MySQL parser.
//!
//! Kept in their own file because the crate root is long enough without
//! them; they still reach the parser's private items through `super`.

use super::*;
use turso_parser::ast::{AlterTable as TursoAlterTable, AlterTableBody as TursoAlterTableBody};

#[test]
fn json_columns_reject_defaults_and_indexes_mysql_rejects() {
    let mode = SessionSqlMode::default();
    let literal_default = "CREATE TABLE documents (doc JSON DEFAULT '{}')";
    assert_eq!(
        parse_create_table(literal_default, mode),
        Err(ParseError::JsonLiteralDefault)
    );
    assert!(matches!(
        parse_schema_ddl_ast(literal_default, mode),
        Err(ParseError::JsonLiteralDefault)
    ));
    for sql in [
        "CREATE TABLE documents (doc JSON PRIMARY KEY)",
        "CREATE TABLE documents (doc JSON UNIQUE)",
        "CREATE TABLE documents (doc JSON, PRIMARY KEY (doc))",
        "CREATE TABLE documents (doc JSON, UNIQUE KEY doc_key (doc))",
    ] {
        assert_eq!(
            parse_create_table(sql, mode),
            Err(ParseError::JsonIndex),
            "{sql}"
        );
    }
    assert!(parse_create_table("CREATE TABLE documents (doc JSON DEFAULT NULL)", mode).is_ok());
    assert_eq!(
        parse_optional_create_table_with_keys(
            "CREATE TABLE documents (doc JSON, KEY doc_key (doc))",
            mode,
        ),
        Err(ParseError::JsonIndex)
    );
    let alter_default = "ALTER TABLE documents ADD COLUMN doc JSON DEFAULT '{}'";
    assert!(matches!(
        parse_alter_table_ast(alter_default, mode),
        Err(ParseError::JsonLiteralDefault)
    ));
    assert!(parse_alter_table_ast(
        "ALTER TABLE documents ADD COLUMN doc JSON DEFAULT NULL",
        mode,
    )
    .is_ok());
}

#[test]
fn decimal_ddl_and_values_keep_their_written_digits() {
    let mode = SessionSqlMode::default();
    let table = parse_create_table(
        "CREATE TABLE amounts (v DECIMAL(65,30) DEFAULT 1.235, u DECIMAL(5,2) UNSIGNED)",
        mode,
    )
    .unwrap();
    assert!(table
        .as_sql()
        .contains("mysql_decimal(65,30) DEFAULT '1.235000000000000000000000000000'"));
    assert!(table.as_sql().contains("mysql_decimal_unsigned(5,2)"));

    let insert = parse_dml(
        "INSERT INTO amounts (v,u) VALUES (1.234567890123456789012345678901, 1.235)",
        mode,
    )
    .unwrap();
    assert!(insert
        .as_sql()
        .contains("'1.234567890123456789012345678901'"));
    assert!(insert.as_sql().contains("'1.235'"));

    assert_eq!(round_decimal_to_scale("-1.235", 5, 2).unwrap(), "-1.24");
    assert!(round_decimal_to_scale("999.995", 5, 2).is_err());
    assert!(round_decimal_to_scale("1e100000000", 65, 30).is_err());
    assert_eq!(
        round_decimal_to_scale("1e-100000000", 65, 30).unwrap(),
        format!("0.{}", "0".repeat(30))
    );
}

#[test]
fn decimal_aggregates_use_exact_core_functions() {
    assert!(parse_select(
        "SELECT SUM(v), AVG(v) FROM amounts",
        SessionSqlMode::default(),
    )
    .unwrap()
    .needs_column_types());
    let translated = parse_select_knowing_decimal_columns(
        "SELECT SUM(v), AVG(v) FROM amounts",
        SessionSqlMode::default(),
        &[],
        &[],
        &[],
        &[],
        &[],
        &[("v".to_string(), 2)],
    )
    .unwrap();
    assert!(translated.as_sql().contains("mysql_decimal_sum(\"v\")"));
    assert!(translated.as_sql().contains("mysql_decimal_avg(\"v\")"));
}

#[test]
fn a_rounded_total_or_average_rounds_the_exact_decimal_to_its_own_scale() {
    let sql =
        "SELECT ROUND(AVG(v), 2), ROUND(AVG(v), 9), ROUND(SUM(v), 1), ROUND(AVG(n)) FROM amounts";
    assert!(parse_select(sql, SessionSqlMode::default())
        .unwrap()
        .needs_column_types());
    let translated = parse_select_knowing_decimal_columns(
        sql,
        SessionSqlMode::default(),
        &[],
        &[],
        &[],
        &[],
        &[],
        &[("v".to_string(), 2)],
    )
    .unwrap();
    assert_eq!(
        translated.as_sql(),
        "SELECT mysql_decimal_round(mysql_decimal_avg(\"v\"), 2) AS \"ROUND(AVG(v), 2)\", mysql_decimal_round(mysql_decimal_avg(\"v\"), 6) AS \"ROUND(AVG(v), 9)\", mysql_decimal_round(mysql_decimal_sum(\"v\"), 1) AS \"ROUND(SUM(v), 1)\", mysql_decimal_round(mysql_decimal_avg(\"n\"), 0) AS \"ROUND(AVG(n))\" FROM \"amounts\""
    );
}

#[test]
fn decimal_arithmetic_keeps_fractional_literals_exact() {
    let mode = SessionSqlMode::default();
    for (source, rendered) in [
        ("v + 0.001", "numeric_add(\"v\", '0.001')"),
        ("v - 0.001", "numeric_sub(\"v\", '0.001')"),
        ("v * 0.001", "numeric_mul(\"v\", '0.001')"),
        (
            "SUM(v) + 0.001",
            "numeric_add(mysql_decimal_sum(\"v\"), '0.001')",
        ),
    ] {
        let sql = format!("SELECT {source} FROM amounts");
        assert!(
            parse_select(&sql, mode).unwrap().needs_column_types(),
            "{sql}"
        );
        let translated = parse_select_knowing_decimal_columns(
            &sql,
            mode,
            &[],
            &[],
            &[],
            &[],
            &[],
            &[("v".to_string(), 2)],
        )
        .unwrap();
        assert!(
            translated.as_sql().contains(rendered),
            "{sql}: {}",
            translated.as_sql()
        );
    }
    let wide = parse_select_knowing_decimal_columns(
        "SELECT v * 0.5 FROM amounts",
        mode,
        &[],
        &[],
        &[],
        &[],
        &[],
        &[("v".to_string(), 30)],
    )
    .unwrap();
    assert!(wide
        .as_sql()
        .contains("mysql_decimal_round(numeric_mul(\"v\", '0.5'), 30)"));
    let divided = parse_select_knowing_decimal_columns(
        "SELECT v / 2 FROM amounts",
        mode,
        &[],
        &[],
        &[],
        &[],
        &[],
        &[("v".to_string(), 2)],
    )
    .unwrap();
    assert!(divided
        .as_sql()
        .contains("mysql_decimal_div_round(\"v\", '2', 6)"));
    assert!(parse_select_knowing_decimal_columns(
        "SELECT v / 0 FROM amounts",
        mode,
        &[],
        &[],
        &[],
        &[],
        &[],
        &[("v".to_string(), 2)],
    )
    .is_err());
}

#[test]
fn scalar_numbers_and_mixed_numeric_columns_keep_their_kinds() {
    let mode = SessionSqlMode::default();
    let json = parse_select("SELECT JSON_ARRAY(1, 'a', NULL, 1.5) FROM amounts", mode).unwrap();
    assert!(json.as_sql().contains("json_array(1, 'a', NULL, 1.5)"));
    let deviation = parse_select("SELECT STDDEV_SAMP(n) FROM amounts", mode).unwrap();
    assert!(deviation.as_sql().contains("stddev(\"n\")"));
    let truncate_sql = "SELECT TRUNCATE(amount, 1) FROM amounts";
    assert!(parse_select(truncate_sql, mode)
        .unwrap()
        .needs_column_types());
    let decimal_truncate = parse_select_knowing_numeric_columns(
        truncate_sql,
        mode,
        &[],
        &[],
        &[],
        &[],
        &[],
        &[("amount".to_string(), 2)],
        &[],
        &[],
    )
    .unwrap();
    assert!(decimal_truncate
        .as_sql()
        .contains("mysql_decimal_truncate(\"amount\", 1)"));
    for (expression, expected) in [
        ("n + amount", "numeric_add(\"n\", \"amount\")"),
        (
            "SUM(n) + SUM(amount)",
            "numeric_add(SUM(\"n\"), mysql_decimal_sum(\"amount\"))",
        ),
    ] {
        let sql = format!("SELECT {expression} FROM amounts");
        let translated = parse_select_knowing_numeric_columns(
            &sql,
            mode,
            &[],
            &[],
            &[],
            &[],
            &[],
            &[("amount".to_string(), 2)],
            &["n".to_string()],
            &["d".to_string()],
        )
        .unwrap();
        assert!(
            translated.as_sql().contains(expected),
            "{sql}: {}",
            translated.as_sql()
        );
    }
    assert!(parse_select_knowing_numeric_columns(
        "SELECT d + amount FROM amounts",
        mode,
        &[],
        &[],
        &[],
        &[],
        &[],
        &[("amount".to_string(), 2)],
        &["n".to_string()],
        &["d".to_string()],
    )
    .is_err());
}

#[test]
fn translates_the_checked_sqlite_subset() {
    let translated = parse_create_table(
        "CREATE TABLE app.users (id INTEGER NOT NULL UNIQUE, name TEXT NOT NULL UNIQUE DEFAULT 'guest', data BLOB, CHECK (id >= 0), FOREIGN KEY (id) REFERENCES accounts (id) ON DELETE CASCADE)",
        SessionSqlMode::default(),
    )
    .unwrap();

    assert_eq!(
        translated.as_sql(),
        "CREATE TABLE \"app\".\"users\" (\"id\" INTEGER NOT NULL UNIQUE, \"name\" TEXT NOT NULL UNIQUE DEFAULT 'guest' COLLATE MYSQL_UCA9_AI_CI, \"data\" BLOB, CHECK (id >= 0), FOREIGN KEY (\"id\") REFERENCES \"accounts\" (\"id\") ON DELETE CASCADE)"
    );
}

#[test]
fn mysql_ddl_renderer_hides_both_current_and_legacy_internal_collations() {
    for internal in ["MYSQL_UCA9_AI_CI", "NOCASE"] {
        let statement =
            parse_sqlite_create_table(&format!("CREATE TABLE t (name TEXT COLLATE {internal})"));
        let shown =
            render_create_table_mysql_with_mode(&statement, SessionSqlMode::default()).unwrap();
        assert!(shown.contains("`name` TEXT"), "{internal}: {shown}");
        assert!(!shown.contains("COLLATE"), "{internal}: {shown}");
    }
}

#[test]
fn foreign_key_only_create_table_uses_the_atomic_index_path() {
    let checked = parse_optional_create_table_with_keys(
        "CREATE TABLE child (a INT, CONSTRAINT fk_a FOREIGN KEY (a) REFERENCES parent(id))",
        SessionSqlMode::default(),
    )
    .unwrap()
    .expect("a foreign key needs an index beside the table");
    assert_eq!(checked.table().as_str(), "child");
    assert!(checked.indexes().is_empty());
    assert!(checked.table_sql().contains("FOREIGN KEY"));

    let named = parse_optional_create_table_with_keys(
        "CREATE TABLE child2 (a INT, KEY MiXeD(a))",
        SessionSqlMode::default(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(named.indexes()[0].name(), "MiXeD");
}

#[test]
fn an_explicit_utf8mb4_bin_column_keeps_its_pad_space_collation() {
    let sql = "CREATE TABLE t (name VARCHAR(8) COLLATE utf8mb4_bin UNIQUE)";
    let translated = parse_create_table(sql, SessionSqlMode::default()).unwrap();
    assert!(translated.as_sql().contains("COLLATE MYSQL_UTF8MB4_BIN"));
    let statement = parse_create_table_ast(sql, SessionSqlMode::default()).unwrap();
    let shown = render_create_table_mysql_with_mode(&statement, SessionSqlMode::default()).unwrap();
    assert!(shown.contains("COLLATE utf8mb4_bin"), "{shown}");
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
        r#"CREATE TABLE "t" ("value" TEXT DEFAULT 'a\nb' COLLATE MYSQL_UCA9_AI_CI)"#
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
            "CREATE TABLE \"t\" (\"value\" INT DEFAULT 1)",
            "CREATE TABLE `t` (`value` INT DEFAULT 1)",
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
        "CREATE TABLE `app`.`users` (`id` INTEGER NOT NULL UNIQUE, `name` TEXT NOT NULL UNIQUE DEFAULT 'guest', `data` BLOB, CHECK (`id` >= 0), FOREIGN KEY (`id`) REFERENCES `accounts`(`id`) ON DELETE CASCADE)"
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
        "SELECT \"id\" FROM \"users\" WHERE (\"name\" IS 'admin')"
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
        "SELECT \"id\" FROM \"users\" AS \"u\" WHERE (\"u\".\"name\" = 'alice')"
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
        "SELECT \"id\" FROM \"users\" AS \"u\" WHERE (mysql_uca9_like(\"u\".\"name\", 'a%', '\\'))"
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
fn a_text_comparison_uses_the_columns_stored_collation() {
    // A bare column keeps its stored collation, including an explicit
    // utf8mb4_bin override.
    let translated = parse_select(
        "SELECT id FROM users WHERE name = 'a''b' AND id = 1",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert_eq!(
        translated.as_sql(),
        "SELECT \"id\" FROM \"users\" WHERE ((\"name\" = 'a''b') AND (\"id\" = 1))"
    );
    assert_eq!(
        translated.checked_comparisons()[0].rhs(),
        &CheckedSelectComparisonRhs::Text("a'b".to_string())
    );
}

#[test]
fn text_equality_order_group_and_unique_key_use_the_same_uca9_collation() {
    let mode = SessionSqlMode::default();
    let table = parse_create_table("CREATE TABLE names (name VARCHAR(64) UNIQUE)", mode).unwrap();
    assert!(table
        .as_sql()
        .contains("\"name\" VARCHAR(64) UNIQUE COLLATE MYSQL_UCA9_AI_CI"));

    let equality = parse_select("SELECT name FROM names WHERE name = 'café'", mode).unwrap();
    assert!(equality.as_sql().contains("\"name\" = 'café'"));

    let columns = ["name".to_owned()];
    let ordered = parse_select_with_column_types(
        "SELECT name FROM names ORDER BY name",
        mode,
        &columns,
        &columns,
        &[],
    )
    .unwrap();
    assert!(ordered.as_sql().contains("ORDER BY \"name\" ASC"));

    let grouped = parse_select_with_column_types(
        "SELECT name, COUNT(*) FROM names GROUP BY name",
        mode,
        &columns,
        &columns,
        &[],
    )
    .unwrap();
    // The CREATE TABLE collation travels with the column reference used by
    // GROUP BY, so the grouping key needs no explicit COLLATE expression.
    assert!(grouped.as_sql().contains("GROUP BY \"name\""));
}

#[test]
fn a_scalar_call_renders_as_the_engine_spells_it() {
    for (sql, rendered) in [
        (
            "SELECT LOWER(v) FROM s",
            "SELECT mysql_lower(\"v\") AS \"LOWER(v)\" FROM \"s\"",
        ),
        (
            "SELECT UPPER(v) FROM s",
            "SELECT mysql_upper(\"v\") AS \"UPPER(v)\" FROM \"s\"",
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
            "SELECT mysql_lower(\"v\") AS \"folded\" FROM \"s\"",
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
            "SELECT mysql_round(\"n\", 0) AS \"ROUND(n)\" FROM \"s\"",
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
            "SELECT mysql_instr(\"v\", 'b') AS \"INSTR(v, 'b')\" FROM \"s\"",
        ),
        (
            "SELECT LOCATE('b', v) FROM s",
            "SELECT mysql_locate('b', \"v\") AS \"LOCATE('b', v)\" FROM \"s\"",
        ),
        (
            "SELECT HEX(v) FROM s",
            concat!(
                "SELECT CASE WHEN \"v\" IS NULL THEN NULL WHEN typeof(\"v\") IN ('integer', 'real') ",
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
            "SELECT CASE WHEN typeof(\"n\") = 'text' OR typeof(10) = 'text' THEN mysql_text_greatest(\"n\", 10) ELSE max(\"n\", 10) END AS \"GREATEST(n, 10)\" FROM \"s\"",
        ),
        (
            "SELECT LEAST(n, 10) FROM s",
            "SELECT CASE WHEN typeof(\"n\") = 'text' OR typeof(10) = 'text' THEN mysql_text_least(\"n\", 10) ELSE min(\"n\", 10) END AS \"LEAST(n, 10)\" FROM \"s\"",
        ),
        (
            "SELECT NULLIF(n, 0) FROM s",
            "SELECT CASE WHEN typeof(\"n\") = 'text' THEN mysql_text_nullif(\"n\", 0) ELSE nullif(\"n\", 0) END AS \"NULLIF(n, 0)\" FROM \"s\"",
        ),
        (
            "SELECT NULLIF(v, 'abc') FROM s",
            "SELECT CASE WHEN typeof(\"v\") = 'text' THEN mysql_text_nullif(\"v\", 'abc') ELSE nullif(\"v\", 'abc') END AS \"NULLIF(v, 'abc')\" FROM \"s\"",
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
            "SELECT mysql_substring(\"v\", 1, 2) AS \"SUBSTRING(v, 1, 2)\" FROM \"s\"",
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
        // `SUBSTR` is `SUBSTRING`, and the count can be left out.
        (
            "SELECT SUBSTR(v, -2) FROM s",
            "SELECT mysql_substring(\"v\", (-2)) AS \"SUBSTR(v, -2)\" FROM \"s\"",
        ),
        (
            "SELECT SUBSTRING_INDEX(v, '@', -1) FROM s",
            "SELECT mysql_substring_index(\"v\", '@', -1) AS \"SUBSTRING_INDEX(v, '@', -1)\" FROM \"s\"",
        ),
        (
            "SELECT CONCAT_WS('-', v, 'x', n) FROM s",
            "SELECT concat_ws('-', \"v\", 'x', \"n\") AS \"CONCAT_WS('-', v, 'x', n)\" FROM \"s\"",
        ),
        (
            "SELECT SHA2(v, 256) FROM s",
            "SELECT mysql_sha2(\"v\", 256) AS \"SHA2(v, 256)\" FROM \"s\"",
        ),
        (
            "SELECT SUBSTRING(v FROM 1 FOR 2) FROM s",
            "SELECT mysql_substring(\"v\", 1, 2) AS \"SUBSTRING(v FROM 1 FOR 2)\" FROM \"s\"",
        ),
        (
            "SELECT FLOOR(n) FROM s",
            "SELECT floor(\"n\") AS \"FLOOR(n)\" FROM \"s\"",
        ),
        (
            "SELECT CEIL(n) FROM s",
            "SELECT ceil(\"n\") AS \"CEIL(n)\" FROM \"s\"",
        ),
        (
            "SELECT CEILING(n) FROM s",
            "SELECT ceil(\"n\") AS \"CEILING(n)\" FROM \"s\"",
        ),
        // ROUND names its places, and the dialect rounds each kind of number
        // the way MySQL does.
        (
            "SELECT ROUND(n, -1) FROM s",
            "SELECT mysql_round(\"n\", -1) AS \"ROUND(n, -1)\" FROM \"s\"",
        ),
        // `%`, `DIV` and a negated column keep MySQL's spelling as their name.
        (
            "SELECT n % 2, n DIV 2, -n FROM s",
            "SELECT (\"n\" % 2) AS \"n % 2\", (\"n\" / 2) AS \"n DIV 2\", (-\"n\") AS \"-n\" FROM \"s\"",
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
        // NOW takes no more than six places of a second.
        "SELECT LOWER(v || 'z') FROM s",
        "SELECT NOW(7) FROM s",
        // A count this cannot read leaves no width to answer with.
        "SELECT LEFT(v, n) FROM s",
        "SELECT SUBSTRING(v, 1, n) FROM s",
        "SELECT SUBSTRING(v, n) FROM s",
        "SELECT SUBSTRING_INDEX(v, '@', n) FROM s",
        // MySQL answers NULL for a size SHA2 does not have, with a warning.
        "SELECT SHA2(v, 1) FROM s",
        "SELECT CONCAT_WS(NULL, v) FROM s",
        // MySQL removes whole copies of what TRIM was given where the engine
        // removes any of its characters, and they only agree on one.
        "SELECT TRIM(LEADING 'ax' FROM v) FROM s",
        "SELECT TRIM(LEADING v FROM v) FROM s",
        "SELECT TRIM('abc') FROM s",
        // MySQL's bare side is `TRIM(LEADING FROM v)`, which the parser
        // library does not read; what it does read is not MySQL.
        "SELECT TRIM(LEADING v) FROM s",
        // A CASE whose every branch is NULL has no width left, and a written
        // word beside a written number is a coercion.
        "SELECT CASE WHEN n > 1 THEN NULL ELSE NULL END FROM s",
        "SELECT CASE WHEN n > 1 THEN 1 ELSE 'n' END FROM s",
        // `CASE col WHEN` compares by one rule chosen over every value, which
        // is the rule each comparison on its own chooses only when the values
        // are of one kind.
        "SELECT CASE n WHEN 1 THEN 'y' WHEN '2' THEN 'z' END FROM s",
        "SELECT CASE n WHEN v THEN 'y' END FROM s",
        "SELECT CASE n + 1 WHEN 1 THEN 'y' END FROM s",
        // A fallback that can be null defeats the point of IFNULL.
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

/// A `CASE` naming a column and `IFNULL` over two columns are written by what
/// their columns hold, which only a second reading with the table's column
/// types knows. The first one says so.
#[test]
fn a_condition_over_a_column_waits_for_the_column_types() {
    for sql in [
        "SELECT CASE WHEN n > 1 THEN v ELSE 'n' END FROM s",
        "SELECT IFNULL(n, v) FROM s",
        "SELECT COALESCE(n, v, w) FROM s",
        "SELECT SUM(CASE WHEN n > 1 THEN m ELSE 0 END) FROM s",
    ] {
        let translated = parse_select(sql, SessionSqlMode::default()).unwrap();
        assert!(
            translated.renders_a_condition_without_column_types(),
            "{sql}"
        );
        assert!(translated.needs_column_types(), "{sql}");
    }
    // `CASE col WHEN` over written values is `CASE WHEN col = value`, which
    // needs no column type to be written.
    let translated = parse_select(
        "SELECT CASE n WHEN 1 THEN 'y' ELSE 'n' END FROM s",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert!(!translated.renders_a_condition_without_column_types());
    assert_eq!(
        translated.as_sql(),
        concat!(
            "SELECT CASE WHEN (\"n\" = 1) THEN 'y' ELSE 'n' END ",
            "AS \"CASE n WHEN 1 THEN 'y' ELSE 'n' END\" FROM \"s\""
        )
    );
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

    // A body that aggregates says what each of its columns answers, and one
    // projecting `*` reads its table column for column.
    let translated = parse_select(
        "WITH c AS (SELECT n, COUNT(*) AS k FROM f GROUP BY n) SELECT c.n, c.k FROM c WHERE c.k > 1",
        SessionSqlMode::default(),
    )
    .unwrap();
    let [source] = translated.source_tables() else {
        panic!("one table");
    };
    let derived = source.derived().unwrap();
    assert!(derived.materialized());
    assert_eq!(derived.names(), ["n", "k"]);
    assert_eq!(derived.answer(0), None);
    assert_eq!(derived.answer(1), Some(&StaticSelectMetadata::Count));
    // The count answers a whole number, which is what its comparison is
    // held to.
    let [comparison] = translated.checked_comparisons() else {
        panic!("one comparison");
    };
    assert_eq!(
        comparison.answers(),
        Some(CheckedComparisonAnswer::WholeNumber)
    );
    let translated = parse_select(
        "WITH c AS (SELECT * FROM f WHERE id > 1) SELECT c.id FROM c",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert!(translated.source_tables()[0].projected_columns().is_empty());

    for sql in [
        // MySQL reads a body that does not aggregate straight through, and
        // what it reports for a call there has not been measured.
        "WITH c AS (SELECT id + 1 FROM f) SELECT c.id FROM c",
        "WITH c AS (SELECT DATE(d) AS day FROM f) SELECT c.day FROM c",
        // 1060: two columns going by one name.
        "WITH c AS (SELECT COUNT(*) AS k, MAX(id) AS k FROM f) SELECT c.k FROM c",
        // A total has not been measured against a value.
        "WITH c AS (SELECT SUM(id) AS s FROM f) SELECT c.s FROM c WHERE c.s > 1",
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
fn a_recursive_cte_counting_through_numbers_reads_no_table() {
    let translated = parse_select(
        "WITH RECURSIVE n AS (SELECT 1 AS x UNION ALL SELECT x + 2 FROM n WHERE x <= 9) SELECT x FROM n WHERE x > 3",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert_eq!(
        translated.as_sql(),
        concat!(
            "WITH RECURSIVE \"n\"(\"x\") AS (SELECT 1 UNION ALL SELECT \"x\" + 2 FROM \"n\" WHERE \"x\" <= 9) ",
            "SELECT \"x\" FROM \"n\" WHERE (\"x\" > 3)"
        )
    );
    // The sequence reads no table, and its columns hold whole numbers.
    assert!(translated.source_tables().is_empty());
    assert_eq!(
        translated.checked_comparisons()[0].answers(),
        Some(CheckedComparisonAnswer::WholeNumber)
    );
    assert_eq!(
        translated.static_result_metadata(),
        [StaticSelectProjectionMetadata::Literal(
            StaticSelectMetadata::CountedColumn {
                table: "n".to_owned(),
                column: "x".to_owned(),
                length: 2,
            }
        )]
    );
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

    // One column against another by anything but equality is held to the
    // rule a WHERE holds it to, which the frontend applies.
    let ordered = parse_select(
        "SELECT users.id FROM users JOIN accounts ON users.id > accounts.user_id",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert_eq!(
        ordered.checked_comparisons()[0].rhs(),
        &CheckedSelectComparisonRhs::Column {
            qualifier: Some("accounts".to_owned()),
            name: "user_id".to_owned(),
        }
    );

    for sql in [
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
            "JOIN \"b\" ON (\"a\".\"id\" = \"b\".\"a_id\") WHERE (\"b\".\"tag\" = 'z'))"
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
    // The two columns are held to each other by the frontend, which can see
    // both of their types.
    assert_eq!(
        with_literal.checked_comparisons()[0].rhs(),
        &CheckedSelectComparisonRhs::Column {
            qualifier: Some("accounts".to_owned()),
            name: "user_id".to_owned(),
        }
    );
    assert_eq!(
        with_literal.checked_comparisons()[1].qualifier(),
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
        // A name on the offered row with no upsert to use it names nothing.
        (
            "INSERT INTO up (id, hits) VALUES (4, 1), (5, 2) AS offered",
            "INSERT INTO \"up\" (\"id\", \"hits\") VALUES (4, 1), (5, 2)",
        ),
        // A column read before the clause writes it, and the offered row read
        // after, answer the same whichever order the assignments run in.
        (
            "INSERT INTO up (id, a, b) VALUES (1, 5, 5) ON DUPLICATE KEY UPDATE b = a, a = a + 10, b2 = VALUES(a)",
            "INSERT INTO \"up\" (\"id\", \"a\", \"b\") VALUES (1, 5, 5) ON CONFLICT DO UPDATE SET \"b\" = \"a\", \"a\" = (\"a\" + 10), \"b2\" = \"excluded\".\"a\"",
        ),
        (
            "INSERT INTO up (id, a, b) VALUES (1, 5, 5) AS o ON DUPLICATE KEY UPDATE a = o.a, b = o.a + 1",
            "INSERT INTO \"up\" (\"id\", \"a\", \"b\") VALUES (1, 5, 5) ON CONFLICT DO UPDATE SET \"a\" = \"excluded\".\"a\", \"b\" = (\"excluded\".\"a\" + 1)",
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
        // Renaming what the offered row carries has not been measured.
        "INSERT INTO up (id, hits) VALUES (2, 3) AS offered (a, b) ON DUPLICATE KEY UPDATE hits = offered.b",
        // MySQL reads what the clause wrote a moment before; the engine reads
        // the row as it stood.
        "INSERT INTO up (id, a, b) VALUES (1, 5, 5) ON DUPLICATE KEY UPDATE a = a + 10, b = a",
        "INSERT INTO up (id, a, b) VALUES (1, 5, 5) AS o ON DUPLICATE KEY UPDATE a = o.a, b = up.a + 1",
        "INSERT INTO up (id, a, b) VALUES (1, 5, 5) ON DUPLICATE KEY UPDATE a = 1, a = 2",
    ] {
        assert!(parse_dml(sql, SessionSqlMode::default()).is_err(), "{sql}");
    }
}

/// Rails 8's `upsert_all` touches `updated_at` only for a row whose named
/// columns it changes, comparing each with `<=>` — the engine's `IS` — and
/// writes the same clause with `VALUES()` for a server older than 8.0.19.
#[test]
fn rails_touch_of_an_upserted_row_renders_as_the_engines_case() {
    let touched = "CASE WHEN ((\"name\" IS \"excluded\".\"name\") AND (\"balance\" IS \"excluded\".\"balance\")) THEN \"updated_at\" ELSE (substr(strftime('%Y-%m-%d %H:%M:%f', 'now'), 1, 23) || '000') END";
    for sql in [
        "INSERT INTO `users` (`email`,`name`,`balance`,`updated_at`) VALUES ('a@x', 'A', 1.0, CURRENT_TIMESTAMP(6)) AS `users_values` ON DUPLICATE KEY UPDATE updated_at=(CASE WHEN (`users`.`name`<=>`users_values`.`name` AND `users`.`balance`<=>`users_values`.`balance`) THEN `users`.updated_at ELSE CURRENT_TIMESTAMP(6) END),`name`=`users_values`.`name`,`balance`=`users_values`.`balance`",
        "INSERT INTO `users` (`email`,`name`,`balance`,`updated_at`) VALUES ('a@x', 'A', 1.0, CURRENT_TIMESTAMP(6)) ON DUPLICATE KEY UPDATE updated_at=(CASE WHEN (`name`<=>VALUES(`name`) AND `balance`<=>VALUES(`balance`)) THEN updated_at ELSE CURRENT_TIMESTAMP(6) END),`name`=VALUES(`name`),`balance`=VALUES(`balance`)",
    ] {
        let translated = parse_dml(sql, SessionSqlMode::default()).unwrap();
        assert!(
            translated.as_sql().contains(&format!("\"updated_at\" = {touched}, ")),
            "{sql}\n{}",
            translated.as_sql()
        );
        assert!(translated.parse_ast().is_ok(), "{sql}");
    }
    for clause in [
        // Two columns compared with each other.
        "updated_at = (CASE WHEN (u.name <=> o.balance) THEN u.updated_at ELSE NOW(6) END)",
        // Anything kept but the column written.
        "updated_at = (CASE WHEN (u.name <=> o.name) THEN u.created_at ELSE NOW(6) END)",
        // Anything written but the clock.
        "updated_at = (CASE WHEN (u.name <=> o.name) THEN u.updated_at ELSE o.updated_at END)",
        // A comparison other than `<=>`, and more than one condition.
        "updated_at = (CASE WHEN (u.name = o.name) THEN u.updated_at ELSE NOW(6) END)",
        "updated_at = (CASE WHEN u.name <=> o.name THEN u.updated_at WHEN u.balance <=> o.balance THEN u.updated_at ELSE NOW(6) END)",
        // A bare column is 1052 once the offered row carries a name.
        "updated_at = (CASE WHEN (name <=> o.name) THEN u.updated_at ELSE NOW(6) END)",
    ] {
        let sql = format!(
            "INSERT INTO u (email, name, balance, updated_at) VALUES ('a@x', 'A', 1.0, NOW(6)) AS o ON DUPLICATE KEY UPDATE {clause}"
        );
        assert!(parse_dml(&sql, SessionSqlMode::default()).is_err(), "{sql}");
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
            "UPDATE \"users\" SET \"name\" = mysql_lower(\"name\") WHERE (\"id\" = 1)",
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
/// `utf8mb4_bin` compares bytes after ignoring trailing spaces. The engine
/// needs its own PAD SPACE collation because a text column uses UCA9 by default.
#[test]
fn a_comparison_renders_the_collation_it_names() {
    for (sql, normalized) in [
        (
            "SELECT id FROM users WHERE name = 'a' COLLATE utf8mb4_bin",
            "SELECT \"id\" FROM \"users\" WHERE (\"name\" COLLATE MYSQL_UTF8MB4_BIN = 'a')",
        ),
        (
            "SELECT id FROM users WHERE name COLLATE utf8mb4_bin = 'a'",
            "SELECT \"id\" FROM \"users\" WHERE (\"name\" COLLATE MYSQL_UTF8MB4_BIN = 'a')",
        ),
        (
            "SELECT id FROM users WHERE name = 'a' COLLATE utf8mb4_0900_ai_ci",
            "SELECT \"id\" FROM \"users\" WHERE (\"name\" COLLATE MYSQL_UCA9_AI_CI = 'a')",
        ),
        (
            "SELECT id FROM users WHERE name < 'a' COLLATE utf8mb4_bin",
            "SELECT \"id\" FROM \"users\" WHERE (\"name\" COLLATE MYSQL_UTF8MB4_BIN < 'a')",
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

/// An ordering may name a collation. The first rendering pass has no column
/// type, and the second applies PAD SPACE to a text column when requested.
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
    let columns = ["name".to_owned()];
    let with_types = parse_select_with_column_types(
        "SELECT id FROM users ORDER BY name COLLATE utf8mb4_bin",
        SessionSqlMode::default(),
        &columns,
        &columns,
        &[],
    )
    .unwrap();
    assert!(with_types
        .as_sql()
        .contains("ORDER BY \"name\" COLLATE MYSQL_UTF8MB4_BIN ASC"));
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

/// `TIMESTAMPDIFF` is counted by the frontend's own reading, which counts a
/// month by the calendar and a second to the microsecond. Each moment is a
/// column, a reading of the clock, or a moment written out as a word.
#[test]
fn timestampdiff_renders_as_the_frontends_count() {
    for (sql, normalized) in [
        (
            "SELECT TIMESTAMPDIFF(SECOND, a, b) FROM td",
            "SELECT mysql_timestampdiff('second', \"a\", \"b\") AS \"TIMESTAMPDIFF(SECOND, a, b)\" FROM \"td\"",
        ),
        (
            "SELECT TIMESTAMPDIFF(MONTH, a, b) FROM td",
            "SELECT mysql_timestampdiff('month', \"a\", \"b\") AS \"TIMESTAMPDIFF(MONTH, a, b)\" FROM \"td\"",
        ),
        (
            "SELECT timestampdiff(microsecond, a, b) FROM td",
            "SELECT mysql_timestampdiff('microsecond', \"a\", \"b\") AS \"timestampdiff(microsecond, a, b)\" FROM \"td\"",
        ),
        (
            "SELECT TIMESTAMPDIFF(DAY, a, NOW()) FROM td",
            "SELECT mysql_timestampdiff('day', \"a\", datetime('now')) AS \"TIMESTAMPDIFF(DAY, a, NOW())\" FROM \"td\"",
        ),
        (
            "SELECT TIMESTAMPDIFF(YEAR, '2000-02-29', CURDATE()) FROM td",
            "SELECT mysql_timestampdiff('year', '2000-02-29', date('now')) AS \"TIMESTAMPDIFF(YEAR, '2000-02-29', CURDATE())\" FROM \"td\"",
        ),
    ] {
        let translated = parse_select(sql, SessionSqlMode::default()).unwrap();
        assert_eq!(translated.as_sql(), normalized, "{sql}");
        assert!(translated.parse_ast().is_ok(), "{sql}");
    }
    for sql in [
        "SELECT TIMESTAMPDIFF(FORTNIGHT, a, b) FROM td",
        "SELECT TIMESTAMPDIFF(`DAY`, a, b) FROM td",
        // A time of day is a span rather than a moment.
        "SELECT TIMESTAMPDIFF(DAY, a, CURTIME()) FROM td",
        // A word that names no moment answers NULL with a warning in MySQL.
        "SELECT TIMESTAMPDIFF(DAY, a, '2024-02-30') FROM td",
        "SELECT TIMESTAMPDIFF(DAY, a, ?) FROM td",
        "SELECT TIMESTAMPDIFF(DAY, a, 20240101) FROM td",
        "SELECT TIMESTAMPDIFF(DAY, a) FROM td",
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
            "SELECT \"id\" FROM \"users\" WHERE (mysql_uca9_like(\"name\", '%lph%', '\\'))",
        ),
        (
            "SELECT id FROM users WHERE name LIKE '%lph%'",
            "SELECT \"id\" FROM \"users\" WHERE (mysql_uca9_like(\"name\", '%lph%', '\\'))",
        ),
        (
            "SELECT id FROM users WHERE name NOT LIKE CONCAT('al', '%')",
            "SELECT \"id\" FROM \"users\" WHERE (NOT mysql_uca9_like(\"name\", 'al%', '\\'))",
        ),
        // A backslash remains the pattern escape after written pieces join.
        (
            "SELECT id FROM users WHERE name LIKE CONCAT('a\\\\b', '%')",
            "SELECT \"id\" FROM \"users\" WHERE (mysql_uca9_like(\"name\", 'a\\b%', '\\'))",
        ),
        (
            "SELECT id FROM users WHERE name LIKE CONCAT('%', ?, '%')",
            "SELECT \"id\" FROM \"users\" WHERE (mysql_uca9_like(\"name\", ('%' || ? || '%'), '\\'))",
        ),
        // A pattern written as one `?` remains one bound argument.
        (
            "SELECT id FROM users WHERE name LIKE ?",
            "SELECT \"id\" FROM \"users\" WHERE (mysql_uca9_like(\"name\", ?, '\\'))",
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

    // MySQL rounds AVG to four places and compares the column against that
    // decimal, so the engine's exact decimal average is compared as a number.
    let translated = parse_select(
        "SELECT id FROM users WHERE score > (SELECT AVG(score) FROM users)",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert_eq!(
        translated.as_sql(),
        "SELECT \"id\" FROM \"users\" WHERE numeric_lt((SELECT mysql_decimal_avg(\"score\") AS \"AVG(score)\" FROM \"users\"), \"score\")"
    );
    assert_eq!(
        translated
            .checked_comparisons()
            .iter()
            .map(|comparison| (comparison.qualifier(), comparison.column_name()))
            .collect::<Vec<_>>(),
        [(None, "score"), (Some("users"), "score")]
    );

    for sql in [
        // 1242 in MySQL: a plain column can answer more than one row.
        "SELECT id FROM users WHERE id = (SELECT owner_id FROM teams)",
        // A total has not been measured against a column.
        "SELECT id FROM users WHERE score > (SELECT SUM(score) FROM users)",
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
    // A rollup is each level's grouped statement, joined, ordered by each key
    // after whether that level rolled it up.
    let translated = parse_select(
        "SELECT team, COUNT(*) FROM users GROUP BY team WITH ROLLUP",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert_eq!(
        translated.as_sql(),
        concat!(
            "SELECT \"team\", \"COUNT(*)\" FROM (",
            "SELECT \"team\" AS \"team\", COUNT(*) AS \"COUNT(*)\", 0 AS \"__rollup_level_0\", ",
            "\"team\" AS \"__rollup_key_0\" FROM \"users\" GROUP BY \"team\" UNION ALL ",
            "SELECT NULL AS \"team\", COUNT(*) AS \"COUNT(*)\", 1 AS \"__rollup_level_0\", ",
            "NULL AS \"__rollup_key_0\" FROM \"users\" HAVING (COUNT(*) > 0)",
            ") AS \"__rollup\" ORDER BY \"__rollup_level_0\", \"__rollup_key_0\""
        )
    );
    // A key that is a call is rendered the way the projection renders it, so
    // the engine groups on the value the client reads back.
    let translated = parse_select(
        "SELECT date(joined) AS d, COUNT(*) FROM users GROUP BY DATE(joined) HAVING d > '2026-01-01' ORDER BY d",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert_eq!(
        translated.as_sql(),
        "SELECT date(\"joined\") AS \"d\", COUNT(*) AS \"COUNT(*)\" FROM \"users\" GROUP BY date(\"joined\") HAVING (date(\"joined\") > '2026-01-01') ORDER BY \"d\" ASC"
    );

    // A column beside the keys is left to the frontend, which knows whether
    // the keys hold a key of its table and so decide it.
    let translated = parse_select(
        "SELECT team, score FROM users GROUP BY team",
        SessionSqlMode::default(),
    )
    .unwrap();
    let decided = translated
        .columns_the_keys_decide()
        .expect("score is not a key");
    assert_eq!(
        decided
            .columns()
            .iter()
            .map(|column| (column.table(), column.column()))
            .collect::<Vec<_>>(),
        [(None, "score")]
    );
    assert_eq!(
        decided
            .keys()
            .iter()
            .map(|column| (column.table(), column.column()))
            .collect::<Vec<_>>(),
        [(None, "team")]
    );
    let translated = parse_select(
        "SELECT u.name FROM posts p LEFT JOIN users u ON u.id = p.user_id AND u.active = 1 GROUP BY p.id",
        SessionSqlMode::default(),
    )
    .unwrap();
    let [join] = translated.columns_the_keys_decide().unwrap().joins() else {
        panic!("one join");
    };
    assert_eq!(join.reference(), "u");
    assert!(join.left_join());
    assert_eq!(join.matched_columns().len(), 1);
    assert_eq!(
        join.other_columns()
            .unwrap()
            .iter()
            .map(|column| (column.table(), column.column()))
            .collect::<Vec<_>>(),
        [(Some("u"), "active")]
    );

    for sql in [
        // Each projection here lands in one row of several, which MySQL
        // answers 1055 for under its own default sql_mode.
        "SELECT * FROM users GROUP BY team",
        // A subquery, a derived table and a branch of a `UNION` do not have
        // the columns their keys decide worked out.
        "SELECT (SELECT name FROM users GROUP BY id) FROM teams",
        "SELECT t.name FROM (SELECT name FROM users GROUP BY id) t",
        "SELECT name FROM users GROUP BY id UNION SELECT name FROM teams",
        // A comma, `USING` and a `RIGHT JOIN` are not read for what they
        // match.
        "SELECT u.name FROM users u, posts p GROUP BY u.id",
        "SELECT u.name FROM users u JOIN posts p USING (id) GROUP BY u.id",
        "SELECT u.name FROM users u RIGHT JOIN posts p ON p.user_id = u.id GROUP BY u.id",
        // A key that is an expression has to be one the engine groups the
        // way MySQL does.
        "SELECT team FROM users GROUP BY team + 1",
        "SELECT UPPER(team), COUNT(*) FROM users GROUP BY UPPER(team)",
        // A key inside a larger expression is not the key: 1055.
        "SELECT UPPER(DATE(joined)) FROM users GROUP BY DATE(joined)",
        // 1055 for an ordered column, 1054 for a HAVING column.
        "SELECT team FROM users GROUP BY team ORDER BY score",
        "SELECT id FROM users GROUP BY id HAVING score > 1",
        "SELECT DATE(joined) FROM users GROUP BY DATE(joined) HAVING DATE(joined) > '2026-01-01'",
        // A rollup over an expression, a rollup read again for each level's
        // parameter, and one ordered, whose shapes MySQL answers otherwise.
        "SELECT UPPER(team), COUNT(*) FROM users GROUP BY UPPER(team) WITH ROLLUP",
        "SELECT team, COUNT(*) FROM users WHERE id > ? GROUP BY team WITH ROLLUP",
        "SELECT team, COUNT(*) FROM users GROUP BY team WITH ROLLUP ORDER BY team",
        "SELECT team, COUNT(*) FROM users GROUP BY team WITH ROLLUP HAVING team = 'a'",
        "SELECT team, COUNT(*) FROM users GROUP BY a, b, c, d WITH ROLLUP",
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
fn a_like_uses_mysql_uca9_weights_and_names_its_escape() {
    let translated = parse_select(
        "SELECT id FROM users WHERE name LIKE 'a%' AND name NOT LIKE '_b'",
        SessionSqlMode::default(),
    )
    .unwrap();
    // MySQL takes a backslash as the pattern's escape where the statement
    // names none.
    assert_eq!(
        translated.as_sql(),
        "SELECT \"id\" FROM \"users\" WHERE ((mysql_uca9_like(\"name\", 'a%', '\\')) AND (NOT mysql_uca9_like(\"name\", '_b', '\\')))"
    );
    assert_eq!(
        translated.checked_comparisons()[1].operator(),
        CheckedSelectComparisonOperator::NotLike
    );

    // An escaped wildcard and an explicit escape reach the same matcher.
    for (sql, normalized) in [
        (
            "SELECT id FROM users WHERE name LIKE 'a\\%'",
            "SELECT \"id\" FROM \"users\" WHERE (mysql_uca9_like(\"name\", 'a\\%', '\\'))",
        ),
        (
            "SELECT id FROM users WHERE name LIKE 'a%' ESCAPE '!'",
            "SELECT \"id\" FROM \"users\" WHERE (mysql_uca9_like(\"name\", 'a%', '!'))",
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
    // Under `NO_BACKSLASH_ESCAPES` a pattern has no implicit escape.
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
        "SELECT \"id\" FROM \"users\" WHERE (mysql_uca9_like(\"name\", 'a\\%', ''))"
    );
    // A `LIKE` reads one column, and a qualifier names a table this cannot
    // resolve it against.
    assert!(parse_select(
        "SELECT id FROM users WHERE other.name LIKE 'a%'",
        SessionSqlMode::default()
    )
    .is_err());

    // A pattern is bound as readily as it is written.
    let bound = parse_select(
        "SELECT id FROM users WHERE name LIKE ?",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert_eq!(
        bound.as_sql(),
        "SELECT \"id\" FROM \"users\" WHERE (mysql_uca9_like(\"name\", ?, '\\'))"
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
        let sql = format!("SELECT id FROM users WHERE id {operator} CAST(1 AS SIGNED)");
        assert!(
            parse_select(&sql, SessionSqlMode::default()).is_err(),
            "expected unsupported comparison form for {sql}"
        );
        // Another column is taken here and held to the first by the frontend,
        // which can see both types.
        let sql = format!("SELECT id FROM users WHERE id {operator} other_id");
        assert!(matches!(
            parse_select(&sql, SessionSqlMode::default())
                .unwrap()
                .checked_comparisons(),
            [comparison] if matches!(comparison.rhs(), CheckedSelectComparisonRhs::Column { .. })
        ));
        for rhs in ["9223372036854775808", "-9223372036854775809"] {
            let sql = format!("SELECT id FROM users WHERE id {operator} {rhs}");
            assert!(parse_select(&sql, SessionSqlMode::default())
                .unwrap()
                .needs_column_types());
            assert!(parse_select_knowing_decimal_columns(
                &sql,
                SessionSqlMode::default(),
                &[],
                &["id".to_string()],
                &[],
                &[],
                &[],
                &[],
            )
            .is_err());
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
            "SELECT \"id\" FROM \"users\" WHERE (\"id\" = 'admin')",
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
        "SELECT \"id\" FROM \"users\" WHERE (\"name\" IN ('a', 'C'))"
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
        "SELECT \"id\" FROM \"users\" WHERE (\"name\" IN (?, ?))"
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
            "SELECT \"id\" FROM \"users\" WHERE (\"n\" > '1.5')",
            "1.5",
        ),
        (
            "SELECT id FROM users WHERE n >= 1e6",
            "SELECT \"id\" FROM \"users\" WHERE (\"n\" >= '1e6')",
            "1e6",
        ),
        (
            "SELECT id FROM users WHERE n > -1.5",
            "SELECT \"id\" FROM \"users\" WHERE (\"n\" > '-1.5')",
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

    // A large integer needs a second pass to prove its column is DECIMAL.
    let large = "SELECT id FROM users WHERE n > 9223372036854775808";
    assert!(parse_select(large, SessionSqlMode::default())
        .unwrap()
        .needs_column_types());
    assert!(parse_select_knowing_decimal_columns(
        large,
        SessionSqlMode::default(),
        &[],
        &["id".to_string(), "n".to_string()],
        &[],
        &[],
        &[],
        &[],
    )
    .is_err());
}

#[test]
fn decimal_comparison_accepts_65_digit_integer_only_for_decimal_column() {
    let digits = "1".repeat(65);
    let sql = format!("SELECT v FROM amounts WHERE v = {digits}");
    assert!(parse_select(&sql, SessionSqlMode::default())
        .unwrap()
        .needs_column_types());
    let translated = parse_select_knowing_decimal_columns(
        &sql,
        SessionSqlMode::default(),
        &[],
        &["v".to_string()],
        &[],
        &[],
        &[],
        &[("v".to_string(), 0)],
    )
    .unwrap();
    assert!(translated
        .as_sql()
        .contains(&format!("WHERE (\"v\" = '{digits}')")));
    assert!(parse_select_knowing_decimal_columns(
        &sql,
        SessionSqlMode::default(),
        &[],
        &["v".to_string()],
        &[],
        &[],
        &[],
        &[],
    )
    .is_err());
}

#[test]
fn exact_numeric_in_list_keeps_unsigned_bigint_literals() {
    let sql = "SELECT v FROM amounts WHERE v IN (9223372036854775808, 18446744073709551615)";
    assert!(parse_select(sql, SessionSqlMode::default())
        .unwrap()
        .needs_column_types());
    let translated = parse_select_knowing_decimal_columns(
        sql,
        SessionSqlMode::default(),
        &[],
        &["v".to_string()],
        &[],
        &[],
        &[],
        &[("v".to_string(), 0)],
    )
    .unwrap();
    assert!(translated
        .as_sql()
        .contains("numeric_eq(\"v\", '9223372036854775808')"));
    assert!(translated
        .as_sql()
        .contains("numeric_eq(\"v\", '18446744073709551615')"));
}

#[test]
fn exact_numeric_parameter_order_uses_numeric_comparison() {
    let sql = "SELECT v FROM amounts WHERE v < ?";
    let translated = parse_select_knowing_decimal_columns(
        sql,
        SessionSqlMode::default(),
        &[],
        &["v".to_string()],
        &[],
        &[],
        &[],
        &[("v".to_string(), 0)],
    )
    .unwrap();
    assert!(translated.as_sql().contains("numeric_lt(\"v\", ?)"));
    let null_safe = parse_select_knowing_decimal_columns(
        "SELECT v FROM amounts WHERE v <=> ?",
        SessionSqlMode::default(),
        &[],
        &["v".to_string()],
        &[],
        &[],
        &[],
        &[("v".to_string(), 0)],
    )
    .unwrap();
    assert!(null_safe.as_sql().contains("numeric_nullsafe_eq(\"v\", ?)"));
    let literal = parse_select_knowing_decimal_columns(
        "SELECT v FROM amounts WHERE v <=> 18446744073709551615",
        SessionSqlMode::default(),
        &[],
        &["v".to_string()],
        &[],
        &[],
        &[],
        &[("v".to_string(), 0)],
    )
    .unwrap();
    assert!(literal
        .as_sql()
        .contains("numeric_nullsafe_eq(\"v\", '18446744073709551615')"));
}

#[test]
fn unsigned_bigint_arithmetic_checks_its_result_range() {
    let translated = parse_select_knowing_numeric_columns(
        "SELECT v + 1, v - 1, v * 2 FROM amounts",
        SessionSqlMode::default(),
        &[],
        &["v".to_string()],
        &[],
        &[],
        &[],
        &[("v".to_string(), 0)],
        &["v".to_string()],
        &[],
    )
    .unwrap();
    assert!(translated
        .as_sql()
        .contains("mysql_uint64_result(numeric_add(\"v\", '1'))"));
    assert!(translated
        .as_sql()
        .contains("mysql_uint64_result(numeric_sub(\"v\", '1'))"));
    assert!(translated
        .as_sql()
        .contains("mysql_uint64_result(numeric_mul(\"v\", '2'))"));
    let summed = parse_select_knowing_numeric_columns(
        "SELECT SUM(v) + 1 FROM amounts",
        SessionSqlMode::default(),
        &[],
        &["v".to_string()],
        &[],
        &[],
        &[],
        &[("v".to_string(), 0)],
        &["v".to_string()],
        &[],
    )
    .unwrap();
    assert!(summed
        .as_sql()
        .contains("numeric_add(mysql_decimal_sum(\"v\"), '1')"));
}

#[test]
fn decimal_abs_keeps_exact_digits_and_scale() {
    let sql = "SELECT ABS(v) FROM amounts";
    assert!(parse_select(sql, SessionSqlMode::default())
        .unwrap()
        .needs_column_types());
    let translated = parse_select_knowing_decimal_columns(
        sql,
        SessionSqlMode::default(),
        &[],
        &["v".to_string()],
        &[],
        &[],
        &[],
        &[("v".to_string(), 2)],
    )
    .unwrap();
    assert!(translated
        .as_sql()
        .contains("CASE WHEN numeric_lt(\"v\", '0') THEN numeric_sub('0', \"v\") ELSE \"v\" END"));
    translated.parse_ast().unwrap();
}

#[test]
fn decimal_scalar_calls_without_exact_comparison_are_rejected() {
    // ROUND is rounded as a DECIMAL, to the places held to the column's scale.
    assert_eq!(
        parse_select_knowing_decimal_columns(
            "SELECT ROUND(v), ROUND(v, 1), ROUND(v, 5), FORMAT(v, 40) FROM amounts",
            SessionSqlMode::default(),
            &[],
            &["v".to_string()],
            &[],
            &[],
            &[],
            &[("v".to_string(), 2)],
        )
        .unwrap()
        .as_sql(),
        concat!(
            "SELECT mysql_decimal_round(\"v\", 0) AS \"ROUND(v)\", ",
            "mysql_decimal_round(\"v\", 1) AS \"ROUND(v, 1)\", ",
            "mysql_decimal_round(\"v\", 2) AS \"ROUND(v, 5)\", ",
            "mysql_format(mysql_decimal_round(\"v\", 30), 30) AS \"FORMAT(v, 40)\" ",
            "FROM \"amounts\""
        )
    );
    for sql in [
        "SELECT ROUND(v, -1) FROM amounts",
        "SELECT -v FROM amounts",
        "SELECT v % 2 FROM amounts",
        "SELECT v DIV 2 FROM amounts",
        "SELECT MOD(v, 3) FROM amounts",
        "SELECT GREATEST(v, w) FROM amounts",
        "SELECT LEAST(v, w) FROM amounts",
        "SELECT HEX(v) FROM amounts",
        "SELECT NULLIF(v, 0) FROM amounts",
        "SELECT CAST(v AS SIGNED) FROM amounts",
        "SELECT CONVERT(v, SIGNED) FROM amounts",
        "SELECT SQRT(v) FROM amounts",
        "SELECT POW(v, 2) FROM amounts",
        "SELECT LENGTH(v) FROM amounts",
        "SELECT SUBSTRING(v, 1, 1) FROM amounts",
        "SELECT TRIM(v) FROM amounts",
        "SELECT EXTRACT(YEAR FROM v) FROM amounts",
    ] {
        assert!(
            parse_select(sql, SessionSqlMode::default())
                .unwrap()
                .needs_column_types(),
            "{sql}"
        );
        assert!(
            parse_select_knowing_decimal_columns(
                sql,
                SessionSqlMode::default(),
                &[],
                &["v".to_string(), "w".to_string()],
                &[],
                &[],
                &[],
                &[("v".to_string(), 2), ("w".to_string(), 2)],
            )
            .is_err(),
            "{sql}"
        );
    }
}

#[test]
fn typed_select_keeps_non_decimal_casts_and_defaulted_aggregates() {
    for sql in [
        "SELECT CAST(ratio AS SIGNED), CAST(n AS SIGNED) FROM readings ORDER BY id",
        "SELECT IFNULL(SUM(n), 0) FROM readings",
        "SELECT COALESCE(MAX(n), 0) FROM readings",
        "SELECT IFNULL(COUNT(*), 0) FROM readings",
    ] {
        let translated = parse_select_knowing_decimal_columns(
            sql,
            SessionSqlMode::default(),
            &[],
            &[
                "id".to_string(),
                "ratio".to_string(),
                "n".to_string(),
                "money".to_string(),
            ],
            &[],
            &[],
            &[],
            &[("money".to_string(), 2)],
        )
        .unwrap_or_else(|error| panic!("{sql}: {error}"));
        translated.parse_ast().unwrap();
    }

    for sql in [
        "SELECT CAST(money AS SIGNED) FROM readings",
        "SELECT IFNULL(SUM(money), 0) FROM readings",
    ] {
        assert!(
            parse_select_knowing_decimal_columns(
                sql,
                SessionSqlMode::default(),
                &[],
                &[
                    "id".to_string(),
                    "ratio".to_string(),
                    "n".to_string(),
                    "money".to_string()
                ],
                &[],
                &[],
                &[],
                &[("money".to_string(), 2)],
            )
            .is_err(),
            "{sql}"
        );
    }
}

#[test]
fn decimal_window_arguments_are_rejected() {
    for sql in [
        "SELECT SUM(v) OVER (ORDER BY id) FROM amounts",
        "SELECT AVG(v) OVER (ORDER BY id) FROM amounts",
        "SELECT LAG(v) OVER (ORDER BY id) FROM amounts",
    ] {
        assert!(
            parse_select_knowing_decimal_columns(
                sql,
                SessionSqlMode::default(),
                &[],
                &["id".to_string(), "v".to_string()],
                &[],
                &[],
                &[],
                &[("v".to_string(), 2)],
            )
            .is_err(),
            "{sql}"
        );
    }
}

#[test]
fn decimal_order_expressions_require_exact_numeric_ordering() {
    for sql in [
        "SELECT id FROM amounts ORDER BY ABS(v)",
        "SELECT id FROM amounts ORDER BY GREATEST(v, w)",
        "SELECT id FROM amounts ORDER BY v + 1",
        "SELECT id FROM amounts GROUP BY id ORDER BY SUM(v)",
        "SELECT ABS(v) AS magnitude FROM amounts ORDER BY magnitude",
        "SELECT v + 1 AS next_amount FROM amounts ORDER BY next_amount",
        "SELECT SUM(v) AS total FROM amounts ORDER BY total",
    ] {
        assert!(
            parse_select(sql, SessionSqlMode::default())
                .unwrap()
                .needs_column_types(),
            "{sql}"
        );
        assert!(
            parse_select_knowing_decimal_columns(
                sql,
                SessionSqlMode::default(),
                &[],
                &["id".to_string(), "v".to_string(), "w".to_string()],
                &[],
                &[],
                &[],
                &[("v".to_string(), 2), ("w".to_string(), 2)],
            )
            .is_err(),
            "{sql}"
        );
    }

    let sql = "SELECT id FROM amounts ORDER BY LENGTH(v)";
    assert!(parse_select(sql, SessionSqlMode::default())
        .unwrap()
        .needs_column_types());
    assert!(parse_select_knowing_decimal_columns(
        sql,
        SessionSqlMode::default(),
        &[],
        &["id".to_string(), "v".to_string()],
        &[],
        &[],
        &[],
        &[("v".to_string(), 2)],
    )
    .is_err());

    let sql = "SELECT v AS amount FROM amounts ORDER BY amount";
    assert!(parse_select_knowing_decimal_columns(
        sql,
        SessionSqlMode::default(),
        &[],
        &["v".to_string()],
        &[],
        &[],
        &[],
        &[("v".to_string(), 2)],
    )
    .is_ok());
}

#[test]
fn decimal_predicates_do_not_use_blob_numeric_or_text_rules() {
    for sql in [
        "SELECT id FROM amounts WHERE FIELD(v, '2') = 1",
        "SELECT id FROM amounts WHERE LENGTH(v) = 4",
        "SELECT id FROM amounts WHERE v LIKE '1%'",
        "SELECT id FROM amounts WHERE v REGEXP '^1'",
        "SELECT id FROM amounts WHERE v",
        "SELECT id FROM amounts GROUP BY id HAVING SUM(v) > 1",
    ] {
        assert!(
            parse_select(sql, SessionSqlMode::default())
                .unwrap()
                .needs_column_types(),
            "{sql}"
        );
        assert!(
            parse_select_knowing_decimal_columns(
                sql,
                SessionSqlMode::default(),
                &[],
                &["id".to_string(), "v".to_string()],
                &[],
                &[],
                &[],
                &[("v".to_string(), 2)],
            )
            .is_err(),
            "{sql}"
        );
    }
}

/// The members follow the same coercion rule a single literal comparison
/// follows, so nothing but an exact integer, a number written with a fraction,
/// a string, NULL or `?` reaches the engine.
#[test]
fn select_in_list_refuses_members_a_comparison_would_refuse() {
    for sql in [
        "SELECT id FROM users WHERE id IN (1, id)",
        "SELECT id FROM users WHERE id + 1 IN (1, 2)",
        "SELECT id FROM users WHERE u.id IN (1, 2)",
    ] {
        assert!(
            parse_select(sql, SessionSqlMode::default()).is_err(),
            "{sql}"
        );
    }
    let large = "SELECT id FROM users WHERE id IN (9223372036854775808)";
    assert!(parse_select(large, SessionSqlMode::default())
        .unwrap()
        .needs_column_types());
    assert!(parse_select_knowing_decimal_columns(
        large,
        SessionSqlMode::default(),
        &[],
        &["id".to_string()],
        &[],
        &[],
        &[],
        &[],
    )
    .is_err());
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

/// `JSON_EXTRACT` and `JSON_UNQUOTE` over it are read by the dialect: the
/// engine's `->` reads `$[0]` over a lone value as nothing where MySQL reads
/// the value, and its `->>` answers the JSON null as no value where MySQL
/// answers the word `null`.
#[test]
fn select_json_extract_renders_the_dialect_reading() {
    let mode = SessionSqlMode::default();
    let translated = parse_select("SELECT JSON_EXTRACT(doc, '$.a') FROM j", mode).unwrap();
    assert_eq!(
        translated.as_sql(),
        concat!(
            "SELECT mysql_json_extract(\"doc\", '$.a') ",
            "AS \"JSON_EXTRACT(doc, '$.a')\" FROM \"j\""
        )
    );

    let unquoted =
        parse_select("SELECT JSON_UNQUOTE(JSON_EXTRACT(doc, '$.s')) FROM j", mode).unwrap();
    assert_eq!(
        unquoted.as_sql(),
        concat!(
            "SELECT mysql_json_unquote(mysql_json_extract(\"doc\", '$.s')) ",
            "AS \"JSON_UNQUOTE(JSON_EXTRACT(doc, '$.s'))\" FROM \"j\""
        )
    );

    // MySQL spells the same two readings with operators too.
    let arrow = parse_select("SELECT doc -> '$.a' FROM j", mode).unwrap();
    assert_eq!(
        arrow.as_sql(),
        "SELECT mysql_json_extract(\"doc\", '$.a') AS \"doc -> '$.a'\" FROM \"j\""
    );
    let long_arrow = parse_select("SELECT doc ->> '$.s' FROM j", mode).unwrap();
    assert_eq!(
        long_arrow.as_sql(),
        concat!(
            "SELECT mysql_json_unquote(mysql_json_extract(\"doc\", '$.s')) ",
            "AS \"doc ->> '$.s'\" FROM \"j\""
        )
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

#[test]
fn json_string_where_compares_the_document_string_as_bytes() {
    let json_columns = vec!["doc".to_owned()];
    let parse = |sql| {
        parse_select_knowing_json_columns(
            sql,
            SessionSqlMode::default(),
            &[],
            &["id".to_owned(), "doc".to_owned()],
            &[],
            &[],
            &[],
            &[],
            &[],
            &[],
            &json_columns,
        )
        .unwrap()
    };
    let equal = parse("SELECT id FROM documents WHERE doc = 'word'");
    assert!(equal
        .as_sql()
        .contains("CAST(\"doc\" AS BLOB) = CAST(mysql_json_quote('word') AS BLOB)"));
    let members = parse("SELECT id FROM documents WHERE doc IN ('word', 'Word')");
    assert!(members.as_sql().contains(
        "CAST(\"doc\" AS BLOB) = CAST(mysql_json_quote('word') AS BLOB) OR CAST(\"doc\" AS BLOB) = CAST(mysql_json_quote('Word') AS BLOB)"
    ));
    let number = parse("SELECT id FROM documents WHERE doc = 9007199254740993");
    assert!(number
        .as_sql()
        .contains("mysql_json_equals_integer(\"doc\", 9007199254740993)"));
    let ordered_number = parse("SELECT id FROM documents WHERE doc < 9007199254740993");
    assert!(ordered_number
        .as_sql()
        .contains("mysql_json_compare_integer(\"doc\", 9007199254740993) < 0"));
    let ordered_text = parse("SELECT id FROM documents WHERE doc >= 'a'");
    assert!(ordered_text
        .as_sql()
        .contains("mysql_json_compare_string(\"doc\", 'a') >= 0"));
    let mixed = parse("SELECT id FROM documents WHERE doc IN (1, 'word')");
    assert!(mixed.as_sql().contains(
        "mysql_json_equals_integer(\"doc\", 1) OR CAST(\"doc\" AS BLOB) = CAST(mysql_json_quote('word') AS BLOB)"
    ));
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

#[test]
fn select_order_by_a_set_uses_the_declared_bits() {
    let set_columns = vec![(
        "flags".to_string(),
        vec!["read".to_string(), "write".to_string(), "exec".to_string()],
    )];
    let mode = SessionSqlMode::default();
    let initial = parse_select("SELECT id FROM permissions ORDER BY flags", mode).unwrap();
    assert!(initial.needs_column_types());

    for sql in [
        "SELECT id FROM permissions ORDER BY flags",
        "SELECT id FROM permissions ORDER BY permissions.flags",
        "SELECT id, flags FROM permissions ORDER BY 2",
        "SELECT flags AS ordered FROM permissions ORDER BY ordered",
    ] {
        let translated =
            parse_select_knowing_the_columns(sql, mode, &[], &[], &[], &set_columns, &[]).unwrap();
        assert!(
            matches!(translated.parse_ast(), Ok(Stmt::Select(_))),
            "{sql}"
        );
        let order = translated.as_sql().split_once(" ORDER BY ").unwrap().1;
        assert!(order.contains("',exec,') > 0 END ASC"), "{sql}: {order}");
        assert!(order.contains("',write,') > 0 END ASC"), "{sql}: {order}");
        assert!(order.contains("',read,') > 0 END ASC"), "{sql}: {order}");
        assert!(
            order.find("',exec,')").unwrap() < order.find("',write,')").unwrap()
                && order.find("',write,')").unwrap() < order.find("',read,')").unwrap(),
            "{sql}: {order}"
        );
        assert!(order.contains("IS NULL THEN NULL"), "{sql}: {order}");
    }

    let descending = parse_select_knowing_the_columns(
        "SELECT * FROM permissions ORDER BY 2 DESC",
        mode,
        &[],
        &["id".to_string(), "flags".to_string()],
        &[],
        &set_columns,
        &[],
    )
    .unwrap();
    assert!(descending.as_sql().contains("',exec,') > 0 END DESC, CASE"));

    let alias = parse_select_knowing_the_columns(
        "SELECT id AS flags FROM permissions ORDER BY flags",
        mode,
        &[],
        &[],
        &[],
        &set_columns,
        &[],
    )
    .unwrap();
    assert!(alias.as_sql().ends_with(" ORDER BY \"flags\" ASC"));
}

#[test]
fn select_order_by_a_set_keeps_all_64_bits_exact() {
    let members = (0..64).map(|bit| format!("bit{bit}")).collect::<Vec<_>>();
    let set_columns = vec![("flags".to_string(), members)];
    let translated = parse_select_knowing_the_columns(
        "SELECT flags FROM permissions ORDER BY flags",
        SessionSqlMode::default(),
        &[],
        &[],
        &[],
        &set_columns,
        &[],
    )
    .unwrap();
    assert!(matches!(translated.parse_ast(), Ok(Stmt::Select(_))));
    let order = translated.as_sql().split_once(" ORDER BY ").unwrap().1;
    assert_eq!(order.matches(" END ASC").count(), 64);
    assert!(order.find("',bit63,')").unwrap() < order.find("',bit0,')").unwrap());
}

#[test]
fn float_columns_are_included_in_assignment_metadata() {
    let spec = parse_mysql_numeric_spec(
        "CREATE TABLE samples (f FLOAT, fu FLOAT UNSIGNED, d DOUBLE, n INT)",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert!(!spec.is_empty());
    assert!(spec.is_float(0));
    assert!(spec.is_float(1));
    assert!(!spec.is_float(2));
    assert!(!spec.is_float(3));
    assert!(spec.is_unsigned_real(1));
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
        "SELECT \"id\", \"name\" FROM \"users\" ORDER BY \"name\" ASC"
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
        "SELECT * FROM \"users\" ORDER BY \"name\" ASC"
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
        "SELECT * FROM \"users\" ORDER BY \"name\" DESC, \"id\" ASC"
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
    // A body projecting its whole table reads it column for column, so the
    // derived table is the table under another name.
    let translated = parse_select("SELECT id FROM (SELECT * FROM users) AS rows", mode).unwrap();
    let [source] = translated.source_tables() else {
        panic!("a derived table is one source");
    };
    assert_eq!(
        (source.reference(), source.table().as_str()),
        ("rows", "users")
    );
    assert!(source.projected_columns().is_empty());
    assert!(source
        .derived()
        .is_some_and(|derived| derived.names().is_empty()));

    assert!(
        parse_select("SELECT id FROM app.users", mode).is_err(),
        "expected source-table metadata to reject a qualified table"
    );
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
    assert_eq!(
        MySqlIntegerType::BigInt.bounds(),
        (i128::from(i64::MIN), i128::from(i64::MAX))
    );

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

/// MySQL's unsigned bounds include all 64 bits of BIGINT. The engine stores
/// that last type as an exact custom numeric blob; its MySQL name is kept in
/// the frontend metadata.
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
    let translated = parse_create_table(create, SessionSqlMode::default()).unwrap();
    assert!(translated.as_sql().contains("\"e\" mysql_uint64"));
    for (ordinal, integer_type, bounds) in [
        (0, MySqlIntegerType::TinyIntUnsigned, (0, 255)),
        (1, MySqlIntegerType::SmallIntUnsigned, (0, 65_535)),
        (2, MySqlIntegerType::MediumIntUnsigned, (0, 16_777_215)),
        (3, MySqlIntegerType::IntUnsigned, (0, 4_294_967_295)),
        (4, MySqlIntegerType::BigIntUnsigned, (0, u64::MAX as i128)),
    ] {
        assert_eq!(spec.column(ordinal), Some(integer_type), "{ordinal}");
        assert_eq!(integer_type.bounds(), bounds, "{ordinal}");
        assert!(integer_type.is_unsigned(), "{ordinal}");
    }
    assert!(!MySqlIntegerType::Int.is_unsigned());
}

#[test]
fn bigint_unsigned_default_keeps_u64_max_as_decimal_text() {
    let translated = parse_create_table(
        "CREATE TABLE unsigned_default (v BIGINT UNSIGNED DEFAULT 18446744073709551615)",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert!(translated
        .as_sql()
        .contains("\"v\" mysql_uint64 DEFAULT '18446744073709551615'"));
    assert!(parse_create_table(
        "CREATE TABLE unsigned_default (v BIGINT UNSIGNED DEFAULT 18446744073709551616)",
        SessionSqlMode::default(),
    )
    .is_err());
}

/// Measured on MySQL 8.4.11: a word naming a number is taken as the default
/// of a column of whole numbers and rounded half away from zero, and one
/// outside the column's range after rounding, or naming no number, is 1067.
#[test]
fn a_whole_number_column_takes_a_word_naming_a_number_as_its_default() {
    let stored = |declared: &str| {
        parse_create_table(
            &format!("CREATE TABLE t (v {declared})"),
            SessionSqlMode::default(),
        )
        .map(|translated| translated.as_sql().to_owned())
    };
    for (declared, expected) in [
        ("INT NOT NULL DEFAULT '5'", "\"v\" INT NOT NULL DEFAULT 5"),
        ("INT DEFAULT '4.5'", "\"v\" INT DEFAULT 5"),
        ("INT DEFAULT '4.4'", "\"v\" INT DEFAULT 4"),
        ("INT DEFAULT '-4.5'", "\"v\" INT DEFAULT -5"),
        ("INT DEFAULT ' 7'", "\"v\" INT DEFAULT 7"),
        ("INT DEFAULT '  -3  '", "\"v\" INT DEFAULT -3"),
        ("INT DEFAULT '+8'", "\"v\" INT DEFAULT 8"),
        ("INT DEFAULT '007'", "\"v\" INT DEFAULT 7"),
        ("INT DEFAULT '.5'", "\"v\" INT DEFAULT 1"),
        ("INT DEFAULT '5.'", "\"v\" INT DEFAULT 5"),
        ("INT DEFAULT '1e2'", "\"v\" INT DEFAULT 100"),
        ("INT DEFAULT '25e-1'", "\"v\" INT DEFAULT 3"),
        ("INT DEFAULT 4.5", "\"v\" INT DEFAULT 5"),
        ("INT DEFAULT -2147483648.4", "\"v\" INT DEFAULT -2147483648"),
        ("SMALLINT DEFAULT '-3'", "\"v\" SMALLINT DEFAULT -3"),
        (
            "TINYINT NOT NULL DEFAULT '1'",
            "\"v\" TINYINT NOT NULL DEFAULT 1",
        ),
        (
            "TINYINT(1) NOT NULL DEFAULT '0'",
            "\"v\" BOOLEAN NOT NULL DEFAULT 0",
        ),
        ("BOOLEAN DEFAULT '2'", "\"v\" BOOLEAN DEFAULT 2"),
        (
            "INT UNSIGNED DEFAULT '-0.4'",
            "\"v\" INT UNSIGNED DEFAULT 0",
        ),
        (
            "INT UNSIGNED DEFAULT '4294967295'",
            "\"v\" INT UNSIGNED DEFAULT 4294967295",
        ),
        (
            "BIGINT DEFAULT '-9223372036854775808.4'",
            "\"v\" BIGINT DEFAULT -9223372036854775808",
        ),
        (
            "BIGINT UNSIGNED DEFAULT ' 7'",
            "\"v\" mysql_uint64 DEFAULT '7'",
        ),
        (
            "BIGINT UNSIGNED DEFAULT '18446744073709551615.4'",
            "\"v\" mysql_uint64 DEFAULT '18446744073709551615'",
        ),
    ] {
        let stored = stored(declared).unwrap_or_else(|error| panic!("{declared}: {error}"));
        assert!(stored.contains(expected), "{declared}: {stored}");
    }
    for declared in [
        "INT DEFAULT ''",
        "INT DEFAULT ' '",
        "INT DEFAULT 'abc'",
        "INT DEFAULT '5a'",
        "INT DEFAULT '0x10'",
        "INT DEFAULT '1,5'",
        "INT DEFAULT '1 000'",
        "INT DEFAULT '- 1'",
        "INT DEFAULT '+-1'",
        "INT DEFAULT 'NULL'",
        "INT DEFAULT '.'",
        "INT DEFAULT 'e2'",
        "TINYINT DEFAULT '300'",
        "TINYINT DEFAULT '127.5'",
        "TINYINT DEFAULT 300",
        "TINYINT DEFAULT -129",
        "TINYINT UNSIGNED DEFAULT '-1'",
        "INT UNSIGNED DEFAULT '-0.5'",
        "INT DEFAULT '2147483648'",
        "INT DEFAULT -2147483648.5",
        "MEDIUMINT DEFAULT '8388608'",
        "BOOLEAN DEFAULT '128'",
        "BIGINT DEFAULT '9223372036854775807.5'",
        // Taken by MySQL, and not here: an unquoted exponent is rounded half
        // to even there, a bare trailing `e` is read as though it were not
        // written, and a tab is skipped where a newline is not.
        "INT DEFAULT 2.5e0",
        "INT DEFAULT '1e'",
        "INT DEFAULT '\t7'",
    ] {
        assert!(stored(declared).is_err(), "{declared}");
    }
}

/// Measured on MySQL 8.4.11: a word naming no number is 1067 on a `DOUBLE`,
/// and one naming a number prints as the number the column holds, so only a
/// word MySQL prints back as it was written is taken.
#[test]
fn a_floating_column_takes_a_word_mysql_prints_back_unchanged() {
    let stored = |declared: &str| {
        parse_create_table(
            &format!("CREATE TABLE t (v {declared})"),
            SessionSqlMode::default(),
        )
    };
    for declared in [
        "DOUBLE DEFAULT '0'",
        "DOUBLE DEFAULT '1.5'",
        "DOUBLE DEFAULT '-12.5'",
        "DOUBLE DEFAULT '-0'",
        "DOUBLE DEFAULT '0.0000001'",
        "DOUBLE DEFAULT '123456789012345'",
        "FLOAT DEFAULT '0.1'",
        "FLOAT DEFAULT '3.14159'",
        "FLOAT DEFAULT '0.000001'",
        "DOUBLE UNSIGNED DEFAULT '1.5'",
        "DOUBLE DEFAULT 1.25",
        "DOUBLE DEFAULT -3",
    ] {
        assert!(stored(declared).is_ok(), "{declared}");
    }
    for declared in [
        "DOUBLE DEFAULT ''",
        "DOUBLE DEFAULT 'x'",
        "DOUBLE DEFAULT ' 1'",
        "DOUBLE DEFAULT '1.50'",
        "DOUBLE DEFAULT '1.'",
        "DOUBLE DEFAULT '007'",
        "DOUBLE DEFAULT '.5'",
        "DOUBLE DEFAULT '1e2'",
        "DOUBLE DEFAULT '1234567890123456'",
        "FLOAT DEFAULT '1234567'",
        "DOUBLE UNSIGNED DEFAULT '-1'",
        "DOUBLE DEFAULT 1.50",
        "DOUBLE DEFAULT +1.5",
    ] {
        assert!(stored(declared).is_err(), "{declared}");
    }
}

/// Measured on MySQL 8.4.11: `ALTER COLUMN c SET DEFAULT v` and `DROP
/// DEFAULT` leave the column as it was but for its default.
#[test]
fn a_default_change_is_a_modify_of_the_column_with_its_new_default() {
    let stored = "CREATE TABLE `s1` (`id` INT NOT NULL PRIMARY KEY, `a` INT DEFAULT NULL, `b` INT NOT NULL DEFAULT 3, `c` VARCHAR(10) DEFAULT 'x', `d` DECIMAL(8,2) DEFAULT NULL)";
    let restated =
        |sql: &str| alter_column_default_restated(stored, sql, SessionSqlMode::default());
    assert_eq!(
        restated("ALTER TABLE s1 ALTER COLUMN a SET DEFAULT 5").unwrap(),
        Some(MySqlColumnDefaultChange::Restated(
            "ALTER TABLE `s1` MODIFY COLUMN `a` INT DEFAULT 5".to_owned()
        ))
    );
    assert_eq!(
        restated("ALTER TABLE `s1` ALTER `b` DROP DEFAULT, ALTER c SET DEFAULT 'y'").unwrap(),
        Some(MySqlColumnDefaultChange::Restated(
            "ALTER TABLE `s1` MODIFY COLUMN `b` INT NOT NULL, MODIFY COLUMN `c` VARCHAR(10) DEFAULT 'y'"
                .to_owned()
        ))
    );
    assert_eq!(
        restated("ALTER TABLE s1 ALTER COLUMN nope SET DEFAULT 1").unwrap(),
        Some(MySqlColumnDefaultChange::NoSuchColumn("nope".to_owned()))
    );
    for sql in [
        "ALTER TABLE s1 ADD COLUMN n INT",
        "ALTER TABLE s1 ALTER COLUMN a SET DEFAULT 1, ADD COLUMN n INT",
    ] {
        assert_eq!(restated(sql).unwrap(), None, "{sql}");
    }
    // Taken by MySQL, which then prints the column with no default at all.
    assert!(restated("ALTER TABLE s1 ALTER COLUMN a DROP DEFAULT").is_err());
    // A DECIMAL restated with only its default changed keeps its size.
    assert_eq!(
        restated("ALTER TABLE s1 ALTER COLUMN d SET DEFAULT 1").unwrap(),
        Some(MySqlColumnDefaultChange::Restated(
            "ALTER TABLE `s1` MODIFY COLUMN `d` DECIMAL(8,2) DEFAULT 1".to_owned()
        ))
    );
    // Measured: 1067, written either way.
    for sql in [
        "CREATE TABLE t (a INT NOT NULL DEFAULT NULL)",
        "CREATE TABLE t (a VARCHAR(3) DEFAULT NULL NOT NULL)",
    ] {
        assert!(
            parse_create_table(sql, SessionSqlMode::default()).is_err(),
            "{sql}"
        );
    }
}

/// Measured on MySQL 8.4.11: `bit` is `bit(1)`, and its default is written
/// 0, 1, `FALSE`, `TRUE`, `b'0'` or `b'1'`; 2, `b'10'` and the word `'1'` are
/// 1067.
#[test]
fn a_bit_holds_one_bit_as_the_integer_it_is() {
    let stored = |declared: &str| {
        parse_create_table(
            &format!("CREATE TABLE t (v {declared})"),
            SessionSqlMode::default(),
        )
        .map(|translated| translated.as_sql().to_owned())
    };
    for (declared, expected) in [
        ("BIT", "\"v\" BIT"),
        ("BIT(1) NOT NULL", "\"v\" BIT NOT NULL"),
        ("BIT DEFAULT b'1'", "\"v\" BIT DEFAULT 1"),
        ("BIT DEFAULT b'0'", "\"v\" BIT DEFAULT 0"),
        ("BIT DEFAULT 1", "\"v\" BIT DEFAULT 1"),
        ("BIT DEFAULT FALSE", "\"v\" BIT DEFAULT 0"),
        ("BIT DEFAULT NULL", "\"v\" BIT DEFAULT NULL"),
    ] {
        let stored = stored(declared).unwrap_or_else(|error| panic!("{declared}: {error}"));
        assert!(stored.contains(expected), "{declared}: {stored}");
    }
    for declared in [
        "BIT(8)",
        "BIT(0)",
        "BIT DEFAULT 2",
        "BIT DEFAULT b'10'",
        "BIT DEFAULT '1'",
        "BIT DEFAULT ''",
        "BIT DEFAULT x'01'",
    ] {
        assert!(stored(declared).is_err(), "{declared}");
    }
    let statement = parse_create_table_ast(
        "CREATE TABLE t (v BIT NOT NULL DEFAULT b'1')",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert_eq!(
        render_create_table_mysql_with_mode(&statement, SessionSqlMode::default()).unwrap(),
        "CREATE TABLE `t` (`v` BIT(1) NOT NULL DEFAULT 1)"
    );
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
fn composite_primary_key_makes_undeclared_nullability_not_null() {
    let mode = SessionSqlMode::default();
    let sql = "CREATE TABLE pair (a INT, b VARCHAR(20), PRIMARY KEY (a, b))";
    let statement = parse_schema_ddl_ast(sql, mode).unwrap();
    let rendered = render_create_table_mysql_with_mode(&statement, mode).unwrap();
    assert!(rendered.contains("`a` INT NOT NULL"), "{rendered}");
    assert!(rendered.contains("`b` VARCHAR(20) NOT NULL"), "{rendered}");
    assert!(rendered.contains("PRIMARY KEY (`a`,`b`)"), "{rendered}");
    assert_eq!(
        render_create_table_mysql_with_mode(&parse_schema_ddl_ast(&rendered, mode).unwrap(), mode)
            .unwrap(),
        rendered
    );

    for sql in [
        "CREATE TABLE pair (a INT NULL, b INT, PRIMARY KEY (a, b))",
        "CREATE TABLE pair (a INT DEFAULT NULL, b INT, PRIMARY KEY (a, b))",
    ] {
        assert!(parse_schema_ddl_ast(sql, mode).is_err(), "{sql}");
    }
}

#[test]
fn splits_mixed_index_and_column_alter_operations() {
    let mode = SessionSqlMode::default();
    for sql in [
        "ALTER TABLE mixed ADD COLUMN c INT, ADD INDEX ix_a (a)",
        "ALTER TABLE mixed DROP INDEX ix_a, ADD COLUMN d INT",
        "ALTER TABLE mixed DROP KEY ix_a, ADD COLUMN d INT",
    ] {
        assert!(parse_optional_alter_table_indexes(sql, mode)
            .unwrap()
            .is_none());
        let split = split_alter_table_operations(sql, mode).unwrap();
        assert_eq!(split.len(), 2, "{sql}");
        assert!(split[0].starts_with("ALTER TABLE `mixed` "), "{split:?}");
        assert!(split[1].starts_with("ALTER TABLE `mixed` "), "{split:?}");
        assert!(split.iter().any(|statement| {
            parse_optional_alter_table_indexes(statement, mode)
                .unwrap()
                .is_some()
        }));
    }
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
        // Measured on MySQL 8.4.11: 1067.
        "CREATE TABLE t (value MEDIUMINT NOT NULL DEFAULT NULL)",
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
fn insert_empty_row_uses_defaults_and_takes_a_number() {
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
    // The form names no columns and offers no values, so the row for the
    // number is made at bind time, where the column it goes in is known.
    for checked in [
        parse_auto_increment_insert(sql, mode).unwrap(),
        parse_prepared_auto_increment_insert(sql, mode).unwrap(),
    ] {
        assert!(checked.columns().is_empty());
        assert_eq!(checked.row_count().get(), 1);
    }
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
    let delete_like = parse_dml(
        "DELETE FROM t WHERE value LIKE 'café%'",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert!(delete_like
        .as_sql()
        .contains("mysql_uca9_like(\"value\", 'café%', '\\')"));
    let update_like = parse_dml(
        "UPDATE t SET value = 1 WHERE value NOT LIKE 'ß_'",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert!(update_like
        .as_sql()
        .contains("NOT mysql_uca9_like(\"value\", 'ß_', '\\')"));

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
        "SELECT COUNT(DISTINCT \"team\") AS \"COUNT(DISTINCT team)\" FROM \"users\""
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
        // modulo and a comparison of numbers with a point in a projection
        // are not.
        "SELECT 1.5 + 1",
        "SELECT 1 % 2",
        "SELECT 1.5 = 1",
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
        // A joined column's `MIN`, `MAX` and `SUM` are left to the server,
        // which knows its table, where each stands as a result column of its
        // own; its average, and one anywhere else, are not taken.
        "SELECT AVG(u.id) FROM users u JOIN posts p ON p.user_id = u.id",
        "SELECT (SUM(p.views)) FROM users u JOIN posts p ON p.user_id = u.id",
        "SELECT MAX(p.views) IS NULL FROM users u JOIN posts p ON p.user_id = u.id",
        "SELECT u.id FROM users u JOIN posts p ON p.user_id = u.id GROUP BY u.id ORDER BY SUM(p.views)",
        "SELECT u.id, (SELECT MAX(p.views) FROM posts p JOIN tags t ON t.id = p.id) FROM users u",
        // An order of one bare column is worked out when the parts are
        // joined; one of more has not been measured.
        "SELECT GROUP_CONCAT(name ORDER BY name, id) FROM users",
        "SELECT GROUP_CONCAT(name ORDER BY UPPER(name)) FROM users",
        // MySQL drops values equal under the column's collation and joins
        // the rest in its order, which the engine's DISTINCT does not.
        "SELECT GROUP_CONCAT(DISTINCT name) FROM users",
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
        "SELECT mysql_group_concat(group_concat(CASE WHEN \"name\" IS NULL THEN 'N' \
         ELSE 'V' || length(CAST((\"name\" || '') AS BLOB)) || ':' || \"name\" END, ''), \
         ',', 0, 1, 1) AS \"GROUP_CONCAT(name)\" FROM \"users\""
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
    assert_eq!(
        minimum.as_sql(),
        "SELECT (-9223372036854775808) AS \"-9223372036854775808\""
    );
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
    ] {
        assert!(
            parse_auto_increment_create_table(sql, SessionSqlMode::default()).is_err(),
            "{sql}"
        );
    }
    let implicit_not_null = parse_auto_increment_create_table(
        "CREATE TABLE t (id INT AUTO_INCREMENT, PRIMARY KEY (id))",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert!(implicit_not_null
        .normalized_mysql_ddl
        .contains("`id` INT NOT NULL AUTO_INCREMENT PRIMARY KEY"));
}

#[test]
fn gorm_counted_table_uses_the_primary_key_to_make_id_not_null() {
    let sql = "CREATE TABLE `gorm_e2e_parents` (`id` bigint AUTO_INCREMENT, `code` varchar(32) NOT NULL, `created_at` datetime NULL, PRIMARY KEY (`id`), UNIQUE INDEX `uk_gorm_parent_code` (`code`), INDEX `idx_gorm_shared_code` (`code`))";
    let split = parse_optional_create_table_with_keys(sql, SessionSqlMode::default())
        .unwrap()
        .unwrap();
    assert_eq!(split.indexes().len(), 2);
    let counted =
        parse_auto_increment_create_table(split.table_sql(), SessionSqlMode::default()).unwrap();
    assert!(counted
        .normalized_mysql_ddl
        .contains("`id` BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY"));
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
        // The same option twice says nothing more the second time, and MySQL
        // takes the last one written, which this would have to read as well.
        "CREATE TABLE t (id INT NOT NULL, PRIMARY KEY (id)) CHARSET=utf8mb4 CHARSET=utf8mb4",
        "CREATE TABLE t (id INT NOT NULL, PRIMARY KEY (id)) COMMENT='a' COMMENT='b'",
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

/// MySQL takes a counted column's three attributes in any order. Measured on
/// MySQL 8.4.11: Django's `bigint AUTO_INCREMENT NOT NULL PRIMARY KEY` and
/// `INT NOT NULL PRIMARY KEY AUTO_INCREMENT` both print back as the one
/// definition.
#[test]
fn takes_a_counted_columns_attributes_in_any_order() {
    for sql in [
        "CREATE TABLE `django_migrations` (`id` bigint AUTO_INCREMENT NOT NULL PRIMARY KEY, `app` varchar(255) NOT NULL)",
        "CREATE TABLE `django_migrations` (`id` bigint NOT NULL PRIMARY KEY AUTO_INCREMENT, `app` varchar(255) NOT NULL)",
        "CREATE TABLE `django_migrations` (`id` bigint PRIMARY KEY NOT NULL AUTO_INCREMENT, `app` varchar(255) NOT NULL)",
    ] {
        let checked = parse_auto_increment_create_table(sql, SessionSqlMode::default())
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
        assert_eq!(
            checked.normalized_mysql_ddl,
            "CREATE TABLE `django_migrations` (`id` BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY, `app` VARCHAR(255) NOT NULL)",
            "{sql}"
        );
    }
}

#[test]
fn rejects_auto_increment_shapes_outside_the_checked_slice() {
    for sql in [
        "CREATE TABLE app.t (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY)",
        "CREATE TEMPORARY TABLE t (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY)",
        "CREATE TABLE t (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY DEFAULT 1)",
        "CREATE TABLE t (id INT NOT NULL NOT NULL AUTO_INCREMENT PRIMARY KEY)",
        "CREATE TABLE t (id INT AUTO_INCREMENT AUTO_INCREMENT PRIMARY KEY)",
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
fn unsigned_bigint_auto_increment_uses_a_non_rowid_primary_key() {
    let checked = parse_auto_increment_create_table(
        "CREATE TABLE t (id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY, note TEXT)",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert_eq!(
        checked.allocator_column_type,
        MySqlIntegerType::BigIntUnsigned
    );
    assert_eq!(checked.allocator_column_written_type, "BIGINT UNSIGNED");
    let Stmt::CreateTable {
        body: TursoCreateTableBody::ColumnsAndConstraints { columns, .. },
        ..
    } = checked.sqlite_statement
    else {
        panic!("expected CREATE TABLE columns");
    };
    let column = &columns[checked.allocator_column_ordinal];
    assert_eq!(column.col_type.as_ref().unwrap().name, "mysql_uint64");
    assert!(column.constraints.iter().any(|constraint| matches!(
        constraint.constraint,
        TursoColumnConstraint::PrimaryKey { .. }
    )));
}

#[test]
fn unsigned_bigint_auto_increment_takes_an_upsert_that_leaves_its_id_alone() {
    let table = parse_auto_increment_create_table(
        "CREATE TABLE t (id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY, note TEXT)",
        SessionSqlMode::default(),
    )
    .unwrap();
    let insert = parse_auto_increment_insert(
        "INSERT INTO t (note) VALUES ('x') ON DUPLICATE KEY UPDATE note = 'y'",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert!(insert.bind_allocator_table(&table).is_ok());
    let insert = parse_auto_increment_insert(
        "INSERT INTO t (note) VALUES ('x') ON DUPLICATE KEY UPDATE id = 7",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert!(insert.bind_allocator_table(&table).is_err());
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
fn parses_mixed_explicit_null_default_and_bound_auto_increment_ids() {
    let table = parse_auto_increment_create_table(
        "CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT)",
        SessionSqlMode::default(),
    )
    .unwrap();
    let checked = parse_auto_increment_insert(
        "INSERT INTO users (id, name) VALUES (10, 'a'), (NULL, 'b'), (DEFAULT, 'c'), (3, 'd')",
        SessionSqlMode::default(),
    )
    .unwrap();
    let bound = checked.bind_allocator_table(&table).unwrap();
    assert_eq!(
        bound.row_values(),
        [
            AutoIncrementRowValue::Explicit(10),
            AutoIncrementRowValue::Generated,
            AutoIncrementRowValue::Generated,
            AutoIncrementRowValue::Explicit(3),
        ]
    );
    let Stmt::Insert { body, .. } = bound
        .inject_row_ids(&[None, Some(11), Some(12), None])
        .unwrap()
    else {
        panic!("expected INSERT");
    };
    let turso_parser::ast::InsertBody::Select(select, _) = body else {
        panic!("expected VALUES");
    };
    let OneSelect::Values(rows) = select.body.select else {
        panic!("expected rows");
    };
    assert!(
        matches!(rows[1][0].as_ref(), TursoExpr::Literal(TursoLiteral::Numeric(id)) if id == "11")
    );
    assert!(
        matches!(rows[2][0].as_ref(), TursoExpr::Literal(TursoLiteral::Numeric(id)) if id == "12")
    );

    let checked = parse_prepared_auto_increment_insert(
        "INSERT INTO users (id, name) VALUES (?, ?), (?, ?)",
        SessionSqlMode::default(),
    )
    .unwrap();
    let bound = checked.bind_allocator_table(&table).unwrap();
    assert_eq!(
        bound.row_values(),
        [
            AutoIncrementRowValue::Parameter(0),
            AutoIncrementRowValue::Parameter(2)
        ]
    );
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
fn a_counted_insert_takes_the_clock_readings_an_ordinary_insert_takes() {
    let mode = SessionSqlMode::default();
    for sql in [
        "INSERT INTO users (name, created_at) VALUES ('a', NOW())",
        "INSERT INTO users (name, created_at) VALUES ('a', CURDATE()), ('b', CURRENT_TIMESTAMP)",
        "INSERT INTO users (name, created_at) VALUES ('a', NOW() + INTERVAL 1 DAY)",
        "INSERT INTO users (name, created_at) VALUES ('a', DATE_SUB(NOW(), INTERVAL 30 DAY))",
        "INSERT INTO users (name, created_at) VALUES ('a', NOW(6)), ('b', CURRENT_TIMESTAMP(3))",
    ] {
        let checked = parse_auto_increment_insert(sql, mode).unwrap();
        assert!(checked.reads_the_clock(), "{sql}");
    }
    assert!(parse_prepared_auto_increment_insert(
        "INSERT INTO users (name, created_at) VALUES (?, NOW()), (?, CURTIME())",
        mode
    )
    .unwrap()
    .reads_the_clock());
    assert!(
        !parse_auto_increment_insert("INSERT INTO users (name) VALUES ('a')", mode)
            .unwrap()
            .reads_the_clock()
    );
    // A call the ordinary path does not write stays refused here too.
    for sql in [
        "INSERT INTO users (name, created_at) VALUES ('a', NOW(7))",
        "INSERT INTO users (name, created_at) VALUES ('a', CURTIME(6))",
        "INSERT INTO users (name) VALUES (CONCAT('a', 'b'))",
        "INSERT INTO users (id, name) VALUES (NOW(), 'a')",
    ] {
        let refused = parse_auto_increment_insert(sql, mode).and_then(|checked| {
            checked.bind_allocator_table(
                &parse_auto_increment_create_table(
                    "CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT, created_at DATETIME)",
                    mode,
                )
                .unwrap(),
            )
        });
        assert!(refused.is_err(), "{sql}");
    }
    // Written one row at a time, the rows read one moment between them, which
    // the frontend holds them to.
    let table = parse_auto_increment_create_table(
        "CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT UNIQUE, created_at DATETIME)",
        mode,
    )
    .unwrap();
    assert!(parse_auto_increment_insert(
        "INSERT IGNORE INTO users (name, created_at) VALUES ('a', NOW()), ('b', NOW())",
        mode
    )
    .unwrap()
    .bind_allocator_table(&table)
    .is_ok());
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
        // The copy is written with no upsert clause, so one is refused rather
        // than dropped.
        "INSERT INTO dst (id, n) SELECT id, n FROM src ON DUPLICATE KEY UPDATE n = n + 1",
        // Whether the keys decide a column beside them is checked only for a
        // statement answering rows.
        "INSERT INTO dst (id, n) SELECT id, n FROM src GROUP BY id",
    ] {
        assert!(parse_dml(sql, SessionSqlMode::default()).is_err(), "{sql}");
    }
    assert!(parse_optional_create_table_as_select(
        "CREATE TABLE dst AS SELECT id, n FROM src GROUP BY id",
        SessionSqlMode::default()
    )
    .is_err());
}

#[test]
fn an_insert_select_takes_the_select_the_frontend_rendered_knowing_its_types() {
    let mode = SessionSqlMode::default();
    let sql = "INSERT INTO dst (n, m) SELECT n, m FROM src WHERE name = ? ORDER BY n LIMIT ?";
    assert!(matches!(
        parse_dml(sql, mode),
        Err(ParseError::Unsupported { feature }) if feature == INSERT_SELECT_NEEDING_COLUMN_TYPES
    ));
    let select_sql = insert_select_source_sql(sql, mode).unwrap().unwrap();
    assert_eq!(
        select_sql,
        "SELECT n, m FROM src WHERE name = ? ORDER BY n LIMIT ?"
    );
    let select = parse_select_with_column_types(
        &select_sql,
        mode,
        &["name".to_owned()],
        &["n".to_owned(), "m".to_owned(), "name".to_owned()],
        &[],
    )
    .unwrap();
    let translated = parse_insert_select_knowing_its_select(sql, mode, &select).unwrap();
    assert!(translated.copies_a_select_rendered_knowing_its_types());
    assert_eq!(
        translated.as_sql(),
        format!("INSERT INTO \"dst\" (\"n\", \"m\") {}", select.as_sql())
    );
    assert_eq!(translated.row_count_parameters(), [1]);
    assert_eq!(translated.source_table(), Some("src"));
    assert_eq!(
        translated.checked_comparisons(),
        select.checked_comparisons()
    );
    assert!(translated.parse_ast().is_ok());

    // The column list is the first parenthesis the statement opens, and the
    // SELECT is everything after it, parentheses and all.
    assert_eq!(
        insert_select_source_sql("INSERT INTO dst (n) (SELECT n FROM src)", mode)
            .unwrap()
            .as_deref(),
        Some("(SELECT n FROM src)")
    );
    assert_eq!(
        insert_select_source_sql("INSERT INTO dst (n) VALUES (1)", mode).unwrap(),
        None
    );
    assert_eq!(
        insert_select_source_sql("INSERT INTO dst SELECT n FROM src", mode).unwrap(),
        None
    );
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
    assert_eq!(
        explicit_allocator
            .bind_allocator_table(&table)
            .unwrap()
            .row_values(),
        [AutoIncrementRowValue::Parameter(0)]
    );
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
        "INSERT INTO users (name) VALUES (X'ABC')",
        "INSERT INTO users (name) VALUES (1), (2, 3)",
        "INSERT INTO users (name, NAME) VALUES ('a', 'b')",
        "INSERT INTO app.users (name) VALUES ('a')",
        "INSERT INTO users (name) VALUE ('a')",
        "INSERT INTO users (name) VALUES ROW ('a')",
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
    let table = parse_auto_increment_create_table(
        "CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT)",
        SessionSqlMode::default(),
    )
    .unwrap();
    for sql in [
        "INSERT IGNORE INTO users (name) VALUES ('a'), ('b')",
        "INSERT INTO users (name) VALUES ('a'), ('b') ON DUPLICATE KEY UPDATE name = 'c'",
    ] {
        let bound = parse_auto_increment_insert(sql, SessionSqlMode::default())
            .unwrap()
            .bind_allocator_table(&table)
            .unwrap();
        assert!(bound.rowwise_conflicts());
        assert!(bound.inject_one_row(1, 7).is_ok());
    }
    assert!(parse_auto_increment_insert(
        "INSERT INTO users (name) VALUES ('a'), ('b') ON DUPLICATE KEY UPDATE id = VALUES(id)",
        SessionSqlMode::default(),
    )
    .unwrap()
    .bind_allocator_table(&table)
    .is_err());
    let generated_null = parse_auto_increment_insert(
        "INSERT IGNORE INTO users (id, name) VALUES (NULL, 'a'), (DEFAULT, 'b'), (0, 'c')",
        SessionSqlMode::default(),
    )
    .unwrap()
    .bind_allocator_table(&table)
    .unwrap();
    assert!(generated_null.rowwise_conflicts());
    assert_eq!(
        generated_null.row_values(),
        [AutoIncrementRowValue::Generated; 3]
    );
    assert!(parse_auto_increment_insert(
        "INSERT IGNORE INTO users (id, name) VALUES (NULL, NULL), (DEFAULT, 'b')",
        SessionSqlMode::default(),
    )
    .unwrap()
    .bind_allocator_table(&table)
    .is_err());

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
fn accepts_explicit_allocator_columns_and_rejects_invalid_reserved_ranges() {
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
    assert_eq!(
        explicit_allocator
            .bind_allocator_table(&table)
            .unwrap()
            .row_values(),
        [AutoIncrementRowValue::Explicit(1)]
    );
    let uppercase_allocator = parse_auto_increment_insert(
        "INSERT INTO USERS (ID, name) VALUES (1, 'Ada')",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert_eq!(
        uppercase_allocator
            .bind_allocator_table(&table)
            .unwrap()
            .row_values(),
        [AutoIncrementRowValue::Explicit(1)]
    );

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
        "CREATE VIEW users_view AS SELECT name FROM users WHERE name LIKE 'A%'",
        "CREATE VIEW users_view AS SELECT name FROM users GROUP BY name HAVING COUNT(*) > 1",
        "CREATE VIEW users_view AS SELECT name FROM users WITH CASCADED CHECK OPTION",
    ] {
        assert!(
            parse_create_view_ast(sql, SessionSqlMode::default()).is_err(),
            "expected rejection for {sql}"
        );
    }
}

#[test]
fn keeps_a_trigger_body_the_way_mysql_keeps_it() {
    // Each body is what MySQL 8.4.11's `SHOW CREATE TRIGGER` printed for the
    // same statement: the header written its own way, the body as written
    // without the whitespace around it or the `;` ending the statement.
    for (sql, written, body) in [
        (
            "CREATE TRIGGER copy_user AFTER INSERT ON users FOR EACH ROW BEGIN INSERT INTO audit_log (user_name, kind) VALUES (NEW.`name`, 'created'); END",
            "CREATE TRIGGER `copy_user` AFTER INSERT ON `users` FOR EACH ROW BEGIN INSERT INTO audit_log (user_name, kind) VALUES (NEW.`name`, 'created'); END",
            "BEGIN INSERT INTO audit_log (user_name, kind) VALUES (NEW.`name`, 'created'); END",
        ),
        (
            "create   trigger t9 after   insert on posts for each row   insert into audit(note) values (concat('x ', new.title))  ;",
            "CREATE TRIGGER `t9` AFTER INSERT ON `posts` FOR EACH ROW insert into audit(note) values (concat('x ', new.title))",
            "insert into audit(note) values (concat('x ', new.title))",
        ),
        (
            "CREATE TRIGGER t10 AFTER UPDATE ON posts FOR EACH ROW\nBEGIN\n  INSERT INTO audit (note) VALUES (CONCAT(OLD.title, ' -> ', NEW.title));\nEND",
            "CREATE TRIGGER `t10` AFTER UPDATE ON `posts` FOR EACH ROW BEGIN\n  INSERT INTO audit (note) VALUES (CONCAT(OLD.title, ' -> ', NEW.title));\nEND",
            "BEGIN\n  INSERT INTO audit (note) VALUES (CONCAT(OLD.title, ' -> ', NEW.title));\nEND",
        ),
        (
            "CREATE TRIGGER semi2 AFTER DELETE ON users FOR EACH ROW INSERT INTO counters (id, n) VALUES (OLD.id, 2) ;  ",
            "CREATE TRIGGER `semi2` AFTER DELETE ON `users` FOR EACH ROW INSERT INTO counters (id, n) VALUES (OLD.id, 2)",
            "INSERT INTO counters (id, n) VALUES (OLD.id, 2)",
        ),
        (
            "/*!50003 CREATE*/ /*!50017 DEFINER=`dump_owner`@`%`*/ /*!50003 TRIGGER `posts_audit` AFTER INSERT ON `posts` FOR EACH ROW INSERT INTO audit (note) VALUES (CONCAT('post ', NEW.title)) */",
            "CREATE TRIGGER `posts_audit` AFTER INSERT ON `posts` FOR EACH ROW INSERT INTO audit (note) VALUES (CONCAT('post ', NEW.title))",
            "INSERT INTO audit (note) VALUES (CONCAT('post ', NEW.title))",
        ),
    ] {
        let mode = SessionSqlMode::default();
        let kept = trigger_written_as_mysql_keeps_it(sql, mode)
            .unwrap()
            .unwrap_or_else(|| panic!("{sql}"));
        assert_eq!(kept, written, "{sql}");
        assert_eq!(
            trigger_written_as_mysql_keeps_it(&kept, mode).unwrap().as_deref(),
            Some(written)
        );
        assert_eq!(written_trigger(&kept, mode).unwrap().body(), body);
        let statement = parse_create_trigger_ast(&kept, mode).unwrap();
        assert_eq!(mysql_create_trigger_ddl(&statement, &kept, mode).unwrap(), kept);
    }

    let kept = written_trigger(
        "CREATE TRIGGER `t10` AFTER UPDATE ON `posts` FOR EACH ROW BEGIN\n  INSERT INTO audit (note) VALUES (CONCAT(OLD.title, ' -> ', NEW.title));\nEND",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert_eq!(kept.name().as_str(), "t10");
    assert_eq!(kept.table().as_str(), "posts");
    assert_eq!(kept.timing(), MySqlTriggerTiming::After);
    assert_eq!(kept.event(), MySqlTriggerEvent::Update);
    assert_eq!(
        kept.show_create("root"),
        "CREATE DEFINER=`root`@`%` TRIGGER `t10` AFTER UPDATE ON `posts` FOR EACH ROW BEGIN\n  INSERT INTO audit (note) VALUES (CONCAT(OLD.title, ' -> ', NEW.title));\nEND"
    );
}

#[test]
fn translates_a_trigger_body_the_same_whatever_its_columns_hold() {
    for (sql, engine) in [
        (
            "CREATE TRIGGER t2 AFTER INSERT ON users FOR EACH ROW INSERT INTO audit (msg) VALUES (CONCAT('new ', NEW.name))",
            "CREATE TRIGGER \"t2\" AFTER INSERT ON \"users\" FOR EACH ROW BEGIN INSERT INTO \"audit\" (\"msg\") VALUES (('new ' || NEW.\"name\")); END",
        ),
        (
            "CREATE TRIGGER t4 AFTER INSERT ON posts FOR EACH ROW UPDATE counters SET n = n + 1 WHERE id = NEW.owner_id",
            "CREATE TRIGGER \"t4\" AFTER INSERT ON \"posts\" FOR EACH ROW BEGIN UPDATE \"counters\" SET \"n\" = (\"n\" + 1) WHERE (\"id\" = NEW.\"owner_id\"); END",
        ),
        (
            "CREATE TRIGGER t5 AFTER DELETE ON posts FOR EACH ROW BEGIN DELETE FROM tags WHERE post_id = OLD.id AND kind = 2; UPDATE counters SET n = n - 1, last = NULL WHERE id = OLD.owner_id; END",
            "CREATE TRIGGER \"t5\" AFTER DELETE ON \"posts\" FOR EACH ROW BEGIN DELETE FROM \"tags\" WHERE (\"post_id\" = OLD.\"id\") AND (\"kind\" = 2); UPDATE \"counters\" SET \"n\" = (\"n\" - 1), \"last\" = NULL WHERE (\"id\" = OLD.\"owner_id\"); END",
        ),
    ] {
        let mode = SessionSqlMode::default();
        assert_eq!(
            crate::trigger_definition::translate_create_trigger(sql, mode).unwrap(),
            engine,
            "{sql}"
        );
    }
    let body = trigger_body_readings(
        "CREATE TRIGGER t4 AFTER UPDATE ON posts FOR EACH ROW UPDATE counters SET n = n + 1, label = CONCAT(OLD.title, '/', NEW.title) WHERE id = NEW.owner_id",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert_eq!(body.table().as_str(), "posts");
    assert_eq!(body.event(), MySqlTriggerEvent::Update);
    let [write] = body.writes() else {
        panic!("one statement");
    };
    assert_eq!(write.table().as_str(), "counters");
    assert_eq!(write.kind(), MySqlTriggerWriteKind::Update);
    assert_eq!(
        write.assigned(),
        [
            (
                "n".to_owned(),
                MySqlTriggerValue::Shifted {
                    column: Box::new(MySqlTriggerValue::Column("n".to_owned())),
                    by: 1
                }
            ),
            (
                "label".to_owned(),
                MySqlTriggerValue::Joined(vec![
                    MySqlTriggerValue::Row {
                        new: false,
                        column: "title".to_owned()
                    },
                    MySqlTriggerValue::Word("/".to_owned()),
                    MySqlTriggerValue::Row {
                        new: true,
                        column: "title".to_owned()
                    },
                ])
            ),
        ]
    );
    assert_eq!(
        write.compared(),
        [(
            "id".to_owned(),
            MySqlTriggerValue::Row {
                new: true,
                column: "owner_id".to_owned()
            }
        )]
    );
}

#[test]
fn reads_a_before_trigger_setting_the_row_it_runs_for() {
    use turso_parser::ast::{TriggerCmd, TriggerTime};

    let mode = SessionSqlMode::default();
    let sql = "CREATE TRIGGER t1 BEFORE INSERT ON posts FOR EACH ROW SET NEW.slug = CONCAT(NEW.title, '-x')";
    assert_eq!(
        trigger_written_as_mysql_keeps_it(sql, mode).unwrap().as_deref(),
        Some("CREATE TRIGGER `t1` BEFORE INSERT ON `posts` FOR EACH ROW SET NEW.slug = CONCAT(NEW.title, '-x')")
    );
    let Stmt::CreateTrigger { time, commands, .. } = parse_create_trigger_ast(sql, mode).unwrap()
    else {
        panic!("a CREATE TRIGGER");
    };
    assert_eq!(time, Some(TriggerTime::Before));
    let [TriggerCmd::SetNew { sets }] = commands.as_slice() else {
        panic!("one SET NEW: {commands:?}");
    };
    assert_eq!(sets.len(), 1);
    assert_eq!(sets[0].col_names[0].as_str(), "slug");

    let Stmt::CreateTrigger { commands, .. } = parse_create_trigger_ast(
        "CREATE TRIGGER t3 BEFORE UPDATE ON posts FOR EACH ROW BEGIN SET NEW.a = 1, NEW.b = NOW(); SET NEW.c = OLD.c + 1; END",
        mode,
    )
    .unwrap() else {
        panic!("a CREATE TRIGGER");
    };
    assert_eq!(
        commands
            .iter()
            .map(|command| match command {
                TriggerCmd::SetNew { sets } => sets
                    .iter()
                    .map(|set| set.col_names[0].as_str().to_owned())
                    .collect::<Vec<_>>(),
                _ => panic!("only SET NEW: {command:?}"),
            })
            .collect::<Vec<_>>(),
        [vec!["a", "b"], vec!["c"]]
    );

    for sql in [
        // 1362 and 1363 in MySQL.
        "CREATE TRIGGER t AFTER INSERT ON posts FOR EACH ROW SET NEW.slug = 'x'",
        "CREATE TRIGGER t BEFORE DELETE ON posts FOR EACH ROW SET NEW.slug = 'x'",
        "CREATE TRIGGER t BEFORE UPDATE ON posts FOR EACH ROW SET OLD.slug = 'x'",
        "CREATE TRIGGER t BEFORE INSERT ON posts FOR EACH ROW SET @slug = 'x'",
        "CREATE TRIGGER t BEFORE INSERT ON posts FOR EACH ROW SET NEW.slug = LOWER(NEW.title)",
        "CREATE TRIGGER t BEFORE INSERT ON posts FOR EACH ROW UPDATE counters SET n = n + 1 WHERE id = NEW.id",
    ] {
        assert!(parse_create_trigger_ast(sql, mode).is_err(), "{sql}");
    }
}

#[test]
fn rejects_unsafe_create_trigger_forms() {
    for sql in [
        // A `BEFORE` trigger can change the row, which the engine has no
        // statement for.
        "CREATE TRIGGER before_insert BEFORE INSERT ON users FOR EACH ROW BEGIN INSERT INTO audit (name) VALUES (NEW.name); END",
        "CREATE TRIGGER conditional AFTER INSERT ON users FOR EACH ROW WHEN NEW.name IS NOT NULL BEGIN INSERT INTO audit (name) VALUES (NEW.name); END",
        "CREATE TRIGGER expression AFTER INSERT ON users FOR EACH ROW BEGIN INSERT INTO audit (name) VALUES (LOWER(NEW.name)); END",
        "CREATE TRIGGER select_insert AFTER INSERT ON users FOR EACH ROW BEGIN INSERT INTO audit (name) SELECT name FROM users; END",
        "CREATE TRIGGER upsert AFTER INSERT ON users FOR EACH ROW BEGIN INSERT INTO audit (name) VALUES (NEW.name) ON DUPLICATE KEY UPDATE name = NEW.name; END",
        "CREATE TRIGGER ignored AFTER INSERT ON users FOR EACH ROW BEGIN INSERT IGNORE INTO audit (name) VALUES (NEW.name); END",
        "CREATE TRIGGER two_rows AFTER INSERT ON users FOR EACH ROW INSERT INTO audit (name) VALUES (NEW.name), ('x')",
        "CREATE TRIGGER no_columns AFTER INSERT ON users FOR EACH ROW INSERT INTO audit VALUES (NEW.name)",
        // 1363 in MySQL.
        "CREATE TRIGGER no_new AFTER DELETE ON users FOR EACH ROW INSERT INTO audit (name) VALUES (NEW.name)",
        "CREATE TRIGGER no_old AFTER INSERT ON users FOR EACH ROW INSERT INTO audit (name) VALUES (OLD.name)",
        // 1442 in MySQL when the trigger runs.
        "CREATE TRIGGER own_table AFTER INSERT ON users FOR EACH ROW UPDATE users SET n = 1 WHERE id = NEW.id",
        "CREATE TRIGGER fraction AFTER INSERT ON users FOR EACH ROW INSERT INTO audit (n) VALUES (1.5)",
        "CREATE TRIGGER sorted AFTER INSERT ON users FOR EACH ROW UPDATE counters SET n = 1 ORDER BY id LIMIT 1",
        "CREATE TRIGGER everything AFTER INSERT ON users FOR EACH ROW DELETE FROM counters",
        "CREATE TRIGGER compared AFTER INSERT ON users FOR EACH ROW UPDATE counters SET n = 1 WHERE id > NEW.id",
        "CREATE TRIGGER twice AFTER INSERT ON users FOR EACH ROW UPDATE counters SET n = 1, m = n WHERE id = NEW.id",
        "CREATE TRIGGER other_db AFTER INSERT ON users FOR EACH ROW INSERT INTO other.audit (name) VALUES (NEW.name)",
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
            only_if_missing: false,
            collation: MySqlTableCollation::Utf8mb40900AiCi,
        }
    );
    assert_eq!(
        parse_admin_command("DROP DATABASE Reports", SessionSqlMode::default()).unwrap(),
        MySqlAdminCommand::DropDatabase {
            name: MySqlDatabaseName::parse("reports").unwrap(),
            only_if_present: false,
        }
    );
    for sql in [
        "DROP DATABASE IF EXISTS Reports",
        "drop schema if exists `reports`;",
    ] {
        assert_eq!(
            parse_admin_command(sql, SessionSqlMode::default()),
            Ok(MySqlAdminCommand::DropDatabase {
                name: MySqlDatabaseName::parse("reports").unwrap(),
                only_if_present: true,
            }),
            "{sql}"
        );
    }
    for sql in ["DROP DATABASE IF reports", "DROP DATABASE IF EXISTS"] {
        assert_eq!(
            parse_admin_command(sql, SessionSqlMode::default()),
            Err(ParseError::ExpectedAdminCommand),
            "{sql}"
        );
    }
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
        // time either carries, and the frontend counts them the same way.
        (
            "SELECT DATEDIFF(a, b) FROM d",
            "SELECT mysql_datediff(\"a\", \"b\") AS \"DATEDIFF(a, b)\" FROM \"d\"",
        ),
        (
            "SELECT DATEDIFF(NOW(), a) FROM d",
            "SELECT mysql_datediff(datetime('now'), \"a\") AS \"DATEDIFF(NOW(), a)\" FROM \"d\"",
        ),
        (
            "SELECT DATEDIFF('2026-12-25', CURRENT_DATE) FROM d",
            "SELECT mysql_datediff('2026-12-25', date('now')) AS \"DATEDIFF('2026-12-25', CURRENT_DATE)\" FROM \"d\"",
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
        "SELECT CURTIME(7) FROM d",
        "SELECT YEAR() FROM d",
        "SELECT YEAR(a, b) FROM d",
        "SELECT DATEDIFF(a) FROM d",
        "SELECT DATEDIFF(a, b, c) FROM d",
        "SELECT DATEDIFF(a, 'soon') FROM d",
        "SELECT DATEDIFF(a, CURTIME()) FROM d",
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
        (
            "SELECT CHARACTER_MAXIMUM_LENGTH, NUMERIC_PRECISION, NUMERIC_SCALE, COLLATION_NAME FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'records' ORDER BY ORDINAL_POSITION",
            vec![
                MySqlInformationSchemaColumnsColumn::CharacterMaximumLength,
                MySqlInformationSchemaColumnsColumn::NumericPrecision,
                MySqlInformationSchemaColumnsColumn::NumericScale,
                MySqlInformationSchemaColumnsColumn::CollationName,
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
        // The same column named twice is refused.
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
        ("SHOW FULL COLUMNS FROM reports FROM `archive`", "reports"),
        // MySQL's own synonyms, which a schema reader written against it
        // reaches for: `FIELDS` for `COLUMNS` and `IN` for `FROM`.
        ("SHOW FIELDS FROM reports", "reports"),
        ("SHOW FULL FIELDS FROM reports", "reports"),
        ("SHOW COLUMNS IN reports", "reports"),
        ("show fields in `RePoRtS` like 'id%';", "reports"),
    ] {
        assert_eq!(
            parse_show_columns(sql, mode).map(|command| command.table().as_str().to_owned()),
            Ok(table.to_owned()),
            "expected SHOW COLUMNS form to be accepted: {sql}"
        );
    }

    for sql in [
        "SHOW COLUMNS reports",
        "SHOW COLUMNS FROM archive.reports IN archive",
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

    // Connector/J's `getPrimaryKeys` and `getIndexInfo` without
    // information_schema name the database after the table.
    for sql in [
        "SHOW KEYS FROM `posts` FROM `dbtools`",
        "SHOW INDEX FROM `posts` IN dbtools",
    ] {
        let command = parse_show_index(sql, mode).unwrap();
        assert_eq!(
            command.database().map(MySqlDatabaseName::as_str),
            Some("dbtools"),
            "{sql}"
        );
        assert_eq!(command.table().as_str(), "posts", "{sql}");
    }

    for sql in [
        "SHOW INDEX",
        "SHOW INDEX FROM",
        "SHOW INDEX reports",
        "SHOW INDEX FROM reports extra",
        "SHOW INDEX FROM a.reports FROM b",
        "SHOW INDEX FROM reports FROM",
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
        "CREATE DATABASE reports CHARACTER SET utf8mb4 /* hidden */",
        "CREATE DATABASE IF EXISTS reports",
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

/// `mysqldump` writes its `CREATE DATABASE` inside versioned comments, and
/// Prisma and Laravel name a character set and collation. Measured on MySQL
/// 8.4.11: each option may start with `DEFAULT`, take an `=`, come in any
/// order, and name its value bare, in backticks or as a string.
#[test]
fn reads_the_collation_create_database_gives_its_database() {
    let mode = SessionSqlMode::default();
    let unicode = MySqlTableCollation::Utf8mb4UnicodeCi;
    let uca9 = MySqlTableCollation::Utf8mb40900AiCi;
    for (sql, name, only_if_missing, collation) in [
        (
            "CREATE DATABASE /*!32312 IF NOT EXISTS*/ `probe` /*!40100 DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci */ /*!80016 DEFAULT ENCRYPTION='N' */",
            "probe",
            true,
            uca9,
        ),
        ("CREATE DATABASE IF NOT EXISTS other", "other", true, uca9),
        (
            "create database `laravel` default character set `utf8mb4` default collate `utf8mb4_unicode_ci`",
            "laravel",
            false,
            unicode,
        ),
        (
            "CREATE DATABASE prisma CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci",
            "prisma",
            false,
            unicode,
        ),
        (
            "CREATE DATABASE o3 CHARSET = 'utf8mb4' COLLATE = 'utf8mb4_0900_ai_ci' ENCRYPTION 'N';",
            "o3",
            false,
            uca9,
        ),
        (
            "/*!40000 CREATE DATABASE o4 COLLATE utf8mb4_unicode_ci CHARACTER SET utf8mb4 */",
            "o4",
            false,
            unicode,
        ),
        ("CREATE SCHEMA o5 COLLATE \"UTF8MB4_UNICODE_CI\"", "o5", false, unicode),
        (
            "CREATE DATABASE o6 COLLATE utf8mb4_bin COLLATE utf8mb4_unicode_ci",
            "o6",
            false,
            unicode,
        ),
        ("CREATE DATABASE o7 CHARSET utf8mb4", "o7", false, uca9),
    ] {
        assert_eq!(
            parse_admin_command(sql, mode),
            Ok(MySqlAdminCommand::CreateDatabase {
                name: MySqlDatabaseName::parse(name).unwrap(),
                only_if_missing,
                collation,
            }),
            "{sql}"
        );
    }
    for (sql, error) in [
        (
            "CREATE DATABASE e1 COLLATE nope",
            ParseError::UnknownCollation,
        ),
        (
            "CREATE DATABASE e2 CHARACTER SET nope",
            ParseError::UnknownCharacterSet,
        ),
        (
            "CREATE DATABASE e3 CHARACTER SET latin1 COLLATE utf8mb4_bin",
            ParseError::CollationOfAnotherCharacterSet,
        ),
        (
            "CREATE DATABASE e4 CHARACTER SET utf8 COLLATE utf8mb4_bin",
            ParseError::CollationOfAnotherCharacterSet,
        ),
        (
            "CREATE DATABASE e5 COLLATE latin1_swedish_ci CHARACTER SET utf8mb4",
            ParseError::ConflictingCharacterSets,
        ),
        (
            "CREATE DATABASE e6 CHARACTER SET utf8mb4 CHARSET utf8",
            ParseError::ConflictingCharacterSets,
        ),
        (
            "CREATE DATABASE e7 ENCRYPTION 'Y' COLLATE nope",
            ParseError::UnknownCollation,
        ),
    ] {
        assert_eq!(parse_admin_command(sql, mode), Err(error), "{sql}");
    }
    for sql in [
        "CREATE DATABASE l1 CHARACTER SET latin1",
        "CREATE DATABASE l2 CHARSET utf8",
        "CREATE DATABASE l3 COLLATE utf8_general_ci",
        "CREATE DATABASE l4 COLLATE utf8mb4_bin",
        "CREATE DATABASE l5 COLLATE utf8mb4_general_ci",
        "CREATE DATABASE l6 COLLATE binary",
        "CREATE DATABASE e1 ENCRYPTION 'Y'",
    ] {
        assert!(
            matches!(
                parse_admin_command(sql, mode),
                Err(ParseError::Unsupported { .. })
            ),
            "{sql}"
        );
    }
    for sql in [
        "CREATE DATABASE /*!99999 IF NOT EXISTS*/ future",
        "CREATE DATABASE /*!32312 IF NOT EXISTS future",
        "CREATE DATABASE c CHARACTER utf8mb4",
        "CREATE DATABASE c CHARACTER SET + utf8mb4",
        "CREATE DATABASE c CHARACTER SET = DEFAULT",
        "CREATE DATABASE IF EXISTS c",
    ] {
        assert!(parse_admin_command(sql, mode).is_err(), "{sql}");
    }
}

/// Measured on MySQL 8.4.11: the name may be left out for the selected
/// database, an `ALTER DATABASE` naming no option is 1064, and `READ ONLY`
/// is an option of its own.
#[test]
fn reads_alter_database_and_show_create_database() {
    let mode = SessionSqlMode::default();
    for (sql, name, collation) in [
        (
            "ALTER DATABASE App COLLATE utf8mb4_unicode_ci",
            Some("app"),
            Some(MySqlTableCollation::Utf8mb4UnicodeCi),
        ),
        (
            "ALTER SCHEMA DEFAULT CHARACTER SET = utf8mb4;",
            None,
            Some(MySqlTableCollation::Utf8mb40900AiCi),
        ),
        (
            "ALTER DATABASE /*!40100 DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci */",
            None,
            Some(MySqlTableCollation::Utf8mb4UnicodeCi),
        ),
        ("ALTER DATABASE `app` ENCRYPTION 'N'", Some("app"), None),
    ] {
        assert_eq!(
            parse_admin_command(sql, mode),
            Ok(MySqlAdminCommand::AlterDatabase {
                name: name.map(|name| MySqlDatabaseName::parse(name).unwrap()),
                collation,
            }),
            "{sql}"
        );
    }
    for sql in [
        "ALTER DATABASE app",
        "ALTER DATABASE",
        "ALTER DATABASE app garbage",
    ] {
        assert!(
            matches!(
                parse_admin_command(sql, mode),
                Err(ParseError::ExpectedAdminCommand | ParseError::TrailingAdminCommandTokens)
            ),
            "{sql}"
        );
    }
    assert!(matches!(
        parse_admin_command("ALTER DATABASE app READ ONLY = 1", mode),
        Err(ParseError::Unsupported { .. })
    ));
    assert_eq!(
        parse_optional_admin_command("ALTER TABLE t ADD COLUMN c INT", mode),
        Ok(None)
    );

    assert_eq!(
        parse_admin_command("SHOW CREATE SCHEMA IF NOT EXISTS `MixedDb`", mode),
        Ok(MySqlAdminCommand::ShowCreateDatabase {
            name: MySqlDatabaseName::parse("mixeddb").unwrap(),
            written_name: "MixedDb".to_owned(),
            only_if_missing: true,
        })
    );
    assert_eq!(
        parse_admin_command("SHOW CREATE DATABASE app;", mode),
        Ok(MySqlAdminCommand::ShowCreateDatabase {
            name: MySqlDatabaseName::parse("app").unwrap(),
            written_name: "app".to_owned(),
            only_if_missing: false,
        })
    );
    for sql in ["SHOW CREATE TABLE app", "SHOW CREATE VIEW app"] {
        assert_eq!(parse_optional_admin_command(sql, mode), Ok(None), "{sql}");
    }
}

#[test]
fn rejects_non_database_commands_and_incomplete_commands() {
    for sql in [
        "",
        "SELECT 1",
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
            only_if_missing: false,
            collation: MySqlTableCollation::Utf8mb40900AiCi,
        }))
    );
    assert_eq!(
        parse_optional_admin_command("SHOW DATABASES", mode),
        Ok(Some(MySqlAdminCommand::ListDatabases))
    );
    // Flyway asks whether it runs on RDS this way.
    assert_eq!(
        parse_optional_admin_command("SHOW DATABASES LIKE 'RDSAdmin';", mode),
        Ok(Some(MySqlAdminCommand::ListDatabasesLike {
            pattern: MySqlLikePattern::new("RDSAdmin", mode),
        }))
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
    // MySQL Workbench opens with `SET @@SESSION.autocommit = ON`; the rest
    // were measured on MySQL 8.4.11 to set the session's autocommit too.
    for (sql, enabled) in [
        ("SET autocommit = 0", false),
        ("set session AUTOCOMMIT=1;", true),
        ("SET @@SESSION.autocommit = ON", true),
        ("SET @@session.autocommit = OFF", false),
        ("SET @@LOCAL.autocommit = 1", true),
        ("SET LOCAL autocommit = 0", false),
        ("SET @@autocommit = 1", true),
        ("SET autocommit = 'ON'", true),
        ("SET autocommit = 'off'", false),
        ("SET autocommit = TRUE", true),
        ("SET autocommit = FALSE", false),
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
        "SET @@GLOBAL.autocommit = 0",
        "SET autocommit = 2",
        "SET autocommit = 'maybe'",
        "SET autocommit = 1, sql_mode = ''",
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
            MySqlTransactionCommand::BeginReadWrite,
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
            Ok(Some(MySqlTransactionCommand::BeginWithConsistentSnapshot)),
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
        "SHOW DATABASES LIKE tenant",
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
        "CREATE TABLE \"t\" (\"id\" INT NOT NULL UNIQUE, \"a\" TINYTEXT COLLATE MYSQL_UCA9_AI_CI, \"b\" TEXT COLLATE MYSQL_UCA9_AI_CI, \"c\" MEDIUMTEXT COLLATE MYSQL_UCA9_AI_CI, \"d\" LONGTEXT COLLATE MYSQL_UCA9_AI_CI, \"e\" TINYBLOB, \"f\" BLOB, \"g\" MEDIUMBLOB, \"h\" LONGBLOB)"
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
        // Beside an upsert the offered row carries the column's default, which
        // is what the engine's `excluded` row carries for a column left out.
        (
            "INSERT INTO d (id, n) VALUES (1, DEFAULT) ON DUPLICATE KEY UPDATE n = VALUES(n) + 1",
            "INSERT INTO \"d\" (\"id\") VALUES (1) ON CONFLICT DO UPDATE SET \"n\" = (\"excluded\".\"n\" + 1)",
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
        // The engine's `DEFAULT VALUES` leaves no room for the upsert clause.
        "INSERT INTO d (id, n) VALUES (DEFAULT, DEFAULT) ON DUPLICATE KEY UPDATE n = 1",
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

    // Every column defaulted is the row of defaults written the other way, and
    // it takes a number too. It names no columns once they are read off, and a
    // row is made for the number at bind time.
    let defaulted = parse_auto_increment_insert(
        "INSERT INTO `users` (`id`, `name`) VALUES (DEFAULT, DEFAULT)",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert!(defaulted.columns().is_empty());
    assert_eq!(defaulted.row_count().get(), 1);

    // Several rows of defaults leave no way to say which number the
    // statement reports.
    let sql = "INSERT INTO `users` (`id`, `name`) VALUES (DEFAULT, DEFAULT), (DEFAULT, DEFAULT)";
    assert!(
        parse_auto_increment_insert(sql, SessionSqlMode::default()).is_err(),
        "{sql}"
    );
    let mixed = parse_auto_increment_insert(
        "INSERT INTO `users` (`id`, `name`) VALUES (DEFAULT, 'Ada'), (7, 'Grace')",
        SessionSqlMode::default(),
    )
    .unwrap();
    let table = parse_auto_increment_create_table(
        "CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT)",
        SessionSqlMode::default(),
    )
    .unwrap();
    assert_eq!(
        mixed.bind_allocator_table(&table).unwrap().row_values(),
        [
            AutoIncrementRowValue::Generated,
            AutoIncrementRowValue::Explicit(7)
        ]
    );
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
            "CREATE TABLE \"t\" (\"a\" mysql_uint64)",
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

/// The server reads a statement to authorize it and again to prepare it, so
/// the last one read is answered from memory, and only for the same text and
/// mode.
#[test]
fn the_last_select_read_is_answered_without_reading_it_again() {
    let reads = || crate::SELECT_READS.with(std::cell::Cell::get);
    let before = reads();
    let sql = "SELECT id FROM the_last_select_read WHERE id = 1";
    let first = parse_select(sql, SessionSqlMode::default()).unwrap();
    assert_eq!(parse_select(sql, SessionSqlMode::default()).unwrap(), first);
    assert_eq!(reads(), before + 1);
    let ansi = SessionSqlMode {
        ansi_quotes: true,
        no_backslash_escapes: false,
    };
    parse_select(sql, ansi).unwrap();
    assert_eq!(reads(), before + 2);
    assert!(parse_select("SELECT id FROM the_last_select_read WHERE", ansi).is_err());
    assert!(parse_select("SELECT id FROM the_last_select_read WHERE", ansi).is_err());
    assert_eq!(reads(), before + 3);
}

/// While reads are kept, a text read again answers what reading it afresh
/// answers, a failure as well, without being read again; a nested guard keeps
/// them no longer than the outer one, and they are let go of with it.
#[test]
fn a_kept_reading_answers_what_reading_afresh_would() {
    let mode = SessionSqlMode::default();
    let sql = "INSERT INTO kept_reads (a, b) VALUES (1, 'it''s'), (2, 'two')";
    let broken = "INSERT INTO kept_reads (a) VALUES ('never closed";
    let read_afresh = (
        parse_dml(sql, mode),
        parse_dml(broken, mode),
        parse_optional_drop_table(broken, mode),
        parse_auto_increment_insert(sql, mode),
    );
    let read_everything = || {
        (
            parse_dml(sql, mode),
            parse_dml(broken, mode),
            parse_optional_drop_table(broken, mode),
            parse_auto_increment_insert(sql, mode),
        )
    };

    let kept = keep_reads();
    assert_eq!(read_everything(), read_afresh);
    let first_reading = bytes_read();
    {
        let _nested = keep_reads();
        assert_eq!(read_everything(), read_afresh);
    }
    assert_eq!(read_everything(), read_afresh);
    assert_eq!(bytes_read(), first_reading);
    drop(kept);

    assert_eq!(read_everything(), read_afresh);
    assert!(bytes_read().tokenized > first_reading.tokenized);
    assert!(bytes_read().parsed > first_reading.parsed);
    assert!(bytes_read().parsed_by_the_engine > first_reading.parsed_by_the_engine);
}

/// The session's two settings for `/*!NNNNN ... */` comments read a text with
/// none in it as the same tokens, so it is tokenized once for both; a text
/// holding one is tokenized under each, and each reads it as it would afresh.
#[test]
fn a_text_without_executable_comments_is_tokenized_once_for_both_settings() {
    let mode = SessionSqlMode::default();
    let expanding = SessionMySqlDialect::new(mode);
    let keeping_comments = SessionMySqlDialect::without_executable_comments(mode);
    let read_afresh = |sql: &str| {
        (
            statement_reads::tokens(&expanding, sql),
            statement_reads::tokens(&keeping_comments, sql),
        )
    };
    for (sql, readings) in [
        ("INSERT INTO t VALUES (1, '/* a */ /*! b */')", 2),
        ("INSERT INTO t VALUES (1) /*!80000 , (2) */", 2),
        ("INSERT INTO t VALUES (1, 'x') /* plain */", 1),
        ("INSERT INTO t VALUES ('never closed", 1),
    ] {
        let afresh = read_afresh(sql);
        let _kept = keep_reads();
        let before = bytes_read().tokenized;
        assert_eq!(read_afresh(sql), afresh, "{sql}");
        assert_eq!(read_afresh(sql), afresh, "{sql}");
        assert_eq!(
            bytes_read().tokenized - before,
            readings * sql.len(),
            "{sql}"
        );
    }
    let (expanded, kept) = read_afresh("SELECT 1 /*!80000 , 2 */");
    assert_ne!(expanded.unwrap(), kept.unwrap());
}

/// A word is found in any letter case at any place, the first and the last
/// included, and a text shorter than the word never holds it; what it finds
/// is what comparing every window of the text found.
#[test]
fn a_word_is_found_in_any_case_wherever_it_is_written() {
    let every_window = |text: &str, word: &str| {
        text.as_bytes()
            .windows(word.len())
            .any(|window| window.eq_ignore_ascii_case(word.as_bytes()))
    };
    for (text, word, found) in [
        ("CREATE VIEW v AS SELECT 1", "VIEW", true),
        ("create view v as select 1", "VIEW", true),
        ("view", "VIEW", true),
        ("SELECT * FROM t WHERE a = 'preview'", "VIEW", true),
        (
            "SELECT v FROM information_schema.tables",
            "INFORMATION_SCHEMA",
            true,
        ),
        (
            "SELECT 1 FROM Information_Schema",
            "INFORMATION_SCHEMA",
            true,
        ),
        (
            "SELECT 1 FROM informationschema",
            "INFORMATION_SCHEMA",
            false,
        ),
        ("VIE", "VIEW", false),
        ("", "VIEW", false),
        ("INSERT INTO t VALUES ('café', 'ビュー')", "TRIGGER", false),
        ("INSERT INTO t VALUES ('trigger 🙂')", "TRIGGER", true),
        ("vvvvvview", "VIEW", true),
    ] {
        assert_eq!(mentions_ignoring_case(text, word), found, "{text} / {word}");
        assert_eq!(every_window(text, word), found, "{text} / {word}");
    }
}

/// `LOCK IN SHARE MODE` is read as `FOR SHARE`, the lock Laravel's
/// `sharedLock()` and Rails' `lock("LOCK IN SHARE MODE")` ask for. Measured on
/// MySQL 8.4.11: it follows `LIMIT`, may be written in any case and with a
/// comment between its words, and takes none of `NOWAIT`, `SKIP LOCKED` or
/// `OF`, each of which is 1064 there.
#[test]
fn lock_in_share_mode_is_read_as_for_share() {
    let mode = SessionSqlMode::default();
    let for_share = parse_select("SELECT v FROM a WHERE id = 1 FOR SHARE", mode).unwrap();
    for sql in [
        "SELECT v FROM a WHERE id = 1 LOCK IN SHARE MODE",
        "SELECT v FROM a WHERE id = 1 lock  in share\nmode",
        "SELECT v FROM a WHERE id = 1 LOCK /* x */ IN SHARE MODE",
    ] {
        let read = parse_select(sql, mode).unwrap();
        assert!(read.locks_rows(), "{sql}");
        assert_eq!(read.as_sql(), for_share.as_sql(), "{sql}");
    }
    assert!(parse_select(
        "SELECT v FROM a ORDER BY id LIMIT 1 LOCK IN SHARE MODE",
        mode
    )
    .unwrap()
    .locks_rows());
    for sql in [
        "SELECT v FROM a LOCK IN SHARE MODE NOWAIT",
        "SELECT v FROM a LOCK IN SHARE MODE SKIP LOCKED",
        "SELECT v FROM a LOCK IN SHARE MODE OF a",
        "SELECT v FROM a LOCK IN SHARE MODE LIMIT 1",
        "SELECT v FROM a LOCK IN SHARE",
    ] {
        assert!(
            matches!(parse_select(sql, mode), Err(ParseError::Sqlparser(_))),
            "{sql}: {:?}",
            parse_select(sql, mode)
        );
    }
    let quoted = parse_select("SELECT 'LOCK IN SHARE MODE' FROM a", mode).unwrap();
    assert!(!quoted.locks_rows());
}

/// A path is read out of a document the way MySQL 8.4.11 reads it: a member
/// only out of an object, an element out of an array, and `[0]` over anything
/// else is the value itself.
#[test]
fn a_json_path_is_read_the_way_mysql_reads_it() {
    let found = |document: &str, path: &str| json_extract(document, path).unwrap();
    assert_eq!(found(r#"{"a":1}"#, "$[0]").as_deref(), Some(r#"{"a": 1}"#));
    assert_eq!(found("5", "$[0]").as_deref(), Some("5"));
    assert_eq!(found(r#""x""#, "$[0][0]").as_deref(), Some(r#""x""#));
    assert_eq!(found("[5]", "$[1]"), None);
    assert_eq!(found(r#"[{"a":1}]"#, "$.a"), None);
    assert_eq!(found(r#"{"a":{"b":1}}"#, "$.a[0].b").as_deref(), Some("1"));
    assert_eq!(found(r#"{"a b":3}"#, r#"$."a b""#).as_deref(), Some("3"));
    assert_eq!(found(r#"{"":5}"#, r#"$."""#).as_deref(), Some("5"));
    assert_eq!(found(r#"{"é":1}"#, r#"$."é""#).as_deref(), Some("1"));
    assert_eq!(found("[1,2]", "$[01]").as_deref(), Some("2"));
    assert_eq!(found(r#"{"$a":1,"_b":2}"#, "$.$a").as_deref(), Some("1"));
    assert_eq!(found(r#"{"A":1}"#, "$.a"), None);
    assert_eq!(found(r#"{"a":null}"#, "$.a").as_deref(), Some("null"));
    assert_eq!(
        found(r#"{"a":{"y":1,"x":[true,null]}}"#, "$.a").as_deref(),
        Some(r#"{"x": [true, null], "y": 1}"#)
    );
    for refused in [
        "$.1a",
        "$.é",
        "$ .a",
        "$.a ",
        "$[ 1 ]",
        "$[*]",
        "$.*",
        "$**.a",
        "$[last]",
        "$[1 to 2]",
        r#"$."a\"b""#,
        "$[1234567890]",
        "a",
        "",
    ] {
        assert!(!is_a_json_path_this_reads(refused), "{refused}");
    }
    assert_eq!(json_extract("not a document", "$"), None);
}

/// `JSON_UNQUOTE` takes the quotes off a string and writes anything else the
/// way MySQL prints it.
#[test]
fn json_unquote_answers_what_mysql_answers() {
    assert_eq!(json_unquote(r#""a\nb""#).as_deref(), Some("a\nb"));
    assert_eq!(json_unquote(r#""é""#).as_deref(), Some("é"));
    assert_eq!(json_unquote("null").as_deref(), Some("null"));
    assert_eq!(json_unquote("true").as_deref(), Some("true"));
    assert_eq!(json_unquote("1.50").as_deref(), Some("1.5"));
    assert_eq!(json_unquote("-0").as_deref(), Some("0"));
    assert_eq!(json_unquote("1e300").as_deref(), Some("1e300"));
    assert_eq!(json_unquote("[1,  2]").as_deref(), Some("[1, 2]"));
}

/// Measured on MySQL 8.4.11 by sending each text as one `COM_QUERY`.
#[test]
fn text_holding_only_comments_or_nothing_is_told_apart_from_a_statement() {
    for sql in [
        "-- MySQL dump 10.13  Distrib 8.4.11, for Linux (aarch64)",
        "--",
        "-- ",
        "--\tx",
        "--\u{b}x",
        "--\u{1}x",
        "--\u{7f}x",
        "\t-- x",
        "\u{c}-- x",
        "-- a\r\n",
        "-- a\n-- b",
        "#",
        "# a\n# b\n",
        "/* c */",
        "/* a */ /* b */",
        "/*+ x */",
        "/*M!100000 SELECT 1 */",
        "/*!99999 SET @a=1 */",
        "/*!123456 x */",
        "/*!80412 SELECT 1 */",
        "-- a\n;",
        "-- a\n;;",
        "/* c */ ;",
        "/*!99999 x */;",
    ] {
        assert_eq!(
            nothing_to_run(sql),
            Some(NothingToRun::OnlyComments),
            "{sql:?}"
        );
    }
    for sql in ["", "   ", "\n", "\u{b}", ";", ";;", " ; "] {
        assert_eq!(nothing_to_run(sql), Some(NothingToRun::Empty), "{sql:?}");
    }
    // Each is a statement to MySQL, or an error other than 1065.
    for sql in [
        "select 1",
        "-- x\nselect 1",
        "--x",
        "-\t- x",
        "-- a\n\u{1c}",
        "/* unterminated",
        "/*!99999 unterminated",
        "/*!40101 */",
        "/*!80411 */",
        "/*!*/",
        "/*!50001 CREATE*/",
        "; -- a",
        ";/* c */",
    ] {
        assert_eq!(nothing_to_run(sql), None, "{sql:?}");
    }
}

/// Measured on MySQL 8.4.11: each of the first list is 1064, each of the
/// second a name, a word or a variable.
#[test]
fn a_word_opening_a_dollar_quote_is_told_apart_from_a_name_with_a_dollar() {
    let mode = SessionSqlMode::default();
    for sql in [
        "select $$",
        "select $$ ;",
        "select $$a",
        "select $a$",
        "select $a$$",
        "select $$$",
        "select $$abc$$",
        "select 1 as $$",
        "select 1 $$",
        "select $é$",
        "select 1 from dual where 1=1 and $$x",
    ] {
        assert!(opens_a_dollar_quote(sql, mode), "{sql}");
    }
    for sql in [
        "select $",
        "select $a",
        "select $ $",
        "select a$$",
        "select x$$y",
        "select _$$",
        "select é$$",
        "select 1$$",
        "select `$$`",
        "select '$$'",
        "select \"$$\"",
        "select 'it\\'s $$'",
        "select @$$",
        "set @$a$ = 1",
        "select x.$$ from (select 1 as x) x",
        "select 1 -- $$",
        "select 1 /* $$ */",
        "select 1 # $$",
    ] {
        assert!(!opens_a_dollar_quote(sql, mode), "{sql}");
    }
    // Under NO_BACKSLASH_ESCAPES the quote after the backslash ends the
    // string and leaves `$$` outside it.
    assert!(!opens_a_dollar_quote("select '\\' $$ '", mode));
    assert!(opens_a_dollar_quote(
        "select '\\' $$ '",
        SessionSqlMode {
            no_backslash_escapes: true,
            ..SessionSqlMode::default()
        }
    ));
}

/// Prisma's catalog reads are recognized whatever their spacing and with the
/// comment Prisma writes into two of them; a read that says anything else is
/// left to the checked `SELECT`, which refuses `BINARY`.
#[test]
fn prisma_catalog_reads_are_recognized_and_nothing_near_them() {
    let mode = SessionSqlMode::default();
    let read = |sql: &str| parse_optional_prisma_information_schema_query(sql, mode).unwrap();
    assert_eq!(
        read("SELECT table_name AS table_name, index_name AS index_name, column_name AS column_name, sub_part AS partial, seq_in_index AS seq_in_index, collation AS column_order, non_unique AS non_unique, index_type AS index_type FROM information_schema.statistics WHERE table_schema = ? ORDER BY BINARY table_name, BINARY index_name, seq_in_index"),
        Some(PrismaInformationSchemaQuery::Indexes)
    );
    assert_eq!(
        read("SELECT DISTINCT BINARY table_info.table_name AS table_name\nFROM information_schema.tables AS table_info\nJOIN information_schema.columns AS column_info\nON BINARY column_info.table_name = BINARY table_info.table_name\nWHERE table_info.table_schema = ?\nAND column_info.table_schema = ?\n-- Exclude views.\nAND table_info.table_type = 'BASE TABLE'\nORDER BY BINARY table_info.table_name"),
        Some(PrismaInformationSchemaQuery::MigrationTableNames)
    );
    assert_eq!(
        PrismaInformationSchemaQuery::MigrationTableNames.parameter_count(),
        2
    );
    for other in [
        "SELECT table_name AS table_name, index_name AS index_name, column_name AS column_name, sub_part AS partial, seq_in_index AS seq_in_index, collation AS column_order, non_unique AS non_unique, index_type AS index_type FROM information_schema.statistics WHERE table_schema = 'prisma' ORDER BY BINARY table_name, BINARY index_name, seq_in_index",
        "SELECT DISTINCT BINARY table_name FROM information_schema.tables WHERE table_schema = ?",
        "SELECT 1",
    ] {
        assert_eq!(read(other), None, "{other}");
    }
}
