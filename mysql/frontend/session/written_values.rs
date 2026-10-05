//! What the values a statement writes straight into its columns come to there,
//! worked out before the statement runs, and the warnings MySQL raises for
//! them once it has.
//!
//! The engine reads a word naming a number into a column of whole numbers as
//! a double before the frontend's check sees it, and a written number with a
//! point reaches it as such a word, so the check cannot tell a double, a
//! decimal and a word apart. MySQL reads each by rules of its own, so each
//! value written straight into a column is put into the form that column
//! stores while the statement still says what it is: a written one by
//! rewriting the statement, a bound one by binding the value it comes to.
//!
//! The same reading says where MySQL warns. Measured on MySQL 8.4.11: a value
//! rounded to a `DECIMAL`'s places raises note 1265 for its row, and under
//! `IGNORE` a whole number past its column's range is cut to the nearest one
//! the column holds with warning 1264, and a NULL meeting a `NOT NULL`
//! column stores the column's empty value with warning 1048.

use super::*;
use turso_mysql_parser::{
    ValuesWrittenIntoColumns, WholeNumber, WordAsWholeNumber, WrittenPlace, WrittenValueKind,
};

/// What a statement's written values come to, read before it runs.
#[derive(Debug, Clone, Default)]
pub(super) struct WrittenValuesPlan {
    /// The statement with each written value put into the form its column
    /// stores, or `None` where that changes nothing.
    pub(super) rewritten: Option<String>,
    pub(super) warnings: Vec<PlannedWarning>,
    pub(super) parameters: Vec<BoundIntoAColumn>,
    pub(super) shape: StatementShape,
}

/// What decides which rows the warnings of a statement are raised for.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct StatementShape {
    updates: bool,
    rows: usize,
}

