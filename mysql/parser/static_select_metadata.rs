//! Source-level metadata for the checked MySQL `SELECT` literal subset.
//!
//! This module deliberately describes SQL syntax rather than MySQL wire
//! fields. The adapter can map the descriptor to a column definition while
//! retaining the literal spelling that MySQL uses when it chooses a display
//! width.

use crate::CheckedComparisonNow;
use sqlparser::ast::{Expr, TrimWhereField, UnaryOperator, Value};

/// The sign written around a checked integer literal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaticIntegerSign {
    /// No explicit sign was written.
    None,
    /// The literal used an explicit `+` sign.
    Positive,
    /// The literal used an explicit `-` sign.
    Negative,
}

/// Syntax provenance for a result expression whose metadata is fixed before
/// execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StaticSelectMetadata {
    /// A signed 64-bit integer literal. The digit count includes leading zeroes.
    Integer {
        digit_count: u32,
        sign: StaticIntegerSign,
    },
    /// A SQL boolean literal.
    Boolean(bool),
    /// `EXISTS (subquery)`, which answers 1 or 0 and never NULL — measured on
    /// MySQL 8.4.11, the same shape a boolean literal answers.
    Exists,
    /// A SQL NULL literal.
    Null,
    /// A `COUNT`, whose result metadata is the same whatever it counts.
    Count,
    /// Integer arithmetic, whose result type is worked out from its operands.
    ///
    /// Like `ColumnAggregate` this is finished by the server, which is the only
    /// side that can see a column's precision.
    Arithmetic(ArithmeticShape),
    /// A scalar call, whose result shape is a rule over the columns it names
    /// and the characters its literals contribute.
    ScalarCall {
        function: ScalarFunction,
        columns: Vec<String>,
        literal_characters: u32,
        not_null: bool,
    },
    /// An aggregate whose result type is worked out from the named column.
    ///
    /// Unlike every other variant this one cannot be resolved here: the type
    /// lives in the table, so the server finishes it once it has the column.
    ColumnAggregate {
        column_name: String,
        kind: ColumnAggregateKind,
    },
    /// A `MIN`, `MAX`, `SUM` or `GROUP_CONCAT` over a column named with its
    /// table in a statement reading several tables — `SUM(posts.views)` over a
    /// join — which the server finishes from that table's column.
    QualifiedAggregate {
        table: String,
        column_name: String,
        kind: ColumnAggregateKind,
    },
    /// The same aggregate over a window, which answers almost the same shape.
    WindowAggregate {
        column_name: String,
        kind: ColumnAggregateKind,
    },
    /// A `COUNT` over a window, which answers the same shape a plain `COUNT`
    /// does apart from the binary flag.
    WindowCount,
    /// A scalar subquery in a projection, which answers the shape the aggregate
    /// inside it answers — nullable, whatever the aggregate is.
    ScalarSubquery(Box<StaticSelectMetadata>),
    /// `IFNULL(<aggregate>, <whole number>)`, which answers the shape the
    /// aggregate answers — never null, which is why it is written, and widened
    /// to a `BIGINT` when the aggregate answers any whole number.
    ///
    /// Entity Framework Core falls a total back on a zero written with places,
    /// `COALESCE(SUM(balance), 0.0)`, which answers the aggregate's own shape
    /// only where the aggregate answers a `DECIMAL` with at least those places;
    /// `fallback_places` is how many were written, 0 for a whole number.
    DefaultedAggregate {
        aggregate: Box<StaticSelectMetadata>,
        fallback_places: u32,
    },
    /// `CAST(<aggregate> AS SIGNED)` — Entity Framework Core reads a total as
    /// `CAST(SUM(views) AS SIGNED)` — which answers a `LONGLONG` of 21 with
    /// the binary flag, nullable as a total over no rows is. Only an
    /// aggregate answering a whole number is taken, which the cast leaves as
    /// it is; the server holds the aggregate to that.
    AggregateAsWholeNumber(Box<StaticSelectMetadata>),
    /// A `CASE` or `IF` whose branches are whole numbers or name a column, or
    /// an `IFNULL` or `COALESCE` falling one column back onto another. The
    /// answer is a rule over every branch: the kind they share and the widest
    /// of them.
    ///
    /// Like `Arithmetic` this is finished by the server, because a column
    /// branch's type lives in the table.
    Branches {
        branches: Vec<Branch>,
        /// Whether a row can answer NULL whatever its branches hold: a `CASE`
        /// with no `ELSE`, or a `NULL` branch.
        may_be_null: bool,
        /// Whether this answers its first argument that is not NULL, as
        /// `IFNULL` and `COALESCE` do, rather than the branch a condition
        /// picks.
        falls_back: bool,
    },
    /// `SUM`, `AVG`, `MIN` or `MAX` over a `CASE` or `IF`, which is how a
    /// report counts or totals the rows that meet a condition. Its shape is
    /// the one the aggregate gives the column the `CASE` answers.
    AggregateOverBranches {
        kind: ColumnAggregateKind,
        branches: Box<StaticSelectMetadata>,
    },
    /// A value written out in full, whose shape is fixed by how it is
    /// written.
    WrittenValue(crate::WrittenValue),
    /// `ROUND(SUM(col), n)` or `ROUND(AVG(col), n)`, which answers a decimal
    /// worked out from the aggregate's own precision and scale.
    ///
    /// Like `ColumnAggregate` this is finished by the server, which is the only
    /// side that can see the column's precision.
    RoundedAggregate {
        column_name: String,
        kind: ColumnAggregateKind,
        /// The places it rounds to: zero or more, since a place left of the
        /// point is refused.
        places: u32,
    },
    /// A column of a `WITH RECURSIVE` counted sequence, which MySQL reports as
    /// a nullable `LONGLONG` as wide as its first value's digits and one more.
    CountedColumn {
        /// The name the statement reads the sequence under.
        table: String,
        column: String,
        length: u32,
    },
    /// A grouping key of a statement grouping `WITH ROLLUP`, which answers the
    /// column's own shape as MySQL reports a rolled-up key: naming no table,
    /// and nullable, since a super total answers it as NULL.
    RolledUpKey { column_name: String },
    /// An aggregate of a statement grouping `WITH ROLLUP`.
    FromARollup(Box<StaticSelectMetadata>),
    /// An answer of a statement grouping by an expression, which MySQL groups
    /// in a temporary table and reports the table's column for rather than the
    /// answer's own shape.
    ///
    /// The server finishes it from the answer's own shape.
    FromTheGroupingTable {
        answer: Box<StaticSelectMetadata>,
        /// Whether the answer is one of the grouping keys, which carry a flag
        /// of their own in that table.
        key: bool,
    },
}

/// One thing a `CASE`, `IF`, `IFNULL` or `COALESCE` can answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Branch {
    /// A written whole number, as many digits long as it was written.
    WholeNumber { digit_count: u32 },
    /// A written word, as many characters long as it spells.
    Word { characters: u32 },
    /// A column, whose type lives in the table.
    Column { column_name: String },
}

impl StaticSelectMetadata {
    /// Returns the answer itself, whatever table it is read out of.
    pub fn answer(&self) -> &StaticSelectMetadata {
        match self {
            StaticSelectMetadata::FromTheGroupingTable { answer, .. }
            | StaticSelectMetadata::FromARollup(answer) => answer,
            answer => answer,
        }
    }
}

/// One integer arithmetic expression, whose result type is a rule over its
/// operands rather than a type of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArithmeticShape {
    pub operator: ArithmeticOperator,
    pub left: ArithmeticOperand,
    pub right: ArithmeticOperand,
}

impl ArithmeticShape {
    /// Reports whether any operand is a column, which is what makes the shape
    /// need the table before its type can be worked out.
    pub fn names_a_column(&self) -> bool {
        [&self.left, &self.right]
            .into_iter()
            .any(|operand| match operand {
                ArithmeticOperand::Literal { .. }
                | ArithmeticOperand::DecimalLiteral { .. }
                | ArithmeticOperand::Count => false,
                ArithmeticOperand::Column { .. } | ArithmeticOperand::Aggregate { .. } => true,
                ArithmeticOperand::Nested(shape) => shape.names_a_column(),
            })
    }
}

/// One side of an arithmetic expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArithmeticOperand {
    /// An integer literal. MySQL uses its digit count as the precision, so a
    /// sign and any leading zeroes do not count.
    Literal { digit_count: u32 },
    /// A plain decimal literal with a point, whose written digits set its shape.
    DecimalLiteral { precision: u32, scale: u32 },
    /// A column, whose precision and nullability live in the table.
    Column { column_name: String },
    /// A nested arithmetic expression.
    Nested(Box<ArithmeticShape>),
    /// A `COUNT`, whose shape is the same whatever it counts.
    Count,
    /// An aggregate over a column, whose shape is worked out from the column
    /// the way the aggregate's own result column is.
    Aggregate {
        column_name: String,
        kind: ColumnAggregateKind,
    },
}

/// The arithmetic operators whose MySQL result shape has been measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArithmeticOperator {
    Add,
    Subtract,
    Multiply,
    Divide,
}

/// The scalar functions whose MySQL result shape has been measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalarFunction {
    /// `LOWER` and `UPPER`, which answer their argument's own shape.
    KeepsTextShape,
    /// `LENGTH`, in bytes, and `CHAR_LENGTH`, in characters.
    CountsText,
    /// `NOW` and `CURRENT_TIMESTAMP`, which read no column.
    Now,
    /// `CURDATE` and `CURRENT_DATE`, which read no column either and answer
    /// the day alone.
    Today,
    /// `CURTIME` and `CURRENT_TIME`, which answer the time of day alone.
    TimeOfDay,
    /// `NOW(n)` and its spellings, which answer the moment to `n` places of
    /// a second.
    NowToAFraction { places: u32 },
    /// `CURTIME(n)` and its spellings, which answer the time of day to `n`
    /// places of a second.
    TimeOfDayToAFraction { places: u32 },
    /// `YEAR`, which reads the year out of a date and answers a `YEAR`.
    ReadsTheYear,
    /// `MONTH` and `DAY`, which read a smaller part of the same date.
    ReadsAMonthOrDay,
    /// `HOUR`, which reads the hour out of a moment.
    ReadsTheHour,
    /// `MINUTE` and `SECOND`, which read a smaller part of the same
    /// moment.
    ReadsAMinuteOrSecond,
    /// `QUARTER`, which answers 1 to 4.
    ReadsTheQuarter,
    /// `WEEKDAY` and `DAYOFWEEK`, which each answer a day of the week by a
    /// numbering of their own.
    ReadsADayOfTheWeek,
    /// `DAYOFYEAR`, which answers 1 to 366.
    ReadsTheDayOfTheYear,
    /// `WEEK`, which answers 0 to 53.
    ReadsTheWeek,
    /// `YEARWEEK`, which answers the year and the week as one number.
    ReadsTheYearAndWeek,
    /// `TO_DAYS`, which counts the days from the year zero.
    CountsDaysFromTheYearZero,
    /// `DAYNAME` and `MONTHNAME`, which answer the English name of a day of
    /// the week or of a month.
    NamesTheDayOrMonth,
    /// `LAST_DAY`, which answers the day the month ends on.
    ReadsTheLastDay,
    /// `EXTRACT(YEAR FROM ...)`, which answers a whole number where `YEAR`
    /// answers a YEAR.
    ReadsTheYearAsANumber,
    /// `DATEDIFF`, which answers the days between two dates.
    CountsDaysBetween,
    /// `TIMESTAMPDIFF`, which counts whole units from the first moment to the
    /// second.
    CountsUnitsBetween,
    /// `DATE_ADD` and `DATE_SUB` over an interval of whole days, months or
    /// years, which answer the column's own kind.
    ShiftsByWholeDays,
    /// `DATE_ADD` and `DATE_SUB` over an interval carrying a time, which
    /// answer a moment whatever the column was.
    ShiftsByTime,
    /// `CAST(col AS CHAR)`, which answers the column written out.
    CastsToText,
    /// `CAST(col AS SIGNED)`, which answers the whole number nearest the
    /// column's value.
    CastsToWholeNumber,
    /// `CAST(col AS DATE)`, which answers the day out of a moment.
    CastsToDay,
    /// `CAST(col AS DATETIME)`, which answers the moment a day begins.
    CastsToMoment,
    /// `DATE_ADD` and `DATE_SUB` over a reading of the moment, which read no
    /// column and answer a moment whatever the interval named.
    ShiftsTheMoment,
    /// `DATE_ADD` and `DATE_SUB` over a reading of today by an interval of
    /// whole days, months or years, which read no column and answer a day.
    ShiftsTheDay,
    /// `DATE_ADD` and `DATE_SUB` over a moment written out as a word, which
    /// answer a word rather than a moment.
    ShiftsAWrittenMoment,
    /// `ABS`, which answers its argument's own numeric shape.
    KeepsNumericShape,
    /// `SIGN`, which answers a whole number however wide the argument was.
    Truncates,
    /// `ROUND`, to the places it names or to a whole number, which answers
    /// the kind of number its column holds.
    RoundsToPlaces { places: i32 },
    /// `FLOOR`, `CEIL` and `CEILING`, which answer the kind of number their
    /// column holds.
    RoundsToWhole,
    /// `-col`, which answers the column's own width as a whole number.
    Negates,
    /// `col DIV n`, which answers the whole number of times it goes in.
    DividesWhole,
    /// A comparison, which answers 1, 0 or NULL.
    Compares,
    /// A comparison over a count of days or units between two moments, a
    /// shifted reading of the clock, or a subquery's count, which MySQL
    /// reports as nullable whatever it reads.
    ComparesACallOrASubquery,
    /// `NOT col`, which answers 1, 0 or NULL.
    NegatesTruth,
    /// `col IS TRUE`, `IS FALSE`, `IS NOT TRUE` and `IS NOT FALSE`, which
    /// answer 1 or 0 and never NULL.
    TestsTruth,
    /// `IFNULL` and `COALESCE`, which answer the column's shape and cannot be
    /// null when a later argument cannot.
    Defaulted,
    /// The same over a column of words, with a written word to fall back on.
    DefaultedText,
    /// `CONCAT`, whose answer is as wide as its arguments laid end to end.
    Concatenates,
    /// `LEFT` and `RIGHT`, whose answer is as wide as the count they were
    /// asked for.
    TakesCharacters,
    /// `SUBSTRING` and `SUBSTR`, whose answer is as wide as what is left of
    /// the column after the place it starts from, held to the count it was
    /// asked for.
    TakesASubstring { from: i32, count: Option<i32> },
    /// `SUBSTRING_INDEX`, which answers the part of the column before or
    /// after a delimiter, and is as wide as the column.
    SplitsOnADelimiter,
    /// A `CASE` or `IF`, whose answer is as wide as its widest branch.
    Branches,
    /// `REPEAT`, whose answer is as wide as its column's character length times the repeat count.
    Repeats,
    /// `LOCATE` and `INSTR`, which answer a signed 64-bit integer of length 11 and decimals 0.
    Locates,
    /// `HEX`, whose answer is as wide as its column's character length times 8.
    Hexadecimal,
    /// `ASCII`, which answers the first byte of a word.
    ReadsTheFirstByte,
    /// `ORD`, which answers the bytes of a word's first character as one
    /// number.
    ReadsTheFirstCharacter,
    /// `CRC32`, which answers the checksum of the value written out.
    ChecksTheBytes,
    /// `QUOTE`, which writes the value out as a quoted SQL word.
    QuotesForSql,
    /// `TO_BASE64`, which writes the value out in base64.
    EncodesInBase64,
    /// `INET_ATON`, which reads a dotted address as a number.
    ReadsAnAddress,
    /// `TIME_TO_SEC`, which counts the seconds in a time.
    CountsTheSecondsOfATime,
    /// `SEC_TO_TIME`, which writes a count of seconds out as a time.
    WritesSecondsAsATime,
    /// `INET_NTOA`, which writes a number out as a dotted address.
    WritesAnAddress,
    /// `IS_IPV4`, which answers whether a word is a dotted address.
    ChecksAnAddress,
    /// `JSON_EXTRACT` over one path, which answers the JSON value it found —
    /// a string comes back with its quotes.
    ReadsAJsonValue,
    /// `JSON_UNQUOTE` over that, which answers the text inside the value.
    ReadsJsonText,
    /// `CAST(JSON_EXTRACT(...) AS SIGNED INTEGER)`, which reads the value as
    /// a whole number.
    ReadsJsonAsWholeNumber,
    /// `JSON_EXTRACT(...) + 0.0...`, which reads the value as a double.
    ReadsJsonAsDouble,
    /// `JSON_VALID`, which answers one or zero.
    ChecksJson,
    /// `JSON_TYPE`, which names the kind of the document it was given.
    NamesAJsonKind,
    /// `JSON_LENGTH`, which counts what a document holds at the top.
    CountsJsonMembers,
    /// `JSON_KEYS`, which answers an object's keys as a document of their own.
    ListsJsonKeys,
    /// `JSON_QUOTE`, which writes text as a JSON string.
    QuotesAsJson,
    /// `JSON_ARRAY` and `JSON_OBJECT`, which build a document out of what they
    /// are given.
    BuildsJson,
    /// `JSON_SET`, `JSON_INSERT`, `JSON_REPLACE` and `JSON_REMOVE`, which
    /// answer a document with one member changed.
    ChangesJson,
    /// `JSON_ARRAYAGG` over a `JSON_OBJECT` or `JSON_ARRAY` built from each
    /// row, which answers an array of the documents.
    CollectsBuiltJson,
    /// `FORMAT`, which writes a number for a person to read.
    GroupsDigits,
    /// `TRUNCATE`, which cuts a number off at a count of places.
    CutsDigits,
    /// `JSON_CONTAINS` and `JSON_CONTAINS_PATH`, which answer whether a
    /// document holds something.
    SearchesJson,
    /// `JSON_OVERLAPS`, which answers whether two documents share anything.
    SharesJson,
    /// `UNIX_TIMESTAMP`, which counts the seconds from the epoch to a moment.
    CountsEpochSeconds,
    /// `FROM_UNIXTIME`, which reads a moment back out of those seconds.
    ReadsFromEpoch,
    /// `FROM_UNIXTIME` with a format, which writes that moment out the way
    /// `DATE_FORMAT` does.
    WritesAnEpochMoment,
    /// `DATE_FORMAT` over a literal format, whose answer is as wide as the
    /// format could make it.
    WritesAMoment,
    /// `STR_TO_DATE` over a literal format naming only day parts.
    ReadsADay,
    /// `STR_TO_DATE` over a literal format naming only clock parts.
    ReadsAClock,
    /// `STR_TO_DATE` over a literal format naming both.
    ReadsAMoment,
    /// `RAND` with no argument, which answers a double between zero and one.
    Randomises,
    /// `UUID`, which answers a new identifier and reads no column.
    Identifies,
    /// `MD5`, `SHA1` and `SHA2`, whose answer is as many hexadecimal
    /// characters as the digest has.
    Digests,
    /// `ROW_NUMBER`, `RANK`, `DENSE_RANK` and `NTILE` over a window, which
    /// answer an unsigned 64-bit row count.
    RanksRows,
    /// `PERCENT_RANK` and `CUME_DIST` over a window, which answer a double
    /// between zero and one.
    RanksFraction,
    /// `LAG`, `LEAD`, `FIRST_VALUE`, `LAST_VALUE` and `NTH_VALUE` over a
    /// window, which answer another row's value for the column they name, and
    /// NULL where there is no such row.
    ShiftsRow,
    /// `LAG` and `LEAD` over a whole-number column with a written whole number
    /// to answer where there is no such row, which widens the answer to the
    /// default's own width.
    ShiftsRowOrNumber,
    /// The same over a `VARCHAR` with a written word to answer.
    ShiftsRowOrWord,
    /// `SQRT` and `POW`, which answer a floating-point DOUBLE of length 23 and not-fixed decimals.
    Approximates,
    /// `PI`, which reads nothing and answers a narrower double than the rest.
    NamesTheCircle,
    /// `BIN` and `OCT`, which write a whole number out in another radix.
    WritesInAnotherRadix,
    /// `FIELD`, which answers the place a word holds among the ones that
    /// follow it, or nothing's place, 0.
    FindsThePlace,
    /// `ELT`, which reads one of the words that follow it out by its place.
    ReadsThePlace,
    /// `MOD`, which answers its argument's own numeric shape but can be null.
    Modulo,
    /// `GREATEST` and `LEAST`, which answer the widest shape among their arguments.
    Widest,
    /// `NULLIF`, which answers its first argument's shape but can always be null.
    NullsOnMatch,
}

