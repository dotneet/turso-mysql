//! `CHECK` constraints: the names MySQL gives them, the text it reads them
//! back as, and adding and dropping one with `ALTER TABLE`.

use sqlparser::ast::{
    AlterTableOperation, BinaryOperator, CheckConstraint, ColumnOption, CreateTable, Expr, Ident,
    Spanned, Statement, TableConstraint, UnaryOperator, Value,
};
use sqlparser::tokenizer::{Token, Tokenizer};

use super::{
    parse_one_statement, render_table_written_again, unsupported, MySqlTableRewrite, ParseError,
    SessionMySqlDialect, SessionSqlMode,
};

/// One `CHECK` constraint of a stored table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlCheckConstraint {
    name: String,
    clause: Option<String>,
}

impl MySqlCheckConstraint {
    /// The name MySQL knows the constraint by.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The expression as `information_schema.CHECK_CONSTRAINTS` reads it
    /// back, where this knows how MySQL writes every part of it.
    pub fn clause(&self) -> Option<&str> {
        self.clause.as_deref()
    }
}

/// What an `ALTER TABLE` that adds or drops a `CHECK` does to its table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MySqlCheckChange {
    /// The table written again with the constraint added or taken away.
    TableWrittenAgain(MySqlTableRewrite),
    /// The statement names a constraint the table has not got: measured on
    /// MySQL 8.4.11, 3821.
    NoSuchCheck(String),
    /// The statement names a constraint another has already taken: measured on
    /// MySQL 8.4.11, 3822, a name being the database's rather than the table's.
    DuplicateName(String),
}

/// The `CHECK` constraints of one stored table, in the order MySQL names
/// them, each under the name MySQL gives it.
///
/// Measured on MySQL 8.4.11: a constraint written without a name is
/// `<table>_chk_<n>`, counting from one over the unnamed ones in the order
/// they are written, a column's own among them; a named one takes no number.
/// The stored table holds its columns before its table-level constraints, so
/// that order is the written one wherever no unnamed table-level constraint
/// was written before a column carrying an unnamed one of its own, which
/// [`refuse_checks_numbered_out_of_order`] keeps a new table from being.
pub fn check_constraints_of(
    stored_ddl: &str,
    mode: SessionSqlMode,
) -> Result<Vec<MySqlCheckConstraint>, ParseError> {
    let Statement::CreateTable(table) = parse_one_statement(stored_ddl, mode)? else {
        return Err(ParseError::ExpectedCreateTable);
    };
    Ok(named_checks(&table)
        .into_iter()
        .map(|(name, expr)| MySqlCheckConstraint {
            name,
            clause: mysql_check_clause(expr),
        })
        .collect())
}

/// Refuses a `CREATE TABLE` that writes an unnamed table-level `CHECK` before
/// a column carrying an unnamed `CHECK` of its own.
///
/// MySQL numbers the two in the order written, and the table is stored with
/// its columns first, which would number them the other way round.
pub fn refuse_checks_numbered_out_of_order(
    create_sql: &str,
    mode: SessionSqlMode,
) -> Result<(), ParseError> {
    let Ok(Statement::CreateTable(table)) = parse_one_statement(create_sql, mode) else {
        return Ok(());
    };
    let last_column_check = table
        .columns
        .iter()
        .flat_map(|column| column.options.iter())
        .filter_map(|option| match &option.option {
            ColumnOption::Check(check) if option.name.is_none() && check.name.is_none() => {
                Some(check.expr.span().start)
            }
            _ => None,
        })
        .max();
    let first_table_check = table
        .constraints
        .iter()
        .filter_map(|constraint| match constraint {
            TableConstraint::Check(check) if check.name.is_none() => Some(check.expr.span().start),
            _ => None,
        })
        .min();
    match (first_table_check, last_column_check) {
        (Some(table_check), Some(column_check)) if table_check < column_check => {
            unsupported("an unnamed table CHECK written before a column's own unnamed CHECK")
        }
        _ => Ok(()),
    }
}