/// A warning one written or bound value raises, for its row.
#[derive(Debug, Clone)]
pub(super) struct PlannedWarning {
    place: WrittenPlace,
    kind: WarningKind,
    column: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WarningKind {
    /// Warning 1264: a whole number cut to its column's range.
    OutOfRange,
    /// Warning 1048: a NULL stored as its column's empty value.
    CannotBeNull,
    /// Note 1265: a number rounded to a `DECIMAL`'s places.
    RoundedToItsPlaces,
}

/// A `?` written straight into a column.
#[derive(Debug, Clone)]
pub(super) struct BoundIntoAColumn {
    ordinal: usize,
    place: WrittenPlace,
    target: ColumnTarget,
    ignores: bool,
}

/// What a column does with a value written into it.
#[derive(Debug, Clone)]
struct ColumnTarget {
    name: String,
    kind: TargetKind,
    nullable: bool,
    /// Whether the table counts its ids in this column, where a NULL asks for
    /// the next one and an id is read by rules of its own.
    counted: bool,
}

#[derive(Debug, Clone, Copy)]
enum TargetKind {
    Whole { min: i128, max: i128 },
    Decimal { places: u32 },
    Real,
    Words,
    Other,
}

impl TargetKind {
    fn unsigned(self) -> bool {
        matches!(self, Self::Whole { min: 0, .. })
    }
}

/// Reads what a statement's written values come to in the columns of the
/// table it writes, `columns`.
pub(super) fn plan_written_values(
    sql: &str,
    written: ValuesWrittenIntoColumns,
    columns: &[MySqlColumnMetadata],
) -> WrittenValuesPlan {
    let mut plan = WrittenValuesPlan {
        shape: StatementShape {
            updates: written.updates,
            rows: written.rows,
        },
        ..WrittenValuesPlan::default()
    };
    let mut replacements = Vec::new();
    for value in written.values {
        let column = match &value.column {
            turso_mysql_parser::WrittenColumn::Named(name) => columns
                .iter()
                .find(|column| column.name().eq_ignore_ascii_case(name)),
            turso_mysql_parser::WrittenColumn::AtPlace(place) => columns.get(*place),
        };
        let Some(column) = column else {
            continue;
        };
        let target = column_target(column);
        if let WrittenValueKind::Bound(ordinal) = value.value {
            if !target.counted {
                plan.parameters.push(BoundIntoAColumn {
                    ordinal,
                    place: value.place,
                    target,
                    ignores: written.ignores,
                });
            }
            continue;
        }
        let outcome = written_outcome(&value.value, &target, written.ignores);
        if let Some(kind) = outcome.warning {
            plan.warnings.push(PlannedWarning {
                place: value.place,
                kind,
                column: target.name.clone(),
            });
        }
        if let Some(replacement) = outcome.replacement {
            replacements.push((value.at, replacement));
        }
    }
    replacements.sort_by_key(|(at, _)| std::cmp::Reverse(at.start));
    let mut rewritten = sql.to_owned();
    let mut changed = false;
    for (at, replacement) in replacements {
        if rewritten[at.clone()] != replacement {
            rewritten.replace_range(at, &replacement);
            changed = true;
        }
    }
    plan.rewritten = changed.then_some(rewritten);
    plan
}

fn column_target(column: &MySqlColumnMetadata) -> ColumnTarget {
    let type_name = column.type_name();
    let kind = if let Some((min, max)) = whole_number_range(type_name) {
        TargetKind::Whole { min, max }
    } else if let Some((_, places)) = column.decimal_size() {
        TargetKind::Decimal { places }
    } else if matches!(
        type_name,
        "FLOAT" | "DOUBLE" | "FLOAT UNSIGNED" | "DOUBLE UNSIGNED"
    ) {
        TargetKind::Real
    } else if column.character_length().is_some()
        || matches!(type_name, "TINYTEXT" | "TEXT" | "MEDIUMTEXT" | "LONGTEXT")
    {
        TargetKind::Words
    } else {
        TargetKind::Other
    };
    ColumnTarget {
        name: column.name().to_owned(),
        kind,
        nullable: column.nullable(),
        counted: column.extra().eq_ignore_ascii_case("AUTO_INCREMENT"),
    }
}

/// The range of a column of whole numbers the engine keeps as an integer.
/// A `BIGINT UNSIGNED` is kept in a form of its own and is left alone.
fn whole_number_range(type_name: &str) -> Option<(i128, i128)> {
    use turso_mysql_parser::MySqlIntegerType as Whole;
    let whole = match type_name {
        "TINYINT" | "BOOLEAN" => Whole::TinyInt,
        "SMALLINT" => Whole::SmallInt,
        "MEDIUMINT" => Whole::MediumInt,
        "INT" | "INTEGER" => Whole::Int,
        "BIGINT" => Whole::BigInt,
        "TINYINT UNSIGNED" => Whole::TinyIntUnsigned,
        "SMALLINT UNSIGNED" => Whole::SmallIntUnsigned,
        "MEDIUMINT UNSIGNED" => Whole::MediumIntUnsigned,
        "INT UNSIGNED" | "INTEGER UNSIGNED" => Whole::IntUnsigned,
        _ => return None,
    };
    Some(whole.bounds())
}

/// What one written value comes to: the text to write in its place, and the
/// warning it raises.
#[derive(Debug, Default)]
struct Outcome {
    replacement: Option<String>,
    warning: Option<WarningKind>,
}

fn written_outcome(value: &WrittenValueKind, target: &ColumnTarget, ignores: bool) -> Outcome {
    if matches!(value, WrittenValueKind::Null) {
        return match empty_value(target, ignores) {
            Some(empty) => Outcome {
                replacement: Some(empty.to_owned()),
                warning: Some(WarningKind::CannotBeNull),
            },
            None => Outcome::default(),
        };
    }
    match target.kind {
        TargetKind::Whole { min, max } => {
            let Some(read) = whole_number_written(value, target.kind.unsigned()) else {
                return Outcome::default();
            };
            let written_as_a_number = !matches!(value, WrittenValueKind::Word(_));
            let spelled = |whole: i128| {
                if written_as_a_number {
                    whole.unsigned_abs().to_string()
                } else {
                    whole.to_string()
                }
            };
            if target.counted {
                return Outcome {
                    replacement: match read {
                        WholeRead::Within(whole)
                            if !matches!(value, WrittenValueKind::Whole { .. }) =>
                        {
                            i64::try_from(whole).ok().map(|_| spelled(whole))
                        }
                        _ => None,
                    },
                    warning: None,
                };
            }
            match read.held_to(min, max) {
                Held::Within(whole) => Outcome {
                    replacement: (!matches!(value, WrittenValueKind::Whole { .. }))
                        .then(|| spelled(whole)),
                    warning: None,
                },
                Held::Cut(nearest) if ignores => Outcome {
                    replacement: Some(spelled(nearest)),
                    warning: Some(WarningKind::OutOfRange),
                },
                // Left past the range, for the column's check to answer 1264.
                Held::Cut(_) => Outcome {
                    replacement: match read {
                        WholeRead::BelowZero => Some(spelled(-1)),
                        WholeRead::Within(whole) if i64::try_from(whole).is_ok() => {
                            (!matches!(value, WrittenValueKind::Whole { .. }))
                                .then(|| spelled(whole))
                        }
                        _ => None,
                    },
                    warning: None,
                },
            }
        }
        TargetKind::Decimal { places } => Outcome {
            replacement: None,
            warning: decimal_written(value)
                .is_some_and(|number| reaches_past(&number, places))
                .then_some(WarningKind::RoundedToItsPlaces),
        },
        _ => Outcome::default(),
    }
}

/// The empty value a NULL is stored as under `IGNORE` in a column refusing
/// NULL, written as SQL, or `None` where it is stored as written. Measured on
/// MySQL 8.4.11: 0 for a number and `''` for words. The empty values of the
/// other types — `0000-00-00`, an `ENUM`'s `''`, a JSON `null` — are not
/// written here.
fn empty_value(target: &ColumnTarget, ignores: bool) -> Option<&'static str> {
    if !ignores || target.nullable || target.counted {
        return None;
    }
    match target.kind {
        TargetKind::Whole { .. } | TargetKind::Decimal { .. } | TargetKind::Real => Some("0"),
        TargetKind::Words => Some("''"),
        TargetKind::Other => None,
    }
}

