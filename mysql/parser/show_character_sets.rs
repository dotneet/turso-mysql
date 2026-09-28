//! `SHOW COLLATION` and `SHOW CHARACTER SET`, which GUI tools and drivers send
//! to learn which collations and character sets the server has.

use sqlparser::ast::{BinaryOperator, Expr, ShowStatementFilter, Statement, Value};

use super::{parse_one_statement, MySqlLikePattern, ParseError, SessionSqlMode};

/// Which listing a `SHOW` asks for, and what it keeps of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MySqlShowCharacterSetsCommand {
    /// `SHOW COLLATION`.
    Collations(MySqlShowListingFilter),
    /// `SHOW CHARACTER SET`, or `SHOW CHARSET`.
    CharacterSets(MySqlShowListingFilter),
}

/// What a `SHOW` listing keeps of its rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MySqlShowListingFilter {
    Everything,
    /// `LIKE 'pattern'`, matched against the listing's first column.
    Like(MySqlLikePattern),
    /// `WHERE` over the listing's own columns.
    Where(MySqlShowCondition),
}

/// A `WHERE` of a `SHOW` listing: tests of its columns joined by `AND` and
/// `OR`, in whatever parentheses they were written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MySqlShowCondition {
    Test(MySqlShowColumnTest),
    /// `AND`: both hold.
    Both(Box<MySqlShowCondition>, Box<MySqlShowCondition>),
    /// `OR`: either holds.
    Either(Box<MySqlShowCondition>, Box<MySqlShowCondition>),
}

impl MySqlShowCondition {
    /// Returns every test the condition makes, in the order it was written.
    pub fn tests(&self) -> Vec<&MySqlShowColumnTest> {
        match self {
            Self::Test(test) => vec![test],
            Self::Both(left, right) | Self::Either(left, right) => {
                let mut tests = left.tests();
                tests.extend(right.tests());
                tests
            }
        }
    }
}

/// One test a `WHERE` makes of one column of a `SHOW` listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlShowColumnTest {
    column: String,
    test: MySqlShowValueTest,
}

/// What one column of a listing is tested against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MySqlShowValueTest {
    /// `column = 'word'`.
    EqualsWord(String),
    /// `column = number`.
    EqualsNumber(u64),
    /// `column LIKE 'pattern'`.
    Like(MySqlLikePattern),
}

impl MySqlShowColumnTest {
    /// Returns the column the test names, as written.
    pub fn column(&self) -> &str {
        &self.column
    }

    /// Returns what the column is tested against.
    pub const fn test(&self) -> &MySqlShowValueTest {
        &self.test
    }
}