/// The aggregates whose result type is a rule over the argument column's type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnAggregateKind {
    /// `MIN` or `MAX`, which answer the column's own type.
    MinMax,
    /// `SUM`, which widens a decimal by 22 digits.
    Sum,
    /// `AVG`, which widens a decimal by 4 digits and 4 decimal places.
    Avg,
    /// `GROUP_CONCAT`, which answers a BLOB of length 65536 and 31 decimals.
    Concatenated,
    /// `STDDEV_SAMP`, which answers a DOUBLE whatever the column is.
    DeviatesBySample,
    /// `JSON_ARRAYAGG`, which answers a JSON array of every value, and NULL
    /// over no rows at all.
    CollectsIntoJson,
}

/// Source-level kind of one checked `SELECT` projection item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StaticSelectProjectionMetadata {
    /// A literal whose result metadata is fixed by its source spelling.
    Literal(StaticSelectMetadata),
    /// A wildcard whose expansion can add result columns.
    Wildcard,
    /// An expression whose result metadata is not described by this module.
    Other,
}

/// Classifies the checked literal forms whose result metadata is independent
/// of returned rows or prepared parameter values.
pub(super) fn classify_static_select_expr(expr: &Expr) -> Option<StaticSelectMetadata> {
    match expr {
        Expr::Value(value) => match &value.value {
            Value::Number(digits, false) => classify_integer(digits, StaticIntegerSign::None),
            Value::Boolean(value) => Some(StaticSelectMetadata::Boolean(*value)),
            Value::Null => Some(StaticSelectMetadata::Null),
            _ => None,
        },
        Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr: negated,
        } if matches!(negated.as_ref(), Expr::Identifier(_)) => {
            let Expr::Identifier(column) = negated.as_ref() else {
                unreachable!("the guard requires a column");
            };
            Some(StaticSelectMetadata::ScalarCall {
                function: ScalarFunction::Negates,
                columns: vec![column.value.clone()],
                literal_characters: 0,
                not_null: false,
            })
        }
        Expr::UnaryOp {
            op: UnaryOperator::Not,
            expr: negated,
        } => {
            let Expr::Identifier(column) = negated.as_ref() else {
                return None;
            };
            Some(StaticSelectMetadata::ScalarCall {
                function: ScalarFunction::NegatesTruth,
                columns: vec![column.value.clone()],
                literal_characters: 0,
                not_null: false,
            })
        }
        Expr::IsTrue(tested)
        | Expr::IsFalse(tested)
        | Expr::IsNotTrue(tested)
        | Expr::IsNotFalse(tested) => {
            let Expr::Identifier(column) = tested.as_ref() else {
                return None;
            };
            Some(StaticSelectMetadata::ScalarCall {
                function: ScalarFunction::TestsTruth,
                columns: vec![column.value.clone()],
                literal_characters: 0,
                not_null: true,
            })
        }
        Expr::UnaryOp { op, expr } => {
            let sign = match op {
                UnaryOperator::Plus => StaticIntegerSign::Positive,
                UnaryOperator::Minus => StaticIntegerSign::Negative,
                _ => return None,
            };
            let Expr::Value(value) = expr.as_ref() else {
                return None;
            };
            let Value::Number(digits, false) = &value.value else {
                return None;
            };
            classify_integer(digits, sign)
        }
        Expr::Nested(inner) => classify_static_select_expr(inner),
        Expr::Case { .. } if json_member_unless_null(expr).is_some() => {
            json_member_unless_null(expr)
        }
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            if let Some(operand) = operand {
                compared_against_written_values(
                    operand,
                    conditions.iter().map(|when| &when.condition),
                )?;
            }
            classify_branches(
                conditions.iter().map(|when| &when.result),
                else_result.as_deref(),
            )
        }
        Expr::Substring {
            expr,
            substring_from,
            substring_for,
            ..
        } => classify_substring(expr, substring_from.as_deref(), substring_for.as_deref()),
        // `POSITION(substr IN str)` is `LOCATE(substr, str)` written with a
        // keyword, and measured on MySQL 8.4.11 answers what it answers.
        Expr::Position { expr: needle, r#in } => {
            let Expr::Identifier(column) = r#in.as_ref() else {
                return None;
            };
            if !matches!(needle.as_ref(), Expr::Value(value)
                if matches!(&value.value, Value::SingleQuotedString(_) | Value::DoubleQuotedString(_)))
            {
                return None;
            }
            Some(StaticSelectMetadata::ScalarCall {
                function: ScalarFunction::Locates,
                columns: vec![column.value.clone()],
                literal_characters: 0,
                not_null: false,
            })
        }
        Expr::Trim {
            trim_where,
            trim_what,
            expr,
            trim_characters,
        } => classify_trim(
            *trim_where,
            trim_what.as_deref(),
            expr,
            trim_characters.as_deref(),
        ),
        Expr::Cast {
            kind,
            expr,
            data_type,
            format,
            array,
        } => classify_cast(kind, expr, data_type, format.as_ref(), *array),
        Expr::Convert { .. } => classify_convert(expr),
        Expr::Extract {
            field,
            syntax,
            expr,
        } => classify_extract(field, syntax, expr),
        Expr::Floor { expr, field } => classify_floor_ceil(expr, field),
        Expr::Ceil { expr, field } => classify_floor_ceil(expr, field),
        Expr::BinaryOp { .. } => classify_json_arrow(expr)
            .or_else(|| interval_shift_as_call(expr).and_then(|call| scalar_call(&call)))
            .or_else(|| classify_comparison(expr))
            .or_else(|| classify_whole_division(expr))
            .or_else(|| classify_arithmetic(expr).map(StaticSelectMetadata::Arithmetic)),
        Expr::Subquery(query) => classify_scalar_subquery(query),
        Expr::Exists { .. } => Some(StaticSelectMetadata::Exists),
        Expr::Function(function) if function.over.is_some() => classify_window_call(function),
        Expr::Function(function) if is_count_call(function) => Some(StaticSelectMetadata::Count),
        Expr::Function(function) => column_aggregate_argument(function)
            .map(|(kind, column)| StaticSelectMetadata::ColumnAggregate {
                column_name: column.value.clone(),
                kind,
            })
            .or_else(|| {
                qualified_aggregate_argument(function).map(|(kind, table, column)| {
                    StaticSelectMetadata::QualifiedAggregate {
                        table: table.value.clone(),
                        column_name: column.value.clone(),
                        kind,
                    }
                })
            })
            .or_else(|| aggregate_over_branches(function))
            .or_else(|| scalar_call(function)),
        _ => None,
    }
}

/// Classifies the `CASE` SQLAlchemy writes to read a JSON member —
/// `CASE JSON_EXTRACT(col, 'path') WHEN 'null' THEN NULL ELSE <reading> END`
/// — where the reading reads the same path out of the same column:
/// `JSON_UNQUOTE(...)` for `as_string()`, `CAST(... AS SIGNED INTEGER)` for
/// `as_integer()` and `... + 0.0000000000000000000000` for `as_float()`.
///
/// Measured on MySQL 8.4.11: each reports the column its reading reports on
/// its own — a LONG_BLOB, a LONGLONG of 21 and a DOUBLE of 23 — and answers
/// what the reading answers except no value where the path finds the JSON
/// null; the JSON string `"null"` is read like any other string.
pub(crate) fn json_member_unless_null(expr: &Expr) -> Option<StaticSelectMetadata> {
    let Expr::Case {
        operand: Some(operand),
        conditions,
        else_result: Some(otherwise),
        ..
    } = expr
    else {
        return None;
    };
    let [when] = conditions.as_slice() else {
        return None;
    };
    let tests_for_the_null = matches!(&when.condition, Expr::Value(value)
        if matches!(&value.value, Value::SingleQuotedString(word) if word == "null"));
    let answers_no_value =
        matches!(&when.result, Expr::Value(value) if matches!(value.value, Value::Null));
    if !tests_for_the_null || !answers_no_value {
        return None;
    }
    let Expr::Function(read) = operand.as_ref() else {
        return None;
    };
    let Some(StaticSelectMetadata::ScalarCall {
        function: ScalarFunction::ReadsAJsonValue,
        columns,
        ..
    }) = scalar_call(read)
    else {
        return None;
    };
    let (function, read_again) = member_reading(otherwise)?;
    (read_again == operand.as_ref()).then_some(StaticSelectMetadata::ScalarCall {
        function,
        columns,
        literal_characters: 0,
        not_null: false,
    })
}

/// Reads the `ELSE` of SQLAlchemy's `CASE`: what it reads the member as, and
/// the `JSON_EXTRACT` it reads.
pub(crate) fn member_reading(expr: &Expr) -> Option<(ScalarFunction, &Expr)> {
    use sqlparser::ast::{CastKind, DataType};

    match expr {
        Expr::Function(unquoted) => {
            let [sqlparser::ast::ObjectNamePart::Identifier(name)] = unquoted.name.0.as_slice()
            else {
                return None;
            };
            if name.quote_style.is_some() || !name.value.eq_ignore_ascii_case("JSON_UNQUOTE") {
                return None;
            }
            let sqlparser::ast::FunctionArguments::List(arguments) = &unquoted.args else {
                return None;
            };
            let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(read))] =
                arguments.args.as_slice()
            else {
                return None;
            };
            Some((ScalarFunction::ReadsJsonText, read))
        }
        Expr::Cast {
            kind: CastKind::Cast,
            expr: read,
            data_type: DataType::Signed | DataType::SignedInteger,
            format: None,
            array: false,
        } => Some((ScalarFunction::ReadsJsonAsWholeNumber, read)),
        Expr::BinaryOp {
            left: read,
            op: sqlparser::ast::BinaryOperator::Plus,
            right,
        } => {
            let Expr::Value(value) = right.as_ref() else {
                return None;
            };
            let Value::Number(digits, false) = &value.value else {
                return None;
            };
            let zeros = digits.strip_prefix("0.")?;
            (!zeros.is_empty() && zeros.bytes().all(|digit| digit == b'0'))
                .then_some((ScalarFunction::ReadsJsonAsDouble, read))
        }
        _ => None,
    }
}

/// Classifies `col -> '$.path'` and `col ->> '$.path'`, which are MySQL's own
/// spellings of `JSON_EXTRACT` and the `JSON_UNQUOTE` around it.
fn classify_json_arrow(expr: &Expr) -> Option<StaticSelectMetadata> {
    let Expr::BinaryOp { left, op, right } = expr else {
        return None;
    };
    let function = match op {
        sqlparser::ast::BinaryOperator::Arrow => ScalarFunction::ReadsAJsonValue,
        sqlparser::ast::BinaryOperator::LongArrow => ScalarFunction::ReadsJsonText,
        _ => return None,
    };
    let Expr::Identifier(column) = left.as_ref() else {
        return None;
    };
    let Expr::Value(path) = right.as_ref() else {
        return None;
    };
    let (Value::SingleQuotedString(path) | Value::DoubleQuotedString(path)) = &path.value else {
        return None;
    };
    if !names_a_plain_json_path(path) {
        return None;
    }
    Some(StaticSelectMetadata::ScalarCall {
        function,
        columns: vec![column.value.clone()],
        literal_characters: 0,
        not_null: false,
    })
}

/// Reads `x + INTERVAL n unit`, `INTERVAL n unit + x` and `x - INTERVAL n
/// unit` as the `DATE_ADD` or `DATE_SUB` each is.
///
/// Measured on MySQL 8.4.11, the operator and the call answer the same moment
/// and the same shape — `created_at + INTERVAL 1 DAY` and `DATE_ADD(created_at,
/// INTERVAL 1 DAY)` are both a `DATETIME` of 19 — so everything the call is
/// read for, the operator is read for as well.
pub(super) fn interval_shift_as_call(expr: &Expr) -> Option<sqlparser::ast::Function> {
    use sqlparser::ast::{
        BinaryOperator, FunctionArg, FunctionArgExpr, FunctionArgumentList, FunctionArguments,
        Ident, ObjectName, ObjectNamePart,
    };
    let Expr::BinaryOp { left, op, right } = expr else {
        return None;
    };
    let (name, shifted, interval) = match (left.as_ref(), op, right.as_ref()) {
        (Expr::Interval(_), _, Expr::Interval(_)) => return None,
        (shifted, BinaryOperator::Plus, Expr::Interval(interval)) => {
            ("DATE_ADD", shifted, interval)
        }
        (Expr::Interval(interval), BinaryOperator::Plus, shifted) => {
            ("DATE_ADD", shifted, interval)
        }
        (shifted, BinaryOperator::Minus, Expr::Interval(interval)) => {
            ("DATE_SUB", shifted, interval)
        }
        _ => return None,
    };
    Some(sqlparser::ast::Function {
        name: ObjectName(vec![ObjectNamePart::Identifier(Ident::new(name))]),
        uses_odbc_syntax: false,
        parameters: FunctionArguments::None,
        args: FunctionArguments::List(FunctionArgumentList {
            duplicate_treatment: None,
            args: vec![
                FunctionArg::Unnamed(FunctionArgExpr::Expr(shifted.clone())),
                FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Interval(interval.clone()))),
            ],
            clauses: Vec::new(),
        }),
        filter: None,
        null_treatment: None,
        over: None,
        within_group: Vec::new(),
    })
}

/// Classifies a comparison standing as a result column.
///
/// Each shape is one the `WHERE` comparison reader takes, rendered the same
/// way. Measured on MySQL 8.4.11, each answers a `LONGLONG` of length 1, NOT
/// NULL where nothing it reads can be null: a column against a written number
/// or word, or against a reading of the clock — `NOW() > created_at` — is NOT
/// NULL where the column is, and a `COUNT` or a written whole number against a
/// written whole number always is. A count of days or units between two
/// moments, a shifted reading of the clock and a subquery's count are reported
/// nullable whatever they read — `TIMESTAMPDIFF(DAY, created_at, NOW()) >= 0`,
/// `DATE_SUB(NOW(), INTERVAL 1 DAY) < created_at` and
/// `(SELECT COUNT(*) FROM posts) > 0` all are, over a NOT NULL column.
fn classify_comparison(expr: &Expr) -> Option<StaticSelectMetadata> {
    let Expr::BinaryOp { left, op, right } = expr else {
        return None;
    };
    if !matches!(
        op,
        sqlparser::ast::BinaryOperator::Eq
            | sqlparser::ast::BinaryOperator::NotEq
            | sqlparser::ast::BinaryOperator::Lt
            | sqlparser::ast::BinaryOperator::LtEq
            | sqlparser::ast::BinaryOperator::Gt
            | sqlparser::ast::BinaryOperator::GtEq
    ) {
        return None;
    }
    let written = |expr: &Expr| match expr {
        Expr::Value(value) => matches!(
            value.value,
            Value::Number(_, false) | Value::SingleQuotedString(_) | Value::DoubleQuotedString(_)
        ),
        Expr::UnaryOp {
            op: UnaryOperator::Minus | UnaryOperator::Plus,
            expr,
        } => {
            matches!(expr.as_ref(), Expr::Value(value) if matches!(value.value, Value::Number(_, false)))
        }
        _ => false,
    };
    let counted = |expr: &Expr| matches!(expr, Expr::Function(function) if is_count_call(function));
    let whole = |expr: &Expr| crate::translate::direct_signed_integer(expr).is_some();
    let reads_the_clock = |expr: &Expr| shifted_moment(expr).is_some();
    let (function, columns, not_null) = match (left.as_ref(), right.as_ref()) {
        (Expr::Identifier(column), other) | (other, Expr::Identifier(column))
            if written(other) || reads_the_clock(other) =>
        {
            (ScalarFunction::Compares, vec![column.value.clone()], false)
        }
        (count, other) | (other, count) if counted(count) && whole(other) => {
            (ScalarFunction::Compares, Vec::new(), true)
        }
        // The name is the text as written, and a sign written first is not
        // part of the span a comparison reports.
        (Expr::Value(number), other)
            if matches!(number.value, Value::Number(_, false)) && whole(left) && whole(other) =>
        {
            (ScalarFunction::Compares, Vec::new(), true)
        }
        (Expr::Identifier(column), shifted) | (shifted, Expr::Identifier(column))
            if matches!(shifted, Expr::Function(function)
                if crate::translate::shifts_a_reading_of_the_clock(function)) =>
        {
            (
                ScalarFunction::ComparesACallOrASubquery,
                vec![column.value.clone()],
                false,
            )
        }
        (Expr::Function(call), other) | (other, Expr::Function(call)) if whole(other) => {
            let Some(StaticSelectMetadata::ScalarCall {
                function: ScalarFunction::CountsDaysBetween | ScalarFunction::CountsUnitsBetween,
                columns,
                ..
            }) = scalar_call(call)
            else {
                return None;
            };
            (ScalarFunction::ComparesACallOrASubquery, columns, false)
        }
        (Expr::Subquery(query), other) | (other, Expr::Subquery(query))
            if whole(other)
                && matches!(
                    classify_scalar_subquery(query),
                    Some(StaticSelectMetadata::ScalarSubquery(counted))
                        if *counted == StaticSelectMetadata::Count
                ) =>
        {
            (ScalarFunction::ComparesACallOrASubquery, Vec::new(), false)
        }
        _ => return None,
    };
    Some(StaticSelectMetadata::ScalarCall {
        function,
        columns,
        literal_characters: 0,
        not_null,
    })
}