/// A whole number as MySQL reads it into a column of whole numbers.
#[derive(Debug, Clone, Copy)]
enum WholeRead {
    Within(i128),
    Past {
        negative: bool,
    },
    /// A decimal below zero that rounds to zero, which MySQL holds past an
    /// unsigned column's range.
    BelowZero,
}

enum Held {
    Within(i128),
    /// Past the range, and the nearest number the column holds.
    Cut(i128),
}

impl WholeRead {
    fn held_to(self, min: i128, max: i128) -> Held {
        match self {
            Self::Within(whole) if whole < min => Held::Cut(min),
            Self::Within(whole) if whole > max => Held::Cut(max),
            Self::Within(whole) => Held::Within(whole),
            Self::Past { negative: true } => Held::Cut(min),
            Self::Past { negative: false } => Held::Cut(max),
            Self::BelowZero => Held::Cut(min),
        }
    }
}

/// Reads a written value into a column of whole numbers. Measured on MySQL
/// 8.4.11: a double is rounded half to even, a decimal and a word half away
/// from zero, and a decimal below zero is held past an unsigned column's range
/// even where it rounds to zero. A word with more after its number is left to
/// the column's check.
fn whole_number_written(value: &WrittenValueKind, unsigned: bool) -> Option<WholeRead> {
    let signed = |whole: WholeNumber, negative: bool| match whole {
        WholeNumber::Within(whole) => WholeRead::Within(if negative { -whole } else { whole }),
        WholeNumber::Past { .. } => WholeRead::Past { negative },
    };
    match value {
        WrittenValueKind::Double { digits, negative } => {
            let rounded = digits.parse::<f64>().ok()?.round_ties_even();
            Some(if rounded < 1e30 {
                WholeRead::Within(if *negative {
                    -(rounded as i128)
                } else {
                    rounded as i128
                })
            } else {
                WholeRead::Past {
                    negative: *negative,
                }
            })
        }
        WrittenValueKind::Decimal { digits, negative } => {
            let whole = turso_mysql_parser::whole_number_a_word_names(digits)?;
            let below_zero = *negative && digits.bytes().any(|digit| matches!(digit, b'1'..=b'9'));
            if unsigned && below_zero && whole == WholeNumber::Within(0) {
                return Some(WholeRead::BelowZero);
            }
            Some(signed(whole, *negative))
        }
        WrittenValueKind::Whole { digits, negative } => Some(signed(
            turso_mysql_parser::whole_number_a_word_names(digits)?,
            *negative,
        )),
        WrittenValueKind::Word(word) => {
            match turso_mysql_parser::whole_number_mysql_reads_from(word) {
                WordAsWholeNumber::Number { whole, cut: false } => Some(signed(whole, false)),
                _ => None,
            }
        }
        WrittenValueKind::Bound(_) | WrittenValueKind::Null => None,
    }
}