/// Reads an `ALTER TABLE` that adds a `CHECK` or drops one, and nothing
/// else, against the table it changes.
///
/// `other_names` are the names of every `CHECK` of every other table, which a
/// new name must not be. Answers `None` for any other statement, and for a
/// `DROP CONSTRAINT` naming a constraint that is not a `CHECK`, which is left
/// to the path that reads it.
///
/// The table is written again with every one of its `CHECK`s named where it
/// stands among the table's constraints, which is how MySQL keeps them: a
/// name once given is the constraint's for good, so dropping one leaves the
/// others their numbers.
pub fn table_with_a_check_changed(
    stored_ddl: &str,
    alter_sql: &str,
    other_names: &[String],
    mode: SessionSqlMode,
) -> Result<Option<MySqlCheckChange>, ParseError> {
    let change = match dropped_check_name(alter_sql, mode) {
        Some(name) => CheckAlteration::Drop(name, true),
        None => {
            let Ok(Statement::AlterTable(alter)) = parse_one_statement(alter_sql, mode) else {
                return Ok(None);
            };
            match alter.operations.as_slice() {
                [AlterTableOperation::AddConstraint {
                    constraint: TableConstraint::Check(check),
                    not_valid: false,
                }] => {
                    // Measured on MySQL 8.4.11: `NOT ENFORCED` keeps a
                    // constraint MySQL does not check, which the engine has no
                    // way to hold.
                    if check.enforced.is_some() {
                        return unsupported("CHECK enforcement attribute");
                    }
                    CheckAlteration::Add(check.name.clone(), check.expr.clone())
                }
                [AlterTableOperation::DropConstraint {
                    if_exists: false,
                    name,
                    drop_behavior: None,
                }] => CheckAlteration::Drop(name.value.clone(), false),
                _ => return Ok(None),
            }
        }
    };
    let Statement::CreateTable(mut table) = parse_one_statement(stored_ddl, mode)? else {
        return Err(ParseError::ExpectedCreateTable);
    };
    let mut checks = named_checks(&table)
        .into_iter()
        .map(|(name, expr)| (name, expr.clone()))
        .collect::<Vec<_>>();
    match change {
        CheckAlteration::Add(name, expr) => {
            let name = match name {
                Some(name) => name.value,
                // Measured on MySQL 8.4.11: one past the highest number the
                // table's names already carry, so a number a dropped
                // constraint had is not handed out again.
                None => {
                    let prefix = format!("{}_chk_", table_name(&table)?);
                    let highest = checks
                        .iter()
                        .filter_map(|(name, _)| {
                            name.get(prefix.len()..)
                                .filter(|_| {
                                    name.get(..prefix.len())
                                        .is_some_and(|start| start.eq_ignore_ascii_case(&prefix))
                                })
                                .and_then(|number| number.parse::<u64>().ok())
                        })
                        .max()
                        .unwrap_or(0);
                    format!("{prefix}{}", highest + 1)
                }
            };
            if checks
                .iter()
                .map(|(taken, _)| taken)
                .chain(other_names)
                .any(|taken| taken.eq_ignore_ascii_case(&name))
            {
                return Ok(Some(MySqlCheckChange::DuplicateName(name)));
            }
            checks.push((name, *expr));
        }
        CheckAlteration::Drop(name, written_as_a_check) => {
            let Some(at) = checks
                .iter()
                .position(|(taken, _)| taken.eq_ignore_ascii_case(&name))
            else {
                return Ok(written_as_a_check.then_some(MySqlCheckChange::NoSuchCheck(name)));
            };
            checks.remove(at);
        }
    }
    for column in &mut table.columns {
        column
            .options
            .retain(|option| !matches!(option.option, ColumnOption::Check(_)));
    }
    table
        .constraints
        .retain(|constraint| !matches!(constraint, TableConstraint::Check(_)));
    table
        .constraints
        .extend(checks.into_iter().map(|(name, expr)| {
            TableConstraint::Check(CheckConstraint {
                name: Some(Ident::with_quote('`', name)),
                expr: Box::new(expr),
                enforced: None,
            })
        }));
    let carried_columns = table
        .columns
        .iter()
        .map(|column| (column.name.value.clone(), column.name.value.clone()))
        .collect();
    Ok(Some(MySqlCheckChange::TableWrittenAgain(
        MySqlTableRewrite {
            create_sql: render_table_written_again(&table, mode)?,
            carried_columns,
        },
    )))
}

