use std::any::TypeId;

use sqlparser::{
    ast::{Expr, Statement},
    dialect::{Dialect, MySqlDialect},
    keywords::Keyword,
    parser::{Parser, ParserError},
};

use crate::case::{SqlMode, SqlModeFlag};

/// MySQL's lexer changes with session SQL modes. Keep those changes local while
/// delegating every MySQL-specific parser hook to the upstream dialect.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionMySqlDialect {
    ansi_quotes: bool,
    no_backslash_escapes: bool,
}

impl SessionMySqlDialect {
    pub fn from_sql_mode(sql_mode: &SqlMode) -> Self {
        Self {
            ansi_quotes: sql_mode.flags.contains(&SqlModeFlag::AnsiQuotes),
            no_backslash_escapes: sql_mode.flags.contains(&SqlModeFlag::NoBackslashEscapes),
        }
    }
}

macro_rules! delegate_mysql_bool {
    ($name:ident) => {
        fn $name(&self) -> bool {
            MySqlDialect {}.$name()
        }
    };
}

impl Dialect for SessionMySqlDialect {
    fn dialect(&self) -> TypeId {
        TypeId::of::<MySqlDialect>()
    }

    fn is_identifier_start(&self, ch: char) -> bool {
        MySqlDialect {}.is_identifier_start(ch)
    }

    fn is_identifier_part(&self, ch: char) -> bool {
        MySqlDialect {}.is_identifier_part(ch)
    }

    fn is_delimited_identifier_start(&self, ch: char) -> bool {
        ch == '`' || (self.ansi_quotes && ch == '"')
    }

    fn identifier_quote_style(&self, identifier: &str) -> Option<char> {
        MySqlDialect {}.identifier_quote_style(identifier)
    }

    fn supports_string_literal_backslash_escape(&self) -> bool {
        !self.no_backslash_escapes
    }

    delegate_mysql_bool!(supports_string_literal_concatenation);
    delegate_mysql_bool!(ignores_wildcard_escapes);
    delegate_mysql_bool!(supports_numeric_prefix);
    delegate_mysql_bool!(supports_bitwise_shift_operators);
    delegate_mysql_bool!(supports_multiline_comment_hints);

    fn parse_infix(
        &self,
        parser: &mut Parser,
        expr: &Expr,
        precedence: u8,
    ) -> Option<Result<Expr, ParserError>> {
        MySqlDialect {}.parse_infix(parser, expr, precedence)
    }

    fn parse_statement(&self, parser: &mut Parser) -> Option<Result<Statement, ParserError>> {
        MySqlDialect {}.parse_statement(parser)
    }

    delegate_mysql_bool!(require_interval_qualifier);
    delegate_mysql_bool!(supports_limit_comma);
    delegate_mysql_bool!(supports_create_table_select);
    delegate_mysql_bool!(supports_insert_set);
    delegate_mysql_bool!(supports_user_host_grantee);

    fn is_table_factor_alias(
        &self,
        explicit: bool,
        keyword: &Keyword,
        parser: &mut Parser,
    ) -> bool {
        MySqlDialect {}.is_table_factor_alias(explicit, keyword, parser)
    }

    delegate_mysql_bool!(supports_table_hints);
    delegate_mysql_bool!(requires_single_line_comment_whitespace);
    delegate_mysql_bool!(supports_match_against);
    delegate_mysql_bool!(supports_select_modifiers);
    delegate_mysql_bool!(supports_set_names);
    delegate_mysql_bool!(supports_comma_separated_set_assignments);
    delegate_mysql_bool!(supports_update_order_by);
    delegate_mysql_bool!(supports_data_type_signed_suffix);
    delegate_mysql_bool!(supports_cross_join_constraint);
    delegate_mysql_bool!(supports_double_ampersand_operator);
    delegate_mysql_bool!(supports_binary_kw_as_cast);
    delegate_mysql_bool!(supports_comment_optimizer_hint);
    delegate_mysql_bool!(supports_constraint_keyword_without_name);
    delegate_mysql_bool!(supports_key_column_option);
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::ast::ColumnOption;

    fn parse(dialect: &dyn Dialect, sql: &str) -> Result<Vec<Statement>, ParserError> {
        Parser::parse_sql(dialect, sql)
    }