/// Classifies `col % n` and `col DIV n`, which MySQL answers as whole numbers
/// the column's own width.
///
/// The divisor has to be a written whole number. Zero is refused: MySQL
/// answers NULL with a warning this does not raise. So is `DIV -1`: measured,
/// the smallest `BIGINT` divided by it is 1690, where the engine answers a
/// real number.
fn classify_whole_division(expr: &Expr) -> Option<StaticSelectMetadata> {
    let Expr::BinaryOp { left, op, right } = expr else {
        return None;
    };
    let function = match op {
        sqlparser::ast::BinaryOperator::Modulo => ScalarFunction::Modulo,
        sqlparser::ast::BinaryOperator::MyIntegerDivide => ScalarFunction::DividesWhole,
        _ => return None,
    };
    let Expr::Identifier(column) = left.as_ref() else {
        return None;
    };
    match crate::translate::direct_signed_integer(right)? {
        0 => return None,
        -1 if function == ScalarFunction::DividesWhole => return None,
        _ => {}
    }
    Some(StaticSelectMetadata::ScalarCall {
        function,
        columns: vec![column.value.clone()],
        literal_characters: 0,
        not_null: false,
    })
}

/// Classifies the integer arithmetic MySQL's result shape has been measured for.
///
/// Division is taken only at the top, because a nested one makes every operator
/// above it decimal arithmetic, whose precision and scale rules are their own
/// and have not been measured.
pub(super) fn classify_arithmetic(expr: &Expr) -> Option<ArithmeticShape> {
    let Expr::BinaryOp { left, op, right } = expr else {
        return None;
    };
    let operator = match op {
        sqlparser::ast::BinaryOperator::Plus => ArithmeticOperator::Add,
        sqlparser::ast::BinaryOperator::Minus => ArithmeticOperator::Subtract,
        sqlparser::ast::BinaryOperator::Multiply => ArithmeticOperator::Multiply,
        sqlparser::ast::BinaryOperator::Divide => ArithmeticOperator::Divide,
        _ => return None,
    };
    Some(ArithmeticShape {
        operator,
        left: classify_arithmetic_operand(left)?,
        right: classify_arithmetic_operand(right)?,
    })
}

fn classify_arithmetic_operand(expr: &Expr) -> Option<ArithmeticOperand> {
    match expr {
        Expr::Identifier(column) => Some(ArithmeticOperand::Column {
            column_name: column.value.clone(),
        }),
        // `SUM(amount) * 2` and `COUNT(*) + 1` are how a report writes a total
        // it has adjusted. An aggregate stands where a column stands, and its
        // shape is the shape it answers on its own.
        Expr::Function(function) if is_count_call(function) => Some(ArithmeticOperand::Count),
        Expr::Function(function) => {
            let (kind, column) = column_aggregate_argument(function)?;
            matches!(
                kind,
                ColumnAggregateKind::MinMax | ColumnAggregateKind::Sum | ColumnAggregateKind::Avg
            )
            .then(|| ArithmeticOperand::Aggregate {
                column_name: column.value.clone(),
                kind,
            })
        }
        Expr::Nested(inner) => classify_arithmetic_operand(inner),
        Expr::Value(value) => {
            if let Value::Number(written, false) = &value.value {
                if let Some((precision, scale)) = decimal_literal_shape(written) {
                    return Some(ArithmeticOperand::DecimalLiteral { precision, scale });
                }
            }
            match classify_static_select_expr(expr)? {
                StaticSelectMetadata::Integer { digit_count, .. } => {
                    Some(ArithmeticOperand::Literal { digit_count })
                }
                _ => None,
            }
        }
        Expr::UnaryOp {
            op: UnaryOperator::Minus | UnaryOperator::Plus,
            expr: inner,
        } => {
            if let Expr::Value(value) = inner.as_ref() {
                if let Value::Number(written, false) = &value.value {
                    if let Some((precision, scale)) = decimal_literal_shape(written) {
                        return Some(ArithmeticOperand::DecimalLiteral { precision, scale });
                    }
                }
            }
            match classify_static_select_expr(expr)? {
                StaticSelectMetadata::Integer { digit_count, .. } => {
                    Some(ArithmeticOperand::Literal { digit_count })
                }
                _ => None,
            }
        }
        Expr::BinaryOp { .. } => {
            let shape = classify_arithmetic(expr)?;
            if shape.operator == ArithmeticOperator::Divide {
                return None;
            }
            Some(ArithmeticOperand::Nested(Box::new(shape)))
        }
        _ => match classify_static_select_expr(expr)? {
            StaticSelectMetadata::Integer { digit_count, .. } => {
                Some(ArithmeticOperand::Literal { digit_count })
            }
            _ => None,
        },
    }
}

fn decimal_literal_shape(written: &str) -> Option<(u32, u32)> {
    let (whole, fraction) = written.split_once('.')?;
    if whole.is_empty()
        || fraction.is_empty()
        || !whole
            .bytes()
            .chain(fraction.bytes())
            .all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let precision = u32::try_from(whole.len().checked_add(fraction.len())?).ok()?;
    let scale = u32::try_from(fraction.len()).ok()?;
    Some((precision, scale))
}

/// Classifies a `CASE` or an `IF`, whose answer is a rule over its branches.
///
/// Measured on MySQL 8.4.11: `CASE WHEN n > 1 THEN 'y' ELSE 'n' END` answers a
/// `VAR_STRING` of length 4 — one character, four bytes each — and is NOT NULL.
/// A branch is a written word, a written whole number, a column or `NULL`; a
/// written number with a point answers a NEWDECIMAL by a rule of its own, and
/// an aggregate or arithmetic in a branch has not been measured.
///
/// Two things make the answer nullable, both measured: no `ELSE`, because a row
/// matching nothing answers NULL, and a `NULL` branch. Either way the width is
/// still the widest branch — `CASE WHEN n < 3 THEN 'low' END` and
/// `... THEN 'low' ELSE NULL END` both answer length 3 with no `NOT_NULL` flag.
pub(super) fn classify_branches<'a>(
    results: impl Iterator<Item = &'a Expr>,
    else_result: Option<&'a Expr>,
) -> Option<StaticSelectMetadata> {
    let mut branches = Vec::new();
    let mut may_be_null = else_result.is_none();
    for result in results.chain(else_result) {
        if matches!(result, Expr::Value(value) if matches!(value.value, Value::Null)) {
            may_be_null = true;
            continue;
        }
        branches.push(branch(result)?);
    }
    let words = branches
        .iter()
        .filter(|branch| matches!(branch, Branch::Word { .. }))
        .count();
    let names_a_column = branches
        .iter()
        .any(|branch| matches!(branch, Branch::Column { .. }));
    // Every branch was NULL, so there is nothing to take a shape from.
    if branches.is_empty() {
        return None;
    }
    if words == branches.len() {
        let characters = branches
            .iter()
            .map(|branch| match branch {
                Branch::Word { characters } => *characters,
                _ => unreachable!("every branch was counted as a word"),
            })
            .max()
            .expect("there is at least one branch");
        // An empty word is no width to answer with.
        if characters == 0 {
            return None;
        }
        return Some(StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::Branches,
            columns: Vec::new(),
            literal_characters: characters,
            not_null: !may_be_null,
        });
    }
    // A written word beside a written number is a coercion, which has not been
    // measured. Beside a column it is left to the server, which takes it only
    // when the column holds words too.
    if words > 0 && !names_a_column {
        return None;
    }
    Some(StaticSelectMetadata::Branches {
        branches,
        may_be_null,
        falls_back: false,
    })
}

/// Reads one branch of a `CASE`, `IF`, `IFNULL` or `COALESCE`.
fn branch(expr: &Expr) -> Option<Branch> {
    match expr {
        Expr::Identifier(column) => Some(Branch::Column {
            column_name: column.value.clone(),
        }),
        Expr::Nested(inner) => branch(inner),
        Expr::Value(value) => match &value.value {
            Value::SingleQuotedString(text) | Value::DoubleQuotedString(text) => {
                Some(Branch::Word {
                    characters: u32::try_from(text.chars().count()).ok()?,
                })
            }
            _ => whole_number_branch(expr),
        },
        _ => whole_number_branch(expr),
    }
}

/// A written whole number, signed or not. A number carrying a point is not
/// one: measured, `THEN 1.5 ELSE 0` answers a NEWDECIMAL rather than this.
fn whole_number_branch(expr: &Expr) -> Option<Branch> {
    match classify_static_select_expr(expr)? {
        StaticSelectMetadata::Integer { digit_count, .. } => {
            Some(Branch::WholeNumber { digit_count })
        }
        _ => None,
    }
}

/// Holds `CASE col WHEN v1 THEN ... WHEN v2 THEN ...` to what it is the same
/// as: `CASE WHEN col = v1 THEN ... WHEN col = v2 THEN ...`.
///
/// MySQL compares the operand against every `WHEN` value by one rule chosen
/// over all of them together, where the spelled-out form chooses one for each
/// comparison on its own. The two agree when every value is of one kind, so
/// the values have to be all written words or all written whole numbers. Each
/// comparison is then rendered and checked the way a `WHERE` comparison is.
fn compared_against_written_values<'a>(
    operand: &Expr,
    values: impl Iterator<Item = &'a Expr>,
) -> Option<()> {
    if !matches!(operand, Expr::Identifier(_)) {
        return None;
    }
    let mut words = 0usize;
    let mut numbers = 0usize;
    for value in values {
        match branch(value)? {
            Branch::Word { .. } => words += 1,
            Branch::WholeNumber { .. } => numbers += 1,
            Branch::Column { .. } => return None,
        }
    }
    (words == 0 || numbers == 0).then_some(())
}

/// Classifies `COUNT`, `SUM`, `AVG`, `MIN` or `MAX` over one `CASE` or `IF`.
///
/// `SUM(CASE WHEN status = 'active' THEN 1 ELSE 0 END)` is how a report
/// counts the rows meeting a condition. Measured on MySQL 8.4.11, a `COUNT`
/// of one answers what any `COUNT` answers, and the others answer the shape
/// they give a column of the kind the `CASE` answers.
pub(super) fn aggregate_over_branches(
    function: &sqlparser::ast::Function,
) -> Option<StaticSelectMetadata> {
    let (kind, argument) = aggregated_branches(function)?;
    let branches = classify_static_select_expr(argument)?;
    if !matches!(
        branches,
        StaticSelectMetadata::Branches { .. }
            | StaticSelectMetadata::ScalarCall {
                function: ScalarFunction::Branches,
                ..
            }
    ) {
        return None;
    }
    let Some(kind) = kind else {
        return Some(StaticSelectMetadata::Count);
    };
    Some(StaticSelectMetadata::AggregateOverBranches {
        kind,
        branches: Box::new(branches),
    })
}

/// Reads an aggregate over one `CASE` or `IF`: which aggregate it is — `None`
/// for a `COUNT` — and the expression it aggregates.
pub(crate) fn aggregated_branches(
    function: &sqlparser::ast::Function,
) -> Option<(Option<ColumnAggregateKind>, &Expr)> {
    let [sqlparser::ast::ObjectNamePart::Identifier(name)] = function.name.0.as_slice() else {
        return None;
    };
    if name.quote_style.is_some() || has_aggregate_modifiers(function) {
        return None;
    }
    let kind = if name.value.eq_ignore_ascii_case("COUNT") {
        None
    } else if name.value.eq_ignore_ascii_case("SUM") {
        Some(ColumnAggregateKind::Sum)
    } else if name.value.eq_ignore_ascii_case("AVG") {
        Some(ColumnAggregateKind::Avg)
    } else if name.value.eq_ignore_ascii_case("MIN") || name.value.eq_ignore_ascii_case("MAX") {
        Some(ColumnAggregateKind::MinMax)
    } else {
        return None;
    };
    let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
        return None;
    };
    if arguments.duplicate_treatment.is_some() || !arguments.clauses.is_empty() {
        return None;
    }
    let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(argument))] =
        arguments.args.as_slice()
    else {
        return None;
    };
    let is_a_condition = match argument {
        Expr::Case { .. } => true,
        Expr::Function(inner) => {
            matches!(inner.name.0.as_slice(), [sqlparser::ast::ObjectNamePart::Identifier(name)]
                if name.quote_style.is_none() && name.value.eq_ignore_ascii_case("IF"))
        }
        _ => false,
    };
    is_a_condition.then_some((kind, argument))
}

/// Classifies a scalar subquery in a projection.
///
/// Measured on MySQL 8.4.11: it answers the shape its aggregate answers on its
/// own — a `MAX` over an `INT` a `LONG` of 11, a `SUM` over a `DECIMAL(10,2)` a
/// `NEWDECIMAL` of 34 with its scale, a `COUNT` a `LONGLONG` of 21 — and is
/// nullable whatever that aggregate is, where a plain `COUNT` is NOT NULL.
///
/// Only an aggregate is taken. A subquery answering a column would answer the
/// column's own shape and a row that is not there as NULL, which is a rule of
/// its own and unmeasured here.
pub(super) fn classify_scalar_subquery(
    query: &sqlparser::ast::Query,
) -> Option<StaticSelectMetadata> {
    let sqlparser::ast::SetExpr::Select(select) = query.body.as_ref() else {
        return None;
    };
    let [item] = select.projection.as_slice() else {
        return None;
    };
    let (sqlparser::ast::SelectItem::UnnamedExpr(expr)
    | sqlparser::ast::SelectItem::ExprWithAlias { expr, .. }) = item
    else {
        return None;
    };
    let inner = match expr {
        Expr::Function(function) if is_count_call(function) => StaticSelectMetadata::Count,
        Expr::Function(function) => {
            let (kind, column) = column_aggregate_argument(function)?;
            StaticSelectMetadata::ColumnAggregate {
                column_name: column.value.clone(),
                kind,
            }
        }
        _ => return None,
    };
    Some(StaticSelectMetadata::ScalarSubquery(Box::new(inner)))
}

/// Classifies the window calls whose MySQL result shape has been measured.
///
/// Measured on MySQL 8.4.11, whatever the window is over:
///
/// * `ROW_NUMBER()`, `RANK()`, `DENSE_RANK()` and `NTILE(n)` answer a
///   `LONGLONG` of length 21 and no decimals, carrying the NOT NULL, unsigned
///   and numeric flags.
/// * `PERCENT_RANK()` and `CUME_DIST()` answer a `DOUBLE` of length 23 with the
///   not-fixed decimals value, carrying the NOT NULL and numeric flags.
/// * `LAG(col)`, `LEAD(col)`, `FIRST_VALUE(col)`, `LAST_VALUE(col)` and
///   `NTH_VALUE(col, n)` answer the column's own shape, widened to
///   `LONGLONG` where it is an integer, and are always nullable because the row
///   they reach for may not be there. They carry the numeric flag and, unlike
///   `ABS`, not the binary one.
///
/// The window has to be written out — a named one is a spelling of its own —
/// and every `PARTITION BY` and `ORDER BY` term has to be a plain column, since
/// that is what the checked ordering path can answer for. A frame clause is
/// refused: it changes nothing for any of these, and taking one silently would
/// mean taking it for the functions where it does change something.
pub(super) fn classify_window_call(
    function: &sqlparser::ast::Function,
) -> Option<StaticSelectMetadata> {
    let [sqlparser::ast::ObjectNamePart::Identifier(name)] = function.name.0.as_slice() else {
        return None;
    };
    if name.quote_style.is_some()
        || function.filter.is_some()
        || function.null_treatment.is_some()
        || !function.within_group.is_empty()
        || function.uses_odbc_syntax
        || function.parameters != sqlparser::ast::FunctionArguments::None
    {
        return None;
    }
    let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
        return None;
    };
    if arguments.duplicate_treatment.is_some() || !arguments.clauses.is_empty() {
        return None;
    }
    let named = |candidates: &[&str]| {
        candidates
            .iter()
            .any(|candidate| name.value.eq_ignore_ascii_case(candidate))
    };
    checked_window_spec(
        function.over.as_ref()?,
        window_answers_over_the_whole_set(&name.value),
    )?;
    let fixed = |function| StaticSelectMetadata::ScalarCall {
        function,
        columns: Vec::new(),
        literal_characters: 0,
        not_null: true,
    };
    if named(&["ROW_NUMBER", "RANK", "DENSE_RANK"]) {
        return arguments
            .args
            .is_empty()
            .then(|| fixed(ScalarFunction::RanksRows));
    }
    if named(&["PERCENT_RANK", "CUME_DIST"]) {
        return arguments
            .args
            .is_empty()
            .then(|| fixed(ScalarFunction::RanksFraction));
    }
    // Measured: `NTILE(0)` answers 1210, so the count has to be one or more.
    if named(&["NTILE"]) {
        let [argument] = arguments.args.as_slice() else {
            return None;
        };
        let sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Value(value),
        )) = argument
        else {
            return None;
        };
        let Value::Number(digits, false) = &value.value else {
            return None;
        };
        return (digits.parse::<u64>().ok()? >= 1).then(|| fixed(ScalarFunction::RanksRows));
    }
    // Measured: a windowed aggregate answers the shape its plain form does,
    // apart from the binary flag, which it does not carry, and MIN and MAX,
    // which widen an INT to LONGLONG where the plain form leaves it LONG.
    if named(&["COUNT"]) {
        return matches!(
            arguments.args.as_slice(),
            [sqlparser::ast::FunctionArg::Unnamed(
                sqlparser::ast::FunctionArgExpr::Wildcard
                    | sqlparser::ast::FunctionArgExpr::Expr(Expr::Identifier(_)),
            )]
        )
        .then_some(StaticSelectMetadata::WindowCount);
    }
    if named(&["SUM", "AVG", "MIN", "MAX"]) {
        let kind = if named(&["SUM"]) {
            ColumnAggregateKind::Sum
        } else if named(&["AVG"]) {
            ColumnAggregateKind::Avg
        } else {
            ColumnAggregateKind::MinMax
        };
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Identifier(column),
        ))] = arguments.args.as_slice()
        else {
            return None;
        };
        return Some(StaticSelectMetadata::WindowAggregate {
            column_name: column.value.clone(),
            kind,
        });
    }
    // `LAG(col, 2)` reads the row two back, and `LAG(col, 1, 0)` answers 0
    // where there is no such row. Measured on MySQL 8.4.11: an offset leaves
    // the shape alone, and 0 reads the row itself; a default widens the answer
    // to its own width — `LAG(n, 1, 99999999999)` over an INT reports 12 and
    // `LAG(s, 1, 'a much longer default')` over a VARCHAR(10) 84 — and a
    // default of NULL is no default at all. A default of another kind than the
    // column's changes the type, `LAG(n, 1, 1.5)` answering a NEWDECIMAL, so
    // a whole number is taken over a whole-number column and a word over a
    // `VARCHAR`, which the frontend checks.
    if named(&["LAG", "LEAD"]) && matches!(arguments.args.len(), 2 | 3) {
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Identifier(column),
        )), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Value(offset),
        )), default @ ..] = arguments.args.as_slice()
        else {
            return None;
        };
        let Value::Number(offset, false) = &offset.value else {
            return None;
        };
        offset.parse::<u64>().ok()?;
        let (function, literal_characters) = match default {
            [] => (ScalarFunction::ShiftsRow, 0),
            [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
                default,
            ))] => match default {
                Expr::Value(value) if matches!(value.value, Value::Null) => {
                    (ScalarFunction::ShiftsRow, 0)
                }
                Expr::Value(value)
                    if matches!(
                        &value.value,
                        Value::SingleQuotedString(_) | Value::DoubleQuotedString(_)
                    ) =>
                {
                    let (Value::SingleQuotedString(word) | Value::DoubleQuotedString(word)) =
                        &value.value
                    else {
                        unreachable!("the guard requires a written word");
                    };
                    (
                        ScalarFunction::ShiftsRowOrWord,
                        u32::try_from(word.chars().count()).ok()?,
                    )
                }
                _ => match classify_static_select_expr(default)? {
                    StaticSelectMetadata::Integer { digit_count, .. } => (
                        ScalarFunction::ShiftsRowOrNumber,
                        digit_count.checked_add(1)?,
                    ),
                    _ => return None,
                },
            },
            _ => return None,
        };
        return Some(StaticSelectMetadata::ScalarCall {
            function,
            columns: vec![column.value.clone()],
            literal_characters,
            not_null: false,
        });
    }
    // `LAG` and `LEAD` read another row of the window and `FIRST_VALUE`,
    // `LAST_VALUE` and `NTH_VALUE` read one of its ends; all five answer the
    // column's own shape and may find no row at all. `NTH_VALUE(col, 0)`
    // answers 1210, so its count is one or more.
    let takes_a_count = named(&["NTH_VALUE"]);
    if named(&["LAG", "LEAD", "FIRST_VALUE", "LAST_VALUE"]) || takes_a_count {
        let (column, count) = match arguments.args.as_slice() {
            [column] if !takes_a_count => (column, None),
            [column, count] if takes_a_count => (column, Some(count)),
            _ => return None,
        };
        let sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Identifier(column),
        )) = column
        else {
            return None;
        };
        if let Some(count) = count {
            let sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
                Expr::Value(value),
            )) = count
            else {
                return None;
            };
            let Value::Number(digits, false) = &value.value else {
                return None;
            };
            if digits.parse::<u64>().ok()? < 1 {
                return None;
            }
        }
        return Some(StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::ShiftsRow,
            columns: vec![column.value.clone()],
            literal_characters: 0,
            not_null: false,
        });
    }
    None
}