enum CheckAlteration {
    Add(Option<Ident>, Box<Expr>),
    /// The name, and whether the statement said `CHECK` rather than
    /// `CONSTRAINT`, which could name some other constraint.
    Drop(String, bool),
}

/// The table an `ALTER TABLE ... DROP CHECK <name>` names, which `sqlparser`
/// does not read.
pub fn table_a_check_is_dropped_from(sql: &str, mode: SessionSqlMode) -> Option<String> {
    dropped_check(sql, mode).map(|(table, _)| table)
}

/// Reads `ALTER TABLE <table> DROP CHECK <name>` as the name, which
/// `sqlparser` does not read.
fn dropped_check_name(sql: &str, mode: SessionSqlMode) -> Option<String> {
    dropped_check(sql, mode).map(|(_, name)| name)
}

/// Reads `ALTER TABLE <table> DROP CHECK <name>` as the table and the name.
fn dropped_check(sql: &str, mode: SessionSqlMode) -> Option<(String, String)> {
    let dialect = SessionMySqlDialect::new(mode);
    let tokens = Tokenizer::new(&dialect, sql).tokenize().ok()?;
    let mut words = tokens
        .into_iter()
        .filter(|token| !matches!(token, Token::Whitespace(_) | Token::SemiColon | Token::EOF));
    let keyword = |token: Option<Token>, expected: &str| {
        matches!(token, Some(Token::Word(word))
            if word.quote_style.is_none() && word.value.eq_ignore_ascii_case(expected))
    };
    if !keyword(words.next(), "ALTER") || !keyword(words.next(), "TABLE") {
        return None;
    }
    let Some(Token::Word(table)) = words.next() else {
        return None;
    };
    if !keyword(words.next(), "DROP") || !keyword(words.next(), "CHECK") {
        return None;
    }
    let Some(Token::Word(name)) = words.next() else {
        return None;
    };
    words.next().is_none().then_some((table.value, name.value))
}

/// Every `CHECK` of a table with the name MySQL gives it, columns' own first.
fn named_checks(table: &CreateTable) -> Vec<(String, &Expr)> {
    let table_name = table
        .name
        .0
        .last()
        .map(ToString::to_string)
        .unwrap_or_default()
        .trim_matches('`')
        .to_owned();
    let written = table
        .columns
        .iter()
        .flat_map(|column| column.options.iter())
        .filter_map(|option| match &option.option {
            ColumnOption::Check(check) => Some((
                option.name.as_ref().or(check.name.as_ref()),
                check.expr.as_ref(),
            )),
            _ => None,
        })
        .chain(
            table
                .constraints
                .iter()
                .filter_map(|constraint| match constraint {
                    TableConstraint::Check(check) => {
                        Some((check.name.as_ref(), check.expr.as_ref()))
                    }
                    _ => None,
                }),
        );
    let mut unnamed = 0;
    written
        .map(|(name, expr)| match name {
            Some(name) => (name.value.clone(), expr),
            None => {
                unnamed += 1;
                (format!("{table_name}_chk_{unnamed}"), expr)
            }
        })
        .collect()
}

fn table_name(table: &CreateTable) -> Result<String, ParseError> {
    match table.name.0.as_slice() {
        [sqlparser::ast::ObjectNamePart::Identifier(name)] => Ok(name.value.clone()),
        _ => unsupported("qualified table name"),
    }
}

/// The text `information_schema.CHECK_CONSTRAINTS` reads one expression back
/// as, where every part of it is one whose writing was measured.
///
/// Measured on MySQL 8.4.11: the whole in parentheses, a column in
/// backquotes, each comparison and each side of an `AND` or `OR` in
/// parentheses of its own, the words in lower case, a word written with its
/// character set in front and its quotes behind backslashes —
/// `(`s` <> _utf8mb4\'x\')` — and a negative number as the minus of a
/// positive one, `-(1)`.
fn mysql_check_clause(expr: &Expr) -> Option<String> {
    let mut whole = expr;
    while let Expr::Nested(inner) = whole {
        whole = inner;
    }
    if !matches!(
        whole,
        Expr::BinaryOp { .. } | Expr::IsNull(_) | Expr::IsNotNull(_)
    ) {
        return None;
    }
    let written = written_operand(expr)?;
    Some(
        if written.starts_with('(') && encloses_everything(&written) {
            written
        } else {
            format!("({written})")
        },
    )
}