    #[test]
    fn default_session_preserves_mysql_specific_hooks() {
        let wrapper = SessionMySqlDialect::default();
        let corpus = [
            "SELECT 5 DIV 2",
            "LOCK TABLES t READ",
            "UNLOCK TABLES",
            "SELECT * FROM t LIMIT 5, 10",
            "CREATE TABLE t2 AS SELECT * FROM t",
            "INSERT INTO t SET a = 1",
            "GRANT SELECT ON db.* TO user@host",
            "SELECT /*+ MAX_EXECUTION_TIME(1000) */ 1",
            "SELECT SQL_CALC_FOUND_ROWS * FROM t",
            "SET NAMES utf8mb4",
            "SET a = 1, b = 2",
            "UPDATE t SET a = 1 ORDER BY b LIMIT 1",
            "SELECT 1 << 2, TRUE && FALSE",
            "SELECT BINARY 'Ab'",
            "CREATE TABLE t (a INT KEY AUTO_INCREMENT)",
            "SELECT * FROM a CROSS JOIN b ON a.id = b.id",
            "SELECT * FROM t USE INDEX (idx)",
            "SELECT MATCH(body) AGAINST ('rust') FROM t",
        ];

        for sql in corpus {
            assert_eq!(
                parse(&MySqlDialect {}, sql),
                parse(&wrapper, sql),
                "wrapper changed the default MySQL parse for {sql}"
            );
        }
    }

    #[test]
    fn ansi_quotes_changes_double_quotes_from_string_to_identifier() {
        let ansi = SessionMySqlDialect {
            ansi_quotes: true,
            no_backslash_escapes: false,
        };
        let sql = "SELECT \"id\" FROM t";

        let baseline = parse(&MySqlDialect {}, sql).unwrap();
        let session = parse(&ansi, sql).unwrap();

        assert_ne!(baseline, session);
        assert!(format!("{baseline:#?}").contains("DoubleQuotedString"));
        assert!(format!("{session:#?}").contains("Identifier"));
    }

    #[test]
    fn no_backslash_escapes_changes_string_tokenization() {
        let no_escapes = SessionMySqlDialect {
            ansi_quotes: false,
            no_backslash_escapes: true,
        };
        let sql = r"SELECT 'a\nb'";

        let baseline = parse(&MySqlDialect {}, sql).unwrap();
        let session = parse(&no_escapes, sql).unwrap();

        assert_ne!(baseline, session);
        assert!(session[0].to_string().contains(r"a\nb"));
    }

    #[test]
    fn wrapper_keeps_mysql_type_identity() {
        let dialect: &dyn Dialect = &SessionMySqlDialect::default();
        assert!(dialect.is::<MySqlDialect>());
    }

    #[test]
    fn release_ddl_semantics_survive_ast_round_trip() {
        let dialect = SessionMySqlDialect::default();
        let sql = "CREATE TABLE d_child (id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT, name VARCHAR(64) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci, normalized VARCHAR(64) GENERATED ALWAYS AS (LOWER(name)) STORED, created_at TIMESTAMP(6) DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6), PRIMARY KEY (id), CONSTRAINT uq_name UNIQUE (name), CONSTRAINT chk_name CHECK (name <> '') ENFORCED) ENGINE=InnoDB DEFAULT CHARACTER SET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci";
        let parsed = parse(&dialect, sql).unwrap();
        let Statement::CreateTable(table) = &parsed[0] else {
            panic!("expected CREATE TABLE AST");
        };
        let id = table
            .columns
            .iter()
            .find(|column| column.name.value == "id")
            .expect("id column");
        assert!(id.options.iter().any(|option| {
            matches!(
                &option.option,
                ColumnOption::DialectSpecific(tokens)
                    if tokens.iter().any(|token| token.to_string() == "AUTO_INCREMENT")
            )
        }));

        let normalized = parsed[0].to_string();
        for required in [
            "BIGINT UNSIGNED",
            "AUTO_INCREMENT",
            "CHARACTER SET utf8mb4",
            "COLLATE utf8mb4_0900_ai_ci",
            "GENERATED ALWAYS AS (LOWER(name)) STORED",
            "ON UPDATE CURRENT_TIMESTAMP(6)",
            "CONSTRAINT uq_name UNIQUE (name)",
            "CHECK (name <> '') ENFORCED",
            "ENGINE = InnoDB",
        ] {
            assert!(
                normalized.contains(required),
                "normalized DDL lost {required}: {normalized}"
            );
        }
        assert_eq!(parse(&dialect, &normalized).unwrap(), parsed);
    }
}