/// The number a written value names, spelled as digits with a point, for a
/// `DECIMAL` column.
fn decimal_written(value: &WrittenValueKind) -> Option<String> {
    match value {
        WrittenValueKind::Double { digits, .. } => Some(shortest_spelling(digits.parse().ok()?)),
        WrittenValueKind::Decimal { digits, .. } | WrittenValueKind::Whole { digits, .. } => {
            Some(digits.clone())
        }
        WrittenValueKind::Word(word) => number_a_word_spells(word),
        WrittenValueKind::Bound(_) | WrittenValueKind::Null => None,
    }
}

/// A double spelled with the fewest digits that read back as it, which is
/// the decimal MySQL reads it as.
fn shortest_spelling(number: f64) -> String {
    format!("{}", number.abs())
}

/// The number a word spells whole, with spaces and tabs around it, written
/// out with its exponent applied, or `None` for a word that is not one.
fn number_a_word_spells(word: &str) -> Option<String> {
    let word = word.trim_matches([' ', '\t']);
    let word = word.strip_prefix(['-', '+']).unwrap_or(word);
    let (mantissa, exponent) = match word.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => (mantissa, exponent.parse::<i64>().ok()?),
        None => (word, 0),
    };
    let (whole, places) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if whole.is_empty() && places.is_empty()
        || !whole
            .bytes()
            .chain(places.bytes())
            .all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let digits = format!("{whole}{places}");
    let point = (whole.len() as i64).saturating_add(exponent);
    if point <= 0 {
        let zeroes = usize::try_from(point.unsigned_abs()).ok()?;
        return Some(format!("0.{}{digits}", "0".repeat(zeroes)));
    }
    let point = usize::try_from(point).ok()?;
    if point >= digits.len() {
        return Some(digits);
    }
    Some(format!("{}.{}", &digits[..point], &digits[point..]))
}

/// Whether a number spelled as digits with a point holds a digit other than
/// 0 past `places` places.
fn reaches_past(number: &str, places: u32) -> bool {
    let Some((_, fraction)) = number.split_once('.') else {
        return false;
    };
    fraction
        .bytes()
        .skip(places as usize)
        .any(|digit| digit != b'0')
}