/// Reports whether a windowed call answers the same value for every row of an
/// empty window.
///
/// `COUNT(*) OVER ()` is how a statement asks for the count of the whole
/// result beside each row, which is what a paged query wants. Measured on
/// MySQL 8.4.11 over three rows, it answers 3 on all three, and `SUM`, `AVG`,
/// `MIN` and `MAX` each answer the whole set's value the same way — none of
/// them depends on which order the rows came in.
///
/// A ranking does depend on it: `ROW_NUMBER() OVER ()` numbers the rows in
/// whatever order they were read, and the two need not read them alike.
pub(crate) fn window_answers_over_the_whole_set(name: &str) -> bool {
    ["COUNT", "SUM", "AVG", "MIN", "MAX"]
        .iter()
        .any(|aggregate| name.eq_ignore_ascii_case(aggregate))
}

/// Reads the window a ranking call is over, if it is one this takes.
pub(crate) fn checked_window_spec(
    over: &sqlparser::ast::WindowType,
    may_be_empty: bool,
) -> Option<&sqlparser::ast::WindowSpec> {
    let sqlparser::ast::WindowType::WindowSpec(spec) = over else {
        return None;
    };
    if spec.window_name.is_some() {
        return None;
    }
    if let Some(frame) = spec.window_frame.as_ref() {
        checked_window_frame(frame)?;
    }
    if !spec
        .partition_by
        .iter()
        .all(|expr| matches!(expr, Expr::Identifier(_)))
    {
        return None;
    }
    for term in &spec.order_by {
        if !matches!(term.expr, Expr::Identifier(_))
            || term.options.nulls_first.is_some()
            || term.with_fill.is_some()
        {
            return None;
        }
    }
    if spec.partition_by.is_empty() && spec.order_by.is_empty() && !may_be_empty {
        return None;
    }
    Some(spec)
}

/// Reads the frame a window is over, if it is one this takes.
///
/// `ROWS` and `RANGE` are taken and `GROUPS` is not: measured, MySQL 8.4.11
/// answers 1235 for it. A bound has to be `CURRENT ROW`, an unbounded end, or a
/// non-negative whole number of rows or of the ordering column's own units.
pub(crate) fn checked_window_frame(
    frame: &sqlparser::ast::WindowFrame,
) -> Option<&sqlparser::ast::WindowFrame> {
    use sqlparser::ast::WindowFrameUnits;
    if matches!(frame.units, WindowFrameUnits::Groups) {
        return None;
    }
    checked_window_frame_bound(&frame.start_bound)?;
    if let Some(end_bound) = frame.end_bound.as_ref() {
        checked_window_frame_bound(end_bound)?;
    }
    Some(frame)
}

fn checked_window_frame_bound(bound: &sqlparser::ast::WindowFrameBound) -> Option<()> {
    use sqlparser::ast::WindowFrameBound;
    let offset = match bound {
        WindowFrameBound::CurrentRow => return Some(()),
        WindowFrameBound::Preceding(offset) | WindowFrameBound::Following(offset) => offset,
    };
    let Some(offset) = offset else {
        return Some(());
    };
    let Expr::Value(value) = offset.as_ref() else {
        return None;
    };
    let Value::Number(digits, false) = &value.value else {
        return None;
    };
    digits.parse::<u64>().ok().map(|_| ())
}

/// Classifies `TRIM(...)`, which answers its column's own shape.
///
/// Measured on MySQL 8.4.11: every form over a `VARCHAR(8)` answers a
/// `VAR_STRING` of length 8 with no flags, including over a `NOT NULL` column
/// — the same shape `LOWER` answers.
///
/// What to trim has to be a single character. MySQL removes whole copies of
/// what it was given, where the engine removes any of the characters in it, and
/// the two only agree when there is one character to remove: measured,
/// `TRIM(LEADING 'ax' FROM 'xaxabxa')` answers the string unchanged where the
/// engine would strip the leading `xaxa`.
///
/// `TRIM(v)` and `TRIM([side] 'x' FROM v)` are the forms taken. MySQL's bare
/// `TRIM(LEADING FROM v)` is not, because the parser library does not read it.
pub(super) fn classify_trim(
    trim_where: Option<TrimWhereField>,
    trim_what: Option<&Expr>,
    expr: &Expr,
    trim_characters: Option<&[Expr]>,
) -> Option<StaticSelectMetadata> {
    // The bracketed list is another dialect's spelling and is not MySQL's.
    if trim_characters.is_some() {
        return None;
    }
    let Expr::Identifier(column) = expr else {
        return None;
    };
    match trim_what {
        Some(trim_what) => {
            trim_single_character(trim_what)?;
        }
        // MySQL spells a bare side `TRIM(LEADING FROM v)`, which the parser
        // library does not read; what it reads instead, `TRIM(LEADING v)`, is
        // not MySQL, so a side with nothing to trim is refused.
        None if trim_where.is_some() => return None,
        None => {}
    }
    Some(StaticSelectMetadata::ScalarCall {
        function: ScalarFunction::KeepsTextShape,
        columns: vec![column.value.clone()],
        literal_characters: 0,
        not_null: false,
    })
}

/// Reads the one character a `TRIM` was asked to remove.
pub(crate) fn trim_single_character(trim_what: &Expr) -> Option<&str> {
    let Expr::Value(value) = trim_what else {
        return None;
    };
    let (Value::SingleQuotedString(text) | Value::DoubleQuotedString(text)) = &value.value else {
        return None;
    };
    (text.chars().count() == 1).then_some(text.as_str())
}

/// Classifies `SUBSTRING(...)`.
///
/// Measured on MySQL 8.4.11: `SUBSTRING(v, 1, 2)` over a `VARCHAR(8)` answers
/// a `VAR_STRING` of length 8 — the count asked for, four bytes a character —
/// like `LEFT` and `RIGHT` already do.
/// Classifies `SUBSTRING(col, from [, count])` and its `SUBSTR` spelling.
///
/// Both places have to be written whole numbers, because the answer's width is
/// worked out from them. Each is held to a signed 32-bit number: MySQL reads a
/// place past that as naming nothing, a rule this does not repeat.
fn classify_substring(
    expr: &Expr,
    substring_from: Option<&Expr>,
    substring_for: Option<&Expr>,
) -> Option<StaticSelectMetadata> {
    let Expr::Identifier(column) = expr else {
        return None;
    };
    let from = i32::try_from(crate::translate::direct_signed_integer(substring_from?)?).ok()?;
    let count = match substring_for {
        Some(count) => Some(i32::try_from(crate::translate::direct_signed_integer(count)?).ok()?),
        None => None,
    };
    Some(StaticSelectMetadata::ScalarCall {
        function: ScalarFunction::TakesASubstring { from, count },
        columns: vec![column.value.clone()],
        literal_characters: 0,
        not_null: false,
    })
}

fn classify_floor_ceil(
    expr: &Expr,
    field: &sqlparser::ast::CeilFloorKind,
) -> Option<StaticSelectMetadata> {
    if !matches!(
        field,
        sqlparser::ast::CeilFloorKind::DateTimeField(sqlparser::ast::DateTimeField::NoDateTime)
    ) {
        return None;
    }
    let Expr::Identifier(column) = expr else {
        return None;
    };
    Some(StaticSelectMetadata::ScalarCall {
        function: ScalarFunction::RoundsToWhole,
        columns: vec![column.value.clone()],
        literal_characters: 0,
        not_null: false,
    })
}

fn is_scalar_literal(expr: &Expr) -> bool {
    match expr {
        Expr::Value(value) => matches!(
            &value.value,
            Value::SingleQuotedString(_)
                | Value::DoubleQuotedString(_)
                | Value::Number(_, _)
                | Value::Null
                | Value::Boolean(_)
        ),
        Expr::UnaryOp {
            op: UnaryOperator::Minus | UnaryOperator::Plus,
            expr,
        } => {
            matches!(expr.as_ref(), Expr::Value(value) if matches!(&value.value, Value::Number(_, _)))
        }
        _ => false,
    }
}