/// Parses `SHOW COLLATION` or `SHOW CHARACTER SET`, with an optional `LIKE`
/// or a `WHERE` made of equality and `LIKE` tests joined by `AND` and `OR`.
///
/// Returns `None` for any other statement. A `WHERE` of any other shape is
/// refused rather than read as something narrower.
pub fn parse_optional_show_character_sets(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlShowCharacterSetsCommand>, ParseError> {
    if !names_a_character_set_listing(sql) {
        return Ok(None);
    }
    match parse_one_statement(sql, mode)? {
        Statement::ShowCollation { filter } => Ok(Some(MySqlShowCharacterSetsCommand::Collations(
            listing_filter(filter, mode)?,
        ))),
        Statement::ShowCharset(show) => Ok(Some(MySqlShowCharacterSetsCommand::CharacterSets(
            listing_filter(show.filter, mode)?,
        ))),
        _ => Ok(None),
    }
}

/// Reports whether a statement begins `SHOW COLLATION`, `SHOW CHARACTER SET`
/// or `SHOW CHARSET`, which is what decides whether this reader owns it.
fn names_a_character_set_listing(sql: &str) -> bool {
    let words = sql
        .split(|character: char| character.is_whitespace() || character == ';')
        .filter(|word| !word.is_empty())
        .take(3)
        .collect::<Vec<_>>();
    match words.as_slice() {
        [show, listing, ..]
            if show.eq_ignore_ascii_case("SHOW")
                && (listing.eq_ignore_ascii_case("COLLATION")
                    || listing.eq_ignore_ascii_case("CHARSET")) =>
        {
            true
        }
        [show, character, set, ..] => {
            show.eq_ignore_ascii_case("SHOW")
                && character.eq_ignore_ascii_case("CHARACTER")
                && set.eq_ignore_ascii_case("SET")
        }
        _ => false,
    }
}

fn listing_filter(
    filter: Option<ShowStatementFilter>,
    mode: SessionSqlMode,
) -> Result<MySqlShowListingFilter, ParseError> {
    match filter {
        None => Ok(MySqlShowListingFilter::Everything),
        Some(ShowStatementFilter::Like(pattern)) => Ok(MySqlShowListingFilter::Like(
            MySqlLikePattern::new(&pattern, mode),
        )),
        Some(ShowStatementFilter::Where(expr)) => {
            Ok(MySqlShowListingFilter::Where(read_condition(&expr, mode)?))
        }
        Some(ShowStatementFilter::ILike(_) | ShowStatementFilter::NoKeyword(_)) => {
            Err(ParseError::Unsupported {
                feature: "SHOW listing filter",
            })
        }
    }
}

fn read_condition(expr: &Expr, mode: SessionSqlMode) -> Result<MySqlShowCondition, ParseError> {
    let refused = || ParseError::Unsupported {
        feature: "SHOW listing WHERE test",
    };
    match expr {
        Expr::Nested(inner) => read_condition(inner, mode),
        Expr::BinaryOp {
            left,
            op: BinaryOperator::And,
            right,
        } => Ok(MySqlShowCondition::Both(
            Box::new(read_condition(left, mode)?),
            Box::new(read_condition(right, mode)?),
        )),
        Expr::BinaryOp {
            left,
            op: BinaryOperator::Or,
            right,
        } => Ok(MySqlShowCondition::Either(
            Box::new(read_condition(left, mode)?),
            Box::new(read_condition(right, mode)?),
        )),
        Expr::BinaryOp {
            left,
            op: BinaryOperator::Eq,
            right,
        } => {
            let Expr::Identifier(column) = left.as_ref() else {
                return Err(refused());
            };
            let Expr::Value(value) = right.as_ref() else {
                return Err(refused());
            };
            let test = match &value.value {
                Value::SingleQuotedString(word) => MySqlShowValueTest::EqualsWord(word.clone()),
                Value::Number(digits, false) => {
                    MySqlShowValueTest::EqualsNumber(digits.parse().map_err(|_| refused())?)
                }
                _ => return Err(refused()),
            };
            Ok(MySqlShowCondition::Test(MySqlShowColumnTest {
                column: column.value.clone(),
                test,
            }))
        }
        Expr::Like {
            negated: false,
            any: false,
            expr,
            pattern,
            escape_char: None,
        } => {
            let Expr::Identifier(column) = expr.as_ref() else {
                return Err(refused());
            };
            let Expr::Value(pattern) = pattern.as_ref() else {
                return Err(refused());
            };
            let Value::SingleQuotedString(pattern) = &pattern.value else {
                return Err(refused());
            };
            Ok(MySqlShowCondition::Test(MySqlShowColumnTest {
                column: column.value.clone(),
                test: MySqlShowValueTest::Like(MySqlLikePattern::new(pattern, mode)),
            }))
        }
        _ => Err(refused()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(sql: &str) -> Result<Option<MySqlShowCharacterSetsCommand>, ParseError> {
        parse_optional_show_character_sets(sql, SessionSqlMode::default())
    }

    #[test]
    fn reads_both_listings_with_each_filter() {
        assert_eq!(
            parse("SHOW COLLATION").unwrap(),
            Some(MySqlShowCharacterSetsCommand::Collations(
                MySqlShowListingFilter::Everything
            ))
        );
        for sql in ["SHOW CHARACTER SET", "show charset", "SHOW CHARSET;"] {
            assert_eq!(
                parse(sql).unwrap(),
                Some(MySqlShowCharacterSetsCommand::CharacterSets(
                    MySqlShowListingFilter::Everything
                )),
                "{sql}"
            );
        }
        let Some(MySqlShowCharacterSetsCommand::Collations(MySqlShowListingFilter::Like(pattern))) =
            parse("SHOW COLLATION LIKE 'utf8mb4%'").unwrap()
        else {
            panic!("a LIKE is read");
        };
        assert!(pattern.matches("UTF8MB4_BIN"));
        let Some(MySqlShowCharacterSetsCommand::Collations(MySqlShowListingFilter::Where(
            condition,
        ))) = parse("SHOW COLLATION WHERE Charset = 'utf8mb4' AND (Id = 46 AND `Default` LIKE '')")
            .unwrap()
        else {
            panic!("a WHERE is read");
        };
        let tests = condition.tests();
        assert_eq!(
            tests.iter().map(|test| test.column()).collect::<Vec<_>>(),
            ["Charset", "Id", "Default"]
        );
        assert_eq!(tests[1].test(), &MySqlShowValueTest::EqualsNumber(46));
    }

    /// Gitea's `CheckCollations` asks which case-sensitive collations there
    /// are this way.
    #[test]
    fn reads_tests_joined_by_or() {
        let Some(MySqlShowCharacterSetsCommand::Collations(MySqlShowListingFilter::Where(
            MySqlShowCondition::Either(left, right),
        ))) = parse(
            "SHOW COLLATION WHERE (Collation = 'utf8mb4_bin') OR (Collation LIKE '%\\_as\\_cs%')",
        )
        .unwrap()
        else {
            panic!("an OR is read");
        };
        assert_eq!(
            left.tests()[0].test(),
            &MySqlShowValueTest::EqualsWord("utf8mb4_bin".to_owned())
        );
        let MySqlShowValueTest::Like(pattern) = right.tests()[0].test() else {
            panic!("the right side is a LIKE");
        };
        assert!(pattern.matches("utf8mb4_0900_as_cs"));
        assert!(!pattern.matches("utf8mb4_0900_asxcs"));
    }

    #[test]
    fn leaves_other_statements_and_refuses_other_tests() {
        for sql in [
            "SHOW TABLES",
            "SHOW COLUMNS FROM t",
            "SELECT 1",
            "SHOW CHARACTER",
        ] {
            assert_eq!(parse(sql).unwrap(), None, "{sql}");
        }
        for sql in [
            "SHOW COLLATION WHERE Charset <> 'utf8mb4'",
            "SHOW COLLATION WHERE Charset NOT LIKE 'utf8%'",
            "SHOW COLLATION WHERE LOWER(Charset) = 'utf8mb4'",
            "SHOW CHARACTER SET WHERE Maxlen = -1",
        ] {
            assert!(parse(sql).is_err(), "{sql}");
        }
    }
}