/// The bound values with each one bound for a column put into the form that
/// column stores, and the warnings they raise.
///
/// Measured on MySQL 8.4.11 through mysql2, in `VALUES`, `SET` and an upsert
/// clause alike: a double bound for a column of whole numbers is rounded half
/// to even, `2.5` storing 2 and `-0.4` 0 in an unsigned column; a word is read
/// as a written word is, `'2.5'` storing 3 and `'0.49999999999999999999'` 0,
/// but one below zero is held past an unsigned column's range even where it
/// rounds to zero, `'-0.4'` and `' -0.1'` alike. A word with more after its
/// number, or with a tab before it, which MySQL reads otherwise when it is
/// bound, is left as it was bound. Under `IGNORE` a whole number past the
/// column's range is cut to it and a NULL refused is stored empty, each with
/// its warning, and a number bound for a `DECIMAL` warns where it is rounded,
/// as written ones do.
pub(super) fn bound_into_their_columns(
    values: Vec<MySqlPreparedValue>,
    parameters: &[BoundIntoAColumn],
) -> (Vec<MySqlPreparedValue>, Vec<PlannedWarning>) {
    let mut values = values;
    let mut warnings = Vec::new();
    for parameter in parameters {
        let Some(value) = values.get_mut(parameter.ordinal) else {
            continue;
        };
        let (stored, warning) = bound_outcome(value, &parameter.target, parameter.ignores);
        if let Some(stored) = stored {
            *value = stored;
        }
        if let Some(kind) = warning {
            warnings.push(PlannedWarning {
                place: parameter.place,
                kind,
                column: parameter.target.name.clone(),
            });
        }
    }
    (values, warnings)
}

fn bound_outcome(
    value: &MySqlPreparedValue,
    target: &ColumnTarget,
    ignores: bool,
) -> (Option<MySqlPreparedValue>, Option<WarningKind>) {
    if matches!(value, MySqlPreparedValue::Null) {
        let empty = match (empty_value(target, ignores), target.kind) {
            (None, _) => return (None, None),
            (Some(_), TargetKind::Words) => MySqlPreparedValue::Text(String::new()),
            (Some(_), TargetKind::Real) => MySqlPreparedValue::Real(0.0),
            (Some(_), _) => MySqlPreparedValue::Integer(0),
        };
        return (Some(empty), Some(WarningKind::CannotBeNull));
    }
    match target.kind {
        TargetKind::Whole { min, max } => {
            let Some(read) = whole_number_bound(value, target.kind.unsigned()) else {
                return (None, None);
            };
            match read.held_to(min, max) {
                Held::Within(whole) => (
                    i64::try_from(whole).ok().map(MySqlPreparedValue::Integer),
                    None,
                ),
                Held::Cut(nearest) if ignores => (
                    i64::try_from(nearest).ok().map(MySqlPreparedValue::Integer),
                    Some(WarningKind::OutOfRange),
                ),
                Held::Cut(_) => (
                    match read {
                        WholeRead::BelowZero => Some(MySqlPreparedValue::Integer(-1)),
                        WholeRead::Within(whole) => {
                            i64::try_from(whole).ok().map(MySqlPreparedValue::Integer)
                        }
                        WholeRead::Past { .. } => None,
                    },
                    None,
                ),
            }
        }
        TargetKind::Decimal { places } => {
            let number = match value {
                MySqlPreparedValue::Real(number) if number.is_finite() => {
                    Some(shortest_spelling(*number))
                }
                MySqlPreparedValue::Text(word) => number_a_word_spells(word),
                _ => None,
            };
            (
                None,
                number
                    .is_some_and(|number| reaches_past(&number, places))
                    .then_some(WarningKind::RoundedToItsPlaces),
            )
        }
        _ => (None, None),
    }
}