/// Classifies the scalar calls whose MySQL result shape has been measured.
///
/// Each reads one plain column, or none: an expression argument would need a
/// length this cannot work out.
pub(super) fn scalar_call(function: &sqlparser::ast::Function) -> Option<StaticSelectMetadata> {
    let [sqlparser::ast::ObjectNamePart::Identifier(name)] = function.name.0.as_slice() else {
        return None;
    };
    if name.quote_style.is_some() || !is_plain_aggregate(function) {
        return None;
    }
    let named = |candidates: &[&str]| {
        candidates
            .iter()
            .any(|candidate| name.value.eq_ignore_ascii_case(candidate))
    };
    // MySQL spells the two clock readings with and without their parentheses,
    // and sqlparser gives the bare form no argument list at all rather than an
    // empty one, so both shapes count as no arguments here.
    let takes_nothing = match &function.args {
        sqlparser::ast::FunctionArguments::None => true,
        sqlparser::ast::FunctionArguments::List(arguments) => arguments.args.is_empty(),
        sqlparser::ast::FunctionArguments::Subquery(_) => false,
    };
    // Measured on MySQL 8.4.11: `LOCALTIME` and `LOCALTIMESTAMP` are two more
    // spellings of `NOW()`, and each of them, like `CURTIME`, takes a count
    // of places for the fraction of a second, from 0 through 6.
    if named(&[
        "NOW",
        "CURRENT_TIMESTAMP",
        "UTC_TIMESTAMP",
        "SYSDATE",
        "LOCALTIME",
        "LOCALTIMESTAMP",
    ]) {
        return clock_places(function).map(|places| StaticSelectMetadata::ScalarCall {
            function: match places {
                0 => ScalarFunction::Now,
                places => ScalarFunction::NowToAFraction { places },
            },
            columns: Vec::new(),
            literal_characters: 0,
            not_null: true,
        });
    }
    // `DATE(col)` reads the day out of a moment, which is what
    // `CAST(col AS DATE)` reads. Measured on MySQL 8.4.11, both answer a
    // nullable DATE of length 10 in the binary character set, so they are one
    // thing under two spellings.
    if named(&["DATE"]) {
        let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
            return None;
        };
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Identifier(column),
        ))] = arguments.args.as_slice()
        else {
            return None;
        };
        return Some(StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::CastsToDay,
            columns: vec![column.value.clone()],
            literal_characters: 0,
            not_null: false,
        });
    }
    // Measured on MySQL 8.4.11: `PI()` answers a DOUBLE of length 8 with 6
    // decimals reporting NOT NULL, where every other reading answers the
    // 23-and-31 shape a double carries. It is the one of these that reads
    // nothing at all.
    if named(&["PI"]) {
        return takes_nothing.then(|| StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::NamesTheCircle,
            columns: Vec::new(),
            literal_characters: 0,
            not_null: true,
        });
    }
    if named(&["CURDATE", "CURRENT_DATE", "UTC_DATE"]) {
        return takes_nothing.then(|| StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::Today,
            columns: Vec::new(),
            literal_characters: 0,
            not_null: true,
        });
    }
    if named(&["CURTIME", "CURRENT_TIME", "UTC_TIME"]) {
        return clock_places(function).map(|places| StaticSelectMetadata::ScalarCall {
            function: match places {
                0 => ScalarFunction::TimeOfDay,
                places => ScalarFunction::TimeOfDayToAFraction { places },
            },
            columns: Vec::new(),
            literal_characters: 0,
            not_null: true,
        });
    }
    // Measured on MySQL 8.4.11: `RAND()` reports NOT NULL, and `UUID()` does
    // not. A seeded `RAND(n)` is refused: the engine has no seeded random, so
    // answering one would answer a different sequence.
    if named(&["RAND"]) {
        return takes_nothing.then(|| StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::Randomises,
            columns: Vec::new(),
            literal_characters: 0,
            not_null: true,
        });
    }
    if named(&["UUID"]) {
        return takes_nothing.then(|| StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::Identifies,
            columns: Vec::new(),
            literal_characters: 0,
            not_null: false,
        });
    }
    let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
        return None;
    };
    // `COALESCE(nickname, name)` falls one column back onto another. Measured
    // on MySQL 8.4.11 it answers the rule a `CASE` over the same columns
    // answers, and is NOT NULL when any one of them is.
    if named(&["IFNULL", "COALESCE"])
        && arguments.args.len() >= 2
        && (named(&["COALESCE"]) || arguments.args.len() == 2)
        && arguments.args.iter().all(|argument| {
            matches!(
                argument,
                sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
                    Expr::Identifier(_)
                ))
            )
        })
    {
        let branches = arguments
            .args
            .iter()
            .map(|argument| match argument {
                sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
                    Expr::Identifier(column),
                )) => Branch::Column {
                    column_name: column.value.clone(),
                },
                _ => unreachable!("every argument was checked to be a column"),
            })
            .collect();
        return Some(StaticSelectMetadata::Branches {
            branches,
            may_be_null: false,
            falls_back: true,
        });
    }
    // `IFNULL(column, literal)` cannot be null, which is the whole reason a
    // client writes it, so the second argument has to be one that is not.
    if named(&["IFNULL", "COALESCE"]) {
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(defaulted)), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(fallback))] =
            arguments.args.as_slice()
        else {
            return None;
        };
        // A written word falls back onto a column of words, which is how a
        // report writes a placeholder for what a row does not carry. Measured
        // on MySQL 8.4.11, the answer is the column's own width whatever the
        // word's own is — `IFNULL(email, 'none')` and `IFNULL(email, 'x')`
        // over a `VARCHAR(80)` both report 320.
        let falls_back_on_a_word = matches!(
            fallback,
            Expr::Value(value)
                if matches!(
                    value.value,
                    Value::SingleQuotedString(_) | Value::DoubleQuotedString(_)
                )
        );
        let fallback_places = written_zero_places(fallback);
        if !falls_back_on_a_word
            && fallback_places.is_none()
            && !matches!(
                classify_static_select_expr(fallback),
                Some(StaticSelectMetadata::Integer { .. } | StaticSelectMetadata::Boolean(_))
            )
        {
            return None;
        }
        // `IFNULL(SUM(amount), 0)` is how a report asks for a total over rows
        // that may not be there. Measured on MySQL 8.4.11, it answers the
        // shape the aggregate answers on its own, so the aggregate travels
        // inside rather than a column name.
        if let Expr::Function(_) = defaulted {
            if falls_back_on_a_word {
                return None;
            }
            let inner = classify_static_select_expr(defaulted)?;
            if !matches!(
                inner,
                StaticSelectMetadata::Count | StaticSelectMetadata::ColumnAggregate { .. }
            ) {
                return None;
            }
            return Some(StaticSelectMetadata::DefaultedAggregate {
                aggregate: Box::new(inner),
                fallback_places: fallback_places.unwrap_or(0),
            });
        }
        if fallback_places.is_some() {
            return None;
        }
        let Expr::Identifier(column) = defaulted else {
            return None;
        };
        return Some(StaticSelectMetadata::ScalarCall {
            function: if falls_back_on_a_word {
                ScalarFunction::DefaultedText
            } else {
                ScalarFunction::Defaulted
            },
            columns: vec![column.value.clone()],
            literal_characters: 0,
            not_null: true,
        });
    }
    // MySQL's `IF` is the call spelling of a two-branch `CASE`, and answers
    // the same shape.
    if named(&["IF"]) {
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(_)), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            then_result,
        )), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            else_result,
        ))] = arguments.args.as_slice()
        else {
            return None;
        };
        return classify_branches(std::iter::once(then_result), Some(else_result));
    }
    // `CONCAT` is as wide as its arguments laid end to end, so every one of
    // them counts: a column contributes its own width and a string literal the
    // characters it spells.
    if named(&["CONCAT"]) {
        let mut columns = Vec::new();
        let mut literal_characters = 0u32;
        for argument in &arguments.args {
            let sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(expr)) =
                argument
            else {
                return None;
            };
            match expr {
                Expr::Identifier(column) => columns.push(column.value.clone()),
                Expr::Value(value) => match &value.value {
                    Value::SingleQuotedString(text) | Value::DoubleQuotedString(text) => {
                        literal_characters =
                            literal_characters.checked_add(text.chars().count() as u32)?;
                    }
                    _ => return None,
                },
                _ => return None,
            }
        }
        if columns.is_empty() {
            return None;
        }
        return Some(StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::Concatenates,
            columns,
            literal_characters,
            not_null: false,
        });
    }
    // `CONCAT_WS(separator, ...)` is as wide as its arguments laid end to end
    // with the separator between each two, so the separator counts once for
    // each gap.
    if named(&["CONCAT_WS"]) {
        let [separator, parts @ ..] = arguments.args.as_slice() else {
            return None;
        };
        if parts.is_empty() {
            return None;
        }
        let separator_characters = written_word(separator)?.chars().count() as u32;
        let mut columns = Vec::new();
        let mut literal_characters =
            separator_characters.checked_mul(u32::try_from(parts.len() - 1).ok()?)?;
        for part in parts {
            let sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(expr)) =
                part
            else {
                return None;
            };
            match expr {
                Expr::Identifier(column) => columns.push(column.value.clone()),
                _ => {
                    literal_characters = literal_characters
                        .checked_add(written_word(part)?.chars().count() as u32)?;
                }
            }
        }
        if columns.is_empty() {
            return None;
        }
        return Some(StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::Concatenates,
            columns,
            literal_characters,
            not_null: false,
        });
    }
    // `SUBSTRING_INDEX(col, delimiter, count)` answers part of the column. The
    // count is written, and the delimiter is written or bound.
    if named(&["SUBSTRING_INDEX"]) {
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Identifier(column),
        )), delimiter, sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(count))] =
            arguments.args.as_slice()
        else {
            return None;
        };
        let bound = matches!(
            delimiter,
            sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(Expr::Value(value)))
                if matches!(&value.value, Value::Placeholder(marker) if marker == "?")
        );
        if !bound {
            written_word(delimiter)?;
        }
        crate::translate::direct_signed_integer(count)?;
        return Some(StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::SplitsOnADelimiter,
            columns: vec![column.value.clone()],
            literal_characters: 0,
            not_null: false,
        });
    }
    // `MD5`, `SHA1` and `SHA2` answer a digest written in hexadecimal, as many
    // characters as the digest has. `SHA2` names its size, and 0 means 256;
    // measured, any other size answers NULL with a warning this does not
    // raise, so it is refused.
    if named(&["MD5", "SHA1", "SHA", "SHA2"]) {
        let (column, characters) = match arguments.args.as_slice() {
            [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
                Expr::Identifier(column),
            ))] if named(&["MD5"]) => (column, 32),
            [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
                Expr::Identifier(column),
            ))] if named(&["SHA1", "SHA"]) => (column, 40),
            [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
                Expr::Identifier(column),
            )), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(bits))]
                if named(&["SHA2"]) =>
            {
                let bits = match crate::translate::direct_signed_integer(bits)? {
                    0 => 256,
                    bits @ (224 | 256 | 384 | 512) => bits,
                    _ => return None,
                };
                (column, u32::try_from(bits / 4).ok()?)
            }
            _ => return None,
        };
        return Some(StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::Digests,
            columns: vec![column.value.clone()],
            literal_characters: characters,
            not_null: false,
        });
    }
    // `REPLACE(col, from, to)` answers the column's own text shape.
    if named(&["REPLACE"]) {
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Identifier(column),
        )), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(from)), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(to))] =
            arguments.args.as_slice()
        else {
            return None;
        };
        let (Expr::Value(from_val), Expr::Value(to_val)) = (from, to) else {
            return None;
        };
        if !matches!(
            &from_val.value,
            Value::SingleQuotedString(_) | Value::DoubleQuotedString(_)
        ) || !matches!(
            &to_val.value,
            Value::SingleQuotedString(_) | Value::DoubleQuotedString(_)
        ) {
            return None;
        }
        return Some(StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::KeepsTextShape,
            columns: vec![column.value.clone()],
            literal_characters: 0,
            not_null: false,
        });
    }
    // `LEFT` and `RIGHT` are as wide as the count they were asked for, which
    // has to be a literal for that to be knowable. `SUBSTRING` is not here:
    // sqlparser gives it its own AST shape rather than a call.
    if named(&["LEFT", "RIGHT"]) {
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Identifier(column),
        )), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(count))] =
            arguments.args.as_slice()
        else {
            return None;
        };
        let Expr::Value(value) = count else {
            return None;
        };
        let Value::Number(digits, false) = &value.value else {
            return None;
        };
        return Some(StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::TakesCharacters,
            columns: vec![column.value.clone()],
            literal_characters: digits.parse().ok()?,
            not_null: false,
        });
    }
    // `REPEAT(col, count)` answers a width proportional to the count.
    if named(&["REPEAT"]) {
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Identifier(column),
        )), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(count))] =
            arguments.args.as_slice()
        else {
            return None;
        };
        let Expr::Value(value) = count else {
            return None;
        };
        let Value::Number(digits, false) = &value.value else {
            return None;
        };
        return Some(StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::Repeats,
            columns: vec![column.value.clone()],
            literal_characters: digits.parse().ok()?,
            not_null: false,
        });
    }
    // `LPAD` and `RPAD` are as wide as the count they were asked for.
    if named(&["LPAD", "RPAD"]) {
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Identifier(column),
        )), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(count)), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(pad))] =
            arguments.args.as_slice()
        else {
            return None;
        };
        let Expr::Value(count_val) = count else {
            return None;
        };
        let Value::Number(digits, false) = &count_val.value else {
            return None;
        };
        let Expr::Value(pad_val) = pad else {
            return None;
        };
        // Measured on MySQL 8.4.11: nothing to pad with answers an empty word
        // where padding is needed, `LPAD('hi', 5, '')` being `''`, and the
        // engine answers the value unpadded.
        if !matches!(
            &pad_val.value,
            Value::SingleQuotedString(pad) | Value::DoubleQuotedString(pad) if !pad.is_empty()
        ) {
            return None;
        }
        return Some(StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::TakesCharacters,
            columns: vec![column.value.clone()],
            literal_characters: digits.parse().ok()?,
            not_null: false,
        });
    }
    // `INSTR(str, substr)` takes the column first and the substring literal second.
    if named(&["INSTR"]) {
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Identifier(column),
        )), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(substr))] =
            arguments.args.as_slice()
        else {
            return None;
        };
        let Expr::Value(value) = substr else {
            return None;
        };
        if !matches!(
            &value.value,
            Value::SingleQuotedString(_) | Value::DoubleQuotedString(_)
        ) {
            return None;
        }
        return Some(StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::Locates,
            columns: vec![column.value.clone()],
            literal_characters: 0,
            not_null: false,
        });
    }
    // `LOCATE(substr, str)` takes the substring literal first and the column second.
    // `LOCATE(needle, haystack, start)` looks from a place in the haystack
    // rather than from its front. The place has to be a literal, because what
    // it renders to depends on whether it names one.
    if named(&["LOCATE"]) && arguments.args.len() == 3 {
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(needle)), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Identifier(column),
        )), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Value(start),
        ))] = arguments.args.as_slice()
        else {
            return None;
        };
        if !matches!(
            needle,
            Expr::Value(value)
                if matches!(
                    &value.value,
                    Value::SingleQuotedString(_) | Value::DoubleQuotedString(_)
                )
        ) {
            return None;
        }
        let Value::Number(digits, false) = &start.value else {
            return None;
        };
        if digits.parse::<u32>().is_err() {
            return None;
        }
        return Some(StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::Locates,
            columns: vec![column.value.clone()],
            literal_characters: 0,
            not_null: false,
        });
    }
    if named(&["LOCATE"]) {
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(substr)), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Identifier(column),
        ))] = arguments.args.as_slice()
        else {
            return None;
        };
        let Expr::Value(value) = substr else {
            return None;
        };
        if !matches!(
            &value.value,
            Value::SingleQuotedString(_) | Value::DoubleQuotedString(_)
        ) {
            return None;
        }
        return Some(StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::Locates,
            columns: vec![column.value.clone()],
            literal_characters: 0,
            not_null: false,
        });
    }
    // `POW(col, exp)` and `POWER(col, exp)` answer DOUBLE.
    if named(&["POW", "POWER"]) {
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Identifier(column),
        )), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(exp))] =
            arguments.args.as_slice()
        else {
            return None;
        };
        let Expr::Value(value) = exp else {
            return None;
        };
        if !matches!(&value.value, Value::Number(_, _)) {
            return None;
        }
        return Some(StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::Approximates,
            columns: vec![column.value.clone()],
            literal_characters: 0,
            not_null: false,
        });
    }
    // `UNIX_TIMESTAMP()` counts the seconds from the epoch to now, and with a
    // moment to the moment it is given. `FROM_UNIXTIME(n)` reads one back.
    // Nothing here converts between zones, which is the same as running in
    // UTC, and UTC is the only zone this session takes.
    // `FROM_UNIXTIME(n, 'fmt')` writes the moment out the way `DATE_FORMAT`
    // does, and measured on MySQL 8.4.11 reserves the width `DATE_FORMAT`
    // reserves for the same format. The count is a whole-number column or a
    // written whole number: a fraction is carried into the moment by rules of
    // MySQL's own.
    if named(&["FROM_UNIXTIME"]) && arguments.args.len() == 2 {
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(seconds)), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Value(format),
        ))] = arguments.args.as_slice()
        else {
            return None;
        };
        let (Value::SingleQuotedString(format) | Value::DoubleQuotedString(format)) = &format.value
        else {
            return None;
        };
        let columns = match seconds {
            Expr::Identifier(column) => vec![column.value.clone()],
            _ if crate::translate::direct_signed_integer(seconds).is_some() => Vec::new(),
            _ => return None,
        };
        return Some(StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::WritesAnEpochMoment,
            columns,
            literal_characters: crate::format_width(format),
            not_null: false,
        });
    }
    if named(&["UNIX_TIMESTAMP", "FROM_UNIXTIME"]) {
        let counts = named(&["UNIX_TIMESTAMP"]);
        if arguments.args.len() > 1 || (!counts && arguments.args.is_empty()) {
            return None;
        }
        let mut columns = Vec::new();
        for argument in &arguments.args {
            json_argument_column(argument, &mut columns)?;
        }
        return Some(StaticSelectMetadata::ScalarCall {
            function: if counts {
                ScalarFunction::CountsEpochSeconds
            } else {
                ScalarFunction::ReadsFromEpoch
            },
            columns,
            literal_characters: 0,
            not_null: false,
        });
    }
    // `FORMAT(col, 2)` writes the column's number grouped in threes. The count
    // is written out rather than read from a column, the way `ROUND`'s is.
    if named(&["FORMAT", "TRUNCATE"]) {
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Identifier(column),
        )), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(count))] =
            arguments.args.as_slice()
        else {
            return None;
        };
        // Measured: a negative count answers no fraction at all, so it is a
        // count this takes rather than one it refuses.
        let (counted, negative) = match count {
            Expr::UnaryOp {
                op: op @ (UnaryOperator::Minus | UnaryOperator::Plus),
                expr: inner,
            } => (inner.as_ref(), *op == UnaryOperator::Minus),
            other => (other, false),
        };
        let Expr::Value(value) = counted else {
            return None;
        };
        let Value::Number(counted, _) = &value.value else {
            return None;
        };
        // `TRUNCATE` over a DECIMAL answers a DECIMAL whose scale is the count
        // it was asked for, held to the column's own, so the count travels
        // with the call. A count at or below zero leaves no fraction, which is
        // the same as a count of zero.
        let counted = match negative {
            true => 0,
            false => counted.parse::<u32>().unwrap_or(0),
        };
        return Some(StaticSelectMetadata::ScalarCall {
            function: if named(&["FORMAT"]) {
                ScalarFunction::GroupsDigits
            } else {
                ScalarFunction::CutsDigits
            },
            columns: vec![column.value.clone()],
            literal_characters: counted,
            not_null: false,
        });
    }
    // `ROUND(col [, places])` rounds to a written number of places; with none
    // it rounds to a whole number, which is the same as naming 0.
    if named(&["ROUND"]) {
        if let Some(rounded) = rounded_aggregate(arguments.args.as_slice()) {
            return Some(rounded);
        }
        let (column, places) = match arguments.args.as_slice() {
            [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
                Expr::Identifier(column),
            ))] => (column, 0),
            [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
                Expr::Identifier(column),
            )), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(places))] => {
                (
                    column,
                    i32::try_from(crate::translate::direct_signed_integer(places)?).ok()?,
                )
            }
            _ => return None,
        };
        return Some(StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::RoundsToPlaces { places },
            columns: vec![column.value.clone()],
            literal_characters: 0,
            not_null: false,
        });
    }
    // `MOD(col, divisor)` answers the column's own numeric shape.
    if named(&["MOD"]) {
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Identifier(column),
        )), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(divisor))] =
            arguments.args.as_slice()
        else {
            return None;
        };
        let Expr::Value(value) = divisor else {
            return None;
        };
        if !matches!(&value.value, Value::Number(_, _)) {
            return None;
        }
        return Some(StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::Modulo,
            columns: vec![column.value.clone()],
            literal_characters: 0,
            not_null: false,
        });
    }
    // `NULLIF(col, literal)` and `NULLIF(col, col)` both answer the first
    // argument's shape and are always nullable — measured on MySQL 8.4.11, a
    // `SMALLINT` first and a `BIGINT` second answers the `SMALLINT`'s shape and
    // the other way round answers the `BIGINT`'s, so it is the first and not
    // the wider that decides.
    if named(&["NULLIF"]) {
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Identifier(column),
        )), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(compared))] =
            arguments.args.as_slice()
        else {
            return None;
        };
        // The second column travels with the first: the shape is the first's,
        // and whether the two can be compared the way MySQL compares them is
        // the frontend's to check, which needs both names.
        let mut columns = vec![column.value.clone()];
        match compared {
            Expr::Identifier(other) => columns.push(other.value.clone()),
            _ if is_scalar_literal(compared) => {}
            _ => return None,
        }
        return Some(StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::NullsOnMatch,
            columns,
            literal_characters: 0,
            not_null: false,
        });
    }
    // `GREATEST` and `LEAST` with 2 or more arguments.
    if named(&["GREATEST", "LEAST"]) {
        if arguments.args.len() < 2 {
            return None;
        }
        let mut columns = Vec::new();
        let mut max_literal_chars: u32 = 0;
        let mut has_text = false;
        let mut has_numeric = false;

        for arg in &arguments.args {
            let sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(expr)) =
                arg
            else {
                return None;
            };
            match expr {
                Expr::Identifier(column) => {
                    columns.push(column.value.clone());
                }
                Expr::Value(value) => match &value.value {
                    Value::SingleQuotedString(text) | Value::DoubleQuotedString(text) => {
                        has_text = true;
                        max_literal_chars = max_literal_chars.max(text.chars().count() as u32);
                    }
                    Value::Number(_, _) => {
                        has_numeric = true;
                    }
                    _ => return None,
                },
                Expr::UnaryOp {
                    op: UnaryOperator::Minus | UnaryOperator::Plus,
                    expr: inner,
                } => match inner.as_ref() {
                    Expr::Value(value) if matches!(&value.value, Value::Number(_, _)) => {
                        has_numeric = true;
                    }
                    _ => return None,
                },
                _ => return None,
            }
        }
        if has_text && has_numeric {
            return None;
        }
        if columns.is_empty() {
            return None;
        }
        return Some(StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::Widest,
            columns,
            literal_characters: max_literal_chars,
            not_null: false,
        });
    }
    // `JSON_ARRAY(...)` and `JSON_OBJECT(k, v, ...)` build a document out of
    // what they are given. Each argument is a column or a plain literal; a
    // nested call is not read here, and `JSON_OBJECT` takes its arguments in
    // pairs. A boolean literal is refused: MySQL writes `true` where the
    // engine has only the number one to write.
    if named(&["JSON_ARRAY", "JSON_OBJECT"]) {
        if named(&["JSON_OBJECT"]) && arguments.args.len() % 2 != 0 {
            return None;
        }
        let mut columns = Vec::new();
        for argument in &arguments.args {
            json_argument_column(argument, &mut columns)?;
        }
        return Some(StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::BuildsJson,
            columns,
            literal_characters: 0,
            not_null: false,
        });
    }
    // `JSON_ARRAYAGG(JSON_OBJECT('id', id, 'name', name))` is how an API
    // answer nests the rows it read into one document. The columns are the
    // ones the document is built from, held to what the builder takes.
    if named(&["JSON_ARRAYAGG"]) {
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Function(built),
        ))] = arguments.args.as_slice()
        else {
            return None;
        };
        let Some(StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::BuildsJson,
            columns,
            ..
        }) = scalar_call(built)
        else {
            return None;
        };
        return Some(StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::CollectsBuiltJson,
            columns,
            literal_characters: 0,
            not_null: false,
        });
    }
    // `JSON_SEARCH(doc, 'one', pattern)` answers the paths to the strings the
    // pattern matches. A fourth argument names the escape character; the fifth
    // and beyond name paths to search inside, which are not read here.
    if named(&["JSON_SEARCH"]) {
        let [document, keyword, pattern, escape @ ..] = arguments.args.as_slice() else {
            return None;
        };
        if escape.len() > 1 {
            return None;
        }
        let mut columns = Vec::new();
        json_argument_column(document, &mut columns)?;
        let keyword = written_word(keyword)?;
        if !keyword.eq_ignore_ascii_case("one") && !keyword.eq_ignore_ascii_case("all") {
            return None;
        }
        json_argument_column(pattern, &mut columns)?;
        for escape in escape {
            json_argument_column(escape, &mut columns)?;
        }
        return Some(StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::ChangesJson,
            columns,
            literal_characters: 0,
            not_null: false,
        });
    }
    // `JSON_OVERLAPS(a, b)` answers whether the two share anything, and the
    // merges answer a document of their own. Each takes documents alone.
    if named(&[
        "JSON_OVERLAPS",
        "JSON_MERGE_PATCH",
        "JSON_MERGE_PRESERVE",
        "JSON_MERGE",
    ]) {
        let shares = named(&["JSON_OVERLAPS"]);
        if arguments.args.len() < 2 || (shares && arguments.args.len() != 2) {
            return None;
        }
        let mut columns = Vec::new();
        for argument in &arguments.args {
            json_argument_column(argument, &mut columns)?;
        }
        return Some(StaticSelectMetadata::ScalarCall {
            function: if shares {
                ScalarFunction::SharesJson
            } else {
                ScalarFunction::ChangesJson
            },
            columns,
            literal_characters: 0,
            not_null: false,
        });
    }
    // `JSON_CONTAINS_PATH(doc, 'one', '$.a', ...)` answers whether the paths
    // are there, one of them or all of them. Measured on MySQL 8.4.11: a
    // member holding the JSON null counts as being there, and the keyword is
    // read without regard to case.
    if named(&["JSON_CONTAINS_PATH"]) {
        let [document, keyword, paths @ ..] = arguments.args.as_slice() else {
            return None;
        };
        if paths.is_empty() {
            return None;
        }
        let mut columns = Vec::new();
        json_argument_column(document, &mut columns)?;
        let keyword = written_word(keyword)?;
        if !keyword.eq_ignore_ascii_case("one") && !keyword.eq_ignore_ascii_case("all") {
            return None;
        }
        for path in paths {
            if !names_a_plain_json_path(written_word(path)?) {
                return None;
            }
        }
        return Some(StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::SearchesJson,
            columns,
            literal_characters: 0,
            not_null: false,
        });
    }
    // `JSON_CONTAINS(target, candidate)` answers whether the target holds the
    // candidate, and a third argument names the part of the target to look in.
    if named(&["JSON_CONTAINS"]) {
        let (document, candidate, path) = match arguments.args.as_slice() {
            [document, candidate] => (document, candidate, None),
            [document, candidate, path] => (document, candidate, Some(path)),
            _ => return None,
        };
        let mut columns = Vec::new();
        json_argument_column(document, &mut columns)?;
        json_argument_column(candidate, &mut columns)?;
        if let Some(path) = path {
            if !names_a_plain_json_path(written_word(path)?) {
                return None;
            }
        }
        return Some(StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::SearchesJson,
            columns,
            literal_characters: 0,
            not_null: false,
        });
    }
    // `JSON_SET(doc, '$.a', v)` and its three relatives answer the document
    // with one member changed. Only a path naming one member of the top-level
    // object is taken — `$.a`, not `$.a.b` or `$.a[0]`. Measured on MySQL
    // 8.4.11, the two disagree about a path that names something not there to
    // name: MySQL leaves `JSON_SET('{}', '$.x.y', 1)` alone where the engine
    // builds the missing parent, and MySQL appends `JSON_SET('[1,2]',
    // '$[5]', 9)` where the engine leaves it. A one-step path cannot reach
    // either.
    if named(&["JSON_SET", "JSON_INSERT", "JSON_REPLACE", "JSON_REMOVE"]) {
        let removes = named(&["JSON_REMOVE"]);
        let [document, rest @ ..] = arguments.args.as_slice() else {
            return None;
        };
        if rest.is_empty() || (!removes && rest.len() % 2 != 0) {
            return None;
        }
        let mut columns = Vec::new();
        json_argument_column(document, &mut columns)?;
        for (position, argument) in rest.iter().enumerate() {
            // A remove takes paths alone; the others take a path and a value
            // in turn.
            if removes || position % 2 == 0 {
                let sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
                    Expr::Value(value),
                )) = argument
                else {
                    return None;
                };
                let (Value::SingleQuotedString(path) | Value::DoubleQuotedString(path)) =
                    &value.value
                else {
                    return None;
                };
                if !names_one_member(path) {
                    return None;
                }
                continue;
            }
            // Laravel writes every JSON update as `json_set(doc, '$."a"', ?)`.
            if matches!(argument, sqlparser::ast::FunctionArg::Unnamed(
                sqlparser::ast::FunctionArgExpr::Expr(value),
            ) if crate::translate::is_a_bare_placeholder(value))
            {
                continue;
            }
            json_argument_column(argument, &mut columns)?;
        }
        return Some(StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::ChangesJson,
            columns,
            literal_characters: 0,
            not_null: false,
        });
    }
    // `DATE_ADD(a, INTERVAL 1 DAY)` is an ordinary call whose second argument
    // is sqlparser's own interval node. Measured on MySQL 8.4.11: over a DATE
    // an interval of whole days, months or years answers a DATE, and any
    // other interval — or any DATETIME column — answers a DATETIME.
    if named(&["DATE_ADD", "DATE_SUB"]) {
        // The thing to shift is a column or a reading of the moment. A reading
        // carries its own kind, so the answer is known without reading any
        // column. Measured on MySQL 8.4.11: over `NOW()` any interval answers a
        // DATETIME, over `CURDATE()` an interval of whole days answers a DATE
        // and one carrying a time answers a DATETIME, and every one of them is
        // nullable where the reading itself is not.
        if let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            shifted,
        )), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Interval(interval),
        ))] = arguments.args.as_slice()
        {
            if let Some(now) = shifted_moment(shifted) {
                let whole_days = checked_interval_unit(interval)?.1;
                checked_interval_count(interval)?;
                return Some(StaticSelectMetadata::ScalarCall {
                    function: if whole_days && now == CheckedComparisonNow::Day {
                        ScalarFunction::ShiftsTheDay
                    } else {
                        ScalarFunction::ShiftsTheMoment
                    },
                    columns: Vec::new(),
                    literal_characters: 0,
                    not_null: false,
                });
            }
            // A moment written out as a word is shifted too, and measured,
            // MySQL answers the shift as a word of its own whatever the
            // interval named.
            if let Expr::Value(value) = shifted {
                let (Value::SingleQuotedString(written) | Value::DoubleQuotedString(written)) =
                    &value.value
                else {
                    return None;
                };
                crate::temporal_value::written_moment_to_shift(written)?;
                checked_interval_unit(interval)?;
                checked_interval_count(interval)?;
                return Some(StaticSelectMetadata::ScalarCall {
                    function: ScalarFunction::ShiftsAWrittenMoment,
                    columns: Vec::new(),
                    literal_characters: 0,
                    not_null: false,
                });
            }
        }
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Identifier(column),
        )), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Interval(interval),
        ))] = arguments.args.as_slice()
        else {
            return None;
        };
        let whole_days = checked_interval_unit(interval)?.1;
        // A shift counts a written number: a count worked out from a row would
        // have to be multiplied for a week or a quarter, which only a written
        // one can be.
        checked_interval_count(interval)?;
        return Some(StaticSelectMetadata::ScalarCall {
            function: if whole_days {
                ScalarFunction::ShiftsByWholeDays
            } else {
                ScalarFunction::ShiftsByTime
            },
            columns: vec![column.value.clone()],
            literal_characters: 0,
            not_null: false,
        });
    }
    // `JSON_TYPE`, `JSON_LENGTH` and `JSON_KEYS` read a whole column or one
    // path out of it, so each takes a reading rather than a plain column.
    for (names, function) in [
        (["JSON_TYPE"], ScalarFunction::NamesAJsonKind),
        (["JSON_LENGTH"], ScalarFunction::CountsJsonMembers),
        (["JSON_KEYS"], ScalarFunction::ListsJsonKeys),
    ] {
        if !named(&names) {
            continue;
        }
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(read))] =
            arguments.args.as_slice()
        else {
            return None;
        };
        let column = json_reading_column(read)?;
        return Some(StaticSelectMetadata::ScalarCall {
            function,
            columns: vec![column],
            literal_characters: 0,
            not_null: false,
        });
    }
    // `JSON_EXTRACT(col, '$.path')` and the `JSON_UNQUOTE` around it. Only one
    // path is read: MySQL takes several and answers an array of what they
    // found, which is a different shape.
    if named(&["JSON_EXTRACT"]) {
        return json_path_call(arguments, ScalarFunction::ReadsAJsonValue);
    }
    if named(&["JSON_UNQUOTE"]) {
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Function(inner),
        ))] = arguments.args.as_slice()
        else {
            return None;
        };
        let [sqlparser::ast::ObjectNamePart::Identifier(inner_name)] = inner.name.0.as_slice()
        else {
            return None;
        };
        if inner_name.quote_style.is_some()
            || !inner_name.value.eq_ignore_ascii_case("JSON_EXTRACT")
        {
            return None;
        }
        let sqlparser::ast::FunctionArguments::List(inner_arguments) = &inner.args else {
            return None;
        };
        return json_path_call(inner_arguments, ScalarFunction::ReadsJsonText);
    }
    // `STR_TO_DATE(col, 'fmt')`. The format has to be a literal, because what
    // it names is what says whether the answer is a day, a clock reading or a
    // moment.
    if named(&["STR_TO_DATE"]) {
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(read)), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Value(format),
        ))] = arguments.args.as_slice()
        else {
            return None;
        };
        // The text to read is a column as often as not, but a word written out
        // is text too, and MySQL reads it the same way.
        let columns = match read {
            Expr::Identifier(column) => vec![column.value.clone()],
            Expr::Value(value)
                if matches!(
                    &value.value,
                    Value::SingleQuotedString(_) | Value::DoubleQuotedString(_)
                ) =>
            {
                Vec::new()
            }
            _ => return None,
        };
        let (Value::SingleQuotedString(format) | Value::DoubleQuotedString(format)) = &format.value
        else {
            return None;
        };
        let function = match crate::format_reads(format)? {
            crate::FormatShape::Day => ScalarFunction::ReadsADay,
            crate::FormatShape::Clock => ScalarFunction::ReadsAClock,
            crate::FormatShape::Moment => ScalarFunction::ReadsAMoment,
        };
        return Some(StaticSelectMetadata::ScalarCall {
            function,
            columns,
            literal_characters: 0,
            not_null: false,
        });
    }
    // `DATE_FORMAT(col, 'fmt')`. The format has to be a literal, because the
    // answer's width is worked out from it.
    if named(&["DATE_FORMAT"]) {
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(written)), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Value(format),
        ))] = arguments.args.as_slice()
        else {
            return None;
        };
        // `DATE_FORMAT(NOW(), '%Y-%m-%d')` is how a statement asks for today
        // written out, so a clock reading stands where a column stands, and so
        // does a moment written out as a word.
        let columns = match written {
            Expr::Identifier(column) => vec![column.value.clone()],
            Expr::Function(inner) if names_a_clock_reading(inner) => Vec::new(),
            Expr::Value(value)
                if matches!(
                    &value.value,
                    Value::SingleQuotedString(_) | Value::DoubleQuotedString(_)
                ) =>
            {
                Vec::new()
            }
            _ => return None,
        };
        let (Value::SingleQuotedString(format) | Value::DoubleQuotedString(format)) = &format.value
        else {
            return None;
        };
        return Some(StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::WritesAMoment,
            columns,
            literal_characters: crate::format_width(format),
            not_null: false,
        });
    }
    // `TIMESTAMPDIFF(<unit>, a, b)` counts whole units from the first moment
    // to the second. Each moment is a column, a reading of the clock, or a
    // moment written out as a word — `TIMESTAMPDIFF(DAY, created_at, NOW())`
    // is how a report asks how old a row is.
    if named(&["TIMESTAMPDIFF"]) {
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(unit)), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(from)), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(to))] =
            arguments.args.as_slice()
        else {
            return None;
        };
        timestampdiff_unit(unit)?;
        let mut columns = Vec::new();
        counted_moment(from, &mut columns)?;
        counted_moment(to, &mut columns)?;
        return Some(StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::CountsUnitsBetween,
            columns,
            literal_characters: 0,
            not_null: false,
        });
    }
    // `BIN(n)` and `OCT(n)` write a whole number out in another radix. A word
    // is refused: measured, MySQL reads one as the number it names, which is 0
    // for a word that names none.
    if named(&["BIN", "OCT"]) {
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Identifier(column),
        ))] = arguments.args.as_slice()
        else {
            return None;
        };
        return Some(StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::WritesInAnotherRadix,
            columns: vec![column.value.clone()],
            literal_characters: 0,
            not_null: false,
        });
    }
    // `FIELD(col, 'a', 'b')` answers where the column's word stands among the
    // ones written after it, and `ELT(col, 'a', 'b')` reads one out by its
    // place. Both hold their choices to written words, which is what says how
    // wide the answer can be.
    if named(&["FIELD", "ELT"]) {
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Identifier(column),
        )), choices @ ..] = arguments.args.as_slice()
        else {
            return None;
        };
        if choices.is_empty() {
            return None;
        }
        let mut widest = 0u32;
        for choice in choices {
            let sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
                Expr::Value(value),
            )) = choice
            else {
                return None;
            };
            let (Value::SingleQuotedString(word) | Value::DoubleQuotedString(word)) = &value.value
            else {
                return None;
            };
            widest = widest.max(word.chars().count() as u32);
        }
        let finds = named(&["FIELD"]);
        return Some(StaticSelectMetadata::ScalarCall {
            function: if finds {
                ScalarFunction::FindsThePlace
            } else {
                ScalarFunction::ReadsThePlace
            },
            columns: vec![column.value.clone()],
            literal_characters: widest,
            not_null: finds,
        });
    }
    // Measured on MySQL 8.4.11: `DATEDIFF(b, a)` answers the days between the
    // two, counting the date alone, as a LONGLONG of length 9. Its moments
    // are the ones `TIMESTAMPDIFF` counts between.
    if named(&["DATEDIFF"]) {
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(later)), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(earlier))] =
            arguments.args.as_slice()
        else {
            return None;
        };
        let mut columns = Vec::new();
        counted_moment(later, &mut columns)?;
        counted_moment(earlier, &mut columns)?;
        return Some(StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::CountsDaysBetween,
            columns,
            literal_characters: 0,
            not_null: false,
        });
    }
    // `ADDDATE` and `SUBDATE` are `DATE_ADD` and `DATE_SUB` under other names,
    // and a bare count is a count of days. Measured on MySQL 8.4.11, each
    // reports what its `DATE_ADD` spelling reports.
    if named(&["ADDDATE", "SUBDATE"]) {
        return scalar_call(&date_shift_spelled_out(function)?);
    }
    // `TIME_TO_SEC` counts the seconds in a time and `SEC_TO_TIME` writes a
    // count back out as one. Each reads a column or a written value; a
    // written value MySQL would warn about — a word naming no time, a count
    // past the widest time — is refused, the warning not being raised here.
    if named(&["TIME_TO_SEC", "SEC_TO_TIME"]) {
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(read))] =
            arguments.args.as_slice()
        else {
            return None;
        };
        let counts = named(&["TIME_TO_SEC"]);
        let columns = match read {
            Expr::Identifier(column) => vec![column.value.clone()],
            _ if !counts => {
                let (_, held) =
                    crate::time_of_seconds(crate::translate::direct_signed_integer(read)?);
                if held {
                    return None;
                }
                Vec::new()
            }
            Expr::Value(value) => {
                let (Value::SingleQuotedString(word) | Value::DoubleQuotedString(word)) =
                    &value.value
                else {
                    return None;
                };
                crate::seconds_in_the_time(&crate::normalize_time(word)?)?;
                Vec::new()
            }
            _ => return None,
        };
        return Some(StaticSelectMetadata::ScalarCall {
            function: if counts {
                ScalarFunction::CountsTheSecondsOfATime
            } else {
                ScalarFunction::WritesSecondsAsATime
            },
            columns,
            literal_characters: 0,
            not_null: false,
        });
    }
    // `INET_ATON` reads a dotted address out of a word, `INET_NTOA` writes a
    // number out as one, and `IS_IPV4` says whether a word is one. Each reads
    // a column or a written value; a written value MySQL answers NULL for
    // comes with warning 1411, which is not raised here, so it is refused.
    if named(&["INET_ATON", "INET_NTOA", "IS_IPV4"]) {
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(read))] =
            arguments.args.as_slice()
        else {
            return None;
        };
        let columns = match read {
            Expr::Identifier(column) => vec![column.value.clone()],
            _ if named(&["INET_NTOA"]) => {
                crate::inet_ntoa(crate::translate::direct_signed_integer(read)?)?;
                Vec::new()
            }
            Expr::Value(value) => {
                let (Value::SingleQuotedString(word) | Value::DoubleQuotedString(word)) =
                    &value.value
                else {
                    return None;
                };
                if named(&["INET_ATON"]) {
                    crate::inet_aton(word)?;
                }
                Vec::new()
            }
            _ => return None,
        };
        return Some(StaticSelectMetadata::ScalarCall {
            function: if named(&["INET_ATON"]) {
                ScalarFunction::ReadsAnAddress
            } else if named(&["INET_NTOA"]) {
                ScalarFunction::WritesAnAddress
            } else {
                ScalarFunction::ChecksAnAddress
            },
            not_null: columns.is_empty() && named(&["IS_IPV4"]),
            columns,
            literal_characters: 0,
        });
    }
    // `TO_DAYS(d)` counts the days from the year zero, and `YEARWEEK(d)` the
    // year and the week together, by the countings a written mode names. Each
    // reads a moment the counts above read.
    if named(&["TO_DAYS", "YEARWEEK"]) {
        let (moment, mode) = match arguments.args.as_slice() {
            [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(moment))] => {
                (moment, None)
            }
            [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(moment)), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(mode))]
                if named(&["YEARWEEK"]) =>
            {
                (moment, Some(mode))
            }
            _ => return None,
        };
        if let Some(mode) = mode {
            week_mode(mode)?;
        }
        let mut columns = Vec::new();
        counted_moment(moment, &mut columns)?;
        return Some(StaticSelectMetadata::ScalarCall {
            function: if named(&["TO_DAYS"]) {
                ScalarFunction::CountsDaysFromTheYearZero
            } else {
                ScalarFunction::ReadsTheYearAndWeek
            },
            columns,
            literal_characters: 0,
            not_null: false,
        });
    }
    // `DAYNAME(d)` and `MONTHNAME(d)` name the day of the week and the month
    // a moment falls in, and `WEEK(d)` numbers its week, by the counting a
    // written mode from 0 through 7 names. Each reads a moment the counts
    // above read.
    if named(&["DAYNAME", "MONTHNAME", "WEEK"]) {
        let (moment, mode) = match arguments.args.as_slice() {
            [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(moment))] => {
                (moment, None)
            }
            [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(moment)), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(mode))]
                if named(&["WEEK"]) =>
            {
                (moment, Some(mode))
            }
            _ => return None,
        };
        if let Some(mode) = mode {
            week_mode(mode)?;
        }
        let mut columns = Vec::new();
        counted_moment(moment, &mut columns)?;
        return Some(StaticSelectMetadata::ScalarCall {
            function: if named(&["WEEK"]) {
                ScalarFunction::ReadsTheWeek
            } else {
                ScalarFunction::NamesTheDayOrMonth
            },
            columns,
            literal_characters: 0,
            not_null: false,
        });
    }
    let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
        Expr::Identifier(column),
    ))] = arguments.args.as_slice()
    else {
        return None;
    };
    // TRIM is not here: MySQL's has LEADING, TRAILING and BOTH forms that
    // sqlparser gives their own shape, so it is read in `classify_trim`.
    let function = if named(&["LOWER", "UPPER", "REVERSE"]) {
        ScalarFunction::KeepsTextShape
    } else if named(&["HEX"]) {
        ScalarFunction::Hexadecimal
    } else if named(&["ASCII"]) {
        ScalarFunction::ReadsTheFirstByte
    } else if named(&["ORD"]) {
        ScalarFunction::ReadsTheFirstCharacter
    } else if named(&["CRC32"]) {
        ScalarFunction::ChecksTheBytes
    } else if named(&["QUOTE"]) {
        ScalarFunction::QuotesForSql
    } else if named(&["TO_BASE64"]) {
        ScalarFunction::EncodesInBase64
    } else if named(&["JSON_VALID"]) {
        ScalarFunction::ChecksJson
    } else if named(&["JSON_QUOTE"]) {
        ScalarFunction::QuotesAsJson
    } else if named(&["LENGTH", "OCTET_LENGTH", "CHAR_LENGTH", "CHARACTER_LENGTH"]) {
        ScalarFunction::CountsText
    } else if named(&["ABS"]) {
        ScalarFunction::KeepsNumericShape
    // Only the readings the two work out the same way. Measured on MySQL
    // 8.4.11 against the engine: `ATAN(10)` answers ...7347 there and ...7345
    // here, and `TAN(10)` ...0866 against ...0867 — a last-place difference
    // between two maths libraries. Every other reading of that family comes
    // from the same library, so agreeing at the points tried is not a promise,
    // and none of them is taken. A square root is required to be rounded
    // exactly, and turning an angle round is one multiplication.
    } else if named(&["SQRT", "DEGREES", "RADIANS"]) {
        ScalarFunction::Approximates
    // FLOOR and CEIL are their own AST shapes, classified above.
    } else if named(&["CEILING"]) {
        ScalarFunction::RoundsToWhole
    } else if named(&["SIGN"]) {
        ScalarFunction::Truncates
    // Measured on MySQL 8.4.11: each of the three reports no NOT NULL flag
    // even over a NOT NULL column, which the `not_null: false` below says.
    } else if named(&["YEAR"]) {
        ScalarFunction::ReadsTheYear
    } else if named(&["MONTH", "DAY"]) {
        ScalarFunction::ReadsAMonthOrDay
    } else if named(&["HOUR"]) {
        ScalarFunction::ReadsTheHour
    } else if named(&["MINUTE", "SECOND"]) {
        ScalarFunction::ReadsAMinuteOrSecond
    } else if named(&["DAYOFMONTH"]) {
        ScalarFunction::ReadsAMonthOrDay
    } else if named(&["QUARTER"]) {
        ScalarFunction::ReadsTheQuarter
    } else if named(&["WEEKDAY", "DAYOFWEEK"]) {
        ScalarFunction::ReadsADayOfTheWeek
    } else if named(&["DAYOFYEAR"]) {
        ScalarFunction::ReadsTheDayOfTheYear
    } else if named(&["LAST_DAY"]) {
        ScalarFunction::ReadsTheLastDay
    } else {
        return None;
    };
    Some(StaticSelectMetadata::ScalarCall {
        function,
        columns: vec![column.value.clone()],
        literal_characters: 0,
        not_null: false,
    })
}