fn written_operand(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Nested(inner) => written_operand(inner),
        Expr::Identifier(column) if !column.value.contains('`') => {
            Some(format!("`{}`", column.value))
        }
        Expr::Value(value) => match &value.value {
            Value::Number(digits, false)
                if !digits.is_empty()
                    && digits.chars().all(|c| c.is_ascii_digit() || c == '.')
                    && digits.matches('.').count() <= 1
                    && !digits.starts_with('.')
                    && !digits.ends_with('.')
                    && (!digits.starts_with('0') || digits == "0" || digits.starts_with("0.")) =>
            {
                Some(digits.clone())
            }
            Value::SingleQuotedString(word) if !word.contains(['\'', '\\']) => {
                Some(format!("_utf8mb4\\'{word}\\'"))
            }
            _ => None,
        },
        Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr,
        } => match expr.as_ref() {
            Expr::Value(_) => Some(format!("-({})", written_operand(expr)?)),
            _ => None,
        },
        Expr::IsNull(inner) => Some(format!("({} is null)", written_operand(inner)?)),
        Expr::IsNotNull(inner) => Some(format!("({} is not null)", written_operand(inner)?)),
        Expr::BinaryOp { left, op, right } => {
            let operator = match op {
                BinaryOperator::Gt => ">",
                BinaryOperator::Lt => "<",
                BinaryOperator::GtEq => ">=",
                BinaryOperator::LtEq => "<=",
                BinaryOperator::Eq => "=",
                BinaryOperator::NotEq => "<>",
                BinaryOperator::And => "and",
                BinaryOperator::Or => "or",
                BinaryOperator::Plus => "+",
                BinaryOperator::Minus => "-",
                BinaryOperator::Multiply => "*",
                _ => return None,
            };
            Some(format!(
                "({} {operator} {})",
                written_operand(left)?,
                written_operand(right)?
            ))
        }
        _ => None,
    }
}