fn whole_number_bound(value: &MySqlPreparedValue, unsigned: bool) -> Option<WholeRead> {
    match value {
        MySqlPreparedValue::Integer(whole) => Some(WholeRead::Within(i128::from(*whole))),
        MySqlPreparedValue::UnsignedInteger(whole) => Some(WholeRead::Within(i128::from(*whole))),
        MySqlPreparedValue::Real(number) if number.is_finite() => {
            let rounded = number.round_ties_even();
            Some(if rounded.abs() < 1e30 {
                WholeRead::Within(rounded as i128)
            } else {
                WholeRead::Past {
                    negative: rounded < 0.0,
                }
            })
        }
        MySqlPreparedValue::Text(word) if !word.trim_start_matches(' ').starts_with('\t') => {
            let WordAsWholeNumber::Number { whole, cut: false } =
                turso_mysql_parser::whole_number_mysql_reads_from(word)
            else {
                return None;
            };
            let below_zero = word.trim_start_matches(' ').starts_with('-')
                && word.split(['e', 'E']).next().is_some_and(|mantissa| {
                    mantissa.bytes().any(|digit| matches!(digit, b'1'..=b'9'))
                });
            Some(match whole {
                WholeNumber::Within(0) if unsigned && below_zero => WholeRead::BelowZero,
                WholeNumber::Within(whole) => WholeRead::Within(whole),
                WholeNumber::Past { negative } => WholeRead::Past { negative },
            })
        }
        _ => None,
    }
}

/// What a statement that ran needs to raise the warnings of its written and
/// bound values.
#[derive(Debug, Clone)]
pub(super) struct WrittenValueWarnings {
    pub(super) planned: Vec<PlannedWarning>,
    pub(super) shape: StatementShape,
    /// How many rows the engine wrote.
    pub(super) written_rows: u64,
    /// Whether an upsert met a row already there.
    pub(super) upserted: bool,
}

impl WrittenValueWarnings {
    /// The warnings raised, a row `IGNORE` skipped counting among the rows
    /// an `UPDATE` matched.
    pub(super) fn raised(&self, skipped_rows: u64) -> Vec<MySqlValueWarning> {
        warnings_raised(
            &self.planned,
            self.shape,
            self.written_rows.saturating_add(skipped_rows),
            self.upserted,
        )
    }
}

/// The warnings a statement that ran raises for its written and bound
/// values, row by row: an `INSERT`'s for each row it lists, and an
/// `UPDATE`'s for each row it matched, `matched_rows` of them. An upsert
/// clause's warnings are raised for a statement of one row that met a row
/// already there, `upserted`; which rows of a longer one did is not known
/// here, so theirs are not raised.
fn warnings_raised(
    planned: &[PlannedWarning],
    shape: StatementShape,
    matched_rows: u64,
    upserted: bool,
) -> Vec<MySqlValueWarning> {
    let mut raised = Vec::new();
    if shape.updates {
        for row in 1..=matched_rows {
            raised.extend(
                planned
                    .iter()
                    .filter(|warning| warning.place == WrittenPlace::Update)
                    .map(|warning| warning.raised_at(row)),
            );
        }
        return raised;
    }
    for row in 0..shape.rows {
        raised.extend(
            planned
                .iter()
                .filter(|warning| warning.place == WrittenPlace::Row(row))
                .map(|warning| warning.raised_at(row as u64 + 1)),
        );
        if shape.rows == 1 && upserted {
            raised.extend(
                planned
                    .iter()
                    .filter(|warning| warning.place == WrittenPlace::Upsert)
                    .map(|warning| warning.raised_at(1)),
            );
        }
    }
    raised
}

impl PlannedWarning {
    fn raised_at(&self, row: u64) -> MySqlValueWarning {
        let column = &self.column;
        match self.kind {
            WarningKind::OutOfRange => MySqlValueWarning {
                level: "Warning",
                code: 1264,
                message: format!("Out of range value for column '{column}' at row {row}"),
            },
            WarningKind::CannotBeNull => MySqlValueWarning {
                level: "Warning",
                code: 1048,
                message: format!("Column '{column}' cannot be null"),
            },
            WarningKind::RoundedToItsPlaces => MySqlValueWarning {
                level: "Note",
                code: 1265,
                message: format!("Data truncated for column '{column}' at row {row}"),
            },
        }
    }
}