/// Reads the places of a second a clock reading was asked for: none written,
/// or a written count from 0 through 6. Measured: MySQL refuses 7 and more
/// with 1426.
pub(super) fn clock_places(function: &sqlparser::ast::Function) -> Option<u32> {
    let arguments = match &function.args {
        sqlparser::ast::FunctionArguments::None => return Some(0),
        sqlparser::ast::FunctionArguments::List(arguments) => arguments,
        sqlparser::ast::FunctionArguments::Subquery(_) => return None,
    };
    match arguments.args.as_slice() {
        [] => Some(0),
        [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Value(value),
        ))] => {
            let Value::Number(places, false) = &value.value else {
                return None;
            };
            places.parse::<u32>().ok().filter(|places| *places <= 6)
        }
        _ => None,
    }
}

/// Writes `ADDDATE(x, n)` and `SUBDATE(x, n)` as the `DATE_ADD(x, INTERVAL n
/// DAY)` and `DATE_SUB` they mean, and `ADDDATE(x, INTERVAL ...)` as the same
/// call under its other name.
pub(super) fn date_shift_spelled_out(
    function: &sqlparser::ast::Function,
) -> Option<sqlparser::ast::Function> {
    let [sqlparser::ast::ObjectNamePart::Identifier(name)] = function.name.0.as_slice() else {
        return None;
    };
    let spelled_out = if name.value.eq_ignore_ascii_case("ADDDATE") {
        "DATE_ADD"
    } else if name.value.eq_ignore_ascii_case("SUBDATE") {
        "DATE_SUB"
    } else {
        return None;
    };
    let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
        return None;
    };
    let [shifted @ sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(_)), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(count))] =
        arguments.args.as_slice()
    else {
        return None;
    };
    let interval = match count {
        Expr::Interval(interval) => interval.clone(),
        count => {
            crate::translate::direct_signed_integer(count)?;
            sqlparser::ast::Interval {
                value: Box::new(count.clone()),
                leading_field: Some(sqlparser::ast::DateTimeField::Day),
                leading_precision: None,
                last_field: None,
                fractional_seconds_precision: None,
            }
        }
    };
    let mut spelled = function.clone();
    spelled.name = sqlparser::ast::ObjectName(vec![sqlparser::ast::ObjectNamePart::Identifier(
        sqlparser::ast::Ident::new(spelled_out),
    )]);
    spelled.args = sqlparser::ast::FunctionArguments::List(sqlparser::ast::FunctionArgumentList {
        duplicate_treatment: None,
        args: vec![
            shifted.clone(),
            sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
                Expr::Interval(interval),
            )),
        ],
        clauses: Vec::new(),
    });
    Some(spelled)
}