/// Reports whether the parenthesis a text opens with is the one it closes
/// with.
fn encloses_everything(written: &str) -> bool {
    let mut depth = 0usize;
    for (at, character) in written.char_indices() {
        match character {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return at + 1 == written.len();
                }
            }
            _ => {}
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checks(ddl: &str) -> Vec<(String, Option<String>)> {
        check_constraints_of(ddl, SessionSqlMode::default())
            .unwrap()
            .into_iter()
            .map(|check| (check.name, check.clause))
            .collect()
    }

    /// Measured on MySQL 8.4.11 over the same table.
    #[test]
    fn names_and_writes_each_check_as_mysql_does() {
        assert_eq!(
            checks(
                "CREATE TABLE chk (a INT CHECK (a > 0), b INT, c INT CHECK (c >= 1), \
                 s VARCHAR(10), d DECIMAL(5,2), CHECK (b > 0), CONSTRAINT named CHECK (a < 100), \
                 CHECK (s <> ''), CHECK (a > 0 AND b > 0), CHECK (a > 0 OR b IS NULL), \
                 CHECK (b IS NOT NULL), CHECK (d > 1.5), CHECK (a <> -1), CHECK (a + b > 0), \
                 CHECK (a = b), CHECK (a * 2 > 0), CHECK (a % 2 = 0))"
            ),
            [
                ("chk_chk_1".to_owned(), Some("(`a` > 0)".to_owned())),
                ("chk_chk_2".to_owned(), Some("(`c` >= 1)".to_owned())),
                ("chk_chk_3".to_owned(), Some("(`b` > 0)".to_owned())),
                ("named".to_owned(), Some("(`a` < 100)".to_owned())),
                (
                    "chk_chk_4".to_owned(),
                    Some("(`s` <> _utf8mb4\\'\\')".to_owned())
                ),
                (
                    "chk_chk_5".to_owned(),
                    Some("((`a` > 0) and (`b` > 0))".to_owned())
                ),
                (
                    "chk_chk_6".to_owned(),
                    Some("((`a` > 0) or (`b` is null))".to_owned())
                ),
                ("chk_chk_7".to_owned(), Some("(`b` is not null)".to_owned())),
                ("chk_chk_8".to_owned(), Some("(`d` > 1.5)".to_owned())),
                ("chk_chk_9".to_owned(), Some("(`a` <> -(1))".to_owned())),
                (
                    "chk_chk_10".to_owned(),
                    Some("((`a` + `b`) > 0)".to_owned())
                ),
                ("chk_chk_11".to_owned(), Some("(`a` = `b`)".to_owned())),
                ("chk_chk_12".to_owned(), Some("((`a` * 2) > 0)".to_owned())),
                // Not measured, so not written.
                ("chk_chk_13".to_owned(), None),
            ]
        );
    }

    #[test]
    fn refuses_a_table_check_mysql_would_number_before_a_columns() {
        let mode = SessionSqlMode::default();
        assert!(refuse_checks_numbered_out_of_order(
            "CREATE TABLE t (a INT, CHECK (a > 0), b INT CHECK (b > 0))",
            mode
        )
        .is_err());
        for sql in [
            "CREATE TABLE t (a INT CHECK (a > 0), b INT, CHECK (b > 0))",
            "CREATE TABLE t (a INT, CONSTRAINT c CHECK (a > 0), b INT CHECK (b > 0))",
            "CREATE TABLE t (a INT, CHECK (a > 0), b INT CONSTRAINT d CHECK (b > 0))",
        ] {
            assert!(
                refuse_checks_numbered_out_of_order(sql, mode).is_ok(),
                "{sql}"
            );
        }
    }

    #[test]
    fn adds_and_drops_a_check_naming_every_one() {
        let mode = SessionSqlMode::default();
        let stored = "CREATE TABLE t (a INT CHECK (a > 0), b INT, CHECK (b > 0))";
        let Some(MySqlCheckChange::TableWrittenAgain(added)) =
            table_with_a_check_changed(stored, "ALTER TABLE t ADD CHECK (b < 10)", &[], mode)
                .unwrap()
        else {
            panic!("an unnamed CHECK is added");
        };
        assert_eq!(
            checks(&added.create_sql)
                .into_iter()
                .map(|(name, _)| name)
                .collect::<Vec<_>>(),
            ["t_chk_1", "t_chk_2", "t_chk_3"]
        );
        let Some(MySqlCheckChange::TableWrittenAgain(dropped)) = table_with_a_check_changed(
            &added.create_sql,
            "ALTER TABLE t DROP CHECK t_chk_2",
            &[],
            mode,
        )
        .unwrap() else {
            panic!("a CHECK is dropped");
        };
        let Some(MySqlCheckChange::TableWrittenAgain(again)) = table_with_a_check_changed(
            &dropped.create_sql,
            "ALTER TABLE t ADD CHECK (a < 5)",
            &[],
            mode,
        )
        .unwrap() else {
            panic!("an unnamed CHECK is added");
        };
        assert_eq!(
            checks(&again.create_sql)
                .into_iter()
                .map(|(name, _)| name)
                .collect::<Vec<_>>(),
            ["t_chk_1", "t_chk_3", "t_chk_4"]
        );
        assert_eq!(
            table_with_a_check_changed(stored, "ALTER TABLE t DROP CHECK nope", &[], mode).unwrap(),
            Some(MySqlCheckChange::NoSuchCheck("nope".to_owned()))
        );
        assert_eq!(
            table_with_a_check_changed(stored, "ALTER TABLE t DROP CONSTRAINT nope", &[], mode)
                .unwrap(),
            None
        );
        assert_eq!(
            table_with_a_check_changed(
                stored,
                "ALTER TABLE t ADD CONSTRAINT taken CHECK (a > 1)",
                &["TAKEN".to_owned()],
                mode
            )
            .unwrap(),
            Some(MySqlCheckChange::DuplicateName("taken".to_owned()))
        );
        assert!(table_with_a_check_changed(
            stored,
            "ALTER TABLE t ADD CONSTRAINT c CHECK (a > 1) NOT ENFORCED",
            &[],
            mode
        )
        .is_err());
    }
}