/// Classifies `ROUND(SUM(col) [, places])` and the same over `AVG`, which is
/// how a report asks for a total or an average it can print.
///
/// Measured on MySQL 8.4.11 it answers a decimal worked out from the
/// aggregate's own. Rounding left of the point is not taken: the engine's
/// decimal rounding stops at the point.
fn rounded_aggregate(arguments: &[sqlparser::ast::FunctionArg]) -> Option<StaticSelectMetadata> {
    let (aggregate, places) = match arguments {
        [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Function(aggregate),
        ))] => (aggregate, 0),
        [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Function(aggregate),
        )), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(places))] => {
            (
                aggregate,
                u32::try_from(crate::translate::direct_signed_integer(places)?).ok()?,
            )
        }
        _ => return None,
    };
    if aggregate.over.is_some() {
        return None;
    }
    let (kind, column) = column_aggregate_argument(aggregate)?;
    if !matches!(kind, ColumnAggregateKind::Sum | ColumnAggregateKind::Avg) {
        return None;
    }
    Some(StaticSelectMetadata::RoundedAggregate {
        column_name: column.value.clone(),
        kind,
        places,
    })
}

/// Classifies `EXTRACT(<field> FROM column)`, which reads a part of a moment
/// out the way the call spelling of that part does.
///
/// Measured on MySQL 8.4.11, each answers the shape its call spelling answers
/// but for the year: `EXTRACT(YEAR FROM d)` answers a whole number of length 5
/// where `YEAR(d)` answers a YEAR of length 4.
fn classify_extract(
    field: &sqlparser::ast::DateTimeField,
    syntax: &sqlparser::ast::ExtractSyntax,
    expr: &Expr,
) -> Option<StaticSelectMetadata> {
    if !matches!(syntax, sqlparser::ast::ExtractSyntax::From) {
        return None;
    }
    let Expr::Identifier(column) = expr else {
        return None;
    };
    let function = match field {
        sqlparser::ast::DateTimeField::Year => ScalarFunction::ReadsTheYearAsANumber,
        sqlparser::ast::DateTimeField::Month | sqlparser::ast::DateTimeField::Day => {
            ScalarFunction::ReadsAMonthOrDay
        }
        sqlparser::ast::DateTimeField::Hour => ScalarFunction::ReadsTheHour,
        sqlparser::ast::DateTimeField::Minute | sqlparser::ast::DateTimeField::Second => {
            ScalarFunction::ReadsAMinuteOrSecond
        }
        _ => return None,
    };
    Some(StaticSelectMetadata::ScalarCall {
        function,
        columns: vec![column.value.clone()],
        literal_characters: 0,
        not_null: false,
    })
}

/// Reports whether a call reads the clock and takes nothing.
///
/// `NOW()` and `CURDATE()` each answer a moment of their own, which is a
/// moment a call over one can be given. `CURTIME()` answers a time of day,
/// which is a span rather than a moment.
fn names_a_clock_reading(function: &sqlparser::ast::Function) -> bool {
    let [sqlparser::ast::ObjectNamePart::Identifier(name)] = function.name.0.as_slice() else {
        return false;
    };
    if name.quote_style.is_some() || !is_plain_aggregate(function) {
        return false;
    }
    matches!(
        CheckedComparisonNow::read(function),
        Some(CheckedComparisonNow::Day | CheckedComparisonNow::Moment)
    )
}

/// Reads one moment `TIMESTAMPDIFF` or `DATEDIFF` counts from or to, and
/// records it when it is a column.
///
/// A reading of the clock and a moment written out as a word carry their own
/// moment. A word that names no moment is left out: measured, MySQL answers
/// NULL for it with warning 1292, and the warning is not raised here. A
/// `CURTIME()` is left out too, a time of day being a span rather than a
/// moment.
fn counted_moment(expr: &Expr, columns: &mut Vec<String>) -> Option<()> {
    match expr {
        Expr::Identifier(column) => columns.push(column.value.clone()),
        Expr::Function(function) if names_a_clock_reading(function) => {}
        Expr::Value(value) => {
            let (Value::SingleQuotedString(written) | Value::DoubleQuotedString(written)) =
                &value.value
            else {
                return None;
            };
            crate::temporal_value::read_moment(written)?;
        }
        _ => return None,
    }
    Some(())
}

/// Reads the written mode a `WEEK` counts by.
///
/// Measured on MySQL 8.4.11, a mode past 7 counts as its last three bits —
/// `WEEK(d, 8)` is `WEEK(d, 0)` and `WEEK(d, -1)` is `WEEK(d, 7)` — and a NULL
/// one as 0; only the eight named ones are taken.
pub(super) fn week_mode(mode: &Expr) -> Option<u32> {
    let Expr::Value(value) = mode else {
        return None;
    };
    let Value::Number(digits, false) = &value.value else {
        return None;
    };
    digits.parse::<u32>().ok().filter(|mode| *mode <= 7)
}

/// Names the unit a `TIMESTAMPDIFF` counts in, the way the rendered call
/// spells it.
pub(super) fn timestampdiff_unit(unit: &Expr) -> Option<&'static str> {
    let named = match unit {
        Expr::Identifier(ident) if ident.quote_style.is_none() => ident.value.clone(),
        Expr::Interval(interval) => match interval.leading_field.as_ref()? {
            sqlparser::ast::DateTimeField::Microsecond => "MICROSECOND".to_owned(),
            sqlparser::ast::DateTimeField::Second => "SECOND".to_owned(),
            sqlparser::ast::DateTimeField::Minute => "MINUTE".to_owned(),
            sqlparser::ast::DateTimeField::Hour => "HOUR".to_owned(),
            sqlparser::ast::DateTimeField::Day => "DAY".to_owned(),
            sqlparser::ast::DateTimeField::Week(None) => "WEEK".to_owned(),
            sqlparser::ast::DateTimeField::Month => "MONTH".to_owned(),
            sqlparser::ast::DateTimeField::Quarter => "QUARTER".to_owned(),
            sqlparser::ast::DateTimeField::Year => "YEAR".to_owned(),
            _ => return None,
        },
        _ => return None,
    };
    [
        "microsecond",
        "second",
        "minute",
        "hour",
        "day",
        "week",
        "month",
        "quarter",
        "year",
    ]
    .into_iter()
    .find(|spelling| named.eq_ignore_ascii_case(spelling))
}

/// Reads `(column, '$path')`, the one shape of `JSON_EXTRACT` this takes.
///
/// The path has to be a literal, because it names what the answer is, and it
/// is held to the plain member-and-element spelling both MySQL and the engine
/// read the same way. MySQL's wildcards — `$.*`, `$[*]` and `$**` — are not
/// among them.
fn json_path_call(
    arguments: &sqlparser::ast::FunctionArgumentList,
    function: ScalarFunction,
) -> Option<StaticSelectMetadata> {
    let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
        Expr::Identifier(column),
    )), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(Expr::Value(
        path,
    )))] = arguments.args.as_slice()
    else {
        return None;
    };
    let (Value::SingleQuotedString(path) | Value::DoubleQuotedString(path)) = &path.value else {
        return None;
    };
    if !names_a_plain_json_path(path) {
        return None;
    }
    Some(StaticSelectMetadata::ScalarCall {
        function,
        columns: vec![column.value.clone()],
        literal_characters: 0,
        not_null: false,
    })
}

/// Reads one argument a JSON builder or changer takes, recording its column.
///
/// A column or a plain literal is taken; a nested call is not read here, and a
/// boolean literal is refused because MySQL writes `true` where the engine has
/// only the number one to write.
fn json_argument_column(
    argument: &sqlparser::ast::FunctionArg,
    columns: &mut Vec<String>,
) -> Option<()> {
    let sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(expr)) =
        argument
    else {
        return None;
    };
    match expr {
        Expr::Identifier(column) => columns.push(column.value.clone()),
        Expr::Value(value)
            if matches!(
                &value.value,
                Value::SingleQuotedString(_)
                    | Value::DoubleQuotedString(_)
                    | Value::Number(_, _)
                    | Value::Null
            ) => {}
        Expr::UnaryOp {
            op: UnaryOperator::Minus | UnaryOperator::Plus,
            expr: inner,
        } if matches!(
            inner.as_ref(),
            Expr::Value(value) if matches!(&value.value, Value::Number(_, _))
        ) => {}
        _ => return None,
    }
    Some(())
}

/// Reads an argument written out as text, which a keyword, a path, a
/// separator or a delimiter has to be.
fn written_word(argument: &sqlparser::ast::FunctionArg) -> Option<&str> {
    let sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(Expr::Value(
        value,
    ))) = argument
    else {
        return None;
    };
    let (Value::SingleQuotedString(text) | Value::DoubleQuotedString(text)) = &value.value else {
        return None;
    };
    Some(text)
}

/// Answers whether a JSON path names one member of the top-level object.
fn names_one_member(path: &str) -> bool {
    let Some(name) = path.strip_prefix("$.") else {
        return false;
    };
    // `$."city"`, the way Laravel writes every member, names the one `$.city`
    // names.
    let name = name
        .strip_prefix('"')
        .and_then(|quoted| quoted.strip_suffix('"'))
        .unwrap_or(name);
    !name.is_empty()
        && !name.starts_with(|character: char| character.is_ascii_digit())
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
}

/// Returns the column a JSON reading names: the column itself, or the one a
/// `JSON_EXTRACT` or an arrow reads a path out of.
fn json_reading_column(expr: &Expr) -> Option<String> {
    if let Expr::Identifier(column) = expr {
        return Some(column.value.clone());
    }
    let Some(StaticSelectMetadata::ScalarCall {
        function, columns, ..
    }) = (match expr {
        Expr::Function(function) => scalar_call(function),
        Expr::BinaryOp { .. } => classify_json_arrow(expr),
        _ => None,
    })
    else {
        return None;
    };
    // Only the reading that answers a document is taken. The unquoted one
    // answers the text inside a string, which is not a document to read again.
    (function == ScalarFunction::ReadsAJsonValue)
        .then(|| columns.first().cloned())
        .flatten()
}

/// Reports whether a path is `$` followed by plain `.member` and `[index]`
/// steps, which is the part of MySQL's path language the engine reads too.
pub fn names_a_plain_json_path(path: &str) -> bool {
    let Some(mut rest) = path.strip_prefix('$') else {
        return false;
    };
    while !rest.is_empty() {
        // Laravel writes every member quoted, `$."city"`, which names the one
        // `$.city` names, and the engine reads it so too.
        if let Some(after) = rest.strip_prefix(".\"") {
            let Some(end) = after.find('"') else {
                return false;
            };
            let member = &after[..end];
            if member.is_empty()
                || !member
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            {
                return false;
            }
            rest = &after[end + 1..];
            continue;
        }
        if let Some(after) = rest.strip_prefix('.') {
            let member = after
                .split(['.', '['])
                .next()
                .expect("splitting a string answers at least one part");
            if member.is_empty()
                || !member
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            {
                return false;
            }
            rest = &after[member.len()..];
            continue;
        }
        let Some(after) = rest.strip_prefix('[') else {
            return false;
        };
        let Some(end) = after.find(']') else {
            return false;
        };
        let index = &after[..end];
        if index.is_empty() || !index.bytes().all(|byte| byte.is_ascii_digit()) {
            return false;
        }
        rest = &after[end + 1..];
    }
    true
}

/// Returns the aggregate kind and the column a plain `MIN`, `MAX`, `SUM` or
/// `AVG` names.
///
/// Each answers a type worked out from that column — measured on MySQL 8.4.11,
/// `MIN` and `MAX` give the column's own type while `SUM` and `AVG` give a
/// decimal derived from its precision — so the call has to name a column.
/// `MIN(*)` is not even valid SQL, and an expression argument would need a type
/// this cannot work out.
pub(super) fn column_aggregate_argument(
    function: &sqlparser::ast::Function,
) -> Option<(ColumnAggregateKind, &sqlparser::ast::Ident)> {
    let [sqlparser::ast::ObjectNamePart::Identifier(name)] = function.name.0.as_slice() else {
        return None;
    };
    if name.quote_style.is_some() {
        return None;
    }
    let kind = if name.value.eq_ignore_ascii_case("MIN") || name.value.eq_ignore_ascii_case("MAX") {
        ColumnAggregateKind::MinMax
    } else if name.value.eq_ignore_ascii_case("SUM") {
        ColumnAggregateKind::Sum
    } else if name.value.eq_ignore_ascii_case("AVG") {
        ColumnAggregateKind::Avg
    } else if name.value.eq_ignore_ascii_case("GROUP_CONCAT") {
        ColumnAggregateKind::Concatenated
    // Only the sample form is here. MySQL spells the population one
    // `STDDEV`, `STD` and `STDDEV_POP`, and the engine has no aggregate for
    // it, so answering one of those would mean answering a different number.
    } else if name.value.eq_ignore_ascii_case("STDDEV_SAMP") {
        ColumnAggregateKind::DeviatesBySample
    } else if name.value.eq_ignore_ascii_case("JSON_ARRAYAGG") {
        ColumnAggregateKind::CollectsIntoJson
    } else {
        return None;
    };
    if has_aggregate_modifiers(function) {
        return None;
    }
    let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
        return None;
    };
    // `GROUP_CONCAT` is the one aggregate here that carries a `SEPARATOR`
    // after its column.
    if kind == ColumnAggregateKind::Concatenated {
        // Measured on MySQL 8.4.11: `DISTINCT` drops values equal under the
        // column's collation and joins the rest in that collation's order —
        // `b,B,é,e,A` answers `A,b,é` — where the engine drops only
        // identical values and keeps them in the order it read them.
        if arguments.duplicate_treatment.is_some() {
            return None;
        }
        if !checked_group_concat_clauses(&arguments.clauses, |key| {
            matches!(key, Expr::Identifier(_))
        }) {
            return None;
        }
    } else if arguments.duplicate_treatment.is_some() || !arguments.clauses.is_empty() {
        return None;
    }
    match arguments.args.as_slice() {
        [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Identifier(column),
        ))] => Some((kind, column)),
        _ => None,
    }
}

/// Returns the kind, the table and the column of a `MIN`, `MAX`, `SUM` or
/// `GROUP_CONCAT` over a column named with its table — `SUM(posts.views)`.
///
/// A statement reading one table has that table's name left out before it is
/// read, so this is how a join writes one, the name saying which table's
/// column the answer's shape comes from. Measured on MySQL 8.4.11, each
/// answers the shape it answers over that table alone.
pub(crate) fn qualified_aggregate_argument(
    function: &sqlparser::ast::Function,
) -> Option<(
    ColumnAggregateKind,
    &sqlparser::ast::Ident,
    &sqlparser::ast::Ident,
)> {
    let [sqlparser::ast::ObjectNamePart::Identifier(name)] = function.name.0.as_slice() else {
        return None;
    };
    if name.quote_style.is_some() {
        return None;
    }
    let kind = if name.value.eq_ignore_ascii_case("MIN") || name.value.eq_ignore_ascii_case("MAX") {
        ColumnAggregateKind::MinMax
    } else if name.value.eq_ignore_ascii_case("SUM") {
        ColumnAggregateKind::Sum
    } else if name.value.eq_ignore_ascii_case("GROUP_CONCAT") {
        ColumnAggregateKind::Concatenated
    } else {
        return None;
    };
    if has_aggregate_modifiers(function) {
        return None;
    }
    let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
        return None;
    };
    if arguments.duplicate_treatment.is_some() {
        return None;
    }
    let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
        Expr::CompoundIdentifier(parts),
    ))] = arguments.args.as_slice()
    else {
        return None;
    };
    let [table, column] = parts.as_slice() else {
        return None;
    };
    // A `GROUP_CONCAT` over a joined column may be ordered by that column
    // itself, the one order whose kind is the column's own.
    let clauses_taken = match kind {
        ColumnAggregateKind::Concatenated => {
            checked_group_concat_clauses(&arguments.clauses, |key| {
                matches!(key, Expr::CompoundIdentifier(key)
                    if key.len() == 2
                        && key[0].value.eq_ignore_ascii_case(&table.value)
                        && key[1].value.eq_ignore_ascii_case(&column.value))
            })
        }
        _ => arguments.clauses.is_empty(),
    };
    clauses_taken.then_some((kind, table, column))
}

/// Returns whether a `GROUP_CONCAT` over a joined column is ordered by that
/// column from the last, when it is ordered at all.
pub(crate) fn qualified_group_concat_order(function: &sqlparser::ast::Function) -> Option<bool> {
    let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
        return None;
    };
    arguments.clauses.iter().find_map(|clause| match clause {
        sqlparser::ast::FunctionArgumentClause::OrderBy(terms) => match terms.as_slice() {
            [sqlparser::ast::OrderByExpr { options, .. }] => Some(options.asc == Some(false)),
            _ => None,
        },
        _ => None,
    })
}

/// Classifies `CAST(col AS <type>)` and the `CONVERT(col, <type>)` spelling.
///
/// Only the targets the engine answers exactly what MySQL answers are read.
/// `UNSIGNED` is not one: measured on 8.4.11, `CAST(-3 AS UNSIGNED)` answers
/// 18446744073709551613, and the engine holds an integer as an `i64`. Neither
/// is `DECIMAL`, which carries a scale the engine does not keep, nor
/// `CHAR(n)`, which cuts the value short and warns about it.
fn classify_cast(
    kind: &sqlparser::ast::CastKind,
    expr: &Expr,
    data_type: &sqlparser::ast::DataType,
    format: Option<&sqlparser::ast::CastFormat>,
    array: bool,
) -> Option<StaticSelectMetadata> {
    use sqlparser::ast::{CastKind, DataType};

    if format.is_some() || array || !matches!(kind, CastKind::Cast | CastKind::DoubleColon) {
        return None;
    }
    if let Expr::Function(function) = expr {
        if !matches!(data_type, DataType::Signed | DataType::SignedInteger) {
            return None;
        }
        let (kind, column) = column_aggregate_argument(function)?;
        if !matches!(kind, ColumnAggregateKind::Sum | ColumnAggregateKind::MinMax) {
            return None;
        }
        return Some(StaticSelectMetadata::AggregateAsWholeNumber(Box::new(
            StaticSelectMetadata::ColumnAggregate {
                column_name: column.value.clone(),
                kind,
            },
        )));
    }
    let Expr::Identifier(column) = expr else {
        return None;
    };
    let function = match data_type {
        DataType::Char(None) | DataType::Character(None) => ScalarFunction::CastsToText,
        DataType::Signed | DataType::SignedInteger => ScalarFunction::CastsToWholeNumber,
        DataType::Date => ScalarFunction::CastsToDay,
        DataType::Datetime(None) => ScalarFunction::CastsToMoment,
        _ => return None,
    };
    Some(StaticSelectMetadata::ScalarCall {
        function,
        columns: vec![column.value.clone()],
        literal_characters: 0,
        not_null: false,
    })
}

/// Classifies `CONVERT(col, <type>)`, which means what `CAST(col AS <type>)`
/// means, and `CONVERT(col USING utf8mb4)`, which means `CAST(col AS CHAR)`.
///
/// The other spellings are refused. Another character set than utf8mb4 would
/// change the collation the answer carries; the T-SQL `CONVERT(<type>, col)`
/// and `TRY_CONVERT` write the two the other way round and answer NULL where
/// MySQL raises, neither of which is MySQL.
fn classify_convert(expr: &Expr) -> Option<StaticSelectMetadata> {
    let Expr::Convert {
        is_try,
        expr,
        data_type,
        charset,
        target_before_value,
        styles,
    } = expr
    else {
        return None;
    };
    if *is_try || *target_before_value || !styles.is_empty() {
        return None;
    }
    // Measured on MySQL 8.4.11: `CONVERT(col USING utf8mb4)` answers what
    // `CAST(col AS CHAR)` answers, the column written out in the character set
    // this server speaks — a VARCHAR(200) reports 800, an INT 44.
    if let Some(charset) = charset {
        let [sqlparser::ast::ObjectNamePart::Identifier(name)] = charset.0.as_slice() else {
            return None;
        };
        if data_type.is_some()
            || name.quote_style.is_some()
            || !name.value.eq_ignore_ascii_case("utf8mb4")
        {
            return None;
        }
        return classify_cast(
            &sqlparser::ast::CastKind::Cast,
            expr,
            &sqlparser::ast::DataType::Char(None),
            None,
            false,
        );
    }
    classify_cast(
        &sqlparser::ast::CastKind::Cast,
        expr,
        data_type.as_ref()?,
        None,
        false,
    )
}

/// What a call answers, when it is one the left of a comparison can be.
///
/// Only the calls whose answer is a kind a value can be held to are here. A
/// call answering a real number is not: what a `DOUBLE` compares equal to is a
/// rule of its own, and it has not been measured.
pub(super) fn comparison_answer(expr: &Expr) -> Option<crate::CheckedComparisonAnswer> {
    use crate::CheckedComparisonAnswer;

    let StaticSelectMetadata::ScalarCall { function, .. } = classify_static_select_expr(expr)?
    else {
        return None;
    };
    Some(match function {
        ScalarFunction::KeepsTextShape
        | ScalarFunction::CastsToText
        | ScalarFunction::NamesTheDayOrMonth
        | ScalarFunction::TakesASubstring { .. }
        | ScalarFunction::SplitsOnADelimiter => CheckedComparisonAnswer::Text,
        ScalarFunction::CountsText
        | ScalarFunction::ReadsTheYear
        | ScalarFunction::ReadsAMonthOrDay
        | ScalarFunction::ReadsTheHour
        | ScalarFunction::ReadsAMinuteOrSecond
        | ScalarFunction::CountsDaysBetween
        | ScalarFunction::CountsUnitsBetween
        | ScalarFunction::ReadsTheQuarter
        | ScalarFunction::ReadsADayOfTheWeek
        | ScalarFunction::ReadsTheDayOfTheYear
        | ScalarFunction::ReadsTheWeek
        | ScalarFunction::ReadsTheYearAsANumber
        | ScalarFunction::FindsThePlace
        | ScalarFunction::CastsToWholeNumber => CheckedComparisonAnswer::WholeNumber,
        ScalarFunction::Today
        | ScalarFunction::CastsToDay
        | ScalarFunction::ShiftsTheDay
        | ScalarFunction::ReadsTheLastDay => CheckedComparisonAnswer::Day,
        ScalarFunction::Now | ScalarFunction::CastsToMoment | ScalarFunction::ShiftsTheMoment => {
            CheckedComparisonAnswer::Moment
        }
        _ => return None,
    })
}

/// Reads which of the four targets a cast names, for the renderer.
pub(super) fn checked_cast_target(
    kind: &sqlparser::ast::CastKind,
    expr: &Expr,
    data_type: &sqlparser::ast::DataType,
    format: Option<&sqlparser::ast::CastFormat>,
    array: bool,
) -> Option<ScalarFunction> {
    match classify_cast(kind, expr, data_type, format, array)? {
        StaticSelectMetadata::ScalarCall { function, .. } => Some(function),
        _ => None,
    }
}

/// Reads which target a `CONVERT` names and what it converts, for the
/// renderer.
pub(super) fn checked_convert_target(expr: &Expr) -> Option<(&Expr, ScalarFunction)> {
    let Expr::Convert { expr: inner, .. } = expr else {
        return None;
    };
    match classify_convert(expr)? {
        StaticSelectMetadata::ScalarCall { function, .. } => Some((inner, function)),
        _ => None,
    }
}

/// Reads the clock reading a shift is over, when the thing shifted is one.
///
/// A time of day is not one of them: a `TIME` holds a span rather than a
/// moment, and shifting a span by a month names nothing.
fn shifted_moment(expr: &Expr) -> Option<CheckedComparisonNow> {
    let Expr::Function(function) = expr else {
        return None;
    };
    match CheckedComparisonNow::read(function)? {
        CheckedComparisonNow::TimeOfDay => None,
        now => Some(now),
    }
}

/// Reports whether a call is a plain `COUNT` or `COUNT(DISTINCT ...)`, which is
/// the one aggregate whose result metadata does not depend on what it counts.
///
/// Measured on MySQL 8.4.11: `COUNT(*)`, `COUNT(col)`, and `COUNT(DISTINCT col)` all answer
/// a non-null `LONGLONG` of length 21, and 0 rather than NULL on an empty table. `MIN` and
/// `MAX` answer their argument's own type, and `SUM` and `AVG` answer DECIMAL,
/// so none of those belong here.
pub(super) fn is_count_call(function: &sqlparser::ast::Function) -> bool {
    let [sqlparser::ast::ObjectNamePart::Identifier(name)] = function.name.0.as_slice() else {
        return false;
    };
    if !name.value.eq_ignore_ascii_case("COUNT") || name.quote_style.is_some() {
        return false;
    }
    if has_aggregate_modifiers(function) {
        return false;
    }
    let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
        return false;
    };
    if !arguments.clauses.is_empty() {
        return false;
    }
    // A count is the one aggregate that can take a qualified column, because
    // it is the one whose result does not depend on what the column holds. A
    // join has to qualify, so `COUNT(p.id)` is the only way to write the count
    // of a joined table's rows.
    let counts_a_column = |argument: &sqlparser::ast::FunctionArg| {
        matches!(
            argument,
            sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
                Expr::Identifier(_)
            ))
        ) || matches!(
            argument,
            sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
                Expr::CompoundIdentifier(parts)
            )) if parts.len() == 2
        )
    };
    match arguments.duplicate_treatment {
        None => match arguments.args.as_slice() {
            [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Wildcard)] => {
                true
            }
            [argument] if counts_every_row(argument) => true,
            [argument] => counts_a_column(argument),
            _ => false,
        },
        Some(sqlparser::ast::DuplicateTreatment::Distinct) => {
            matches!(arguments.args.as_slice(), [argument] if counts_a_column(argument))
        }
        _ => false,
    }
}

/// Reports whether a count's argument is a written whole number, which is
/// never NULL, so the call counts every row as `COUNT(*)` does.
///
/// Measured on MySQL 8.4.11: `COUNT(1)` and `COUNT(0)` answer the row count
/// and the shape `COUNT(*)` answers, and TypeORM counts that way. A written
/// NULL counts nothing, and a word or a fraction has not been needed.
pub(super) fn counts_every_row(argument: &sqlparser::ast::FunctionArg) -> bool {
    matches!(
        argument,
        sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(Expr::Value(
            sqlparser::ast::ValueWithSpan {
                value: sqlparser::ast::Value::Number(number, false),
                ..
            }
        ))) if !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
    )
}

/// Reports whether a call is the bare aggregate form and nothing more.
///
/// Anything past it — DISTINCT, an OVER clause, a filter — has its own meaning
/// that this module does not model.
/// Reads the unit an interval names, and whether it counts whole days.
///
/// The engine spells each unit as a modifier of its own — `'+1 days'` — and
/// takes only these six. Anything else, a fractional-second precision
/// included, is left out.
pub(super) fn checked_interval_unit(
    interval: &sqlparser::ast::Interval,
) -> Option<(&'static str, bool, i64)> {
    use sqlparser::ast::DateTimeField;
    if interval.leading_precision.is_some()
        || interval.last_field.is_some()
        || interval.fractional_seconds_precision.is_some()
    {
        return None;
    }
    // A week and a quarter are made of the units beside them: measured on
    // MySQL 8.4.11, a week is exactly seven days and a quarter exactly three
    // months — `2026-01-31` a quarter on and three months on are both
    // `2026-04-30` — so each is counted in what it is made of.
    match interval.leading_field.as_ref()? {
        DateTimeField::Year => Some(("year", true, 1)),
        DateTimeField::Quarter => Some(("month", true, 3)),
        DateTimeField::Month => Some(("month", true, 1)),
        DateTimeField::Week(None) => Some(("day", true, 7)),
        DateTimeField::Day => Some(("day", true, 1)),
        DateTimeField::Hour => Some(("hour", false, 1)),
        DateTimeField::Minute => Some(("minute", false, 1)),
        DateTimeField::Second => Some(("second", false, 1)),
        _ => None,
    }
}

/// The whole number of units one shift counts.
///
/// A shift counts a written number and nothing else here: a count worked out
/// from a row would have to be multiplied for a week or a quarter, which only
/// a written one can be.
pub(super) fn checked_interval_count(interval: &sqlparser::ast::Interval) -> Option<i64> {
    crate::translate::direct_signed_integer(&interval.value)
}

/// Reports whether a `GROUP_CONCAT` carries nothing but an order of one bare
/// column, a separator, or both, in that order.
///
/// MySQL joins the parts in the order named, which the engine's own
/// `group_concat` cannot say, so the order is worked out when the parts are
/// joined; `NULLS FIRST` and `NULLS LAST` are no MySQL.
pub(super) fn checked_group_concat_clauses(
    clauses: &[sqlparser::ast::FunctionArgumentClause],
    orders_by: impl Fn(&Expr) -> bool,
) -> bool {
    use sqlparser::ast::FunctionArgumentClause;
    let separates = |clause: &FunctionArgumentClause| {
        matches!(
            clause,
            FunctionArgumentClause::Separator(sqlparser::ast::ValueWithSpan {
                value: sqlparser::ast::Value::SingleQuotedString(_),
                ..
            })
        )
    };
    let orders_by_a_column = |clause: &FunctionArgumentClause| {
        matches!(
            clause,
            FunctionArgumentClause::OrderBy(terms)
                if matches!(
                    terms.as_slice(),
                    [sqlparser::ast::OrderByExpr {
                        expr,
                        options: sqlparser::ast::OrderByOptions {
                            nulls_first: None,
                            ..
                        },
                        with_fill: None,
                    }] if orders_by(expr)
                )
        )
    };
    match clauses {
        [] => true,
        [only] => separates(only) || orders_by_a_column(only),
        [order, separator] => orders_by_a_column(order) && separates(separator),
        _ => false,
    }
}

/// Returns the separator a checked `GROUP_CONCAT` was given, if any.
pub(super) fn group_concat_separator(function: &sqlparser::ast::Function) -> Option<&str> {
    let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
        return None;
    };
    arguments.clauses.iter().find_map(|clause| match clause {
        sqlparser::ast::FunctionArgumentClause::Separator(sqlparser::ast::ValueWithSpan {
            value: sqlparser::ast::Value::SingleQuotedString(separator),
            ..
        }) => Some(separator.as_str()),
        _ => None,
    })
}

/// Returns the column a checked `GROUP_CONCAT` orders its parts by, and
/// whether it orders them from the last, if it names one.
pub(super) fn group_concat_order(
    function: &sqlparser::ast::Function,
) -> Option<(&sqlparser::ast::Ident, bool)> {
    let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
        return None;
    };
    arguments.clauses.iter().find_map(|clause| match clause {
        sqlparser::ast::FunctionArgumentClause::OrderBy(terms) => match terms.as_slice() {
            [sqlparser::ast::OrderByExpr {
                expr: Expr::Identifier(column),
                options,
                ..
            }] => Some((column, options.asc == Some(false))),
            _ => None,
        },
        _ => None,
    })
}

fn is_plain_aggregate(function: &sqlparser::ast::Function) -> bool {
    if has_aggregate_modifiers(function) {
        return false;
    }
    match &function.args {
        // `CURRENT_DATE` and `CURRENT_TIMESTAMP` are spelled without
        // parentheses, and sqlparser gives those no argument list at all.
        sqlparser::ast::FunctionArguments::None => true,
        sqlparser::ast::FunctionArguments::List(arguments) => {
            arguments.duplicate_treatment.is_none() && arguments.clauses.is_empty()
        }
        sqlparser::ast::FunctionArguments::Subquery(_) => false,
    }
}

fn has_aggregate_modifiers(function: &sqlparser::ast::Function) -> bool {
    function.filter.is_some()
        || function.over.is_some()
        || function.null_treatment.is_some()
        || !function.within_group.is_empty()
        || function.uses_odbc_syntax
        || function.parameters != sqlparser::ast::FunctionArguments::None
}

fn classify_integer(digits: &str, sign: StaticIntegerSign) -> Option<StaticSelectMetadata> {
    let magnitude = digits.parse::<u64>().ok()?;
    let digit_count = u32::try_from(digits.len()).ok()?;
    let in_range = match sign {
        StaticIntegerSign::Negative => magnitude <= (i64::MAX as u64) + 1,
        StaticIntegerSign::None | StaticIntegerSign::Positive => magnitude <= i64::MAX as u64,
    };
    in_range.then_some(StaticSelectMetadata::Integer { digit_count, sign })
}

/// The places of a zero written with a point — `0.0` has one — or nothing for
/// anything else, a whole zero included.
pub(crate) fn written_zero_places(expr: &Expr) -> Option<u32> {
    let Expr::Value(value) = expr else {
        return None;
    };
    let Value::Number(written, false) = &value.value else {
        return None;
    };
    let (whole, places) = written.split_once('.')?;
    let zeros = |digits: &str| !digits.is_empty() && digits.bytes().all(|digit| digit == b'0');
    (zeros(whole) && zeros(places))
        .then(|| u32::try_from(places.len()).ok())
        .flatten()
}
