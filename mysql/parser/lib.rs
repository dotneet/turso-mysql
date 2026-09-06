//! Conservative MySQL parsing for the SQLite-compatible path.

mod checked_primary_key;
mod drop_table;
mod drop_view;
mod like_pattern;
mod select_database;
mod session_variables;
mod show_full_tables;
mod static_select_metadata;

use static_select_metadata::classify_static_select_expr;

pub use checked_primary_key::{
    parse_checked_primary_key_create_table, CheckedPrimaryKeyCreateTable,
    CheckedPrimaryKeyIntegerType,
};
pub use drop_table::{parse_optional_drop_table, MySqlDropTableCommand};
pub use drop_view::parse_optional_drop_view;
pub use like_pattern::MySqlLikePattern;
pub use select_database::{parse_optional_select_database, MySqlSelectDatabaseQuery};
pub use session_variables::{parse_optional_session_sql_notes, MySqlSessionSqlNotes};
pub use show_full_tables::{
    parse_optional_show_full_tables, parse_show_full_tables, MySqlShowFullTablesCommand,
};
pub use static_select_metadata::{
    ColumnAggregateKind, StaticIntegerSign, StaticSelectMetadata, StaticSelectProjectionMetadata,
};

/// Longest `VARCHAR` this server takes, in characters.
///
/// MySQL bounds a row's VARCHAR at 65535 bytes, which is 16383 characters at
/// the four bytes per character utf8mb4 reserves.
const MAX_VARCHAR_CHARACTERS: u64 = 16_383;

/// Widest `DECIMAL` MySQL takes, and the widest scale inside it.
const MAX_DECIMAL_PRECISION: u64 = 65;
const MAX_DECIMAL_SCALE: u64 = 30;

use std::any::TypeId;
use std::{fmt, num::NonZeroUsize};

use sqlparser::{
    ast::{
        AlterTable, AlterTableOperation, BinaryOperator, CharLengthUnits, CharacterLength,
        ColumnDef, ColumnOption, CreateIndex, CreateTable, CreateTableOptions, CreateTrigger,
        CreateView, DataType, Delete, ExactNumberInfo, Expr, FromTable, FunctionArguments,
        HiveDistributionStyle, Ident, IndexColumn, Insert, ObjectName, ObjectNamePart,
        RenameTableNameKind, SelectFlavor, SelectItem, SetExpr, Statement, TableConstraint,
        TableFactor, TableObject, TriggerEvent as SqlTriggerEvent, TriggerObject,
        TriggerObjectKind, TriggerPeriod, UnaryOperator, Update, Value,
    },
    dialect::{Dialect, MySqlDialect},
    keywords::Keyword,
    parser::{Parser, ParserError},
    tokenizer::{Token, Tokenizer, Whitespace},
};
use turso_parser::{
    ast::{
        Cmd as TursoCmd, ColumnConstraint as TursoColumnConstraint,
        CreateTableBody as TursoCreateTableBody, Expr as TursoExpr, Literal as TursoLiteral,
        Name as TursoName, NamedColumnConstraint, NamedTableConstraint, OneSelect,
        Operator as TursoOperator, QualifiedName, RefAct, RefArg, ResultColumn, SelectTable, Stmt,
        TableConstraint as TursoTableConstraint, Type as TursoType, TypeSize as TursoTypeSize,
        UnaryOperator as TursoUnaryOperator,
    },
    parser::Parser as TursoParser,
};

/// Session SQL modes that change how MySQL tokenizes DDL.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionSqlMode {
    pub ansi_quotes: bool,
    pub no_backslash_escapes: bool,
}

/// A MySQL dialect that applies the session lexer modes relevant to this slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionMySqlDialect {
    mode: SessionSqlMode,
    expand_executable_comments: bool,
}

impl SessionMySqlDialect {
    /// Creates a dialect for one MySQL session.
    pub const fn new(mode: SessionSqlMode) -> Self {
        Self {
            mode,
            expand_executable_comments: true,
        }
    }

    const fn without_executable_comments(mode: SessionSqlMode) -> Self {
        Self {
            mode,
            expand_executable_comments: false,
        }
    }
}

impl Default for SessionMySqlDialect {
    fn default() -> Self {
        Self::new(SessionSqlMode::default())
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
        ch == '`' || (self.mode.ansi_quotes && ch == '"')
    }

    fn identifier_quote_style(&self, identifier: &str) -> Option<char> {
        MySqlDialect {}.identifier_quote_style(identifier)
    }

    fn supports_string_literal_backslash_escape(&self) -> bool {
        !self.mode.no_backslash_escapes
    }

    delegate_mysql_bool!(supports_string_literal_concatenation);
    delegate_mysql_bool!(ignores_wildcard_escapes);
    delegate_mysql_bool!(supports_numeric_prefix);
    delegate_mysql_bool!(supports_bitwise_shift_operators);
    fn supports_multiline_comment_hints(&self) -> bool {
        self.expand_executable_comments && MySqlDialect {}.supports_multiline_comment_hints()
    }

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

/// SQLite DDL produced from one checked MySQL `CREATE TABLE` statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranslatedCreateTable {
    pub sqlite_sql: String,
}

impl TranslatedCreateTable {
    /// Returns the SQLite statement without a trailing semicolon.
    pub fn as_sql(&self) -> &str {
        &self.sqlite_sql
    }
}

/// One checked MySQL `AUTO_INCREMENT` table ready for later allocator wiring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedAutoIncrementCreateTable {
    /// The decoded unqualified table name.
    pub table_name: String,
    /// The zero-based stored-column position owned by the allocator.
    pub allocator_column_ordinal: usize,
    /// The decoded name of the column owned by the allocator.
    pub allocator_column_name: String,
    /// Canonical MySQL DDL, including the checked `AUTO_INCREMENT` declaration.
    pub normalized_mysql_ddl: String,
    /// SQLite-compatible table definition with an `INTEGER PRIMARY KEY` rowid alias.
    pub sqlite_statement: Stmt,
}

/// One checked MySQL `INSERT ... VALUES` statement that is eligible for
/// AUTO_INCREMENT range injection.
///
/// The allocator column is deliberately not part of this value yet. The
/// parser cannot know which table columns are allocator-owned without looking
/// at the durable table definition, so callers must bind that name before a
/// statement can be materialized for execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedAutoIncrementInsert {
    table_name: TursoName,
    columns: Vec<TursoName>,
    row_count: NonZeroUsize,
    sqlite_statement: Stmt,
}

impl CheckedAutoIncrementInsert {
    /// Returns the unqualified target table name.
    pub fn table_name(&self) -> &TursoName {
        &self.table_name
    }

    /// Returns the explicit target columns, which do not include the
    /// allocator column until [`Self::bind_allocator_table`] succeeds.
    pub fn columns(&self) -> &[TursoName] {
        &self.columns
    }

    /// Returns the statically known number of VALUES rows.
    pub const fn row_count(&self) -> NonZeroUsize {
        self.row_count
    }

    /// Returns the checked SQLite AST before allocator range injection.
    pub fn sqlite_statement(&self) -> &Stmt {
        &self.sqlite_statement
    }

    /// Binds the allocator column after the frontend has validated the target
    /// table's durable AUTO_INCREMENT definition.
    pub fn bind_allocator_table(
        self,
        table: &CheckedAutoIncrementCreateTable,
    ) -> Result<BoundAutoIncrementInsert, ParseError> {
        if !self
            .table_name
            .as_str()
            .eq_ignore_ascii_case(&table.table_name)
        {
            return unsupported("AUTO_INCREMENT INSERT table does not match its definition");
        }
        let allocator_column = TursoName::exact(table.allocator_column_name.clone());
        if self.columns.iter().any(|column| {
            column
                .as_str()
                .eq_ignore_ascii_case(allocator_column.as_str())
        }) {
            return unsupported("INSERT explicitly names the AUTO_INCREMENT column");
        }
        Ok(BoundAutoIncrementInsert {
            insert: self,
            allocator_column,
        })
    }
}

/// A checked INSERT whose allocator column has been verified to be omitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundAutoIncrementInsert {
    insert: CheckedAutoIncrementInsert,
    allocator_column: TursoName,
}

impl BoundAutoIncrementInsert {
    /// Returns the unqualified target table name.
    pub fn table_name(&self) -> &TursoName {
        self.insert.table_name()
    }

    /// Returns the explicit non-allocator target columns.
    pub fn columns(&self) -> &[TursoName] {
        self.insert.columns()
    }

    /// Returns the decoded allocator column name.
    pub fn allocator_column(&self) -> &TursoName {
        &self.allocator_column
    }

    /// Returns the statically known number of VALUES rows.
    pub const fn row_count(&self) -> NonZeroUsize {
        self.insert.row_count()
    }

    /// Injects one contiguous, already-reserved positive signed-INT range.
    ///
    /// The returned statement owns the allocator values as typed Turso AST
    /// literals. No SQL text is rebuilt or reparsed after the range is known.
    pub fn inject_reserved_range(&self, first_id: u64) -> Result<Stmt, ParseError> {
        let count = u64::try_from(self.row_count().get()).map_err(|_| ParseError::Unsupported {
            feature: "AUTO_INCREMENT range count outside unsigned 64-bit range",
        })?;
        if first_id == 0 {
            return unsupported("AUTO_INCREMENT range must be positive");
        }
        let last_id = first_id
            .checked_add(count - 1)
            .ok_or(ParseError::Unsupported {
                feature: "AUTO_INCREMENT range outside signed INT range",
            })?;
        if last_id > i64::from(i32::MAX) as u64 {
            return unsupported("AUTO_INCREMENT range outside signed INT range");
        }

        let mut statement = self.insert.sqlite_statement.clone();
        let Stmt::Insert { columns, body, .. } = &mut statement else {
            return Err(ParseError::TursoParser(
                "checked AUTO_INCREMENT INSERT did not produce an INSERT AST".to_string(),
            ));
        };
        let turso_parser::ast::InsertBody::Select(select, upsert) = body else {
            return Err(ParseError::TursoParser(
                "checked AUTO_INCREMENT INSERT did not produce a VALUES body".to_string(),
            ));
        };
        if upsert.is_some()
            || !select.order_by.is_empty()
            || select.limit.is_some()
            || !select.body.compounds.is_empty()
        {
            return Err(ParseError::TursoParser(
                "checked AUTO_INCREMENT INSERT contains unsupported query clauses".to_string(),
            ));
        }
        let turso_parser::ast::OneSelect::Values(rows) = &mut select.body.select else {
            return Err(ParseError::TursoParser(
                "checked AUTO_INCREMENT INSERT did not produce VALUES rows".to_string(),
            ));
        };
        if rows.len() != self.row_count().get() || columns.len() != self.columns().len() {
            return Err(ParseError::TursoParser(
                "checked AUTO_INCREMENT INSERT changed shape before range injection".to_string(),
            ));
        }
        columns.insert(0, self.allocator_column.clone());
        for (offset, row) in rows.iter_mut().enumerate() {
            if row.len() != self.columns().len() {
                return Err(ParseError::TursoParser(
                    "checked AUTO_INCREMENT INSERT row changed shape before range injection"
                        .to_string(),
                ));
            }
            let id = first_id
                .checked_add(offset as u64)
                .ok_or(ParseError::Unsupported {
                    feature: "AUTO_INCREMENT range outside signed INT range",
                })?;
            row.insert(
                0,
                Box::new(TursoExpr::Literal(TursoLiteral::Numeric(id.to_string()))),
            );
        }
        Ok(statement)
    }
}

/// SQLite SQL produced from one checked MySQL `SELECT` statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranslatedSelect {
    pub sqlite_sql: String,
    reads_table: bool,
    source_table: Option<MySqlTableName>,
    static_result_metadata: Vec<StaticSelectProjectionMetadata>,
    checked_comparisons: Vec<CheckedSelectComparison>,
    parameter_count: usize,
}

/// The exact right-hand side forms checked for a SELECT comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckedSelectComparisonRhs {
    /// One signed integer literal represented without loss in an `i64`.
    SignedInteger(i64),
    /// One string literal, compared without regard to case.
    Text(String),
    /// A SQL NULL literal, which retains ordinary SQL three-valued logic.
    Null,
    /// One binary-protocol parameter at the zero-based statement ordinal.
    Placeholder { ordinal: usize },
}

/// The comparison operators accepted by the strict integer SELECT subset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckedSelectComparisonOperator {
    /// Equal to (`=`).
    Equal,
    /// Not equal to (`<>` or `!=`).
    NotEqual,
    /// Less than (`<`).
    LessThan,
    /// Less than or equal to (`<=`).
    LessThanOrEqual,
    /// Greater than (`>`).
    GreaterThan,
    /// Greater than or equal to (`>=`).
    GreaterThanOrEqual,
    /// Matches a pattern (`LIKE`).
    Like,
    /// Does not match a pattern (`NOT LIKE`).
    NotLike,
}

/// One strict integer comparison found while rendering a checked SELECT.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedSelectComparison {
    column_name: String,
    operator: CheckedSelectComparisonOperator,
    rhs: CheckedSelectComparisonRhs,
}

impl CheckedSelectComparison {
    /// Returns the unqualified column name used on the left side.
    pub fn column_name(&self) -> &str {
        &self.column_name
    }

    /// Returns the checked comparison operator.
    pub const fn operator(&self) -> CheckedSelectComparisonOperator {
        self.operator
    }

    /// Returns the exact checked right-hand side form.
    pub const fn rhs(&self) -> &CheckedSelectComparisonRhs {
        &self.rhs
    }
}

/// One assignment in a checked MySQL `UPDATE` statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedUpdateAssignment {
    column_name: String,
    value: CheckedUpdateAssignmentValue,
}

/// The forms that callers may safely distinguish in a checked UPDATE assignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckedUpdateAssignmentValue {
    /// The right side is the same unqualified column identifier.
    SelfAssignment,
    /// The right side is one direct signed integer literal.
    SignedInteger(i64),
    /// Any other expression accepted by the conservative UPDATE grammar.
    Other,
}

impl CheckedUpdateAssignment {
    /// Returns the unqualified assignment target as written by the client.
    pub fn column_name(&self) -> &str {
        &self.column_name
    }

    /// Returns the statically checked form of the right side.
    pub const fn value(&self) -> CheckedUpdateAssignmentValue {
        self.value
    }

    /// Returns whether the assignment is exactly `column = column`.
    pub const fn assigns_column_to_itself(&self) -> bool {
        matches!(self.value, CheckedUpdateAssignmentValue::SelfAssignment)
    }
}

/// The target and assignments of one checked MySQL `UPDATE` statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedUpdate {
    table_name: String,
    assignments: Vec<CheckedUpdateAssignment>,
}

impl CheckedUpdate {
    /// Returns the unqualified target table name.
    pub fn table_name(&self) -> &str {
        &self.table_name
    }

    /// Returns the checked assignments in source order.
    pub fn assignments(&self) -> &[CheckedUpdateAssignment] {
        &self.assignments
    }
}

/// SQLite SQL produced from one checked MySQL `INSERT`, `UPDATE`, or `DELETE` statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranslatedDml {
    sqlite_sql: String,
    checked_update: Option<CheckedUpdate>,
    checked_comparisons: Vec<CheckedSelectComparison>,
    source_table: Option<String>,
}

impl TranslatedDml {
    /// Returns the normalized statement without a trailing semicolon.
    pub fn as_sql(&self) -> &str {
        &self.sqlite_sql
    }

    /// Parses the already-checked normalized SQL into Turso's AST.
    pub fn parse_ast(&self) -> Result<Stmt, ParseError> {
        parse_normalized_dml(self.as_sql())
    }

    /// Returns the comparisons the `WHERE` made, for the frontend to check
    /// against the columns they name.
    pub fn checked_comparisons(&self) -> &[CheckedSelectComparison] {
        &self.checked_comparisons
    }

    /// Returns the table an `UPDATE` or `DELETE` names, which is the table the
    /// comparisons have to be checked against.
    pub fn source_table(&self) -> Option<&str> {
        self.source_table.as_deref()
    }

    /// Returns checked UPDATE target information when this is an UPDATE.
    pub fn checked_update(&self) -> Option<&CheckedUpdate> {
        self.checked_update.as_ref()
    }
}

/// The signed integer range associated with one MySQL table column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MySqlSignedInteger {
    TinyInt,
    SmallInt,
    MediumInt,
    Int,
    BigInt,
}

impl MySqlSignedInteger {
    /// Returns the inclusive i64 bounds used by the first strict assignment slice.
    pub const fn bounds(self) -> (i64, i64) {
        match self {
            Self::TinyInt => (-128, 127),
            Self::SmallInt => (-32_768, 32_767),
            Self::MediumInt => (-8_388_608, 8_388_607),
            Self::Int => (-2_147_483_648, 2_147_483_647),
            Self::BigInt => (i64::MIN, i64::MAX),
        }
    }
}

/// Private MySQL numeric metadata rebuilt from durable normalized table DDL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlNumericSpec {
    columns: Vec<Option<MySqlSignedInteger>>,
    character_lengths: Vec<Option<u32>>,
    datetimes: Vec<bool>,
}

impl MySqlNumericSpec {
    /// Returns the signed range for a stored column position, if this slice owns it.
    pub fn column(&self, index: usize) -> Option<MySqlSignedInteger> {
        self.columns.get(index).copied().flatten()
    }

    /// Returns the declared character count for a stored column position.
    pub fn character_length(&self, index: usize) -> Option<u32> {
        self.character_lengths.get(index).copied().flatten()
    }

    /// Reports whether a stored column position holds a `DATETIME`.
    pub fn is_datetime(&self, index: usize) -> bool {
        self.datetimes.get(index).copied().unwrap_or(false)
    }

    /// Returns the number of columns represented by the durable table DDL.
    pub fn len(&self) -> usize {
        self.columns.len()
    }

    /// Returns whether no column needs checking before storage.
    pub fn is_empty(&self) -> bool {
        self.columns.iter().all(Option::is_none)
            && self.character_lengths.iter().all(Option::is_none)
            && !self.datetimes.iter().any(|is_datetime| *is_datetime)
    }
}

impl TranslatedSelect {
    /// Returns the normalized statement without a trailing semicolon.
    pub fn as_sql(&self) -> &str {
        &self.sqlite_sql
    }

    /// Parses the already-checked normalized SQL into Turso's AST.
    pub fn parse_ast(&self) -> Result<Stmt, ParseError> {
        parse_normalized_select(self.as_sql())
    }

    /// Returns whether this SELECT reads from a table.
    pub const fn reads_table(&self) -> bool {
        self.reads_table
    }

    /// Returns the canonical unqualified table read by this SELECT.
    pub fn source_table(&self) -> Option<&str> {
        self.source_table.as_ref().map(MySqlTableName::as_str)
    }

    /// Returns source metadata parallel to checked projection items.
    pub fn static_result_metadata(&self) -> &[StaticSelectProjectionMetadata] {
        &self.static_result_metadata
    }

    /// Returns strict integer comparisons collected from the SELECT predicate.
    pub fn checked_comparisons(&self) -> &[CheckedSelectComparison] {
        &self.checked_comparisons
    }

    /// Returns the total number of `?` parameters in projection and predicate order.
    pub const fn parameter_count(&self) -> usize {
        self.parameter_count
    }
}

/// Errors produced while parsing or checking the supported DDL subset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    Sqlparser(String),
    TursoParser(String),
    ExpectedOneStatement { actual: usize },
    ExpectedAdminCommand,
    ExpectedTransactionCommand,
    TrailingAdminCommandTokens,
    InvalidDatabaseName { reason: &'static str },
    InvalidTableName { reason: &'static str },
    ExpectedCreateTable,
    ExpectedCreateIndex,
    ExpectedCreateView,
    ExpectedCreateTrigger,
    ExpectedAlterTable,
    ExpectedSelect,
    ExpectedDml,
    Unsupported { feature: &'static str },
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlparser(error) => write!(f, "MySQL parse error: {error}"),
            Self::TursoParser(error) => write!(f, "normalized SQLite parse error: {error}"),
            Self::ExpectedOneStatement { actual } => {
                write!(f, "expected exactly one statement, found {actual}")
            }
            Self::ExpectedAdminCommand => {
                f.write_str("expected CREATE DATABASE, DROP DATABASE, or USE")
            }
            Self::ExpectedTransactionCommand => {
                f.write_str("expected BEGIN, START TRANSACTION, COMMIT, or ROLLBACK")
            }
            Self::TrailingAdminCommandTokens => {
                f.write_str("unexpected token after database-management command")
            }
            Self::InvalidDatabaseName { reason } => {
                write!(f, "invalid MySQL database name: {reason}")
            }
            Self::InvalidTableName { reason } => write!(f, "invalid MySQL table name: {reason}"),
            Self::ExpectedCreateTable => f.write_str("expected a CREATE TABLE statement"),
            Self::ExpectedCreateIndex => f.write_str("expected a CREATE INDEX statement"),
            Self::ExpectedCreateView => f.write_str("expected a CREATE VIEW statement"),
            Self::ExpectedCreateTrigger => f.write_str("expected a CREATE TRIGGER statement"),
            Self::ExpectedAlterTable => f.write_str("expected an ALTER TABLE statement"),
            Self::ExpectedSelect => f.write_str("expected a SELECT statement"),
            Self::ExpectedDml => f.write_str("expected an INSERT, UPDATE, or DELETE statement"),
            Self::Unsupported { feature } => {
                write!(f, "unsupported MySQL schema feature: {feature}")
            }
        }
    }
}

impl std::error::Error for ParseError {}

/// A logical database name accepted by the MySQL compatibility registry.
///
/// The registry deliberately has a smaller name space than MySQL itself. Keeping
/// this checked value in the parser prevents a protocol or SQL caller from
/// turning a logical name into a path, a dot-qualified name, or a hidden file.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MySqlDatabaseName(String);

impl MySqlDatabaseName {
    /// Validates and canonicalizes one logical database name.
    pub fn parse(name: &str) -> Result<Self, ParseError> {
        if name.is_empty() {
            return Err(ParseError::InvalidDatabaseName { reason: "empty" });
        }
        if name.len() > 64 {
            return Err(ParseError::InvalidDatabaseName {
                reason: "longer than 64 bytes",
            });
        }

        let mut canonical = String::with_capacity(name.len());
        for byte in name.bytes() {
            let byte = match byte {
                b'A'..=b'Z' => byte.to_ascii_lowercase(),
                b'a'..=b'z' | b'0'..=b'9' | b'_' | b'$' => byte,
                0 => {
                    return Err(ParseError::InvalidDatabaseName { reason: "NUL byte" });
                }
                b'/' | b'\\' => {
                    return Err(ParseError::InvalidDatabaseName {
                        reason: "path separator",
                    });
                }
                0x80..=u8::MAX => {
                    return Err(ParseError::InvalidDatabaseName {
                        reason: "non-ASCII character",
                    });
                }
                _ => {
                    return Err(ParseError::InvalidDatabaseName {
                        reason: "character outside [A-Za-z0-9_$]",
                    });
                }
            };
            canonical.push(char::from(byte));
        }

        if matches!(canonical.as_str(), "." | "..")
            || matches!(
                canonical.as_str(),
                "information_schema"
                    | "mysql"
                    | "performance_schema"
                    | "sys"
                    | "main"
                    | "temp"
                    | "sqlite_master"
                    | "sqlite_schema"
            )
        {
            return Err(ParseError::InvalidDatabaseName {
                reason: "reserved database name",
            });
        }

        Ok(Self(canonical))
    }

    /// Returns the canonical ASCII-lowercase name.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the canonical name as an owned string.
    pub fn into_string(self) -> String {
        self.0
    }
}

impl AsRef<str> for MySqlDatabaseName {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

/// A table name accepted by the initial MySQL metadata surface.
///
/// This is deliberately constrained to the same ASCII-lowercase name policy
/// recorded for MySQL-owned databases. It prevents the catalog provider from
/// accepting a table name that the current frontend cannot address reliably.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MySqlTableName(String);

impl MySqlTableName {
    /// Validates and canonicalizes one unqualified table name.
    pub fn parse(name: &str) -> Result<Self, ParseError> {
        if name.is_empty() {
            return Err(ParseError::InvalidTableName { reason: "empty" });
        }
        if name.len() > 64 {
            return Err(ParseError::InvalidTableName {
                reason: "longer than 64 bytes",
            });
        }

        let mut canonical = String::with_capacity(name.len());
        for byte in name.bytes() {
            let byte = match byte {
                b'A'..=b'Z' => byte.to_ascii_lowercase(),
                b'a'..=b'z' | b'0'..=b'9' | b'_' | b'$' => byte,
                0 => return Err(ParseError::InvalidTableName { reason: "NUL byte" }),
                0x80..=u8::MAX => {
                    return Err(ParseError::InvalidTableName {
                        reason: "non-ASCII character",
                    });
                }
                _ => {
                    return Err(ParseError::InvalidTableName {
                        reason: "character outside [A-Za-z0-9_$]",
                    });
                }
            };
            canonical.push(char::from(byte));
        }
        Ok(Self(canonical))
    }

    /// Returns the canonical ASCII-lowercase name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for MySqlTableName {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

/// A checked MySQL database-management command.
///
/// These commands are intentionally kept outside the shared SQLite AST. The
/// frontend must perform the corresponding registry operation before any Core
/// connection is selected or changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MySqlAdminCommand {
    /// Create one logical database.
    CreateDatabase { name: MySqlDatabaseName },
    /// Drop one logical database.
    DropDatabase { name: MySqlDatabaseName },
    /// Select one logical database for the current session.
    Use { name: MySqlDatabaseName },
    /// List the logical databases visible to this session.
    ListDatabases,
}

impl MySqlAdminCommand {
    /// Returns the command's logical database name, when it has one.
    pub fn name(&self) -> Option<&MySqlDatabaseName> {
        match self {
            Self::CreateDatabase { name } | Self::DropDatabase { name } | Self::Use { name } => {
                Some(name)
            }
            Self::ListDatabases => None,
        }
    }
}

/// A checked read-only MySQL `SHOW TABLES` command that operates on the
/// selected database rather than on the logical-database registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MySqlShowCommand {
    /// List the tables in the current database.
    Tables,
}

/// The one `information_schema.TABLES` query supported by the catalog surface.
///
/// The value carries no user input because the query always reads the selected
/// database and always returns the same three catalog columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MySqlInformationSchemaTablesQuery;

/// The one `information_schema.SCHEMATA` query supported by the catalog surface.
///
/// The value carries no user input because the query always lists the logical
/// databases visible to the session and always returns one catalog column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MySqlInformationSchemaSchemataQuery;

/// A checked `information_schema.COLUMNS` query for one selected-database table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlInformationSchemaColumnsQuery {
    table: MySqlTableName,
}

impl MySqlInformationSchemaColumnsQuery {
    /// Returns the canonical table identifier selected by the query.
    pub fn table(&self) -> &MySqlTableName {
        &self.table
    }
}

/// A checked read-only MySQL `SHOW COLUMNS` command for one table in the
/// selected database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlShowColumnsCommand {
    database: Option<MySqlDatabaseName>,
    table: MySqlTableName,
}

impl MySqlShowColumnsCommand {
    /// Returns the table identifier selected by the command.
    pub fn table(&self) -> &MySqlTableName {
        &self.table
    }

    /// Returns the `database.` qualifier the command was written with, if any.
    pub fn database(&self) -> Option<&MySqlDatabaseName> {
        self.database.as_ref()
    }
}

/// A checked read-only MySQL `SHOW INDEX` command for one base table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlShowIndexCommand {
    database: Option<MySqlDatabaseName>,
    table: MySqlTableName,
}

impl MySqlShowIndexCommand {
    /// Returns the table identifier selected by the command.
    pub fn table(&self) -> &MySqlTableName {
        &self.table
    }

    /// Returns the `database.` qualifier the command was written with, if any.
    pub fn database(&self) -> Option<&MySqlDatabaseName> {
        self.database.as_ref()
    }
}

/// A checked read-only MySQL `SHOW CREATE TABLE` command for one base table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlShowCreateTableCommand {
    database: Option<MySqlDatabaseName>,
    table: MySqlTableName,
}

impl MySqlShowCreateTableCommand {
    /// Returns the table identifier selected by the command.
    pub fn table(&self) -> &MySqlTableName {
        &self.table
    }

    /// Returns the `database.` qualifier the command was written with, if any.
    pub fn database(&self) -> Option<&MySqlDatabaseName> {
        self.database.as_ref()
    }
}

/// The scope a `SHOW VARIABLES` command reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MySqlVariableScope {
    /// The values this session is using.
    Session,
    /// The values the server started every session from.
    Global,
}

/// A checked read-only MySQL `SHOW VARIABLES` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlShowVariablesCommand {
    scope: MySqlVariableScope,
    pattern: Option<MySqlLikePattern>,
}

impl MySqlShowVariablesCommand {
    /// Returns the scope the command was written with.
    pub fn scope(&self) -> MySqlVariableScope {
        self.scope
    }

    /// Reports whether the command asks for the variable called `name`.
    pub fn selects(&self, name: &str) -> bool {
        self.pattern
            .as_ref()
            .is_none_or(|pattern| pattern.matches(name))
    }
}

/// One `KEY name (columns)` lifted out of a `CREATE TABLE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlInlineIndex {
    name: String,
    columns: Vec<String>,
}

impl MySqlInlineIndex {
    /// Returns the index name as the statement wrote it.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the indexed columns, in order.
    pub fn columns(&self) -> &[String] {
        &self.columns
    }
}

/// A `CREATE TABLE` that declares plain indexes inline, split into the two
/// kinds of statement the engine takes.
///
/// The engine has no inline non-unique index, so one MySQL statement becomes a
/// `CREATE TABLE` and one `CREATE INDEX` per key. They have to apply together,
/// which is the caller's job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlCreateTableWithKeys {
    table: MySqlTableName,
    table_sql: String,
    indexes: Vec<MySqlInlineIndex>,
}

impl MySqlCreateTableWithKeys {
    /// Returns the table the statement creates.
    pub fn table(&self) -> &MySqlTableName {
        &self.table
    }

    /// Returns the `CREATE TABLE` with its key clauses removed.
    pub fn table_sql(&self) -> &str {
        &self.table_sql
    }

    /// Returns the keys the statement declared, in the order it wrote them.
    pub fn indexes(&self) -> &[MySqlInlineIndex] {
        &self.indexes
    }
}

/// Splits a `CREATE TABLE` that carries `KEY` or `INDEX` clauses.
///
/// Returns `None` for a `CREATE TABLE` with no such clause, and for anything
/// that is not a `CREATE TABLE`, so the ordinary path keeps those. An unnamed
/// key is refused: MySQL names one after its first column and then disambiguates
/// with `_2` and `_3`, which is a rule this has not measured. So are the index
/// options MySQL takes here, since none of them could be printed back.
pub fn parse_optional_create_table_with_keys(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlCreateTableWithKeys>, ParseError> {
    let Ok(Statement::CreateTable(table)) = parse_one_statement(sql, mode) else {
        return Ok(None);
    };
    if !table
        .constraints
        .iter()
        .any(|constraint| matches!(constraint, TableConstraint::Index(_)))
    {
        return Ok(None);
    }
    let [ObjectNamePart::Identifier(table_ident)] = table.name.0.as_slice() else {
        return unsupported("schema-qualified CREATE TABLE name");
    };
    let table_name =
        MySqlTableName::parse(&table_ident.value).map_err(|_| ParseError::Unsupported {
            feature: "CREATE TABLE name",
        })?;
    let mut indexes = Vec::new();
    let mut remaining = table.clone();
    remaining.constraints.clear();
    for constraint in &table.constraints {
        let TableConstraint::Index(index) = constraint else {
            remaining.constraints.push(constraint.clone());
            continue;
        };
        if index.index_type.is_some() || !index.index_options.is_empty() {
            return unsupported("index option");
        }
        let Some(index_name) = index.name.as_ref() else {
            return unsupported("unnamed inline KEY");
        };
        indexes.push(MySqlInlineIndex {
            name: MySqlTableName::parse(&index_name.value)
                .map_err(|_| ParseError::Unsupported {
                    feature: "inline KEY name",
                })?
                .as_str()
                .to_owned(),
            columns: inline_index_columns(&index.columns)?,
        });
    }
    Ok(Some(MySqlCreateTableWithKeys {
        table: table_name,
        table_sql: Statement::CreateTable(remaining).to_string(),
        indexes,
    }))
}

/// Reads the plain column names an inline key covers.
fn inline_index_columns(columns: &[IndexColumn]) -> Result<Vec<String>, ParseError> {
    let mut names = Vec::with_capacity(columns.len());
    for column in columns {
        if column.operator_class.is_some()
            || column.column.options.asc.is_some()
            || column.column.options.nulls_first.is_some()
        {
            return unsupported("indexed column ordering");
        }
        let Expr::Identifier(name) = &column.column.expr else {
            return unsupported("indexed column expression");
        };
        names.push(name.value.clone());
    }
    if names.is_empty() {
        return unsupported("inline KEY without columns");
    }
    Ok(names)
}

/// Parses the strict `SHOW TABLES` catalog command.
pub fn parse_show_tables(sql: &str, mode: SessionSqlMode) -> Result<MySqlShowCommand, ParseError> {
    parse_optional_show_tables(sql, mode)?.ok_or(ParseError::Unsupported {
        feature: "SHOW TABLES statement",
    })
}

/// Parses `SHOW TABLES` when the statement belongs to the catalog surface.
///
/// Other `SHOW` forms return `None` so that their own parser can handle them.
/// Once `SHOW TABLES` is recognized, an optional single semicolon is allowed;
/// comments, clauses, and additional statements are rejected.
pub fn parse_optional_show_tables(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlShowCommand>, ParseError> {
    let tokens = tokenize_admin_command(sql, mode)?;
    let mut cursor = skip_admin_comments(&tokens, 0);
    if !consume_admin_word(&tokens, &mut cursor, "SHOW") {
        return Ok(None);
    }
    if !consume_admin_word(&tokens, &mut cursor, "TABLES") {
        return Ok(None);
    }
    if !admin_command_ends(&tokens, cursor) {
        return Err(ParseError::TrailingAdminCommandTokens);
    }
    Ok(Some(MySqlShowCommand::Tables))
}

/// Parses the strict `information_schema.TABLES` catalog query.
pub fn parse_information_schema_tables(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<MySqlInformationSchemaTablesQuery, ParseError> {
    parse_optional_information_schema_tables(sql, mode)?.ok_or(ParseError::Unsupported {
        feature: "information_schema.TABLES query",
    })
}

/// Parses the supported `information_schema.TABLES` query when it is present.
///
/// Other SELECT statements return `None` so that the ordinary SELECT parser can
/// handle them. Once a query names `information_schema.TABLES`, every clause is
/// checked against the one supported shape and unsupported variants fail closed.
pub fn parse_optional_information_schema_tables(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlInformationSchemaTablesQuery>, ParseError> {
    let tokens = tokenize_information_schema_query(sql, mode)?;
    let first = tokens
        .iter()
        .find(|token| !matches!(token, Token::Whitespace(_)));
    if !matches!(first, Some(token) if is_unquoted_word(token, "SELECT")) {
        return Ok(None);
    }
    if !contains_information_schema_tables(&tokens) {
        return Ok(None);
    }
    reject_information_schema_query_tokens(&tokens)?;

    let statement = parse_one_statement(sql, mode)?;
    let Statement::Query(query) = statement else {
        return Err(ParseError::ExpectedSelect);
    };
    validate_information_schema_tables_query(&query)?;
    Ok(Some(MySqlInformationSchemaTablesQuery))
}

/// Parses the strict `information_schema.SCHEMATA` catalog query.
pub fn parse_information_schema_schemata(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<MySqlInformationSchemaSchemataQuery, ParseError> {
    parse_optional_information_schema_schemata(sql, mode)?.ok_or(ParseError::Unsupported {
        feature: "information_schema.SCHEMATA query",
    })
}

/// Parses the supported `information_schema.SCHEMATA` query when it is present.
///
/// Other SELECT statements return `None` so that the ordinary SELECT parser can
/// handle them. Once a query names `information_schema.SCHEMATA`, every clause
/// is checked against the one supported shape and unsupported variants fail
/// closed.
pub fn parse_optional_information_schema_schemata(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlInformationSchemaSchemataQuery>, ParseError> {
    let tokens = tokenize_information_schema_query(sql, mode)?;
    let first = tokens
        .iter()
        .find(|token| !matches!(token, Token::Whitespace(_)));
    if !matches!(first, Some(token) if is_unquoted_word(token, "SELECT")) {
        return Ok(None);
    }
    if !contains_information_schema_object(&tokens, "SCHEMATA") {
        return Ok(None);
    }
    reject_information_schema_schemata_query_tokens(&tokens)?;

    let statement = parse_one_statement(sql, mode)?;
    let Statement::Query(query) = statement else {
        return Err(ParseError::ExpectedSelect);
    };
    validate_information_schema_schemata_query(&query)?;
    Ok(Some(MySqlInformationSchemaSchemataQuery))
}

/// Parses the strict `information_schema.COLUMNS` catalog query.
pub fn parse_information_schema_columns(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<MySqlInformationSchemaColumnsQuery, ParseError> {
    parse_optional_information_schema_columns(sql, mode)?.ok_or(ParseError::Unsupported {
        feature: "information_schema.COLUMNS query",
    })
}

/// Parses the supported `information_schema.COLUMNS` query when it is present.
///
/// Other SELECT statements return `None` so that the ordinary SELECT parser can
/// handle them. Once a query names `information_schema.COLUMNS`, every clause is
/// checked against the one supported shape and unsupported variants fail closed.
pub fn parse_optional_information_schema_columns(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlInformationSchemaColumnsQuery>, ParseError> {
    let tokens = tokenize_information_schema_query(sql, mode)?;
    let first = tokens
        .iter()
        .find(|token| !matches!(token, Token::Whitespace(_)));
    if !matches!(first, Some(token) if is_unquoted_word(token, "SELECT")) {
        return Ok(None);
    }
    if !contains_information_schema_object(&tokens, "COLUMNS") {
        return Ok(None);
    }
    reject_information_schema_columns_query_tokens(&tokens)?;

    let statement = parse_one_statement(sql, mode)?;
    let Statement::Query(query) = statement else {
        return Err(ParseError::ExpectedSelect);
    };
    let table = validate_information_schema_columns_query(&query)?;
    Ok(Some(MySqlInformationSchemaColumnsQuery { table }))
}

/// Parses the strict `SHOW COLUMNS FROM table` catalog command.
pub fn parse_show_columns(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<MySqlShowColumnsCommand, ParseError> {
    parse_optional_show_columns(sql, mode)?.ok_or(ParseError::Unsupported {
        feature: "SHOW COLUMNS statement",
    })
}

/// Parses `SHOW COLUMNS FROM table` when the statement belongs to the catalog
/// surface.
///
/// Other `SHOW` forms return `None` so that their own parser can handle them.
/// Once `SHOW COLUMNS` is recognized, this accepts one unqualified identifier
/// and an optional single semicolon. Comments, clauses, database qualifiers,
/// and additional statements are rejected.
pub fn parse_optional_show_columns(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlShowColumnsCommand>, ParseError> {
    let tokens = tokenize_admin_command(sql, mode)?;
    let mut cursor = skip_admin_comments(&tokens, 0);
    if !consume_admin_word(&tokens, &mut cursor, "SHOW") {
        return Ok(None);
    }
    if !consume_admin_word(&tokens, &mut cursor, "COLUMNS") {
        return Ok(None);
    }
    if !consume_admin_word(&tokens, &mut cursor, "FROM") {
        return Err(ParseError::ExpectedAdminCommand);
    }
    let (database, table) = consume_admin_qualified_table_name(&tokens, &mut cursor)?;
    if !admin_command_ends(&tokens, cursor) {
        return Err(ParseError::TrailingAdminCommandTokens);
    }
    Ok(Some(MySqlShowColumnsCommand { database, table }))
}

/// Parses the strict `SHOW INDEX FROM table` catalog command.
pub fn parse_show_index(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<MySqlShowIndexCommand, ParseError> {
    parse_optional_show_index(sql, mode)?.ok_or(ParseError::Unsupported {
        feature: "SHOW INDEX statement",
    })
}

/// Parses `SHOW INDEX FROM table` when the statement belongs to the catalog
/// surface.
///
/// MySQL spells this three ways and takes either `FROM` or `IN`, so all six
/// spellings are read. Other `SHOW` forms return `None` for their own parser.
pub fn parse_optional_show_index(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlShowIndexCommand>, ParseError> {
    let tokens = tokenize_admin_command(sql, mode)?;
    let mut cursor = skip_admin_comments(&tokens, 0);
    if !consume_admin_word(&tokens, &mut cursor, "SHOW") {
        return Ok(None);
    }
    if !["INDEX", "INDEXES", "KEYS"]
        .iter()
        .any(|word| consume_admin_word(&tokens, &mut cursor, word))
    {
        return Ok(None);
    }
    if !["FROM", "IN"]
        .iter()
        .any(|word| consume_admin_word(&tokens, &mut cursor, word))
    {
        return Err(ParseError::ExpectedAdminCommand);
    }
    let (database, table) = consume_admin_qualified_table_name(&tokens, &mut cursor)?;
    if !admin_command_ends(&tokens, cursor) {
        return Err(ParseError::TrailingAdminCommandTokens);
    }
    Ok(Some(MySqlShowIndexCommand { database, table }))
}

/// Parses the strict `SHOW [SESSION|GLOBAL] VARIABLES [LIKE 'pattern']` command.
pub fn parse_show_variables(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<MySqlShowVariablesCommand, ParseError> {
    parse_optional_show_variables(sql, mode)?.ok_or(ParseError::Unsupported {
        feature: "SHOW VARIABLES statement",
    })
}

/// Parses `SHOW VARIABLES` when the statement belongs to the variable surface.
///
/// Other `SHOW` forms return `None` so that their own parser can handle them.
/// MySQL also takes a `WHERE` clause here; that form is rejected rather than
/// answered from a pattern it did not ask for.
pub fn parse_optional_show_variables(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlShowVariablesCommand>, ParseError> {
    // This parser runs before every other statement surface, so text it cannot
    // even split into tokens belongs to whichever parser comes next.
    let Ok(tokens) = tokenize_admin_command(sql, mode) else {
        return Ok(None);
    };
    let mut cursor = skip_admin_comments(&tokens, 0);
    if !consume_admin_word(&tokens, &mut cursor, "SHOW") {
        return Ok(None);
    }
    let scope = if consume_admin_word(&tokens, &mut cursor, "GLOBAL") {
        MySqlVariableScope::Global
    } else {
        // Measured on MySQL 8.4.11: `LOCAL` names the session scope too.
        let _ = consume_admin_word(&tokens, &mut cursor, "SESSION")
            || consume_admin_word(&tokens, &mut cursor, "LOCAL");
        MySqlVariableScope::Session
    };
    if !consume_admin_word(&tokens, &mut cursor, "VARIABLES") {
        return Ok(None);
    }
    let pattern = if consume_admin_word(&tokens, &mut cursor, "LIKE") {
        let Some(AdminToken::StringLiteral(pattern)) = tokens.get(cursor) else {
            return Err(ParseError::ExpectedAdminCommand);
        };
        cursor += 1;
        Some(MySqlLikePattern::new(pattern, mode))
    } else {
        None
    };
    if !admin_command_ends(&tokens, cursor) {
        return Err(ParseError::TrailingAdminCommandTokens);
    }
    Ok(Some(MySqlShowVariablesCommand { scope, pattern }))
}

/// Parses the strict `SHOW CREATE TABLE table` catalog command.
pub fn parse_show_create_table(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<MySqlShowCreateTableCommand, ParseError> {
    parse_optional_show_create_table(sql, mode)?.ok_or(ParseError::Unsupported {
        feature: "SHOW CREATE TABLE statement",
    })
}

/// Parses `SHOW CREATE TABLE table` when the statement belongs to the catalog
/// surface.
///
/// Other `SHOW` forms return `None` so that their own parser can handle them.
/// Once `SHOW CREATE TABLE` is recognized, this accepts one unqualified
/// identifier and an optional single semicolon. Comments, clauses, database
/// qualifiers, and additional statements are rejected, matching the other
/// catalog commands. MySQL is looser: it also takes a leading comment, a
/// second semicolon, and a `db.table` qualifier.
pub fn parse_optional_show_create_table(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlShowCreateTableCommand>, ParseError> {
    let tokens = tokenize_admin_command(sql, mode)?;
    let mut cursor = skip_admin_comments(&tokens, 0);
    if !consume_admin_word(&tokens, &mut cursor, "SHOW") {
        return Ok(None);
    }
    if !consume_admin_word(&tokens, &mut cursor, "CREATE") {
        return Ok(None);
    }
    if !consume_admin_word(&tokens, &mut cursor, "TABLE") {
        return Ok(None);
    }
    let (database, table) = consume_admin_qualified_table_name(&tokens, &mut cursor)?;
    if !admin_command_ends(&tokens, cursor) {
        return Err(ParseError::TrailingAdminCommandTokens);
    }
    Ok(Some(MySqlShowCreateTableCommand { database, table }))
}

/// Parses the strict `DESCRIBE table` catalog command.
pub fn parse_describe(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<MySqlShowColumnsCommand, ParseError> {
    parse_optional_describe(sql, mode)?.ok_or(ParseError::Unsupported {
        feature: "DESCRIBE statement",
    })
}

/// Parses `DESCRIBE table` or its minimal `DESC table` alias.
///
/// Other commands return `None` so their own parser can handle them. Once
/// either keyword is recognized, this accepts one unqualified identifier and
/// an optional single semicolon. Comments, clauses, database qualifiers, and
/// additional statements are rejected.
pub fn parse_optional_describe(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlShowColumnsCommand>, ParseError> {
    let tokens = tokenize_admin_command(sql, mode)?;
    let mut cursor = skip_admin_comments(&tokens, 0);
    if !consume_admin_word(&tokens, &mut cursor, "DESCRIBE")
        && !consume_admin_word(&tokens, &mut cursor, "DESC")
    {
        return Ok(None);
    }
    let (database, table) = consume_admin_qualified_table_name(&tokens, &mut cursor)?;
    if !admin_command_ends(&tokens, cursor) {
        return Err(ParseError::TrailingAdminCommandTokens);
    }
    Ok(Some(MySqlShowColumnsCommand { database, table }))
}

/// One checked MySQL transaction-control command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MySqlTransactionCommand {
    Begin,
    Commit,
    Rollback,
}

/// One checked change to the MySQL session's autocommit mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MySqlAutocommitSetting {
    pub enabled: bool,
}

/// One MySQL driver bootstrap query recognized by this parser.
///
/// This intentionally identifies the complete wire query, not general MySQL
/// `SELECT` syntax. It keeps the bootstrap response contract separate from
/// queries that happen to read system variables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MySqlDriverBootstrapQuery {
    MaxAllowedPacketAndWaitTimeout,
}

/// Parses the exact settings query sent by the pinned `mysql_async` driver.
///
/// The driver constructs this query as `SELECT @@max_allowed_packet,@@wait_timeout`.
/// It does not send a semicolon, aliases, qualifiers, or extra whitespace.
/// Accepting only those bytes prevents this bootstrap path from becoming a
/// general system-variable SELECT parser.
pub fn parse_driver_bootstrap_query(sql: &str) -> Result<MySqlDriverBootstrapQuery, ParseError> {
    if sql == "SELECT @@max_allowed_packet,@@wait_timeout" {
        Ok(MySqlDriverBootstrapQuery::MaxAllowedPacketAndWaitTimeout)
    } else {
        unsupported("mysql_async driver bootstrap query")
    }
}

/// Parses a strict `SET [SESSION] autocommit = 0|1` statement.
pub fn parse_autocommit_setting(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<MySqlAutocommitSetting, ParseError> {
    parse_optional_autocommit_setting(sql, mode)?.ok_or(ParseError::Unsupported {
        feature: "SET autocommit statement",
    })
}

/// Parses a supported autocommit assignment when the statement starts with `SET`.
pub fn parse_optional_autocommit_setting(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlAutocommitSetting>, ParseError> {
    let dialect = SessionMySqlDialect::without_executable_comments(mode);
    let tokens = Tokenizer::new(&dialect, sql)
        .tokenize()
        .map_err(|error| ParseError::Sqlparser(error.to_string()))?;
    let tokens = tokens
        .iter()
        .filter(|token| {
            !matches!(
                token,
                Token::Whitespace(Whitespace::Space | Whitespace::Newline | Whitespace::Tab)
            )
        })
        .collect::<Vec<_>>();
    let Some(first_significant) = tokens
        .iter()
        .find(|token| !matches!(token, Token::Whitespace(_)))
    else {
        return Ok(None);
    };
    if !is_unquoted_word(first_significant, "SET") {
        return Ok(None);
    }
    if tokens
        .iter()
        .any(|token| matches!(token, Token::Whitespace(_)))
    {
        return unsupported("comments in SET autocommit");
    }
    let tokens = tokens.strip_suffix(&[&Token::SemiColon]).unwrap_or(&tokens);
    let assignment = match tokens {
        [set, name, equals, value]
            if is_unquoted_word(set, "SET")
                && is_unquoted_word(name, "AUTOCOMMIT")
                && matches!(equals, Token::Eq) =>
        {
            value
        }
        [set, session, name, equals, value]
            if is_unquoted_word(set, "SET")
                && is_unquoted_word(session, "SESSION")
                && is_unquoted_word(name, "AUTOCOMMIT")
                && matches!(equals, Token::Eq) =>
        {
            value
        }
        _ => return unsupported("SET autocommit syntax"),
    };
    let enabled = match assignment {
        Token::Number(value, false) if value == "0" => false,
        Token::Number(value, false) if value == "1" => true,
        _ => return unsupported("SET autocommit value; expected 0 or 1"),
    };
    Ok(Some(MySqlAutocommitSetting { enabled }))
}

/// Parses exactly one transaction-control command without options.
pub fn parse_transaction_command(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<MySqlTransactionCommand, ParseError> {
    parse_optional_transaction_command(sql, mode)?.ok_or(ParseError::ExpectedTransactionCommand)
}

/// Parses a transaction-control command when the statement belongs to that surface.
///
/// `BEGIN` and `START TRANSACTION` both return [`MySqlTransactionCommand::Begin`].
/// Transaction modes, chain modifiers, savepoints, comments, and additional
/// statements are rejected.
pub fn parse_optional_transaction_command(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlTransactionCommand>, ParseError> {
    let token_kind = transaction_token_kind(sql, mode)?;
    if token_kind == TransactionTokenKind::Other {
        return Ok(None);
    }
    if token_kind == TransactionTokenKind::Invalid {
        return unsupported("transaction options");
    }
    let statement = parse_one_statement(sql, mode)?;
    let command = match statement {
        Statement::StartTransaction {
            modes,
            begin,
            transaction,
            modifier,
            statements,
            exception,
            has_end_keyword,
        } => {
            if !modes.is_empty()
                || modifier.is_some()
                || !statements.is_empty()
                || exception.is_some()
                || has_end_keyword
                || (!begin && transaction.is_none())
            {
                return unsupported("transaction options");
            }
            MySqlTransactionCommand::Begin
        }
        Statement::Commit {
            chain,
            end,
            modifier,
        } => {
            if chain || end || modifier.is_some() {
                return unsupported("COMMIT options");
            }
            MySqlTransactionCommand::Commit
        }
        Statement::Rollback { chain, savepoint } => {
            if chain || savepoint.is_some() {
                return unsupported("ROLLBACK options");
            }
            MySqlTransactionCommand::Rollback
        }
        _ => return Ok(None),
    };
    Ok(Some(command))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransactionTokenKind {
    Plain,
    Invalid,
    Other,
}

fn transaction_token_kind(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<TransactionTokenKind, ParseError> {
    let dialect = SessionMySqlDialect::new(mode);
    let tokens = Tokenizer::new(&dialect, sql)
        .tokenize()
        .map_err(|error| ParseError::Sqlparser(error.to_string()))?;
    let significant = tokens
        .iter()
        .filter(|token| {
            !matches!(
                token,
                Token::Whitespace(Whitespace::Space | Whitespace::Newline | Whitespace::Tab)
            )
        })
        .collect::<Vec<_>>();
    let Some(first_word) = significant
        .iter()
        .find(|token| matches!(token, Token::Word(_)))
    else {
        return Ok(TransactionTokenKind::Other);
    };
    if !["BEGIN", "START", "COMMIT", "ROLLBACK"]
        .iter()
        .any(|keyword| is_unquoted_word(first_word, keyword))
    {
        return Ok(TransactionTokenKind::Other);
    }
    let significant = significant
        .strip_suffix(&[&Token::SemiColon])
        .unwrap_or(&significant);
    let plain = matches!(
        significant,
        [token] if is_unquoted_word(token, "BEGIN")
            || is_unquoted_word(token, "COMMIT")
            || is_unquoted_word(token, "ROLLBACK")
    ) || matches!(
        significant,
        [start, transaction]
            if is_unquoted_word(start, "START")
                && is_unquoted_word(transaction, "TRANSACTION")
    );
    Ok(if plain {
        TransactionTokenKind::Plain
    } else {
        TransactionTokenKind::Invalid
    })
}

/// Parses one strict MySQL database-management command.
///
/// The accepted grammar is exactly one of `CREATE DATABASE name`, `DROP
/// DATABASE name`, `USE name`, or `SHOW DATABASES`, followed by an optional
/// semicolon. Database options, `IF EXISTS` clauses, comments, qualified
/// names, and all trailing tokens are rejected. Names are checked and returned
/// in canonical ASCII-lowercase form.
pub fn parse_admin_command(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<MySqlAdminCommand, ParseError> {
    parse_optional_admin_command(sql, mode)?.ok_or(ParseError::ExpectedAdminCommand)
}

/// Parses one strict database-management command when the statement belongs to
/// this parser's small administration surface.
///
/// Returns `None` for statements outside that surface, such as `SELECT`,
/// `CREATE TABLE`, and `SHOW TABLES`. Once a statement begins one of the
/// supported forms, malformed syntax remains an error rather than falling back
/// to another SQL path.
pub fn parse_optional_admin_command(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlAdminCommand>, ParseError> {
    let tokens = tokenize_admin_command(sql, mode)?;
    let mut cursor = skip_admin_comments(&tokens, 0);
    let had_leading_comment = cursor != 0;
    let Some(kind) = admin_statement_kind(&tokens, &mut cursor)? else {
        return Ok(None);
    };
    if had_leading_comment {
        return Err(ParseError::Unsupported {
            feature: "comments in database-management command",
        });
    }
    let command = match kind {
        AdminStatementKind::CreateDatabase => MySqlAdminCommand::CreateDatabase {
            name: consume_admin_database_name(&tokens, &mut cursor)?,
        },
        AdminStatementKind::DropDatabase => MySqlAdminCommand::DropDatabase {
            name: consume_admin_database_name(&tokens, &mut cursor)?,
        },
        AdminStatementKind::Use => MySqlAdminCommand::Use {
            name: consume_admin_database_name(&tokens, &mut cursor)?,
        },
        AdminStatementKind::ListDatabases => MySqlAdminCommand::ListDatabases,
    };

    if matches!(tokens.get(cursor), Some(AdminToken::Semicolon)) {
        cursor += 1;
    }
    if cursor != tokens.len() {
        return Err(ParseError::TrailingAdminCommandTokens);
    }
    Ok(Some(command))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AdminToken {
    Word(String),
    QuotedIdentifier(String),
    /// The decoded contents of a `'...'` string literal.
    StringLiteral(String),
    Semicolon,
    /// The `.` that separates a database from a table.
    Dot,
    Comment,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdminStatementKind {
    CreateDatabase,
    DropDatabase,
    Use,
    ListDatabases,
}

fn tokenize_admin_command(sql: &str, mode: SessionSqlMode) -> Result<Vec<AdminToken>, ParseError> {
    let bytes = sql.as_bytes();
    let mut tokens = Vec::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        if byte.is_ascii_whitespace() {
            cursor += 1;
            continue;
        }
        if byte == b'#'
            || (byte == b'-' && bytes.get(cursor + 1) == Some(&b'-'))
            || (byte == b'/' && bytes.get(cursor + 1) == Some(&b'*'))
        {
            tokens.push(AdminToken::Comment);
            cursor = consume_admin_comment(bytes, cursor);
            continue;
        }
        if byte == b';' {
            tokens.push(AdminToken::Semicolon);
            cursor += 1;
            continue;
        }
        if byte == b'`' || (byte == b'"' && mode.ansi_quotes) {
            let quote = byte;
            cursor += 1;
            let mut value = String::new();
            let mut closed = false;
            while cursor < bytes.len() {
                let current = bytes[cursor];
                if current == quote {
                    if bytes.get(cursor + 1) == Some(&quote) {
                        value.push(char::from(quote));
                        cursor += 2;
                    } else {
                        cursor += 1;
                        closed = true;
                        break;
                    }
                } else {
                    value.push(char::from(current));
                    cursor += 1;
                }
            }
            if !closed {
                return Err(ParseError::Sqlparser(
                    "unterminated quoted database name".to_string(),
                ));
            }
            tokens.push(AdminToken::QuotedIdentifier(value));
            continue;
        }
        if byte == b'\'' || byte == b'"' {
            // A double quote only reaches here when ANSI_QUOTES is off, which
            // is exactly when MySQL reads it as a string literal.
            let (value, next) = consume_admin_string_literal(bytes, cursor, mode)?;
            tokens.push(AdminToken::StringLiteral(value));
            cursor = next;
            continue;
        }
        if byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$' {
            let start = cursor;
            cursor += 1;
            while let Some(next) = bytes.get(cursor) {
                if next.is_ascii_alphanumeric() || *next == b'_' || *next == b'$' {
                    cursor += 1;
                } else {
                    break;
                }
            }
            tokens.push(AdminToken::Word(sql[start..cursor].to_string()));
            continue;
        }
        tokens.push(if bytes[cursor] == b'.' {
            AdminToken::Dot
        } else {
            AdminToken::Other
        });
        cursor += 1;
    }
    Ok(tokens)
}

/// Reads one MySQL string literal and returns its value and the byte after it.
///
/// `cursor` points at the opening quote. MySQL doubles the quote to include it,
/// and outside `NO_BACKSLASH_ESCAPES` it also takes backslash escapes. `\%` and
/// `\_` keep their backslash so that a later pattern match still sees an escape;
/// every other unlisted escape drops the backslash.
fn consume_admin_string_literal(
    bytes: &[u8],
    cursor: usize,
    mode: SessionSqlMode,
) -> Result<(String, usize), ParseError> {
    let quote = bytes[cursor];
    let mut cursor = cursor + 1;
    let mut value = Vec::new();
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        if byte == quote {
            if bytes.get(cursor + 1) == Some(&quote) {
                value.push(quote);
                cursor += 2;
                continue;
            }
            return Ok((decoded_string_literal(value), cursor + 1));
        }
        if byte == b'\\' && !mode.no_backslash_escapes {
            let Some(escaped) = bytes.get(cursor + 1).copied() else {
                break;
            };
            match escaped {
                b'0' => value.push(0),
                b'b' => value.push(0x08),
                b'n' => value.push(b'\n'),
                b'r' => value.push(b'\r'),
                b't' => value.push(b'\t'),
                b'Z' => value.push(0x1a),
                b'%' | b'_' => value.extend_from_slice(&[b'\\', escaped]),
                other => value.push(other),
            }
            cursor += 2;
            continue;
        }
        value.push(byte);
        cursor += 1;
    }
    Err(ParseError::Sqlparser(
        "unterminated string literal".to_string(),
    ))
}

/// Rebuilds the literal's text from bytes copied out of a `&str`.
///
/// Every byte either came from the source string or is one this decoder wrote,
/// and the decoder only writes ASCII, so the bytes stay valid UTF-8.
fn decoded_string_literal(value: Vec<u8>) -> String {
    String::from_utf8(value).expect("string literal bytes come from a &str")
}

fn consume_admin_comment(bytes: &[u8], cursor: usize) -> usize {
    match bytes[cursor] {
        b'#' => bytes[cursor..]
            .iter()
            .position(|byte| *byte == b'\n' || *byte == b'\r')
            .map_or(bytes.len(), |offset| cursor + offset),
        b'-' => bytes[cursor + 2..]
            .iter()
            .position(|byte| *byte == b'\n' || *byte == b'\r')
            .map_or(bytes.len(), |offset| cursor + 2 + offset),
        b'/' => bytes[cursor + 2..]
            .windows(2)
            .position(|window| window == b"*/")
            .map_or(bytes.len(), |offset| cursor + 2 + offset + 2),
        _ => unreachable!("only comment starters reach this helper"),
    }
}

fn skip_admin_comments(tokens: &[AdminToken], mut cursor: usize) -> usize {
    while matches!(tokens.get(cursor), Some(AdminToken::Comment)) {
        cursor += 1;
    }
    cursor
}

/// Checks that nothing but what MySQL allows follows a catalog command.
///
/// Measured on MySQL 8.4.11: `SHOW TABLES;;`, `SHOW COLUMNS FROM t # x` and
/// `DESCRIBE t -- x` are all accepted, so any number of semicolons and any
/// comments among them end the statement.
fn admin_command_ends(tokens: &[AdminToken], mut cursor: usize) -> bool {
    loop {
        cursor = skip_admin_comments(tokens, cursor);
        match tokens.get(cursor) {
            None => return true,
            Some(AdminToken::Semicolon) => cursor += 1,
            Some(_) => return false,
        }
    }
}

fn admin_statement_kind(
    tokens: &[AdminToken],
    cursor: &mut usize,
) -> Result<Option<AdminStatementKind>, ParseError> {
    if consume_admin_word(tokens, cursor, "CREATE") {
        return admin_database_statement_kind(tokens, cursor, AdminStatementKind::CreateDatabase);
    }
    if consume_admin_word(tokens, cursor, "DROP") {
        return admin_database_statement_kind(tokens, cursor, AdminStatementKind::DropDatabase);
    }
    if consume_admin_word(tokens, cursor, "USE") {
        return Ok(Some(AdminStatementKind::Use));
    }
    if consume_admin_word(tokens, cursor, "SHOW") {
        return admin_database_statement_kind(tokens, cursor, AdminStatementKind::ListDatabases);
    }
    Ok(None)
}

fn admin_database_statement_kind(
    tokens: &[AdminToken],
    cursor: &mut usize,
    kind: AdminStatementKind,
) -> Result<Option<AdminStatementKind>, ParseError> {
    let expected = match kind {
        AdminStatementKind::CreateDatabase | AdminStatementKind::DropDatabase => "DATABASE",
        AdminStatementKind::ListDatabases => "DATABASES",
        AdminStatementKind::Use => unreachable!("USE does not have a second keyword"),
    };
    if consume_admin_word(tokens, cursor, expected) {
        return Ok(Some(kind));
    }
    match tokens.get(*cursor) {
        Some(AdminToken::Word(_)) => Ok(None),
        Some(_) | None => Err(ParseError::ExpectedAdminCommand),
    }
}

fn consume_admin_word(tokens: &[AdminToken], cursor: &mut usize, expected: &str) -> bool {
    let Some(AdminToken::Word(word)) = tokens.get(*cursor) else {
        return false;
    };
    if !word.eq_ignore_ascii_case(expected) {
        return false;
    }
    *cursor += 1;
    true
}

fn consume_admin_database_name(
    tokens: &[AdminToken],
    cursor: &mut usize,
) -> Result<MySqlDatabaseName, ParseError> {
    let token = tokens
        .get(*cursor)
        .ok_or(ParseError::ExpectedAdminCommand)?;
    let name = match token {
        AdminToken::Word(name) => {
            if is_admin_keyword(name) {
                return Err(ParseError::ExpectedAdminCommand);
            }
            name.as_str()
        }
        AdminToken::QuotedIdentifier(name) => name.as_str(),
        AdminToken::StringLiteral(_)
        | AdminToken::Semicolon
        | AdminToken::Dot
        | AdminToken::Comment
        | AdminToken::Other => {
            return Err(ParseError::ExpectedAdminCommand);
        }
    };
    *cursor += 1;
    MySqlDatabaseName::parse(name)
}

fn consume_admin_table_name(
    tokens: &[AdminToken],
    cursor: &mut usize,
) -> Result<MySqlTableName, ParseError> {
    let token = tokens
        .get(*cursor)
        .ok_or(ParseError::ExpectedAdminCommand)?;
    let name = match token {
        AdminToken::Word(name) | AdminToken::QuotedIdentifier(name) => name.as_str(),
        AdminToken::StringLiteral(_)
        | AdminToken::Semicolon
        | AdminToken::Dot
        | AdminToken::Comment
        | AdminToken::Other => {
            return Err(ParseError::ExpectedAdminCommand);
        }
    };
    let name = MySqlTableName::parse(name)?;
    *cursor += 1;
    Ok(name)
}

/// Reads `table` or `database.table`, which MySQL takes wherever it takes a
/// catalog table name.
fn consume_admin_qualified_table_name(
    tokens: &[AdminToken],
    cursor: &mut usize,
) -> Result<(Option<MySqlDatabaseName>, MySqlTableName), ParseError> {
    let first = consume_admin_table_name(tokens, cursor)?;
    if !matches!(tokens.get(*cursor), Some(AdminToken::Dot)) {
        return Ok((None, first));
    }
    *cursor += 1;
    let table = consume_admin_table_name(tokens, cursor)?;
    Ok((Some(MySqlDatabaseName::parse(first.as_str())?), table))
}

fn is_admin_keyword(word: &str) -> bool {
    matches!(
        word.to_ascii_uppercase().as_str(),
        "CREATE"
            | "DATABASE"
            | "DROP"
            | "USE"
            | "SHOW"
            | "DATABASES"
            | "SCHEMA"
            | "IF"
            | "NOT"
            | "EXISTS"
            | "CHARACTER"
            | "SET"
            | "COLLATE"
            | "ENCRYPTION"
            | "COMMENT"
            | "READ"
            | "ONLY"
    )
}

/// Parses exactly one MySQL `CREATE TABLE` statement and translates the supported subset to SQLite.
pub fn parse_create_table(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<TranslatedCreateTable, ParseError> {
    reject_unsupported_mysql_string_escapes(sql, mode)?;
    let statement = parse_one_statement(sql, mode)?;
    let Statement::CreateTable(table) = statement else {
        return Err(ParseError::ExpectedCreateTable);
    };
    translate_create_table(&table)
}

/// Parses the deliberately narrow MySQL `AUTO_INCREMENT` table shape.
///
/// This is separate from [`parse_create_table`] while the frontend has no
/// allocator-backed execution path. It accepts exactly one inline signed `INT`
/// or `INTEGER NOT NULL AUTO_INCREMENT PRIMARY KEY` column in that token order.
pub fn parse_auto_increment_create_table(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<CheckedAutoIncrementCreateTable, ParseError> {
    reject_unsupported_mysql_string_escapes(sql, mode)?;
    validate_auto_increment_token_shape(sql, mode)?;
    let statement = parse_one_statement(sql, mode)?;
    let Statement::CreateTable(table) = statement else {
        return Err(ParseError::ExpectedCreateTable);
    };
    translate_auto_increment_create_table(&table, mode)
}

fn validate_auto_increment_token_shape(sql: &str, mode: SessionSqlMode) -> Result<(), ParseError> {
    let dialect = SessionMySqlDialect::without_executable_comments(mode);
    let tokens = Tokenizer::new(&dialect, sql)
        .tokenize()
        .map_err(|error| ParseError::Sqlparser(error.to_string()))?;
    if tokens.iter().any(|token| {
        matches!(
            token,
            Token::Whitespace(Whitespace::MultiLineComment(comment)) if comment.starts_with('!')
        )
    }) {
        return unsupported("executable comment in AUTO_INCREMENT definition");
    }
    let tokens = tokens
        .iter()
        .filter(|token| !matches!(token, Token::Whitespace(_)))
        .collect::<Vec<_>>();
    let positions = tokens
        .iter()
        .enumerate()
        .filter_map(|(index, token)| is_unquoted_word(token, "AUTO_INCREMENT").then_some(index))
        .collect::<Vec<_>>();
    let [position] = positions.as_slice() else {
        return unsupported("exactly one AUTO_INCREMENT token");
    };
    let position = *position;
    if position < 2
        || position + 3 >= tokens.len()
        || !is_unquoted_word(tokens[position - 2], "NOT")
        || !is_unquoted_word(tokens[position - 1], "NULL")
        || !is_unquoted_word(tokens[position + 1], "PRIMARY")
        || !is_unquoted_word(tokens[position + 2], "KEY")
        || !matches!(tokens[position + 3], Token::Comma | Token::RParen)
    {
        return unsupported(
            "AUTO_INCREMENT token order; expected NOT NULL AUTO_INCREMENT PRIMARY KEY",
        );
    }
    Ok(())
}

fn is_unquoted_word(token: &Token, expected: &str) -> bool {
    matches!(
        token,
        Token::Word(word)
            if word.quote_style.is_none() && word.value.eq_ignore_ascii_case(expected)
    )
}

/// Parses exactly one MySQL `SELECT` statement and translates the supported
/// semantics-preserving subset to SQLite SQL.
pub fn parse_select(sql: &str, mode: SessionSqlMode) -> Result<TranslatedSelect, ParseError> {
    let statement = parse_one_statement(sql, mode)?;
    let Statement::Query(query) = statement else {
        return Err(ParseError::ExpectedSelect);
    };
    let tokens = Tokenizer::new(&SessionMySqlDialect::new(mode), sql)
        .tokenize()
        .map_err(|error| ParseError::Sqlparser(error.to_string()))?;
    let significant = tokens
        .iter()
        .filter(|token| !matches!(token, Token::Whitespace(_)))
        .collect::<Vec<_>>();
    // sqlparser drops LIMIT ALL from the query AST.
    if significant
        .windows(2)
        .any(|tokens| is_unquoted_word(tokens[0], "LIMIT") && is_unquoted_word(tokens[1], "ALL"))
    {
        return unsupported("SELECT LIMIT ALL");
    }
    let static_result_metadata = select_static_result_metadata(&query);
    let RenderedSelect {
        sqlite_sql,
        source_table,
        checked_comparisons,
        parameter_count,
    } = translate_select_query(&query)?;
    Ok(TranslatedSelect {
        reads_table: source_table.is_some(),
        sqlite_sql,
        source_table,
        static_result_metadata,
        checked_comparisons,
        parameter_count,
    })
}

/// Parses one checked MySQL `SELECT` into Turso's SQLite AST.
pub fn parse_select_ast(sql: &str, mode: SessionSqlMode) -> Result<Stmt, ParseError> {
    let translated = parse_select(sql, mode)?;
    translated.parse_ast()
}

/// Parses exactly one MySQL `INSERT`, `UPDATE`, or `DELETE` statement in the checked DML subset.
pub fn parse_dml(sql: &str, mode: SessionSqlMode) -> Result<TranslatedDml, ParseError> {
    let statement = parse_one_statement(sql, mode)?;
    let mut render_context = SelectRenderContext::default();
    let (sqlite_sql, checked_update, source_table) = match statement {
        Statement::Insert(insert) => (translate_insert(&insert)?, None, None),
        Statement::Update(update) => {
            let checked = checked_update(&update)?;
            let table = checked.table_name().to_owned();
            (
                translate_update(&update, &mut render_context)?,
                Some(checked),
                Some(table),
            )
        }
        Statement::Delete(delete) => (
            translate_delete(&delete, &mut render_context)?,
            None,
            delete_source_table(&delete),
        ),
        _ => return Err(ParseError::ExpectedDml),
    };
    Ok(TranslatedDml {
        sqlite_sql,
        checked_update,
        checked_comparisons: render_context.checked_comparisons,
        source_table,
    })
}

/// Parses one checked MySQL `INSERT`, `UPDATE`, or `DELETE` into Turso's SQLite AST.
pub fn parse_dml_ast(sql: &str, mode: SessionSqlMode) -> Result<Stmt, ParseError> {
    let translated = parse_dml(sql, mode)?;
    translated.parse_ast()
}

/// Parses the first literal-only executable AUTO_INCREMENT INSERT slice.
///
/// This accepts one unqualified table, an explicit unique column list, and a
/// statically known nonempty `VALUES` batch whose expressions are direct
/// literals. The allocator column is checked separately by
/// [`CheckedAutoIncrementInsert::bind_allocator_table`] because only the
/// frontend has the durable table definition.
pub fn parse_auto_increment_insert(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<CheckedAutoIncrementInsert, ParseError> {
    parse_checked_auto_increment_insert(sql, mode, is_direct_insert_literal)
}

/// Parses one AUTO_INCREMENT INSERT that can be executed through a prepared
/// statement.
///
/// This accepts the literal-only direct-execution subset plus bare `?` values.
/// The fixed VALUES shape lets the frontend reserve one ID per row before it
/// injects those IDs as literals, without changing user parameter positions.
pub fn parse_prepared_auto_increment_insert(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<CheckedAutoIncrementInsert, ParseError> {
    parse_checked_auto_increment_insert(sql, mode, is_prepared_insert_value)
}

fn parse_checked_auto_increment_insert(
    sql: &str,
    mode: SessionSqlMode,
    accepts_value: fn(&Expr) -> bool,
) -> Result<CheckedAutoIncrementInsert, ParseError> {
    validate_auto_increment_insert_token_shape(sql, mode)?;
    let statement = parse_one_statement(sql, mode)?;
    let Statement::Insert(insert) = &statement else {
        return Err(ParseError::ExpectedDml);
    };

    let sqlparser::ast::TableObject::TableName(table) = &insert.table else {
        return unsupported("INSERT table source");
    };
    let table_name = insert_name(table)?;
    if insert.columns.is_empty() {
        return unsupported("INSERT without an explicit column list");
    }
    let columns = insert
        .columns
        .iter()
        .map(insert_name)
        .collect::<Result<Vec<_>, _>>()?;
    if columns.iter().enumerate().any(|(index, column)| {
        columns[..index]
            .iter()
            .any(|previous| previous.as_str().eq_ignore_ascii_case(column.as_str()))
    }) {
        return unsupported("duplicate INSERT column");
    }

    let source = insert.source.as_deref().ok_or(ParseError::Unsupported {
        feature: "INSERT without VALUES",
    })?;
    let sqlparser::ast::SetExpr::Values(values) = source.body.as_ref() else {
        return unsupported("INSERT source");
    };
    if values.explicit_row || values.value_keyword || values.rows.is_empty() {
        return unsupported("INSERT VALUES option");
    }
    for row in &values.rows {
        if row.is_empty() || row.len() != columns.len() {
            return unsupported("INSERT VALUES column count");
        }
        if !row.iter().all(accepts_value) {
            return unsupported("INSERT VALUES expression");
        }
    }

    // Reuse the existing checked SQL normalizer only after the stricter shape
    // checks above. The executable path exposes the typed AST, not this SQL.
    let normalized = translate_insert(insert)?;
    let sqlite_statement = parse_normalized_dml(&normalized)?;
    let row_count = NonZeroUsize::new(values.rows.len()).ok_or(ParseError::Unsupported {
        feature: "INSERT without VALUES rows",
    })?;
    Ok(CheckedAutoIncrementInsert {
        table_name,
        columns,
        row_count,
        sqlite_statement,
    })
}

/// Returns the unqualified target of one MySQL `INSERT`, without accepting it
/// for AUTO_INCREMENT range injection.
///
/// The frontend uses this after a narrower executable parse rejects an INSERT
/// so a marked table cannot fall through to generic execution.
pub fn parse_auto_increment_insert_target(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<String>, ParseError> {
    let statement = parse_one_statement(sql, mode)?;
    let Statement::Insert(insert) = statement else {
        return Ok(None);
    };
    let sqlparser::ast::TableObject::TableName(table) = insert.table else {
        return unsupported("INSERT table source");
    };
    Ok(Some(insert_name(&table)?.as_str().to_owned()))
}

fn validate_auto_increment_insert_token_shape(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<(), ParseError> {
    let dialect = SessionMySqlDialect::without_executable_comments(mode);
    let tokens = Tokenizer::new(&dialect, sql)
        .tokenize()
        .map_err(|error| ParseError::Sqlparser(error.to_string()))?;
    if tokens.iter().any(|token| {
        matches!(
            token,
            Token::Whitespace(
                Whitespace::SingleLineComment { .. } | Whitespace::MultiLineComment(_)
            )
        )
    }) {
        return unsupported("comment in AUTO_INCREMENT INSERT");
    }
    Ok(())
}

fn insert_name(name: &ObjectName) -> Result<TursoName, ParseError> {
    let [ObjectNamePart::Identifier(ident)] = name.0.as_slice() else {
        return unsupported("qualified or dynamic INSERT name");
    };
    Ok(TursoName::exact(ident.value.clone()))
}

fn is_direct_insert_literal(expr: &Expr) -> bool {
    match expr {
        Expr::Value(value) => match &value.value {
            Value::Number(value, false) => {
                value.parse::<i64>().is_ok() || value.parse::<f64>().is_ok_and(f64::is_finite)
            }
            Value::SingleQuotedString(_) | Value::DoubleQuotedString(_) => true,
            Value::Boolean(_) | Value::Null => true,
            _ => false,
        },
        Expr::UnaryOp {
            op: UnaryOperator::Minus | UnaryOperator::Plus,
            expr,
        } => {
            let Expr::Value(value) = expr.as_ref() else {
                return false;
            };
            let Value::Number(value, false) = &value.value else {
                return false;
            };
            let Ok(magnitude) = value.parse::<u64>() else {
                return false;
            };
            magnitude <= (i64::MAX as u64) + 1
        }
        _ => false,
    }
}

fn is_prepared_insert_value(expr: &Expr) -> bool {
    is_direct_insert_literal(expr)
        || matches!(
            expr,
            Expr::Value(value) if matches!(&value.value, Value::Placeholder(marker) if marker == "?")
        )
}

/// Rebuilds strict signed-width metadata from normalized MySQL table DDL.
///
/// This deliberately reparses the durable MySQL statement instead of looking
/// at SQLite affinity names. `TINYINT` and `INT` share i64 storage but have
/// different MySQL assignment ranges.
pub fn parse_mysql_numeric_spec(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<MySqlNumericSpec, ParseError> {
    reject_unsupported_mysql_string_escapes(sql, mode)?;
    let statement = parse_one_statement(sql, mode)?;
    let Statement::CreateTable(table) = statement else {
        return Err(ParseError::ExpectedCreateTable);
    };
    if let Err(error) = translate_create_table(&table) {
        if parse_auto_increment_create_table(sql, mode).is_err() {
            parse_checked_primary_key_create_table(sql, mode).map_err(|_| error)?;
        }
    }
    Ok(MySqlNumericSpec {
        columns: table
            .columns
            .iter()
            .map(|column| match column.data_type {
                DataType::TinyInt(None) => Some(MySqlSignedInteger::TinyInt),
                DataType::SmallInt(None) => Some(MySqlSignedInteger::SmallInt),
                DataType::MediumInt(None) => Some(MySqlSignedInteger::MediumInt),
                DataType::Int(None) | DataType::Integer(None) => Some(MySqlSignedInteger::Int),
                DataType::BigInt(None) => Some(MySqlSignedInteger::BigInt),
                // MySQL's BOOLEAN is a TINYINT, so it takes the same range.
                DataType::Boolean | DataType::Bool => Some(MySqlSignedInteger::TinyInt),
                _ => None,
            })
            .collect(),
        character_lengths: table
            .columns
            .iter()
            .map(|column| match column.data_type {
                DataType::Varchar(length) | DataType::Char(length) => {
                    declared_character_length(length).ok()
                }
                _ => None,
            })
            .collect(),
        datetimes: table
            .columns
            .iter()
            .map(|column| {
                matches!(
                    column.data_type,
                    DataType::Datetime(None)
                        | DataType::Timestamp(None, sqlparser::ast::TimezoneInfo::None)
                )
            })
            .collect(),
    })
}

/// Parses exactly one checked MySQL `CREATE TABLE` statement into Turso's SQLite AST.
///
/// The MySQL AST is deliberately kept private. The checked normalizer is the boundary
/// between the two parser representations: it rejects unsupported MySQL syntax before
/// the normalized SQLite statement is parsed into the public Turso AST.
pub fn parse_create_table_ast(sql: &str, mode: SessionSqlMode) -> Result<Stmt, ParseError> {
    let translated = match parse_create_table(sql, mode) {
        Ok(translated) => translated,
        Err(error) => {
            if let Ok(checked) = parse_checked_primary_key_create_table(sql, mode) {
                return Ok(checked.sqlite_statement);
            }
            return Err(error);
        }
    };
    let mut parser = TursoParser::new(translated.as_sql().as_bytes());
    let command = parser
        .next_cmd()
        .map_err(|error| ParseError::TursoParser(error.to_string()))?;
    let Some(TursoCmd::Stmt(statement @ Stmt::CreateTable { .. })) = command else {
        return Err(ParseError::TursoParser(
            "normalized CREATE TABLE did not produce a CREATE TABLE AST".to_string(),
        ));
    };
    if parser
        .next_cmd()
        .map_err(|error| ParseError::TursoParser(error.to_string()))?
        .is_some()
    {
        return Err(ParseError::ExpectedOneStatement { actual: 2 });
    }
    Ok(statement)
}

/// Parses exactly one safe MySQL `ALTER TABLE` statement into Turso's SQLite AST.
pub fn parse_alter_table_ast(sql: &str, mode: SessionSqlMode) -> Result<Stmt, ParseError> {
    reject_unsupported_mysql_string_escapes(sql, mode)?;
    let statement = parse_one_statement(sql, mode)?;
    let Statement::AlterTable(alter) = statement else {
        return Err(ParseError::ExpectedAlterTable);
    };
    let normalized = translate_alter_table(&alter)?;
    parse_normalized_alter_table(&normalized)
}

/// Parses exactly one checked MySQL `CREATE INDEX` statement into Turso's SQLite AST.
pub fn parse_create_index_ast(sql: &str, mode: SessionSqlMode) -> Result<Stmt, ParseError> {
    let statement = parse_one_statement(sql, mode)?;
    let Statement::CreateIndex(index) = statement else {
        return Err(ParseError::ExpectedCreateIndex);
    };
    let normalized = translate_create_index(&index)?;
    parse_normalized_create_index(&normalized)
}

/// Parses exactly one checked MySQL `CREATE VIEW` statement into Turso's SQLite AST.
pub fn parse_create_view_ast(sql: &str, mode: SessionSqlMode) -> Result<Stmt, ParseError> {
    let statement = parse_one_statement(sql, mode)?;
    let Statement::CreateView(view) = statement else {
        return Err(ParseError::ExpectedCreateView);
    };
    let normalized = translate_create_view(&view)?;
    parse_normalized_create_view(&normalized)
}

/// Parses exactly one checked MySQL `CREATE TRIGGER` statement into Turso's SQLite AST.
pub fn parse_create_trigger_ast(sql: &str, mode: SessionSqlMode) -> Result<Stmt, ParseError> {
    let statement = parse_one_statement(sql, mode)?;
    let Statement::CreateTrigger(trigger) = statement else {
        return Err(ParseError::ExpectedCreateTrigger);
    };
    let normalized = translate_create_trigger(&trigger)?;
    parse_normalized_create_trigger(&normalized)
}

/// Parses exactly one supported MySQL schema DDL statement into Turso's SQLite AST.
pub fn parse_schema_ddl_ast(sql: &str, mode: SessionSqlMode) -> Result<Stmt, ParseError> {
    reject_unsupported_mysql_string_escapes(sql, mode)?;
    let statement = parse_one_statement(sql, mode)?;
    match statement {
        Statement::CreateTable(table) => match translate_create_table(&table) {
            Ok(translated) => parse_normalized_create_table(translated.as_sql()),
            Err(error) => parse_checked_primary_key_create_table(sql, mode)
                .map(|checked| checked.sqlite_statement)
                .map_err(|_| error),
        },
        Statement::CreateIndex(index) => {
            let normalized = translate_create_index(&index)?;
            parse_normalized_create_index(&normalized)
        }
        Statement::CreateView(view) => {
            let normalized = translate_create_view(&view)?;
            parse_normalized_create_view(&normalized)
        }
        Statement::CreateTrigger(trigger) => {
            let normalized = translate_create_trigger(&trigger)?;
            parse_normalized_create_trigger(&normalized)
        }
        Statement::AlterTable(alter) => {
            let normalized = translate_alter_table(&alter)?;
            parse_normalized_alter_table(&normalized)
        }
        _ => Err(ParseError::Unsupported {
            feature: "schema statement",
        }),
    }
}

fn parse_one_statement(sql: &str, mode: SessionSqlMode) -> Result<Statement, ParseError> {
    let dialect = SessionMySqlDialect::new(mode);
    let statements = Parser::parse_sql(&dialect, sql)
        .map_err(|error| ParseError::Sqlparser(error.to_string()))?;
    let [statement] = statements.as_slice() else {
        return Err(ParseError::ExpectedOneStatement {
            actual: statements.len(),
        });
    };
    Ok(statement.clone())
}

fn tokenize_information_schema_query(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Vec<Token>, ParseError> {
    Tokenizer::new(&SessionMySqlDialect::without_executable_comments(mode), sql)
        .tokenize()
        .map_err(|error| ParseError::Sqlparser(error.to_string()))
}

fn contains_information_schema_tables(tokens: &[Token]) -> bool {
    contains_information_schema_object(tokens, "TABLES")
}

fn contains_information_schema_object(tokens: &[Token], expected_object: &str) -> bool {
    let significant = tokens
        .iter()
        .filter(|token| !matches!(token, Token::Whitespace(_)))
        .collect::<Vec<_>>();
    significant.windows(3).any(|window| {
        is_information_schema_identifier_token(window[0], "information_schema")
            && matches!(window[1], Token::Period)
            && is_information_schema_identifier_token(window[2], expected_object)
    })
}

fn is_information_schema_identifier_token(token: &Token, expected: &str) -> bool {
    matches!(
        token,
        Token::Word(word) if word.value.eq_ignore_ascii_case(expected)
    )
}

fn reject_information_schema_query_tokens(tokens: &[Token]) -> Result<(), ParseError> {
    if tokens.iter().any(|token| {
        matches!(
            token,
            Token::Whitespace(
                Whitespace::SingleLineComment { .. } | Whitespace::MultiLineComment(_)
            )
        )
    }) {
        return unsupported("comments in information_schema.TABLES query");
    }

    let semicolon_count = tokens
        .iter()
        .filter(|token| matches!(token, Token::SemiColon))
        .count();
    if semicolon_count > 1 {
        return unsupported("multiple information_schema.TABLES statements");
    }
    if semicolon_count == 1 {
        let last = tokens
            .iter()
            .rposition(|token| !matches!(token, Token::Whitespace(_)));
        if !matches!(
            last.and_then(|index| tokens.get(index)),
            Some(Token::SemiColon)
        ) {
            return unsupported("information_schema.TABLES semicolon position");
        }
    }
    Ok(())
}

fn validate_information_schema_tables_query(
    query: &sqlparser::ast::Query,
) -> Result<(), ParseError> {
    if query.with.is_some()
        || query.limit_clause.is_some()
        || query.fetch.is_some()
        || !query.locks.is_empty()
        || query.for_clause.is_some()
        || query.settings.is_some()
        || query.format_clause.is_some()
        || !query.pipe_operators.is_empty()
    {
        return unsupported("information_schema.TABLES query clause");
    }
    let SetExpr::Select(select) = query.body.as_ref() else {
        return unsupported("information_schema.TABLES compound query");
    };
    if !matches!(select.flavor, SelectFlavor::Standard)
        || !select.optimizer_hints.is_empty()
        || select.distinct.is_some()
        || select.select_modifiers.is_some()
        || select.top.is_some()
        || select.top_before_distinct
        || select.exclude.is_some()
        || select.into.is_some()
        || !select.lateral_views.is_empty()
        || select.prewhere.is_some()
        || !select.connect_by.is_empty()
        || !matches!(
            &select.group_by,
            sqlparser::ast::GroupByExpr::Expressions(exprs, _) if exprs.is_empty()
        )
        || !select.cluster_by.is_empty()
        || !select.distribute_by.is_empty()
        || !select.sort_by.is_empty()
        || select.having.is_some()
        || !select.named_window.is_empty()
        || select.qualify.is_some()
        || select.window_before_qualify
        || select.value_table_mode.is_some()
    {
        return unsupported("information_schema.TABLES SELECT feature");
    }

    let [SelectItem::UnnamedExpr(Expr::Identifier(table_schema)), SelectItem::UnnamedExpr(Expr::Identifier(table_name)), SelectItem::UnnamedExpr(Expr::Identifier(table_type))] =
        select.projection.as_slice()
    else {
        return unsupported("information_schema.TABLES projection");
    };
    if !is_identifier_named(table_schema, "TABLE_SCHEMA")
        || !is_identifier_named(table_name, "TABLE_NAME")
        || !is_identifier_named(table_type, "TABLE_TYPE")
    {
        return unsupported("information_schema.TABLES projection");
    }

    let [from] = select.from.as_slice() else {
        return unsupported("information_schema.TABLES table source");
    };
    if !from.joins.is_empty() {
        return unsupported("information_schema.TABLES JOIN");
    }
    let TableFactor::Table {
        name,
        alias,
        args,
        with_hints,
        version,
        with_ordinality,
        partitions,
        json_path,
        sample,
        index_hints,
    } = &from.relation
    else {
        return unsupported("information_schema.TABLES table source");
    };
    if alias.is_some()
        || args.is_some()
        || !with_hints.is_empty()
        || version.is_some()
        || *with_ordinality
        || !partitions.is_empty()
        || json_path.is_some()
        || sample.is_some()
        || !index_hints.is_empty()
    {
        return unsupported("information_schema.TABLES table option");
    }
    let [ObjectNamePart::Identifier(database), ObjectNamePart::Identifier(table)] =
        name.0.as_slice()
    else {
        return unsupported("qualified information_schema.TABLES source");
    };
    if !is_identifier_named(database, "information_schema") || !is_identifier_named(table, "TABLES")
    {
        return unsupported("information_schema.TABLES source");
    }

    let Some(selection) = select.selection.as_ref() else {
        return unsupported("information_schema.TABLES WHERE clause");
    };
    let Expr::BinaryOp { left, op, right } = selection else {
        return unsupported("information_schema.TABLES WHERE clause");
    };
    if !matches!(op, BinaryOperator::Eq)
        || !matches!(left.as_ref(), Expr::Identifier(identifier) if is_identifier_named(identifier, "TABLE_SCHEMA"))
        || !is_database_function(right)
    {
        return unsupported("information_schema.TABLES WHERE clause");
    }

    let Some(order_by) = query.order_by.as_ref() else {
        return unsupported("information_schema.TABLES ORDER BY clause");
    };
    let sqlparser::ast::OrderByKind::Expressions(expressions) = &order_by.kind else {
        return unsupported("information_schema.TABLES ORDER BY clause");
    };
    let [order] = expressions.as_slice() else {
        return unsupported("information_schema.TABLES ORDER BY clause");
    };
    if order_by.interpolate.is_some()
        || order.options != sqlparser::ast::OrderByOptions::default()
        || order.with_fill.is_some()
        || !matches!(&order.expr, Expr::Identifier(identifier) if is_identifier_named(identifier, "TABLE_NAME"))
    {
        return unsupported("information_schema.TABLES ORDER BY clause");
    }
    Ok(())
}

fn validate_information_schema_schemata_query(
    query: &sqlparser::ast::Query,
) -> Result<(), ParseError> {
    if query.with.is_some()
        || query.limit_clause.is_some()
        || query.fetch.is_some()
        || !query.locks.is_empty()
        || query.for_clause.is_some()
        || query.settings.is_some()
        || query.format_clause.is_some()
        || !query.pipe_operators.is_empty()
        || query.order_by.is_some()
    {
        return unsupported("information_schema.SCHEMATA query clause");
    }
    let SetExpr::Select(select) = query.body.as_ref() else {
        return unsupported("information_schema.SCHEMATA compound query");
    };
    if !matches!(select.flavor, SelectFlavor::Standard)
        || !select.optimizer_hints.is_empty()
        || select.distinct.is_some()
        || select.select_modifiers.is_some()
        || select.top.is_some()
        || select.top_before_distinct
        || select.exclude.is_some()
        || select.into.is_some()
        || !select.lateral_views.is_empty()
        || select.prewhere.is_some()
        || !select.connect_by.is_empty()
        || !matches!(
            &select.group_by,
            sqlparser::ast::GroupByExpr::Expressions(exprs, _) if exprs.is_empty()
        )
        || !select.cluster_by.is_empty()
        || !select.distribute_by.is_empty()
        || !select.sort_by.is_empty()
        || select.having.is_some()
        || !select.named_window.is_empty()
        || select.qualify.is_some()
        || select.window_before_qualify
        || select.value_table_mode.is_some()
    {
        return unsupported("information_schema.SCHEMATA SELECT feature");
    }

    let [SelectItem::UnnamedExpr(Expr::Identifier(schema_name))] = select.projection.as_slice()
    else {
        return unsupported("information_schema.SCHEMATA projection");
    };
    if !is_identifier_named(schema_name, "SCHEMA_NAME") {
        return unsupported("information_schema.SCHEMATA projection");
    }

    let [from] = select.from.as_slice() else {
        return unsupported("information_schema.SCHEMATA table source");
    };
    if !from.joins.is_empty() {
        return unsupported("information_schema.SCHEMATA JOIN");
    }
    let TableFactor::Table {
        name,
        alias,
        args,
        with_hints,
        version,
        with_ordinality,
        partitions,
        json_path,
        sample,
        index_hints,
    } = &from.relation
    else {
        return unsupported("information_schema.SCHEMATA table source");
    };
    if alias.is_some()
        || args.is_some()
        || !with_hints.is_empty()
        || version.is_some()
        || *with_ordinality
        || !partitions.is_empty()
        || json_path.is_some()
        || sample.is_some()
        || !index_hints.is_empty()
    {
        return unsupported("information_schema.SCHEMATA table option");
    }
    let [ObjectNamePart::Identifier(database), ObjectNamePart::Identifier(table)] =
        name.0.as_slice()
    else {
        return unsupported("qualified information_schema.SCHEMATA source");
    };
    if !is_identifier_named(database, "information_schema")
        || !is_identifier_named(table, "SCHEMATA")
    {
        return unsupported("information_schema.SCHEMATA source");
    }
    if select.selection.is_some() {
        return unsupported("information_schema.SCHEMATA WHERE clause");
    }
    Ok(())
}

fn reject_information_schema_schemata_query_tokens(tokens: &[Token]) -> Result<(), ParseError> {
    if tokens.iter().any(|token| {
        matches!(
            token,
            Token::Whitespace(
                Whitespace::SingleLineComment { .. } | Whitespace::MultiLineComment(_)
            )
        )
    }) {
        return unsupported("comments in information_schema.SCHEMATA query");
    }

    let semicolon_count = tokens
        .iter()
        .filter(|token| matches!(token, Token::SemiColon))
        .count();
    if semicolon_count > 1 {
        return unsupported("multiple information_schema.SCHEMATA statements");
    }
    if semicolon_count == 1 {
        let last = tokens
            .iter()
            .rposition(|token| !matches!(token, Token::Whitespace(_)));
        if !matches!(
            last.and_then(|index| tokens.get(index)),
            Some(Token::SemiColon)
        ) {
            return unsupported("information_schema.SCHEMATA semicolon position");
        }
    }
    Ok(())
}

fn reject_information_schema_columns_query_tokens(tokens: &[Token]) -> Result<(), ParseError> {
    if tokens.iter().any(|token| {
        matches!(
            token,
            Token::Whitespace(
                Whitespace::SingleLineComment { .. } | Whitespace::MultiLineComment(_)
            )
        )
    }) {
        return unsupported("comments in information_schema.COLUMNS query");
    }

    let semicolon_count = tokens
        .iter()
        .filter(|token| matches!(token, Token::SemiColon))
        .count();
    if semicolon_count > 1 {
        return unsupported("multiple information_schema.COLUMNS statements");
    }
    if semicolon_count == 1 {
        let last = tokens
            .iter()
            .rposition(|token| !matches!(token, Token::Whitespace(_)));
        if !matches!(
            last.and_then(|index| tokens.get(index)),
            Some(Token::SemiColon)
        ) {
            return unsupported("information_schema.COLUMNS semicolon position");
        }
    }
    Ok(())
}

fn validate_information_schema_columns_query(
    query: &sqlparser::ast::Query,
) -> Result<MySqlTableName, ParseError> {
    if query.with.is_some()
        || query.limit_clause.is_some()
        || query.fetch.is_some()
        || !query.locks.is_empty()
        || query.for_clause.is_some()
        || query.settings.is_some()
        || query.format_clause.is_some()
        || !query.pipe_operators.is_empty()
    {
        return unsupported("information_schema.COLUMNS query clause");
    }
    let SetExpr::Select(select) = query.body.as_ref() else {
        return unsupported("information_schema.COLUMNS compound query");
    };
    if !matches!(select.flavor, SelectFlavor::Standard)
        || !select.optimizer_hints.is_empty()
        || select.distinct.is_some()
        || select.select_modifiers.is_some()
        || select.top.is_some()
        || select.top_before_distinct
        || select.exclude.is_some()
        || select.into.is_some()
        || !select.lateral_views.is_empty()
        || select.prewhere.is_some()
        || !select.connect_by.is_empty()
        || !matches!(
            &select.group_by,
            sqlparser::ast::GroupByExpr::Expressions(exprs, _) if exprs.is_empty()
        )
        || !select.cluster_by.is_empty()
        || !select.distribute_by.is_empty()
        || !select.sort_by.is_empty()
        || select.having.is_some()
        || !select.named_window.is_empty()
        || select.qualify.is_some()
        || select.window_before_qualify
        || select.value_table_mode.is_some()
    {
        return unsupported("information_schema.COLUMNS SELECT feature");
    }

    let [SelectItem::UnnamedExpr(Expr::Identifier(column_name)), SelectItem::UnnamedExpr(Expr::Identifier(ordinal_position)), SelectItem::UnnamedExpr(Expr::Identifier(column_default)), SelectItem::UnnamedExpr(Expr::Identifier(is_nullable)), SelectItem::UnnamedExpr(Expr::Identifier(column_type)), SelectItem::UnnamedExpr(Expr::Identifier(column_key)), SelectItem::UnnamedExpr(Expr::Identifier(extra))] =
        select.projection.as_slice()
    else {
        return unsupported("information_schema.COLUMNS projection");
    };
    for (identifier, expected) in [
        (column_name, "COLUMN_NAME"),
        (ordinal_position, "ORDINAL_POSITION"),
        (column_default, "COLUMN_DEFAULT"),
        (is_nullable, "IS_NULLABLE"),
        (column_type, "COLUMN_TYPE"),
        (column_key, "COLUMN_KEY"),
        (extra, "EXTRA"),
    ] {
        if !is_identifier_named(identifier, expected) {
            return unsupported("information_schema.COLUMNS projection");
        }
    }

    let [from] = select.from.as_slice() else {
        return unsupported("information_schema.COLUMNS table source");
    };
    if !from.joins.is_empty() {
        return unsupported("information_schema.COLUMNS JOIN");
    }
    let TableFactor::Table {
        name,
        alias,
        args,
        with_hints,
        version,
        with_ordinality,
        partitions,
        json_path,
        sample,
        index_hints,
    } = &from.relation
    else {
        return unsupported("information_schema.COLUMNS table source");
    };
    if alias.is_some()
        || args.is_some()
        || !with_hints.is_empty()
        || version.is_some()
        || *with_ordinality
        || !partitions.is_empty()
        || json_path.is_some()
        || sample.is_some()
        || !index_hints.is_empty()
    {
        return unsupported("information_schema.COLUMNS table option");
    }
    let [ObjectNamePart::Identifier(database), ObjectNamePart::Identifier(table)] =
        name.0.as_slice()
    else {
        return unsupported("qualified information_schema.COLUMNS source");
    };
    if !is_identifier_named(database, "information_schema")
        || !is_identifier_named(table, "COLUMNS")
    {
        return unsupported("information_schema.COLUMNS source");
    }

    let Some(selection) = select.selection.as_ref() else {
        return unsupported("information_schema.COLUMNS WHERE clause");
    };
    let Expr::BinaryOp {
        left: schema_predicate,
        op: BinaryOperator::And,
        right: table_predicate,
    } = selection
    else {
        return unsupported("information_schema.COLUMNS WHERE clause");
    };
    if !is_information_schema_columns_schema_predicate(schema_predicate) {
        return unsupported("information_schema.COLUMNS WHERE clause");
    }
    let table = information_schema_columns_table_name(table_predicate)?;

    let Some(order_by) = query.order_by.as_ref() else {
        return unsupported("information_schema.COLUMNS ORDER BY clause");
    };
    let sqlparser::ast::OrderByKind::Expressions(expressions) = &order_by.kind else {
        return unsupported("information_schema.COLUMNS ORDER BY clause");
    };
    let [order] = expressions.as_slice() else {
        return unsupported("information_schema.COLUMNS ORDER BY clause");
    };
    if order_by.interpolate.is_some()
        || order.options != sqlparser::ast::OrderByOptions::default()
        || order.with_fill.is_some()
        || !matches!(
            &order.expr,
            Expr::Identifier(identifier) if is_identifier_named(identifier, "ORDINAL_POSITION")
        )
    {
        return unsupported("information_schema.COLUMNS ORDER BY clause");
    }
    Ok(table)
}

fn is_information_schema_columns_schema_predicate(expr: &Expr) -> bool {
    let Expr::BinaryOp {
        left,
        op: BinaryOperator::Eq,
        right,
    } = expr
    else {
        return false;
    };
    matches!(left.as_ref(), Expr::Identifier(identifier) if is_identifier_named(identifier, "TABLE_SCHEMA"))
        && is_database_function(right)
}

fn information_schema_columns_table_name(expr: &Expr) -> Result<MySqlTableName, ParseError> {
    let Expr::BinaryOp {
        left,
        op: BinaryOperator::Eq,
        right,
    } = expr
    else {
        return unsupported("information_schema.COLUMNS WHERE clause");
    };
    if !matches!(left.as_ref(), Expr::Identifier(identifier) if is_identifier_named(identifier, "TABLE_NAME"))
    {
        return unsupported("information_schema.COLUMNS WHERE clause");
    }
    let Expr::Value(value) = right.as_ref() else {
        return unsupported("information_schema.COLUMNS WHERE clause");
    };
    let Value::SingleQuotedString(name) = &value.value else {
        return unsupported("information_schema.COLUMNS WHERE clause");
    };
    MySqlTableName::parse(name)
}

fn is_identifier_named(identifier: &Ident, expected: &str) -> bool {
    identifier.value.eq_ignore_ascii_case(expected)
}

fn is_database_function(expr: &Expr) -> bool {
    let Expr::Function(function) = expr else {
        return false;
    };
    matches!(
        function.name.0.as_slice(),
        [ObjectNamePart::Identifier(identifier)] if is_identifier_named(identifier, "DATABASE")
    ) && !function.uses_odbc_syntax
        && matches!(function.parameters, FunctionArguments::None)
        && matches!(
            &function.args,
            FunctionArguments::List(arguments)
                if arguments.args.is_empty()
                    && arguments.duplicate_treatment.is_none()
                    && arguments.clauses.is_empty()
        )
        && function.filter.is_none()
        && function.null_treatment.is_none()
        && function.over.is_none()
        && function.within_group.is_empty()
}

fn parse_normalized_create_table(sql: &str) -> Result<Stmt, ParseError> {
    let mut parser = TursoParser::new(sql.as_bytes());
    let command = parser
        .next_cmd()
        .map_err(|error| ParseError::TursoParser(error.to_string()))?;
    let Some(TursoCmd::Stmt(statement @ Stmt::CreateTable { .. })) = command else {
        return Err(ParseError::TursoParser(
            "normalized CREATE TABLE did not produce a CREATE TABLE AST".to_string(),
        ));
    };
    if parser
        .next_cmd()
        .map_err(|error| ParseError::TursoParser(error.to_string()))?
        .is_some()
    {
        return Err(ParseError::ExpectedOneStatement { actual: 2 });
    }
    Ok(statement)
}

fn parse_normalized_select(sql: &str) -> Result<Stmt, ParseError> {
    let mut parser = TursoParser::new(sql.as_bytes());
    let command = parser
        .next_cmd()
        .map_err(|error| ParseError::TursoParser(error.to_string()))?;
    let Some(TursoCmd::Stmt(statement @ Stmt::Select(_))) = command else {
        return Err(ParseError::TursoParser(
            "normalized SELECT did not produce a SELECT AST".to_string(),
        ));
    };
    if parser
        .next_cmd()
        .map_err(|error| ParseError::TursoParser(error.to_string()))?
        .is_some()
    {
        return Err(ParseError::ExpectedOneStatement { actual: 2 });
    }
    Ok(statement)
}

fn parse_normalized_dml(sql: &str) -> Result<Stmt, ParseError> {
    let mut parser = TursoParser::new(sql.as_bytes());
    let command = parser
        .next_cmd()
        .map_err(|error| ParseError::TursoParser(error.to_string()))?;
    let Some(TursoCmd::Stmt(
        statement @ (Stmt::Insert { .. } | Stmt::Update(_) | Stmt::Delete { .. }),
    )) = command
    else {
        return Err(ParseError::TursoParser(
            "normalized DML did not produce an INSERT, UPDATE, or DELETE AST".to_string(),
        ));
    };
    if parser
        .next_cmd()
        .map_err(|error| ParseError::TursoParser(error.to_string()))?
        .is_some()
    {
        return Err(ParseError::ExpectedOneStatement { actual: 2 });
    }
    Ok(statement)
}

fn parse_normalized_alter_table(sql: &str) -> Result<Stmt, ParseError> {
    let mut parser = TursoParser::new(sql.as_bytes());
    let command = parser
        .next_cmd()
        .map_err(|error| ParseError::TursoParser(error.to_string()))?;
    let Some(TursoCmd::Stmt(statement @ Stmt::AlterTable(_))) = command else {
        return Err(ParseError::TursoParser(
            "normalized ALTER TABLE did not produce an ALTER TABLE AST".to_string(),
        ));
    };
    if parser
        .next_cmd()
        .map_err(|error| ParseError::TursoParser(error.to_string()))?
        .is_some()
    {
        return Err(ParseError::ExpectedOneStatement { actual: 2 });
    }
    Ok(statement)
}

fn parse_normalized_create_index(sql: &str) -> Result<Stmt, ParseError> {
    let mut parser = TursoParser::new(sql.as_bytes());
    let command = parser
        .next_cmd()
        .map_err(|error| ParseError::TursoParser(error.to_string()))?;
    let Some(TursoCmd::Stmt(statement @ Stmt::CreateIndex { .. })) = command else {
        return Err(ParseError::TursoParser(
            "normalized CREATE INDEX did not produce a CREATE INDEX AST".to_string(),
        ));
    };
    if parser
        .next_cmd()
        .map_err(|error| ParseError::TursoParser(error.to_string()))?
        .is_some()
    {
        return Err(ParseError::ExpectedOneStatement { actual: 2 });
    }
    Ok(statement)
}

fn parse_normalized_create_view(sql: &str) -> Result<Stmt, ParseError> {
    let mut parser = TursoParser::new(sql.as_bytes());
    let command = parser
        .next_cmd()
        .map_err(|error| ParseError::TursoParser(error.to_string()))?;
    let Some(TursoCmd::Stmt(statement @ Stmt::CreateView { .. })) = command else {
        return Err(ParseError::TursoParser(
            "normalized CREATE VIEW did not produce a CREATE VIEW AST".to_string(),
        ));
    };
    if parser
        .next_cmd()
        .map_err(|error| ParseError::TursoParser(error.to_string()))?
        .is_some()
    {
        return Err(ParseError::ExpectedOneStatement { actual: 2 });
    }
    Ok(statement)
}

fn parse_normalized_create_trigger(sql: &str) -> Result<Stmt, ParseError> {
    let mut parser = TursoParser::new(sql.as_bytes());
    let command = parser
        .next_cmd()
        .map_err(|error| ParseError::TursoParser(error.to_string()))?;
    let Some(TursoCmd::Stmt(statement @ Stmt::CreateTrigger { .. })) = command else {
        return Err(ParseError::TursoParser(
            "normalized CREATE TRIGGER did not produce a CREATE TRIGGER AST".to_string(),
        ));
    };
    if parser
        .next_cmd()
        .map_err(|error| ParseError::TursoParser(error.to_string()))?
        .is_some()
    {
        return Err(ParseError::ExpectedOneStatement { actual: 2 });
    }
    Ok(statement)
}

fn translate_create_table(table: &CreateTable) -> Result<TranslatedCreateTable, ParseError> {
    reject_table_attributes(table)?;
    let name = render_name(&table.name)?;
    let columns = table
        .columns
        .iter()
        .map(render_column)
        .collect::<Result<Vec<_>, _>>()?;
    let constraints = table
        .constraints
        .iter()
        .map(render_table_constraint)
        .collect::<Result<Vec<_>, _>>()?;
    if columns.is_empty() && constraints.is_empty() {
        return unsupported("CREATE TABLE without columns or constraints");
    }

    let mut definitions = columns;
    definitions.extend(constraints);
    let temporary = if table.temporary { "TEMPORARY " } else { "" };
    let if_not_exists = if table.if_not_exists {
        "IF NOT EXISTS "
    } else {
        ""
    };
    Ok(TranslatedCreateTable {
        sqlite_sql: format!(
            "CREATE {temporary}TABLE {if_not_exists}{name} ({})",
            definitions.join(", ")
        ),
    })
}

fn translate_auto_increment_create_table(
    table: &CreateTable,
    mode: SessionSqlMode,
) -> Result<CheckedAutoIncrementCreateTable, ParseError> {
    if table.temporary {
        return unsupported("TEMPORARY AUTO_INCREMENT table");
    }
    if table.name.0.len() != 1 {
        return unsupported("qualified AUTO_INCREMENT table name");
    }
    reject_table_attributes(table)?;
    if !table.constraints.is_empty() {
        return unsupported("table-level constraint in AUTO_INCREMENT table");
    }
    for (index, column) in table.columns.iter().enumerate() {
        if table.columns[..index]
            .iter()
            .any(|previous| previous.name.value.eq_ignore_ascii_case(&column.name.value))
        {
            return unsupported("duplicate column name in AUTO_INCREMENT table");
        }
    }

    let mut allocator_column_ordinal = None;
    for (ordinal, column) in table.columns.iter().enumerate() {
        if column_has_auto_increment(column) && allocator_column_ordinal.replace(ordinal).is_some()
        {
            return unsupported("multiple AUTO_INCREMENT columns");
        }
    }
    let Some(allocator_column_ordinal) = allocator_column_ordinal else {
        return unsupported("AUTO_INCREMENT column");
    };
    validate_auto_increment_column(&table.columns[allocator_column_ordinal])?;

    let sqlite_columns = table
        .columns
        .iter()
        .enumerate()
        .map(|(ordinal, column)| {
            if ordinal == allocator_column_ordinal {
                Ok(format!(
                    "{} INTEGER PRIMARY KEY",
                    render_ident(&column.name)
                ))
            } else {
                render_column(column)
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    if sqlite_columns.is_empty() {
        return unsupported("CREATE TABLE without columns");
    }
    let temporary = if table.temporary { "TEMPORARY " } else { "" };
    let if_not_exists = if table.if_not_exists {
        "IF NOT EXISTS "
    } else {
        ""
    };
    let sqlite_sql = format!(
        "CREATE {temporary}TABLE {if_not_exists}{} ({})",
        render_name(&table.name)?,
        sqlite_columns.join(", ")
    );
    let sqlite_statement = parse_normalized_create_table(&sqlite_sql)?;

    Ok(CheckedAutoIncrementCreateTable {
        table_name: match table.name.0.as_slice() {
            [ObjectNamePart::Identifier(name)] => name.value.clone(),
            _ => unreachable!("AUTO_INCREMENT table name was already checked as unqualified"),
        },
        allocator_column_ordinal,
        allocator_column_name: table.columns[allocator_column_ordinal].name.value.clone(),
        normalized_mysql_ddl: render_auto_increment_mysql_ddl(
            table,
            allocator_column_ordinal,
            mode,
        )?,
        sqlite_statement,
    })
}

fn column_has_auto_increment(column: &ColumnDef) -> bool {
    column.options.iter().any(|option| {
        matches!(
            &option.option,
            ColumnOption::DialectSpecific(tokens) if is_auto_increment_tokens(tokens)
        )
    })
}

fn is_auto_increment_tokens(tokens: &[sqlparser::tokenizer::Token]) -> bool {
    matches!(tokens, [token] if token.to_string().eq_ignore_ascii_case("AUTO_INCREMENT"))
}

fn validate_auto_increment_column(column: &ColumnDef) -> Result<(), ParseError> {
    if !matches!(
        column.data_type,
        DataType::Int(None) | DataType::Integer(None)
    ) {
        return unsupported("AUTO_INCREMENT column type");
    }
    let [not_null, auto_increment, primary_key] = column.options.as_slice() else {
        return unsupported("AUTO_INCREMENT column attributes");
    };
    if not_null.name.is_some()
        || !matches!(not_null.option, ColumnOption::NotNull)
        || auto_increment.name.is_some()
        || !matches!(
            &auto_increment.option,
            ColumnOption::DialectSpecific(tokens) if is_auto_increment_tokens(tokens)
        )
        || primary_key.name.is_some()
        || !is_plain_inline_primary_key(&primary_key.option)
    {
        return unsupported("AUTO_INCREMENT column attributes");
    }
    Ok(())
}

fn is_plain_inline_primary_key(option: &ColumnOption) -> bool {
    let ColumnOption::PrimaryKey(primary_key) = option else {
        return false;
    };
    primary_key.name.is_none()
        && primary_key.index_name.is_none()
        && primary_key.index_type.is_none()
        && primary_key.columns.is_empty()
        && primary_key.index_options.is_empty()
        && primary_key.characteristics.is_none()
}

fn render_auto_increment_mysql_ddl(
    table: &CreateTable,
    allocator_column_ordinal: usize,
    mode: SessionSqlMode,
) -> Result<String, ParseError> {
    let columns = table
        .columns
        .iter()
        .enumerate()
        .map(|(ordinal, column)| {
            if ordinal == allocator_column_ordinal {
                render_auto_increment_mysql_column(column)
            } else {
                render_mysql_checked_column(column, mode)
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    let temporary = if table.temporary { "TEMPORARY " } else { "" };
    let if_not_exists = if table.if_not_exists {
        "IF NOT EXISTS "
    } else {
        ""
    };
    Ok(format!(
        "CREATE {temporary}TABLE {if_not_exists}{} ({})",
        render_mysql_object_name(&table.name)?,
        columns.join(", ")
    ))
}

fn render_auto_increment_mysql_column(column: &ColumnDef) -> Result<String, ParseError> {
    let data_type = match column.data_type {
        DataType::Int(None) => "INT",
        DataType::Integer(None) => "INTEGER",
        _ => return unsupported("AUTO_INCREMENT column type"),
    };
    Ok(format!(
        "{} {data_type} NOT NULL AUTO_INCREMENT PRIMARY KEY",
        render_mysql_sqlparser_ident(&column.name)
    ))
}

fn render_mysql_checked_column(
    column: &ColumnDef,
    mode: SessionSqlMode,
) -> Result<String, ParseError> {
    let sqlite_column = render_column(column)?;
    let statement =
        parse_normalized_create_table(&format!("CREATE TABLE \"t\" ({sqlite_column})"))?;
    let Stmt::CreateTable {
        body:
            TursoCreateTableBody::ColumnsAndConstraints {
                columns,
                constraints,
                options,
            },
        ..
    } = statement
    else {
        unreachable!("normalized one-column CREATE TABLE must parse as a table");
    };
    let [column] = columns.as_slice() else {
        unreachable!("normalized one-column CREATE TABLE must keep one column");
    };
    if !constraints.is_empty() || options != turso_parser::ast::TableOptions::empty() {
        unreachable!("normalized one-column CREATE TABLE must not have table attributes");
    }
    render_mysql_column(column, mode)
}

fn render_mysql_object_name(name: &ObjectName) -> Result<String, ParseError> {
    if !(1..=2).contains(&name.0.len()) {
        return unsupported("object name with more than two parts");
    }
    let mut parts = Vec::with_capacity(name.0.len());
    for part in &name.0 {
        let ObjectNamePart::Identifier(ident) = part else {
            return unsupported("dynamic object name");
        };
        parts.push(render_mysql_sqlparser_ident(ident));
    }
    Ok(parts.join("."))
}

fn render_mysql_sqlparser_ident(ident: &Ident) -> String {
    format!("`{}`", ident.value.replace('`', "``"))
}

fn translate_create_index(index: &CreateIndex) -> Result<String, ParseError> {
    let Some(index_name) = index.name.as_ref() else {
        return unsupported("CREATE INDEX without a name");
    };
    if index.using.is_some()
        || index.concurrently
        || index.if_not_exists
        || !index.include.is_empty()
        || index.nulls_distinct.is_some()
        || !index.with.is_empty()
        || index.predicate.is_some()
        || !index.index_options.is_empty()
        || !index.alter_options.is_empty()
    {
        return unsupported("CREATE INDEX option");
    }
    let index_name = render_unqualified_name(index_name)?;
    let table_name = render_unqualified_name(&index.table_name)?;
    let columns = index
        .columns
        .iter()
        .map(render_index_column)
        .collect::<Result<Vec<_>, _>>()?;
    if columns.is_empty() {
        return unsupported("CREATE INDEX without columns");
    }
    let unique = if index.unique { "UNIQUE " } else { "" };
    Ok(format!(
        "CREATE {unique}INDEX {index_name} ON {table_name} ({})",
        columns.join(", ")
    ))
}

fn translate_create_view(view: &CreateView) -> Result<String, ParseError> {
    if view.or_alter
        || view.or_replace
        || view.materialized
        || view.secure
        || view.name_before_not_exists
        || !view.columns.is_empty()
        || !matches!(view.options, CreateTableOptions::None)
        || !view.cluster_by.is_empty()
        || view.comment.is_some()
        || view.with_no_schema_binding
        || view.if_not_exists
        || view.temporary
        || view.copy_grants
        || view.to.is_some()
        || view.params.is_some()
    {
        return unsupported("CREATE VIEW option");
    }
    let view_name = render_unqualified_name(&view.name)?;
    let query = render_simple_view_query(&view.query)?;
    Ok(format!("CREATE VIEW {view_name} AS {query}"))
}

fn translate_create_trigger(trigger: &CreateTrigger) -> Result<String, ParseError> {
    if trigger.or_alter
        || trigger.temporary
        || trigger.or_replace
        || trigger.is_constraint
        || !trigger.period_before_table
        || trigger.referenced_table_name.is_some()
        || !trigger.referencing.is_empty()
        || trigger.condition.is_some()
        || trigger.exec_body.is_some()
        || trigger.statements_as
        || trigger.characteristics.is_some()
    {
        return unsupported("CREATE TRIGGER option");
    }
    if trigger.period != Some(TriggerPeriod::After)
        || !matches!(trigger.events.as_slice(), [SqlTriggerEvent::Insert])
        || !matches!(
            trigger.trigger_object,
            Some(TriggerObjectKind::ForEach(TriggerObject::Row))
        )
    {
        return unsupported("CREATE TRIGGER timing or event");
    }

    let trigger_name = render_unqualified_name(&trigger.name)?;
    let table_name = render_unqualified_name(&trigger.table_name)?;
    let statements = trigger.statements.as_ref().ok_or(ParseError::Unsupported {
        feature: "CREATE TRIGGER body",
    })?;
    let sqlparser::ast::ConditionalStatements::BeginEnd(body) = statements else {
        return unsupported("CREATE TRIGGER body");
    };
    let [Statement::Insert(insert)] = body.statements.as_slice() else {
        return unsupported("CREATE TRIGGER body");
    };
    let TableObject::TableName(target_table) = &insert.table else {
        return unsupported("CREATE TRIGGER INSERT target");
    };
    if !insert.optimizer_hints.is_empty()
        || insert.or.is_some()
        || insert.ignore
        || !insert.into
        || insert.table_alias.is_some()
        || insert.overwrite
        || !insert.assignments.is_empty()
        || insert.partitioned.is_some()
        || !insert.after_columns.is_empty()
        || insert.has_table_keyword
        || insert.on.is_some()
        || insert.returning.is_some()
        || insert.output.is_some()
        || insert.replace_into
        || insert.priority.is_some()
        || insert.insert_alias.is_some()
        || insert.settings.is_some()
        || insert.format_clause.is_some()
        || insert.multi_table_insert_type.is_some()
        || !insert.multi_table_into_clauses.is_empty()
        || !insert.multi_table_when_clauses.is_empty()
        || insert.multi_table_else_clause.is_some()
    {
        return unsupported("CREATE TRIGGER INSERT option");
    }
    let target_table = render_unqualified_name(target_table)?;
    if insert.columns.is_empty() {
        return unsupported("CREATE TRIGGER INSERT without columns");
    }
    let columns = insert
        .columns
        .iter()
        .map(render_unqualified_name)
        .collect::<Result<Vec<_>, _>>()?;
    let Some(source) = &insert.source else {
        return unsupported("CREATE TRIGGER INSERT source");
    };
    if source.with.is_some()
        || source.order_by.is_some()
        || source.limit_clause.is_some()
        || source.fetch.is_some()
        || !source.locks.is_empty()
        || source.for_clause.is_some()
        || source.settings.is_some()
        || source.format_clause.is_some()
        || !source.pipe_operators.is_empty()
    {
        return unsupported("CREATE TRIGGER INSERT source");
    }
    let SetExpr::Values(values) = source.body.as_ref() else {
        return unsupported("CREATE TRIGGER INSERT SELECT");
    };
    if values.explicit_row || values.value_keyword {
        return unsupported("CREATE TRIGGER INSERT VALUES option");
    }
    let [values] = values.rows.as_slice() else {
        return unsupported("CREATE TRIGGER INSERT VALUES rows");
    };
    if values.len() != columns.len() {
        return unsupported("CREATE TRIGGER INSERT value count");
    }
    let values = values
        .iter()
        .map(render_trigger_value)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(format!(
        "CREATE TRIGGER {trigger_name} AFTER INSERT ON {table_name} FOR EACH ROW BEGIN INSERT INTO {target_table} ({}) VALUES ({}); END",
        columns.join(", "),
        values.join(", ")
    ))
}

fn render_trigger_value(value: &Expr) -> Result<String, ParseError> {
    match value {
        Expr::CompoundIdentifier(parts) if matches!(parts.as_slice(), [prefix, _] if prefix.value.eq_ignore_ascii_case("NEW")) => {
            Ok(format!("NEW.{}", render_ident(&parts[1])))
        }
        Expr::Value(value) => match &value.value {
            Value::Number(value, _) => Ok(value.clone()),
            Value::SingleQuotedString(value) | Value::DoubleQuotedString(value) => {
                Ok(format!("'{}'", value.replace('\'', "''")))
            }
            Value::Boolean(value) => Ok(if *value { "TRUE" } else { "FALSE" }.to_string()),
            Value::Null => Ok("NULL".to_string()),
            _ => unsupported("CREATE TRIGGER literal"),
        },
        _ => unsupported("CREATE TRIGGER value expression"),
    }
}

struct RenderedSelect {
    sqlite_sql: String,
    source_table: Option<MySqlTableName>,
    checked_comparisons: Vec<CheckedSelectComparison>,
    parameter_count: usize,
}

fn translate_select_query(query: &sqlparser::ast::Query) -> Result<RenderedSelect, ParseError> {
    if query.with.is_some()
        || query.fetch.is_some()
        || !query.locks.is_empty()
        || query.for_clause.is_some()
        || query.settings.is_some()
        || query.format_clause.is_some()
        || !query.pipe_operators.is_empty()
    {
        return unsupported("SELECT query clause");
    }
    let SetExpr::Select(select) = query.body.as_ref() else {
        return unsupported("compound SELECT query");
    };
    if !matches!(select.flavor, SelectFlavor::Standard)
        || !select.optimizer_hints.is_empty()
        || select.distinct.is_some()
        || select.select_modifiers.is_some()
        || select.top.is_some()
        || select.top_before_distinct
        || select.exclude.is_some()
        || select.into.is_some()
        || !select.lateral_views.is_empty()
        || select.prewhere.is_some()
        || !select.connect_by.is_empty()
        || !matches!(&select.group_by, sqlparser::ast::GroupByExpr::Expressions(exprs, _) if exprs.is_empty())
        || !select.cluster_by.is_empty()
        || !select.distribute_by.is_empty()
        || !select.sort_by.is_empty()
        || select.having.is_some()
        || !select.named_window.is_empty()
        || select.qualify.is_some()
        || select.window_before_qualify
        || select.value_table_mode.is_some()
    {
        return unsupported("SELECT feature");
    }

    let mut render_context = SelectRenderContext::default();
    let projection = select
        .projection
        .iter()
        .map(|item| render_select_item(item, &mut render_context))
        .collect::<Result<Vec<_>, _>>()?;
    if projection.is_empty() {
        return unsupported("SELECT without projections");
    }

    let (from, source_table) = match select.from.as_slice() {
        [] => (None, None),
        [from] if from.joins.is_empty() => {
            let (rendered, source_table) = render_select_table(&from.relation)?;
            (Some(rendered), Some(source_table))
        }
        [_] => return unsupported("SELECT JOIN"),
        _ => return unsupported("multiple SELECT table sources"),
    };

    let mut normalized = format!("SELECT {}", projection.join(", "));
    if let Some(from) = from {
        normalized.push_str(" FROM ");
        normalized.push_str(&from);
    }
    if let Some(selection) = &select.selection {
        normalized.push_str(" WHERE ");
        normalized.push_str(&render_select_predicate(selection, &mut render_context)?);
    }
    if let Some(order_by) = &query.order_by {
        normalized.push_str(" ORDER BY ");
        normalized.push_str(&render_select_order_by(order_by, &mut render_context)?);
    }
    if let Some(limit) = &query.limit_clause {
        normalized.push_str(&render_select_limit(limit)?);
    }
    Ok(RenderedSelect {
        sqlite_sql: normalized,
        source_table,
        checked_comparisons: render_context.checked_comparisons,
        parameter_count: render_context.parameter_count,
    })
}

fn select_static_result_metadata(
    query: &sqlparser::ast::Query,
) -> Vec<StaticSelectProjectionMetadata> {
    let SetExpr::Select(select) = query.body.as_ref() else {
        return Vec::new();
    };
    select
        .projection
        .iter()
        .map(|item| match item {
            SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                classify_static_select_expr(expr).map_or(
                    StaticSelectProjectionMetadata::Other,
                    StaticSelectProjectionMetadata::Literal,
                )
            }
            SelectItem::ExprWithAliases { .. } => StaticSelectProjectionMetadata::Other,
            SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => {
                StaticSelectProjectionMetadata::Wildcard
            }
        })
        .collect()
}

fn render_select_order_by(
    order_by: &sqlparser::ast::OrderBy,
    render_context: &mut SelectRenderContext,
) -> Result<String, ParseError> {
    let sqlparser::ast::OrderByKind::Expressions(expressions) = &order_by.kind else {
        return unsupported("SELECT ORDER BY option");
    };
    if expressions.is_empty() || order_by.interpolate.is_some() {
        return unsupported("SELECT ORDER BY option");
    }
    expressions
        .iter()
        .map(|expression| {
            if expression.options.nulls_first.is_some() || expression.with_fill.is_some() {
                return unsupported("SELECT ORDER BY option");
            }
            match &expression.expr {
                Expr::Identifier(_) => {}
                Expr::CompoundIdentifier(parts) if parts.len() == 2 => {}
                _ => return unsupported("SELECT ORDER BY expression"),
            }
            let direction = if expression.options.asc == Some(false) {
                "DESC"
            } else {
                "ASC"
            };
            Ok(format!(
                "{} {direction}",
                render_select_expr(&expression.expr, render_context)?
            ))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|expressions| expressions.join(", "))
}

fn render_select_limit(clause: &sqlparser::ast::LimitClause) -> Result<String, ParseError> {
    use sqlparser::ast::{LimitClause, OffsetRows};

    let (limit, offset) = match clause {
        LimitClause::LimitOffset {
            limit: Some(limit),
            offset,
            limit_by,
        } if limit_by.is_empty() => {
            if offset
                .as_ref()
                .is_some_and(|offset| offset.rows != OffsetRows::None)
            {
                return unsupported("SELECT OFFSET option");
            }
            (limit, offset.as_ref().map(|offset| &offset.value))
        }
        LimitClause::OffsetCommaLimit { offset, limit } => (limit, Some(offset)),
        _ => return unsupported("SELECT LIMIT option"),
    };
    let mut rendered = format!(" LIMIT {}", render_select_row_count(limit)?);
    if let Some(offset) = offset {
        rendered.push_str(&format!(" OFFSET {}", render_select_row_count(offset)?));
    }
    Ok(rendered)
}

fn render_select_row_count(expr: &Expr) -> Result<i64, ParseError> {
    if let Expr::Value(value) = expr {
        if let Value::Number(number, false) = &value.value {
            if !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit()) {
                if let Ok(number) = number.parse::<i64>() {
                    return Ok(number);
                }
            }
        }
    }
    unsupported("SELECT LIMIT/OFFSET requires an integer literal in 0..=9223372036854775807")
}

fn translate_insert(insert: &Insert) -> Result<String, ParseError> {
    if !insert.optimizer_hints.is_empty()
        || insert.or.is_some()
        || insert.ignore
        || !insert.into
        || insert.table_alias.is_some()
        || insert.overwrite
        || !insert.assignments.is_empty()
        || insert.partitioned.is_some()
        || !insert.after_columns.is_empty()
        || insert.has_table_keyword
        || insert.on.is_some()
        || insert.returning.is_some()
        || insert.output.is_some()
        || insert.replace_into
        || insert.priority.is_some()
        || insert.insert_alias.is_some()
        || insert.settings.is_some()
        || insert.format_clause.is_some()
        || insert.multi_table_insert_type.is_some()
        || !insert.multi_table_into_clauses.is_empty()
        || !insert.multi_table_when_clauses.is_empty()
        || insert.multi_table_else_clause.is_some()
    {
        return unsupported("INSERT option");
    }
    let sqlparser::ast::TableObject::TableName(table) = &insert.table else {
        return unsupported("INSERT table source");
    };
    let table = render_unqualified_name(table)?;
    let columns = insert
        .columns
        .iter()
        .map(render_unqualified_name)
        .collect::<Result<Vec<_>, _>>()?;
    let source = insert.source.as_deref().ok_or(ParseError::Unsupported {
        feature: "INSERT without VALUES",
    })?;
    if source.with.is_some()
        || source.order_by.is_some()
        || source.limit_clause.is_some()
        || source.fetch.is_some()
        || !source.locks.is_empty()
        || source.for_clause.is_some()
        || source.settings.is_some()
        || source.format_clause.is_some()
        || !source.pipe_operators.is_empty()
    {
        return unsupported("INSERT source query option");
    }
    let SetExpr::Values(values) = source.body.as_ref() else {
        return unsupported("INSERT source");
    };
    if values.explicit_row || values.value_keyword || values.rows.is_empty() {
        return unsupported("INSERT VALUES option");
    }
    if columns.is_empty() {
        if values.rows.len() == 1 && values.rows[0].is_empty() {
            return Ok(format!("INSERT INTO {table} DEFAULT VALUES"));
        }
        return unsupported("INSERT without an explicit column list");
    }
    let rows = values
        .rows
        .iter()
        .map(|row| {
            if row.is_empty() || row.len() != columns.len() {
                return unsupported("INSERT VALUES column count");
            }
            row.iter()
                .map(render_dml_expr)
                .collect::<Result<Vec<_>, _>>()
                .map(|values| format!("({})", values.join(", ")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(format!(
        "INSERT INTO {table} ({}) VALUES {}",
        columns.join(", "),
        rows.join(", ")
    ))
}

fn translate_update(
    update: &Update,
    render_context: &mut SelectRenderContext,
) -> Result<String, ParseError> {
    if !update.optimizer_hints.is_empty()
        || !update.table.joins.is_empty()
        || update.from.is_some()
        || update.returning.is_some()
        || update.output.is_some()
        || update.or.is_some()
        || !update.order_by.is_empty()
        || update.limit.is_some()
    {
        return unsupported("UPDATE option");
    }
    let table = render_update_table(&update.table.relation)?;
    if update.assignments.is_empty() {
        return unsupported("UPDATE without assignments");
    }
    let assignments = update
        .assignments
        .iter()
        .map(|assignment| {
            let sqlparser::ast::AssignmentTarget::ColumnName(column) = &assignment.target else {
                return unsupported("UPDATE assignment target");
            };
            Ok(format!(
                "{} = {}",
                render_unqualified_name(column)?,
                render_dml_expr(&assignment.value)?
            ))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut normalized = format!("UPDATE {table} SET {}", assignments.join(", "));
    if let Some(selection) = &update.selection {
        normalized.push_str(" WHERE ");
        normalized.push_str(&render_dml_predicate(selection, render_context)?);
    }
    Ok(normalized)
}

fn checked_update(update: &Update) -> Result<CheckedUpdate, ParseError> {
    let table_name = update_table_name(&update.table.relation)?;
    let assignments = update
        .assignments
        .iter()
        .map(|assignment| {
            let sqlparser::ast::AssignmentTarget::ColumnName(column) = &assignment.target else {
                return unsupported("UPDATE assignment target");
            };
            let [ObjectNamePart::Identifier(column)] = column.0.as_slice() else {
                return unsupported("qualified UPDATE assignment target");
            };
            Ok(CheckedUpdateAssignment {
                column_name: column.value.clone(),
                value: checked_update_assignment_value(&column.value, &assignment.value),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(CheckedUpdate {
        table_name,
        assignments,
    })
}

fn checked_update_assignment_value(
    column_name: &str,
    value: &Expr,
) -> CheckedUpdateAssignmentValue {
    if matches!(
        value,
        Expr::Identifier(identifier) if identifier.value.eq_ignore_ascii_case(column_name)
    ) {
        return CheckedUpdateAssignmentValue::SelfAssignment;
    }
    direct_signed_integer(value)
        .map(CheckedUpdateAssignmentValue::SignedInteger)
        .unwrap_or(CheckedUpdateAssignmentValue::Other)
}

fn direct_signed_integer(expr: &Expr) -> Option<i64> {
    match expr {
        Expr::Value(value) => match &value.value {
            Value::Number(value, false) => value.parse().ok(),
            _ => None,
        },
        Expr::UnaryOp {
            op: UnaryOperator::Plus,
            expr,
        } => match expr.as_ref() {
            Expr::Value(value) => match &value.value {
                Value::Number(value, false) => value.parse().ok(),
                _ => None,
            },
            _ => None,
        },
        Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr,
        } => match expr.as_ref() {
            Expr::Value(value) => match &value.value {
                Value::Number(value, false) => value.parse::<u64>().ok().and_then(|magnitude| {
                    if magnitude == (i64::MAX as u64) + 1 {
                        Some(i64::MIN)
                    } else {
                        i64::try_from(magnitude).ok().map(|value| -value)
                    }
                }),
                _ => None,
            },
            _ => None,
        },
        _ => None,
    }
}

/// Reads the one table a `DELETE` names, when it names one plainly.
fn delete_source_table(delete: &Delete) -> Option<String> {
    let FromTable::WithFromKeyword(tables) = &delete.from else {
        return None;
    };
    let [table] = tables.as_slice() else {
        return None;
    };
    let TableFactor::Table { name, .. } = &table.relation else {
        return None;
    };
    let [ObjectNamePart::Identifier(ident)] = name.0.as_slice() else {
        return None;
    };
    MySqlTableName::parse(&ident.value)
        .ok()
        .map(|name| name.as_str().to_owned())
}

fn translate_delete(
    delete: &Delete,
    render_context: &mut SelectRenderContext,
) -> Result<String, ParseError> {
    if !delete.optimizer_hints.is_empty()
        || !delete.tables.is_empty()
        || delete.using.is_some()
        || delete.returning.is_some()
        || delete.output.is_some()
        || !delete.order_by.is_empty()
        || delete.limit.is_some()
    {
        return unsupported("DELETE option");
    }
    let table = match &delete.from {
        FromTable::WithFromKeyword(from) => match from.as_slice() {
            [from] if from.joins.is_empty() => render_update_table(&from.relation)?,
            _ => return unsupported("DELETE table source"),
        },
        FromTable::WithoutKeyword(_) => return unsupported("DELETE without FROM"),
    };
    let mut normalized = format!("DELETE FROM {table}");
    if let Some(selection) = &delete.selection {
        normalized.push_str(" WHERE ");
        normalized.push_str(&render_dml_predicate(selection, render_context)?);
    }
    Ok(normalized)
}

fn render_update_table(table: &TableFactor) -> Result<String, ParseError> {
    let TableFactor::Table {
        name,
        alias,
        args,
        with_hints,
        version,
        with_ordinality,
        partitions,
        json_path,
        sample,
        index_hints,
    } = table
    else {
        return unsupported("UPDATE table source");
    };
    if alias.is_some()
        || args.is_some()
        || !with_hints.is_empty()
        || version.is_some()
        || *with_ordinality
        || !partitions.is_empty()
        || json_path.is_some()
        || sample.is_some()
        || !index_hints.is_empty()
    {
        return unsupported("UPDATE table option");
    }
    render_unqualified_name(name)
}

fn update_table_name(table: &TableFactor) -> Result<String, ParseError> {
    let TableFactor::Table { name, .. } = table else {
        return unsupported("UPDATE table source");
    };
    let [ObjectNamePart::Identifier(name)] = name.0.as_slice() else {
        return unsupported("qualified UPDATE table name");
    };
    Ok(name.value.clone())
}

fn render_dml_expr(expr: &Expr) -> Result<String, ParseError> {
    match expr {
        Expr::Identifier(ident) => Ok(render_ident(ident)),
        Expr::CompoundIdentifier(parts) if parts.len() == 2 => Ok(format!(
            "{}.{}",
            render_ident(&parts[0]),
            render_ident(&parts[1])
        )),
        Expr::Value(value) => match &value.value {
            Value::Number(value, false) => render_dml_number(value),
            Value::SingleQuotedString(value) | Value::DoubleQuotedString(value) => {
                Ok(format!("'{}'", value.replace('\'', "''")))
            }
            Value::Boolean(value) => Ok(if *value { "TRUE" } else { "FALSE" }.to_string()),
            Value::Null => Ok("NULL".to_string()),
            Value::Placeholder(marker) if marker == "?" => Ok("?".to_string()),
            _ => unsupported("DML literal"),
        },
        Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr,
        } if matches!(expr.as_ref(), Expr::Value(value) if matches!(&value.value, Value::Number(_, false))) =>
        {
            let Expr::Value(value) = expr.as_ref() else {
                unreachable!("guard requires a numeric literal");
            };
            let Value::Number(value, false) = &value.value else {
                unreachable!("guard requires a numeric literal");
            };
            let Ok(magnitude) = value.parse::<u64>() else {
                return Ok(format!("(-{})", render_dml_number(value)?));
            };
            if magnitude > (i64::MAX as u64) + 1 {
                return unsupported("DML numeric literal outside signed 64-bit integer range");
            }
            Ok(format!("(-{magnitude})"))
        }
        Expr::UnaryOp {
            op: UnaryOperator::Plus,
            expr,
        } => Ok(format!("(+{})", render_dml_expr(expr)?)),
        Expr::Nested(expr) => Ok(format!("({})", render_dml_expr(expr)?)),
        _ => unsupported("DML expression"),
    }
}

/// Renders a numeric literal a DML statement may carry.
///
/// An integer is normalized through `i64` so that `007` reads back as `7`. A
/// fractional literal is passed through as written, because it is a `DOUBLE`'s
/// value and the engine reads the same IEEE 754 binary64 MySQL does. The
/// dialect's assignment validator holds an integer column to integers, so a
/// fractional value cannot land in one.
fn render_dml_number(value: &str) -> Result<String, ParseError> {
    if let Ok(integer) = value.parse::<i64>() {
        return Ok(integer.to_string());
    }
    if value.parse::<f64>().is_ok_and(f64::is_finite) {
        return Ok(value.to_owned());
    }
    unsupported("DML numeric literal outside signed 64-bit integer range")
}

/// Renders the `WHERE` of an `UPDATE` or a `DELETE`.
///
/// A comparison goes through the same checked path a `SELECT` comparison does,
/// and is recorded in `render_context` so the frontend can hold it to the same
/// rule: the two engines only agree about a comparison on a signed integer
/// column, which is what that rule was measured for.
fn render_dml_predicate(
    expr: &Expr,
    render_context: &mut SelectRenderContext,
) -> Result<String, ParseError> {
    match expr {
        Expr::BinaryOp { left, op, right }
            if matches!(op, BinaryOperator::And | BinaryOperator::Or) =>
        {
            let op = if matches!(op, BinaryOperator::And) {
                "AND"
            } else {
                "OR"
            };
            Ok(format!(
                "({} {op} {})",
                render_dml_predicate(left, render_context)?,
                render_dml_predicate(right, render_context)?
            ))
        }
        Expr::BinaryOp { left, op, right } if is_checked_select_comparison_operator(op) => {
            render_checked_select_comparison(left, op, right, render_context)
        }
        Expr::Like {
            negated,
            any,
            expr,
            pattern,
            escape_char,
        } => render_checked_like(
            *negated,
            *any,
            expr,
            pattern,
            escape_char.as_ref(),
            render_context,
        ),
        Expr::IsNull(expr) => Ok(format!("({} IS NULL)", render_dml_expr(expr)?)),
        Expr::IsNotNull(expr) => Ok(format!("({} IS NOT NULL)", render_dml_expr(expr)?)),
        Expr::UnaryOp {
            op: UnaryOperator::Not,
            expr,
        } => Ok(format!(
            "(NOT {})",
            render_dml_predicate(expr, render_context)?
        )),
        Expr::Nested(expr) => Ok(format!("({})", render_dml_predicate(expr, render_context)?)),
        Expr::Value(value) if matches!(&value.value, Value::Boolean(_)) => render_dml_expr(expr),
        _ => unsupported("DML WHERE predicate"),
    }
}

#[derive(Default)]
struct SelectRenderContext {
    checked_comparisons: Vec<CheckedSelectComparison>,
    parameter_count: usize,
}

impl SelectRenderContext {
    fn next_parameter_ordinal(&mut self) -> Result<usize, ParseError> {
        let ordinal = self.parameter_count;
        self.parameter_count =
            self.parameter_count
                .checked_add(1)
                .ok_or(ParseError::Unsupported {
                    feature: "SELECT parameter count outside usize range",
                })?;
        Ok(ordinal)
    }
}

fn render_select_item(
    item: &SelectItem,
    render_context: &mut SelectRenderContext,
) -> Result<String, ParseError> {
    match item {
        // The engine names a result column after the expression text, which
        // quotes an identifier. MySQL names it after the call as written, so an
        // unnamed count carries that name as an alias.
        SelectItem::UnnamedExpr(expr @ Expr::Function(function))
            if static_select_metadata::is_count_call(function)
                || static_select_metadata::column_aggregate_argument(function).is_some() =>
        {
            Ok(format!(
                "{} AS \"{}\"",
                render_select_expr(expr, render_context)?,
                mysql_aggregate_column_name(function).replace('"', "\"\"")
            ))
        }
        SelectItem::UnnamedExpr(expr) => render_select_expr(expr, render_context),
        SelectItem::ExprWithAlias { expr, alias } => Ok(format!(
            "{} AS {}",
            render_select_expr(expr, render_context)?,
            render_ident(alias)
        )),
        SelectItem::Wildcard(options) if wildcard_options_are_empty(options) => Ok("*".to_string()),
        SelectItem::Wildcard(_) => unsupported("SELECT wildcard option"),
        _ => unsupported("SELECT projection"),
    }
}

/// Returns the name MySQL gives an unaliased aggregate column.
///
/// Measured on MySQL 8.4.11: the call as written, case included, and with the
/// argument unquoted — `COUNT(n)`, not `COUNT("n")`.
fn mysql_aggregate_column_name(function: &sqlparser::ast::Function) -> String {
    format!("{}({})", function.name, aggregate_argument(function, false))
}

/// Renders a checked aggregate call, keeping the spelling it was written with.
///
/// MySQL names the result column after the call as written, case included:
/// measured, `count(*)` keeps its lower case. The engine names it the same way
/// from this text, so nothing else has to carry the name.
fn render_aggregate_call(function: &sqlparser::ast::Function) -> String {
    format!("{}({})", function.name, aggregate_argument(function, true))
}

fn aggregate_argument(function: &sqlparser::ast::Function, quoted: bool) -> String {
    let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
        unreachable!("a checked aggregate was checked to have an argument list")
    };
    match arguments.args.as_slice() {
        [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Wildcard)] => {
            "*".to_owned()
        }
        [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Identifier(column),
        ))] => {
            if quoted {
                render_ident(column)
            } else {
                column.value.clone()
            }
        }
        _ => unreachable!("a checked aggregate was checked to take one wildcard or column"),
    }
}

fn wildcard_options_are_empty(options: &sqlparser::ast::WildcardAdditionalOptions) -> bool {
    options.opt_ilike.is_none()
        && options.opt_exclude.is_none()
        && options.opt_except.is_none()
        && options.opt_replace.is_none()
        && options.opt_rename.is_none()
        && options.opt_alias.is_none()
}

fn render_select_table(table: &TableFactor) -> Result<(String, MySqlTableName), ParseError> {
    let TableFactor::Table {
        name,
        alias,
        args,
        with_hints,
        version,
        with_ordinality,
        partitions,
        json_path,
        sample,
        index_hints,
    } = table
    else {
        return unsupported("SELECT table source");
    };
    if args.is_some()
        || !with_hints.is_empty()
        || version.is_some()
        || *with_ordinality
        || !partitions.is_empty()
        || json_path.is_some()
        || sample.is_some()
        || !index_hints.is_empty()
    {
        return unsupported("SELECT table option");
    }
    let [ObjectNamePart::Identifier(ident)] = name.0.as_slice() else {
        return unsupported("qualified SELECT table source");
    };
    let source_table = MySqlTableName::parse(&ident.value)?;
    let mut rendered = render_unqualified_name(name)?;
    if let Some(alias) = alias {
        if !alias.columns.is_empty() || alias.at.is_some() {
            return unsupported("SELECT table alias option");
        }
        rendered.push_str(" AS ");
        rendered.push_str(&render_ident(&alias.name));
    }
    Ok((rendered, source_table))
}

fn render_select_expr(
    expr: &Expr,
    render_context: &mut SelectRenderContext,
) -> Result<String, ParseError> {
    match expr {
        Expr::Identifier(ident) => Ok(render_ident(ident)),
        Expr::CompoundIdentifier(parts) if parts.len() == 2 => Ok(format!(
            "{}.{}",
            render_ident(&parts[0]),
            render_ident(&parts[1])
        )),
        Expr::Value(value) => match &value.value {
            Value::Number(value, false) => value
                .parse::<i64>()
                .map(|value| value.to_string())
                .map_err(|_| ParseError::Unsupported {
                    feature: "SELECT numeric literal outside signed 64-bit integer range",
                }),
            Value::SingleQuotedString(value) | Value::DoubleQuotedString(value) => {
                Ok(format!("'{}'", value.replace('\'', "''")))
            }
            Value::Boolean(value) => Ok(if *value { "TRUE" } else { "FALSE" }.to_string()),
            Value::Null => Ok("NULL".to_string()),
            Value::Placeholder(marker) if marker == "?" => {
                render_context.next_parameter_ordinal()?;
                Ok("?".to_string())
            }
            _ => unsupported("SELECT literal"),
        },
        // The engine counts rows and non-null values the way MySQL does, so a
        // COUNT crosses without changing what it means.
        Expr::Function(function)
            if static_select_metadata::is_count_call(function)
                || static_select_metadata::column_aggregate_argument(function).is_some() =>
        {
            Ok(render_aggregate_call(function))
        }
        Expr::IsNull(expr) => Ok(format!(
            "({} IS NULL)",
            render_select_expr(expr, render_context)?
        )),
        Expr::IsNotNull(expr) => Ok(format!(
            "({} IS NOT NULL)",
            render_select_expr(expr, render_context)?
        )),
        Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr,
        } if matches!(expr.as_ref(), Expr::Value(value) if matches!(&value.value, Value::Number(_, false))) =>
        {
            let Expr::Value(value) = expr.as_ref() else {
                unreachable!("guard requires a numeric literal");
            };
            let Value::Number(value, false) = &value.value else {
                unreachable!("guard requires a numeric literal");
            };
            let magnitude = value.parse::<u64>().map_err(|_| ParseError::Unsupported {
                feature: "SELECT numeric literal outside signed 64-bit integer range",
            })?;
            if magnitude > (i64::MAX as u64) + 1 {
                return unsupported("SELECT numeric literal outside signed 64-bit integer range");
            }
            Ok(format!("(-{magnitude})"))
        }
        Expr::UnaryOp {
            op: UnaryOperator::Plus,
            expr,
        } if matches!(expr.as_ref(), Expr::Value(value) if matches!(&value.value, Value::Number(_, false))) => {
            Ok(format!("(+{})", render_select_expr(expr, render_context)?))
        }
        Expr::Nested(expr) => Ok(format!("({})", render_select_expr(expr, render_context)?)),
        Expr::Function(function)
            if matches!(function.name.0.as_slice(), [ObjectNamePart::Identifier(name)] if name.value.eq_ignore_ascii_case("LAST_INSERT_ID"))
                && !function.uses_odbc_syntax
                && matches!(&function.parameters, FunctionArguments::None)
                && matches!(&function.args, FunctionArguments::List(arguments) if arguments.args.is_empty() && arguments.duplicate_treatment.is_none() && arguments.clauses.is_empty())
                && function.filter.is_none()
                && function.null_treatment.is_none()
                && function.over.is_none()
                && function.within_group.is_empty() =>
        {
            Ok("last_insert_id()".to_string())
        }
        _ => unsupported("SELECT expression"),
    }
}

fn render_select_predicate(
    expr: &Expr,
    render_context: &mut SelectRenderContext,
) -> Result<String, ParseError> {
    match expr {
        Expr::IsNull(expr) => Ok(format!(
            "({} IS NULL)",
            render_select_expr(expr, render_context)?
        )),
        Expr::IsNotNull(expr) => Ok(format!(
            "({} IS NOT NULL)",
            render_select_expr(expr, render_context)?
        )),
        Expr::BinaryOp { left, op, right }
            if matches!(op, BinaryOperator::And | BinaryOperator::Or) =>
        {
            let op = if matches!(op, BinaryOperator::And) {
                "AND"
            } else {
                "OR"
            };
            Ok(format!(
                "({} {op} {})",
                render_select_predicate(left, render_context)?,
                render_select_predicate(right, render_context)?
            ))
        }
        Expr::BinaryOp { left, op, right } if is_checked_select_comparison_operator(op) => {
            render_checked_select_comparison(left, op, right, render_context)
        }
        Expr::Like {
            negated,
            any,
            expr,
            pattern,
            escape_char,
        } => render_checked_like(
            *negated,
            *any,
            expr,
            pattern,
            escape_char.as_ref(),
            render_context,
        ),
        Expr::UnaryOp {
            op: UnaryOperator::Not,
            expr,
        } => Ok(format!(
            "(NOT {})",
            render_select_predicate(expr, render_context)?
        )),
        Expr::Nested(expr) => Ok(format!(
            "({})",
            render_select_predicate(expr, render_context)?
        )),
        Expr::Value(value) if matches!(&value.value, Value::Boolean(_)) => {
            render_select_expr(expr, render_context)
        }
        _ => unsupported("SELECT WHERE predicate before coercion calibration"),
    }
}

fn render_checked_select_comparison(
    left: &Expr,
    op: &BinaryOperator,
    right: &Expr,
    render_context: &mut SelectRenderContext,
) -> Result<String, ParseError> {
    let Expr::Identifier(column) = left else {
        return unsupported("SELECT comparison requires one unqualified column");
    };
    let column_name = column.value.clone();
    let (rendered_right, rhs) = render_checked_select_comparison_rhs(right, render_context)?;
    let operator = checked_select_comparison_operator(op).expect("comparison operator guard");
    // MySQL's default collation ignores case, so a text comparison asks the
    // engine for NOCASE rather than its byte order. This is left off every
    // other comparison because a collation the index does not carry stops the
    // planner from using it, and an integer comparison gains nothing from it.
    let collation = match rhs {
        CheckedSelectComparisonRhs::Text(_) => " COLLATE NOCASE",
        _ => "",
    };
    let rendered = format!(
        "({}{collation} {} {rendered_right})",
        render_ident(column),
        checked_select_comparison_sql_operator(op)
    );
    render_context
        .checked_comparisons
        .push(CheckedSelectComparison {
            column_name,
            operator,
            rhs,
        });
    Ok(rendered)
}

/// Renders a `LIKE` against one column, which the engine already matches the
/// way MySQL's default collation does: both ignore ASCII case.
fn render_checked_like(
    negated: bool,
    any: bool,
    expr: &Expr,
    pattern: &Expr,
    escape_char: Option<&sqlparser::ast::ValueWithSpan>,
    render_context: &mut SelectRenderContext,
) -> Result<String, ParseError> {
    if any || escape_char.is_some() {
        return unsupported("SELECT LIKE option");
    }
    let Expr::Identifier(column) = expr else {
        return unsupported("SELECT LIKE requires one unqualified column");
    };
    let Expr::Value(value) = pattern else {
        return unsupported("SELECT LIKE requires a string pattern");
    };
    let (Value::SingleQuotedString(text) | Value::DoubleQuotedString(text)) = &value.value else {
        return unsupported("SELECT LIKE requires a string pattern");
    };
    // MySQL takes a backslash in a pattern as an escape and the engine takes it
    // literally, so a pattern that contains one would match different rows.
    if text.contains('\\') {
        return unsupported("SELECT LIKE pattern with a backslash");
    }
    let rendered = format!(
        "({} {}LIKE '{}')",
        render_ident(column),
        if negated { "NOT " } else { "" },
        text.replace('\'', "''")
    );
    render_context
        .checked_comparisons
        .push(CheckedSelectComparison {
            column_name: column.value.clone(),
            operator: if negated {
                CheckedSelectComparisonOperator::NotLike
            } else {
                CheckedSelectComparisonOperator::Like
            },
            rhs: CheckedSelectComparisonRhs::Text(text.clone()),
        });
    Ok(rendered)
}

fn render_checked_select_comparison_rhs(
    expr: &Expr,
    render_context: &mut SelectRenderContext,
) -> Result<(String, CheckedSelectComparisonRhs), ParseError> {
    match expr {
        Expr::Nested(expr) => {
            let (rendered, rhs) = render_checked_select_comparison_rhs(expr, render_context)?;
            Ok((format!("({rendered})"), rhs))
        }
        Expr::Value(value) => match &value.value {
            Value::Number(number, false) => {
                let value = number.parse::<i64>().map_err(|_| ParseError::Unsupported {
                    feature: "SELECT comparison literal outside signed 64-bit integer range",
                })?;
                Ok((
                    value.to_string(),
                    CheckedSelectComparisonRhs::SignedInteger(value),
                ))
            }
            Value::SingleQuotedString(text) | Value::DoubleQuotedString(text) => Ok((
                format!("'{}'", text.replace('\'', "''")),
                CheckedSelectComparisonRhs::Text(text.clone()),
            )),
            Value::Null => Ok(("NULL".to_string(), CheckedSelectComparisonRhs::Null)),
            Value::Placeholder(marker) if marker == "?" => {
                let ordinal = render_context.next_parameter_ordinal()?;
                Ok((
                    "?".to_string(),
                    CheckedSelectComparisonRhs::Placeholder { ordinal },
                ))
            }
            _ => unsupported(
                "SELECT comparison requires an exact signed integer, a string, NULL, or ?",
            ),
        },
        Expr::UnaryOp { op, expr }
            if matches!(op, UnaryOperator::Minus | UnaryOperator::Plus)
                && matches!(expr.as_ref(), Expr::Value(value) if matches!(&value.value, Value::Number(_, false))) =>
        {
            let Expr::Value(value) = expr.as_ref() else {
                unreachable!("guard requires a numeric literal");
            };
            let Value::Number(number, false) = &value.value else {
                unreachable!("guard requires a numeric literal");
            };
            let magnitude = number.parse::<u64>().map_err(|_| ParseError::Unsupported {
                feature: "SELECT comparison literal outside signed 64-bit integer range",
            })?;
            let value = if matches!(op, UnaryOperator::Minus) {
                if magnitude > (i64::MAX as u64) + 1 {
                    return unsupported(
                        "SELECT comparison literal outside signed 64-bit integer range",
                    );
                }
                if magnitude == (i64::MAX as u64) + 1 {
                    i64::MIN
                } else {
                    -(magnitude as i64)
                }
            } else {
                i64::try_from(magnitude).map_err(|_| ParseError::Unsupported {
                    feature: "SELECT comparison literal outside signed 64-bit integer range",
                })?
            };
            Ok((
                if matches!(op, UnaryOperator::Minus) {
                    format!("(-{magnitude})")
                } else {
                    format!("(+{magnitude})")
                },
                CheckedSelectComparisonRhs::SignedInteger(value),
            ))
        }
        _ => {
            unsupported("SELECT comparison requires an exact signed integer, a string, NULL, or ?")
        }
    }
}

fn is_checked_select_comparison_operator(operator: &BinaryOperator) -> bool {
    matches!(
        operator,
        BinaryOperator::Eq
            | BinaryOperator::NotEq
            | BinaryOperator::Lt
            | BinaryOperator::LtEq
            | BinaryOperator::Gt
            | BinaryOperator::GtEq
    )
}

fn checked_select_comparison_operator(
    operator: &BinaryOperator,
) -> Option<CheckedSelectComparisonOperator> {
    Some(match operator {
        BinaryOperator::Eq => CheckedSelectComparisonOperator::Equal,
        BinaryOperator::NotEq => CheckedSelectComparisonOperator::NotEqual,
        BinaryOperator::Lt => CheckedSelectComparisonOperator::LessThan,
        BinaryOperator::LtEq => CheckedSelectComparisonOperator::LessThanOrEqual,
        BinaryOperator::Gt => CheckedSelectComparisonOperator::GreaterThan,
        BinaryOperator::GtEq => CheckedSelectComparisonOperator::GreaterThanOrEqual,
        _ => return None,
    })
}

fn checked_select_comparison_sql_operator(operator: &BinaryOperator) -> &'static str {
    match operator {
        BinaryOperator::Eq => "=",
        BinaryOperator::NotEq => "<>",
        BinaryOperator::Lt => "<",
        BinaryOperator::LtEq => "<=",
        BinaryOperator::Gt => ">",
        BinaryOperator::GtEq => ">=",
        _ => unreachable!("comparison operator guard"),
    }
}

fn render_simple_view_query(query: &sqlparser::ast::Query) -> Result<String, ParseError> {
    if query.with.is_some()
        || query.order_by.is_some()
        || query.limit_clause.is_some()
        || query.fetch.is_some()
        || !query.locks.is_empty()
        || query.for_clause.is_some()
        || query.settings.is_some()
        || query.format_clause.is_some()
        || !query.pipe_operators.is_empty()
    {
        return unsupported("CREATE VIEW query clause");
    }
    let SetExpr::Select(select) = query.body.as_ref() else {
        return unsupported("CREATE VIEW query");
    };
    if !matches!(select.flavor, SelectFlavor::Standard)
        || !select.optimizer_hints.is_empty()
        || select.distinct.is_some()
        || select.select_modifiers.is_some()
        || select.top.is_some()
        || select.top_before_distinct
        || select.exclude.is_some()
        || select.into.is_some()
        || !select.lateral_views.is_empty()
        || select.prewhere.is_some()
        || select.selection.is_some()
        || !select.connect_by.is_empty()
        || !matches!(&select.group_by, sqlparser::ast::GroupByExpr::Expressions(exprs, _) if exprs.is_empty())
        || !select.cluster_by.is_empty()
        || !select.distribute_by.is_empty()
        || !select.sort_by.is_empty()
        || select.having.is_some()
        || !select.named_window.is_empty()
        || select.qualify.is_some()
        || select.window_before_qualify
        || select.value_table_mode.is_some()
    {
        return unsupported("CREATE VIEW SELECT feature");
    }
    let [from] = select.from.as_slice() else {
        return unsupported("CREATE VIEW FROM clause");
    };
    if !from.joins.is_empty() {
        return unsupported("CREATE VIEW JOIN");
    }
    let TableFactor::Table {
        name,
        alias,
        args,
        with_hints,
        version,
        with_ordinality,
        partitions,
        json_path,
        sample,
        index_hints,
    } = &from.relation
    else {
        return unsupported("CREATE VIEW table source");
    };
    if alias.is_some()
        || args.is_some()
        || !with_hints.is_empty()
        || version.is_some()
        || *with_ordinality
        || !partitions.is_empty()
        || json_path.is_some()
        || sample.is_some()
        || !index_hints.is_empty()
    {
        return unsupported("CREATE VIEW table option");
    }
    let table_name = render_unqualified_name(name)?;
    let columns = select
        .projection
        .iter()
        .map(|item| match item {
            SelectItem::UnnamedExpr(Expr::Identifier(column)) => Ok(render_ident(column)),
            _ => unsupported("CREATE VIEW projection"),
        })
        .collect::<Result<Vec<_>, _>>()?;
    if columns.is_empty() {
        return unsupported("CREATE VIEW without projections");
    }
    Ok(format!("SELECT {} FROM {table_name}", columns.join(", ")))
}

fn translate_alter_table(alter: &AlterTable) -> Result<String, ParseError> {
    if alter.if_exists
        || alter.only
        || alter.location.is_some()
        || alter.on_cluster.is_some()
        || alter.table_type.is_some()
    {
        return unsupported("ALTER TABLE option");
    }
    let [operation] = alter.operations.as_slice() else {
        return unsupported("multiple ALTER TABLE operations");
    };
    let table_name = render_name(&alter.name)?;

    match operation {
        AlterTableOperation::AddColumn {
            if_not_exists,
            column_def,
            column_position,
            ..
        } => {
            if *if_not_exists || column_position.is_some() {
                return unsupported("ADD COLUMN option");
            }
            Ok(format!(
                "ALTER TABLE {table_name} ADD COLUMN {}",
                render_column(column_def)?
            ))
        }
        AlterTableOperation::DropColumn {
            column_names,
            if_exists,
            drop_behavior,
            ..
        } => {
            let [column_name] = column_names.as_slice() else {
                return unsupported("multiple DROP COLUMN names");
            };
            if *if_exists || drop_behavior.is_some() {
                return unsupported("DROP COLUMN option");
            }
            Ok(format!(
                "ALTER TABLE {table_name} DROP COLUMN {}",
                render_ident(column_name)
            ))
        }
        AlterTableOperation::RenameColumn {
            old_column_name,
            new_column_name,
        } => Ok(format!(
            "ALTER TABLE {table_name} RENAME COLUMN {} TO {}",
            render_ident(old_column_name),
            render_ident(new_column_name)
        )),
        AlterTableOperation::RenameTable {
            table_name: RenameTableNameKind::To(new_table_name),
        } => Ok(format!(
            "ALTER TABLE {table_name} RENAME TO {}",
            render_unqualified_name(new_table_name)?
        )),
        AlterTableOperation::RenameTable { .. } => unsupported("RENAME TABLE AS"),
        _ => unsupported("ALTER TABLE operation"),
    }
}

fn render_index_column(column: &IndexColumn) -> Result<String, ParseError> {
    if column.operator_class.is_some()
        || column.column.options.asc.is_some()
        || column.column.options.nulls_first.is_some()
        || column.column.with_fill.is_some()
    {
        return unsupported("CREATE INDEX column option");
    }
    let Expr::Identifier(identifier) = &column.column.expr else {
        return unsupported("CREATE INDEX expression");
    };
    Ok(render_ident(identifier))
}

fn reject_table_attributes(table: &CreateTable) -> Result<(), ParseError> {
    if table.or_replace
        || table.external
        || table.dynamic
        || table.global.is_some()
        || table.transient
        || table.volatile
        || table.iceberg
        || table.snapshot
        || !matches!(table.hive_distribution, HiveDistributionStyle::NONE)
        || table.hive_formats.is_some()
        || !matches!(table.table_options, CreateTableOptions::None)
        || table.file_format.is_some()
        || table.location.is_some()
        || table.query.is_some()
        || table.without_rowid
        || table.like.is_some()
        || table.clone.is_some()
        || table.version.is_some()
        || table.comment.is_some()
        || table.on_commit.is_some()
        || table.on_cluster.is_some()
        || table.primary_key.is_some()
        || table.order_by.is_some()
        || table.partition_by.is_some()
        || table.cluster_by.is_some()
        || table.clustered_by.is_some()
        || table.inherits.is_some()
        || table.partition_of.is_some()
        || table.for_values.is_some()
        || table.strict
        || table.copy_grants
        || table.enable_schema_evolution.is_some()
        || table.change_tracking.is_some()
        || table.data_retention_time_in_days.is_some()
        || table.max_data_extension_time_in_days.is_some()
        || table.default_ddl_collation.is_some()
        || table.with_aggregation_policy.is_some()
        || table.with_row_access_policy.is_some()
        || table.with_storage_lifecycle_policy.is_some()
        || table.with_tags.is_some()
        || table.external_volume.is_some()
        || table.base_location.is_some()
        || table.catalog.is_some()
        || table.catalog_sync.is_some()
        || table.storage_serialization_policy.is_some()
        || table.target_lag.is_some()
        || table.warehouse.is_some()
        || table.refresh_mode.is_some()
        || table.initialize.is_some()
        || table.require_user
        || table.diststyle.is_some()
        || table.distkey.is_some()
        || table.sortkey.is_some()
        || table.backup.is_some()
    {
        return unsupported("table attributes");
    }
    Ok(())
}

fn render_column(column: &ColumnDef) -> Result<String, ParseError> {
    let name = render_ident(&column.name);
    let data_type = match &column.data_type {
        DataType::TinyInt(None) => "TINYINT".to_owned(),
        DataType::SmallInt(None) => "SMALLINT".to_owned(),
        DataType::MediumInt(None) => "MEDIUMINT".to_owned(),
        DataType::Int(None) => "INT".to_owned(),
        DataType::Integer(None) => "INTEGER".to_owned(),
        DataType::BigInt(None) => "BIGINT".to_owned(),
        DataType::Text => "TEXT".to_owned(),
        DataType::Blob(None) => "BLOB".to_owned(),
        DataType::Varchar(length) => format!("VARCHAR({})", declared_character_length(*length)?),
        DataType::Char(length) => format!("CHAR({})", declared_character_length(*length)?),
        // MySQL's DOUBLE and the engine's REAL are both IEEE 754 binary64, so
        // the name carries across without changing what a value means. A
        // precision is MySQL's deprecated `DOUBLE(p,s)`, which is refused.
        DataType::Double(sqlparser::ast::ExactNumberInfo::None) => "DOUBLE".to_owned(),
        // MySQL stores BOOLEAN and BOOL as TINYINT and reports both as
        // `tinyint(1)`. The name is kept so that the display width survives a
        // round trip; the value is a TINYINT's and is checked as one.
        DataType::Boolean | DataType::Bool => "BOOLEAN".to_owned(),
        // A fractional-second precision is refused: MySQL rounds a fractional
        // value to whole seconds without one, measured, and this stores whole
        // seconds only.
        DataType::Datetime(None) => "DATETIME".to_owned(),
        // MySQL's TIMESTAMP is a UTC instant rendered in the session zone; this
        // holds the same text a DATETIME holds and converts nothing, so the two
        // differ only for a session that moves its zone.
        DataType::Timestamp(None, sqlparser::ast::TimezoneInfo::None) => "TIMESTAMP".to_owned(),
        DataType::Decimal(info) | DataType::Numeric(info) | DataType::Dec(info) => {
            let (precision, scale) = declared_decimal_size(*info)?;
            format!("DECIMAL({precision},{scale})")
        }
        _ => return unsupported("column type"),
    };
    reject_duplicate_nullable_column_options(&column.options)?;
    let options = column
        .options
        .iter()
        .map(render_column_option)
        .collect::<Result<Vec<_>, _>>()?;
    let mut definition = format!("{name} {data_type}");
    if !options.is_empty() {
        definition.push(' ');
        definition.push_str(&options.join(" "));
    }
    Ok(definition)
}

/// Reads the precision and scale a `DECIMAL` was declared with.
///
/// A bare `DECIMAL` means `DECIMAL(10,0)`, and a lone precision means a scale of
/// zero — measured on MySQL 8.4.11, where a bare one reports a column_length of
/// 11, the same as `DECIMAL(10,0)`. MySQL caps the precision at 65 and the scale
/// at 30 and requires the scale to fit inside the precision.
fn declared_decimal_size(info: ExactNumberInfo) -> Result<(u32, u32), ParseError> {
    let (precision, scale) = match info {
        ExactNumberInfo::None => (10, 0),
        ExactNumberInfo::Precision(precision) => (precision, 0),
        ExactNumberInfo::PrecisionAndScale(precision, scale) => {
            let scale = u64::try_from(scale).map_err(|_| ParseError::Unsupported {
                feature: "DECIMAL scale",
            })?;
            (precision, scale)
        }
    };
    if precision == 0 || precision > MAX_DECIMAL_PRECISION || scale > MAX_DECIMAL_SCALE {
        return unsupported("DECIMAL size");
    }
    if scale > precision {
        return unsupported("DECIMAL scale wider than its precision");
    }
    Ok((precision as u32, scale as u32))
}

/// Reads the precision and scale from a `DECIMAL` already stored as SQLite DDL.
pub fn stored_decimal_size(data_type: &TursoType) -> Result<(u32, u32), ParseError> {
    let Some(TursoTypeSize::TypeSize(precision, scale)) = data_type.size.as_ref() else {
        return unsupported("DECIMAL without a precision and scale");
    };
    let read = |expr: &TursoExpr| -> Result<u64, ParseError> {
        let TursoExpr::Literal(TursoLiteral::Numeric(text)) = expr else {
            return unsupported("DECIMAL size");
        };
        text.parse::<u64>().map_err(|_| ParseError::Unsupported {
            feature: "DECIMAL size",
        })
    };
    declared_decimal_size(ExactNumberInfo::PrecisionAndScale(
        read(precision)?,
        i64::try_from(read(scale)?).map_err(|_| ParseError::Unsupported {
            feature: "DECIMAL scale",
        })?,
    ))
}

/// Reads the character count a `VARCHAR(n)` or `CHAR(n)` was declared with.
///
/// MySQL counts characters here, not bytes: measured on 8.4.11, a
/// `VARCHAR(4)` stores four multi-byte characters. A bare `VARCHAR` has no
/// length, which MySQL rejects, and so does this.
fn declared_character_length(length: Option<CharacterLength>) -> Result<u32, ParseError> {
    let Some(CharacterLength::IntegerLength { length, unit }) = length else {
        return unsupported("VARCHAR without a length");
    };
    if !matches!(unit, None | Some(CharLengthUnits::Characters)) {
        return unsupported("VARCHAR length unit");
    }
    if length == 0 || length > MAX_VARCHAR_CHARACTERS {
        return unsupported("VARCHAR length");
    }
    u32::try_from(length).map_err(|_| ParseError::Unsupported {
        feature: "VARCHAR length",
    })
}

fn reject_duplicate_nullable_column_options(
    options: &[sqlparser::ast::ColumnOptionDef],
) -> Result<(), ParseError> {
    let mut nullable_options = 0;
    for option in options {
        if matches!(&option.option, ColumnOption::Null | ColumnOption::NotNull) {
            nullable_options += 1;
        }
    }
    if nullable_options > 1 {
        return unsupported("multiple column NULL options");
    }
    Ok(())
}

fn render_column_option(option: &sqlparser::ast::ColumnOptionDef) -> Result<String, ParseError> {
    let name = render_constraint_name(option.name.as_ref());
    match &option.option {
        ColumnOption::Null if option.name.is_none() => Ok("NULL".to_owned()),
        ColumnOption::NotNull if option.name.is_none() => Ok("NOT NULL".to_owned()),
        ColumnOption::PrimaryKey(_) => unsupported("PRIMARY KEY"),
        ColumnOption::Unique(unique) => {
            reject_unique(unique)?;
            Ok(format!("{name}UNIQUE"))
        }
        ColumnOption::Default(expr) if option.name.is_none() => {
            Ok(format!("DEFAULT {}", render_default(expr)?))
        }
        ColumnOption::Check(check) => {
            if check.enforced.is_some() {
                return unsupported("CHECK enforcement attribute");
            }
            Ok(format!("{name}CHECK ({})", render_check(&check.expr)?))
        }
        ColumnOption::ForeignKey(_) => unsupported("column REFERENCES constraint"),
        ColumnOption::Default(_) => unsupported("named DEFAULT constraint"),
        _ => unsupported("column attribute"),
    }
}

fn render_table_constraint(constraint: &TableConstraint) -> Result<String, ParseError> {
    match constraint {
        TableConstraint::PrimaryKey(_) => unsupported("PRIMARY KEY"),
        TableConstraint::Unique(unique) => {
            reject_unique(unique)?;
            Ok(format!(
                "{}UNIQUE ({})",
                render_constraint_name(unique.name.as_ref()),
                render_index_columns(&unique.columns)?
            ))
        }
        TableConstraint::Check(check) => {
            if check.enforced.is_some() {
                return unsupported("CHECK enforcement attribute");
            }
            Ok(format!(
                "{}CHECK ({})",
                render_constraint_name(check.name.as_ref()),
                render_check(&check.expr)?
            ))
        }
        TableConstraint::ForeignKey(foreign_key) => {
            let columns = render_idents(&foreign_key.columns);
            Ok(format!(
                "{}{}",
                render_constraint_name(foreign_key.name.as_ref()),
                render_foreign_key(foreign_key, Some(&columns))?
            ))
        }
        _ => unsupported("table constraint"),
    }
}

fn reject_unique(unique: &sqlparser::ast::UniqueConstraint) -> Result<(), ParseError> {
    if unique.index_name.is_some()
        || !unique.index_type_display.is_none()
        || unique.index_type.is_some()
        || !unique.index_options.is_empty()
        || unique.characteristics.is_some()
        || !matches!(
            unique.nulls_distinct,
            sqlparser::ast::NullsDistinctOption::None
        )
    {
        return unsupported("UNIQUE index attribute");
    }
    Ok(())
}

fn render_foreign_key(
    foreign_key: &sqlparser::ast::ForeignKeyConstraint,
    columns: Option<&str>,
) -> Result<String, ParseError> {
    if foreign_key.index_name.is_some()
        || foreign_key.match_kind.is_some()
        || foreign_key.characteristics.is_some()
    {
        return unsupported("FOREIGN KEY attribute");
    }
    if foreign_key.foreign_table.0.len() != 1 {
        return unsupported("schema-qualified FOREIGN KEY target");
    }
    let name = render_name(&foreign_key.foreign_table)?;
    let referred_columns = if foreign_key.referred_columns.is_empty() {
        String::new()
    } else {
        format!(" ({})", render_idents(&foreign_key.referred_columns))
    };
    let columns = columns.map_or_else(String::new, |columns| format!(" ({columns})"));
    let on_delete = foreign_key
        .on_delete
        .as_ref()
        .map(|action| format!(" ON DELETE {action}"))
        .unwrap_or_default();
    let on_update = foreign_key
        .on_update
        .as_ref()
        .map(|action| format!(" ON UPDATE {action}"))
        .unwrap_or_default();
    Ok(format!(
        "FOREIGN KEY{columns} REFERENCES {name}{referred_columns}{on_delete}{on_update}"
    ))
}

fn render_index_columns(columns: &[IndexColumn]) -> Result<String, ParseError> {
    let mut names = Vec::with_capacity(columns.len());
    for column in columns {
        if column.operator_class.is_some()
            || column.column.options.asc.is_some()
            || column.column.options.nulls_first.is_some()
            || !matches!(&column.column.expr, Expr::Identifier(_))
        {
            return unsupported("indexed column expression or ordering");
        }
        let Expr::Identifier(name) = &column.column.expr else {
            unreachable!("checked indexed column expression")
        };
        names.push(render_ident(name));
    }
    if names.is_empty() {
        return unsupported("empty PRIMARY KEY or UNIQUE column list");
    }
    Ok(names.join(", "))
}

fn render_default(expr: &Expr) -> Result<String, ParseError> {
    match expr {
        Expr::Value(value) => match &value.value {
            Value::Number(value, _) => Ok(value.clone()),
            Value::SingleQuotedString(value) => Ok(format!("'{}'", value.replace('\'', "''"))),
            Value::Boolean(value) => Ok(if *value { "TRUE" } else { "FALSE" }.to_string()),
            Value::Null => Ok("NULL".to_string()),
            _ => unsupported("DEFAULT literal"),
        },
        Expr::UnaryOp { op, expr } => {
            let sign = match op {
                UnaryOperator::Minus => "-",
                UnaryOperator::Plus => "+",
                _ => return unsupported("DEFAULT integer literal"),
            };
            let Expr::Value(value) = expr.as_ref() else {
                return unsupported("DEFAULT integer literal");
            };
            let Value::Number(value, _) = &value.value else {
                return unsupported("DEFAULT integer literal");
            };
            render_signed_integer_default(sign, value)
        }
        _ => unsupported("non-literal DEFAULT expression"),
    }
}

fn render_signed_integer_default(sign: &str, magnitude: &str) -> Result<String, ParseError> {
    let magnitude = magnitude
        .parse::<u64>()
        .map_err(|_| ParseError::Unsupported {
            feature: "DEFAULT integer literal",
        })?;
    let limit = if sign == "-" {
        (i64::MAX as u64) + 1
    } else {
        i64::MAX as u64
    };
    if magnitude > limit {
        return unsupported("DEFAULT integer literal");
    }
    Ok(format!("{sign}{magnitude}"))
}

fn reject_unsupported_mysql_string_escapes(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<(), ParseError> {
    if mode.no_backslash_escapes {
        return Ok(());
    }
    // sqlparser accepts some escapes with semantics that do not match MySQL;
    // reject them before a normalized statement can be persisted.
    let bytes = sql.as_bytes();
    let mut cursor = 0;
    let mut quote = None;
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        if let Some((delimiter, check_escapes)) = quote {
            if check_escapes && byte == b'\\' {
                let Some(escaped) = bytes.get(cursor + 1).copied() else {
                    return unsupported("unsupported MySQL string escape");
                };
                if !matches!(
                    escaped,
                    b'0' | b'\'' | b'"' | b'b' | b'n' | b'r' | b't' | b'Z' | b'\\' | b'%' | b'_'
                ) {
                    return unsupported("unsupported MySQL string escape");
                }
                cursor += 2;
                continue;
            }
            if byte == delimiter {
                if bytes.get(cursor + 1) == Some(&delimiter) {
                    cursor += 2;
                } else {
                    quote = None;
                    cursor += 1;
                }
            } else {
                cursor += 1;
            }
            continue;
        }
        match byte {
            b'\'' => {
                quote = Some((byte, true));
                cursor += 1;
            }
            b'`' => {
                quote = Some((byte, false));
                cursor += 1;
            }
            b'"' => {
                quote = Some((byte, !mode.ansi_quotes));
                cursor += 1;
            }
            b'#' => {
                cursor = bytes[cursor..]
                    .iter()
                    .position(|byte| *byte == b'\n' || *byte == b'\r')
                    .map_or(bytes.len(), |offset| cursor + offset);
            }
            b'-' if bytes.get(cursor + 1) == Some(&b'-') => {
                cursor = bytes[cursor + 2..]
                    .iter()
                    .position(|byte| *byte == b'\n' || *byte == b'\r')
                    .map_or(bytes.len(), |offset| cursor + 2 + offset);
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                let Some(offset) = bytes[cursor + 2..]
                    .windows(2)
                    .position(|window| window == b"*/")
                else {
                    return Ok(());
                };
                cursor += offset + 4;
            }
            _ => cursor += 1,
        }
    }
    Ok(())
}

fn render_check(expr: &Expr) -> Result<String, ParseError> {
    if is_straightforward_check(expr) {
        Ok(expr.to_string())
    } else {
        unsupported("CHECK expression")
    }
}

fn is_straightforward_check(expr: &Expr) -> bool {
    match expr {
        Expr::Identifier(_) => true,
        Expr::CompoundIdentifier(parts) => !parts.is_empty(),
        Expr::Value(value) => matches!(
            value.value,
            Value::Number(_, _) | Value::SingleQuotedString(_) | Value::Boolean(_) | Value::Null
        ),
        Expr::Nested(expr) | Expr::IsNull(expr) | Expr::IsNotNull(expr) => {
            is_straightforward_check(expr)
        }
        Expr::UnaryOp { op, expr } => {
            matches!(op, UnaryOperator::Plus | UnaryOperator::Minus)
                && is_straightforward_check(expr)
        }
        Expr::BinaryOp { left, op, right } => {
            matches!(
                op,
                BinaryOperator::Plus
                    | BinaryOperator::Minus
                    | BinaryOperator::Multiply
                    | BinaryOperator::Modulo
                    | BinaryOperator::Gt
                    | BinaryOperator::Lt
                    | BinaryOperator::GtEq
                    | BinaryOperator::LtEq
                    | BinaryOperator::Eq
                    | BinaryOperator::NotEq
                    | BinaryOperator::And
                    | BinaryOperator::Or
            ) && is_straightforward_check(left)
                && is_straightforward_check(right)
        }
        Expr::Between {
            expr,
            negated,
            low,
            high,
        } => {
            !negated
                && is_straightforward_check(expr)
                && is_straightforward_check(low)
                && is_straightforward_check(high)
        }
        Expr::InList { expr, list, .. } => {
            is_straightforward_check(expr) && list.iter().all(is_straightforward_check)
        }
        _ => false,
    }
}

fn render_name(name: &ObjectName) -> Result<String, ParseError> {
    if !(1..=2).contains(&name.0.len()) {
        return unsupported("object name with more than two parts");
    }
    let mut parts = Vec::with_capacity(name.0.len());
    for part in &name.0 {
        let ObjectNamePart::Identifier(ident) = part else {
            return unsupported("dynamic object name");
        };
        parts.push(render_ident(ident));
    }
    Ok(parts.join("."))
}

fn render_unqualified_name(name: &ObjectName) -> Result<String, ParseError> {
    let [ObjectNamePart::Identifier(ident)] = name.0.as_slice() else {
        return unsupported("schema-qualified rename target");
    };
    Ok(render_ident(ident))
}

fn render_idents(idents: &[Ident]) -> String {
    idents
        .iter()
        .map(render_ident)
        .collect::<Vec<_>>()
        .join(", ")
}

fn render_constraint_name(name: Option<&Ident>) -> String {
    name.map(|name| format!("CONSTRAINT {} ", render_ident(name)))
        .unwrap_or_default()
}

fn render_ident(ident: &Ident) -> String {
    format!("\"{}\"", ident.value.replace('"', "\"\""))
}

/// Renders one checked Turso `CREATE TABLE` AST as normalized MySQL DDL.
///
/// This is deliberately a checked renderer. It accepts only the AST shapes that
/// [`parse_create_table_ast`] can produce, so callers cannot accidentally turn a
/// broader SQLite AST into MySQL DDL with different behavior.
pub fn render_create_table_mysql(statement: &Stmt) -> Result<String, ParseError> {
    render_create_table_mysql_with_mode(statement, SessionSqlMode::default())
}

/// Renders a checked Turso `CREATE TABLE` AST using one session's string rules.
pub fn render_create_table_mysql_with_mode(
    statement: &Stmt,
    mode: SessionSqlMode,
) -> Result<String, ParseError> {
    let Stmt::CreateTable {
        temporary,
        if_not_exists,
        tbl_name,
        body,
    } = statement
    else {
        return Err(ParseError::ExpectedCreateTable);
    };
    let TursoCreateTableBody::ColumnsAndConstraints {
        columns,
        constraints,
        options,
    } = body
    else {
        return unsupported("CREATE TABLE AS SELECT");
    };
    if options.without_rowid_text.is_some() || options.strict_text.is_some() {
        return unsupported("SQLite table options");
    }
    if columns.is_empty() {
        return unsupported("CREATE TABLE without columns");
    }

    let mut definitions = columns
        .iter()
        .map(|column| render_mysql_column(column, mode))
        .collect::<Result<Vec<_>, _>>()?;
    definitions.extend(
        constraints
            .iter()
            .map(|constraint| render_mysql_table_constraint(constraint, mode))
            .collect::<Result<Vec<_>, _>>()?,
    );

    let temporary = if *temporary { "TEMPORARY " } else { "" };
    let if_not_exists = if *if_not_exists { "IF NOT EXISTS " } else { "" };
    Ok(format!(
        "CREATE {temporary}TABLE {if_not_exists}{} ({})",
        render_mysql_qualified_name(tbl_name)?,
        definitions.join(", ")
    ))
}

/// Renders one checked Turso `CREATE INDEX` AST as normalized MySQL DDL.
pub fn render_create_index_mysql(statement: &Stmt) -> Result<String, ParseError> {
    render_create_index_mysql_with_mode(statement, SessionSqlMode::default())
}

/// Renders one checked Turso `CREATE INDEX` AST using one session's string rules.
pub fn render_create_index_mysql_with_mode(
    statement: &Stmt,
    _mode: SessionSqlMode,
) -> Result<String, ParseError> {
    let Stmt::CreateIndex {
        unique,
        if_not_exists,
        idx_name,
        tbl_name,
        using,
        columns,
        with_clause,
        where_clause,
    } = statement
    else {
        return Err(ParseError::ExpectedCreateIndex);
    };
    if *if_not_exists
        || idx_name.db_name.is_some()
        || idx_name.alias.is_some()
        || using.is_some()
        || !with_clause.is_empty()
        || where_clause.is_some()
    {
        return unsupported("CREATE INDEX option");
    }
    let columns = render_mysql_sorted_columns(columns)?;
    let unique = if *unique { "UNIQUE " } else { "" };
    Ok(format!(
        "CREATE {unique}INDEX {} ON {} ({columns})",
        render_mysql_name(&idx_name.name),
        render_mysql_name(tbl_name)
    ))
}

/// Renders one checked Turso `CREATE VIEW` AST as normalized MySQL DDL.
pub fn render_create_view_mysql(statement: &Stmt) -> Result<String, ParseError> {
    render_create_view_mysql_with_mode(statement, SessionSqlMode::default())
}

/// Renders one checked Turso `CREATE VIEW` AST using one session's string rules.
pub fn render_create_view_mysql_with_mode(
    statement: &Stmt,
    _mode: SessionSqlMode,
) -> Result<String, ParseError> {
    let Stmt::CreateView {
        temporary,
        if_not_exists,
        view_name,
        columns,
        select,
    } = statement
    else {
        return Err(ParseError::ExpectedCreateView);
    };
    if *temporary
        || *if_not_exists
        || view_name.db_name.is_some()
        || view_name.alias.is_some()
        || !columns.is_empty()
    {
        return unsupported("CREATE VIEW option");
    }
    Ok(format!(
        "CREATE VIEW {} AS {}",
        render_mysql_name(&view_name.name),
        render_mysql_view_select(select)?
    ))
}

/// Renders one checked Turso `CREATE TRIGGER` AST as normalized MySQL DDL.
pub fn render_create_trigger_mysql(statement: &Stmt) -> Result<String, ParseError> {
    render_create_trigger_mysql_with_mode(statement, SessionSqlMode::default())
}

/// Renders one checked Turso `CREATE TRIGGER` AST using one session's string rules.
pub fn render_create_trigger_mysql_with_mode(
    statement: &Stmt,
    mode: SessionSqlMode,
) -> Result<String, ParseError> {
    let Stmt::CreateTrigger {
        temporary,
        if_not_exists,
        trigger_name,
        time,
        event,
        tbl_name,
        for_each_row,
        when_clause,
        commands,
    } = statement
    else {
        return Err(ParseError::ExpectedCreateTrigger);
    };
    if *temporary
        || *if_not_exists
        || trigger_name.db_name.is_some()
        || trigger_name.alias.is_some()
        || tbl_name.db_name.is_some()
        || tbl_name.alias.is_some()
        || *time != Some(turso_parser::ast::TriggerTime::After)
        || !matches!(event, turso_parser::ast::TriggerEvent::Insert)
        || !*for_each_row
        || when_clause.is_some()
    {
        return unsupported("CREATE TRIGGER option");
    }
    let [
        turso_parser::ast::TriggerCmd::Insert {
            or_conflict: None,
            tbl_name: target_table,
            col_names,
            select,
            upsert: None,
            returning,
        },
    ] = commands.as_slice()
    else {
        return unsupported("CREATE TRIGGER body");
    };
    if col_names.is_empty() || !returning.is_empty() {
        return unsupported("CREATE TRIGGER INSERT option");
    }
    if select.with.is_some() || !select.order_by.is_empty() || select.limit.is_some() {
        return unsupported("CREATE TRIGGER INSERT source");
    }
    let OneSelect::Values(rows) = &select.body.select else {
        return unsupported("CREATE TRIGGER INSERT SELECT");
    };
    if !select.body.compounds.is_empty() {
        return unsupported("CREATE TRIGGER INSERT source");
    }
    let [values] = rows.as_slice() else {
        return unsupported("CREATE TRIGGER INSERT VALUES rows");
    };
    if values.len() != col_names.len() {
        return unsupported("CREATE TRIGGER INSERT value count");
    }
    let values = values
        .iter()
        .map(|value| render_mysql_trigger_value(value, mode))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(format!(
        "CREATE TRIGGER {} AFTER INSERT ON {} FOR EACH ROW BEGIN INSERT INTO {} ({}) VALUES ({}); END",
        render_mysql_name(&trigger_name.name),
        render_mysql_name(&tbl_name.name),
        render_mysql_name(target_table),
        col_names
            .iter()
            .map(render_mysql_name)
            .collect::<Vec<_>>()
            .join(", "),
        values.join(", ")
    ))
}

fn render_mysql_trigger_value(
    value: &TursoExpr,
    mode: SessionSqlMode,
) -> Result<String, ParseError> {
    match value {
        TursoExpr::Qualified(prefix, column) if prefix.as_str().eq_ignore_ascii_case("NEW") => {
            Ok(format!("NEW.{}", render_mysql_name(column)))
        }
        TursoExpr::Literal(literal) => render_mysql_literal(literal, mode),
        _ => unsupported("CREATE TRIGGER value expression"),
    }
}

fn render_mysql_view_select(select: &turso_parser::ast::Select) -> Result<String, ParseError> {
    if select.with.is_some() || !select.order_by.is_empty() || select.limit.is_some() {
        return unsupported("CREATE VIEW query clause");
    }
    if !select.body.compounds.is_empty() {
        return unsupported("CREATE VIEW compound query");
    }
    let OneSelect::Select {
        distinctness,
        columns,
        from,
        where_clause,
        group_by,
        window_clause,
    } = &select.body.select
    else {
        return unsupported("CREATE VIEW query");
    };
    if distinctness.is_some()
        || where_clause.is_some()
        || group_by.is_some()
        || !window_clause.is_empty()
    {
        return unsupported("CREATE VIEW SELECT feature");
    }
    let Some(from) = from else {
        return unsupported("CREATE VIEW FROM clause");
    };
    if !from.joins.is_empty() {
        return unsupported("CREATE VIEW JOIN");
    }
    let SelectTable::Table(table_name, alias, indexed) = from.select.as_ref() else {
        return unsupported("CREATE VIEW table source");
    };
    if table_name.db_name.is_some()
        || table_name.alias.is_some()
        || alias.is_some()
        || indexed.is_some()
    {
        return unsupported("CREATE VIEW table option");
    }
    let columns = columns
        .iter()
        .map(|column| {
            let ResultColumn::Expr(expr, alias) = column else {
                return unsupported("CREATE VIEW projection");
            };
            if alias.as_ref().is_some_and(|alias| alias.is_explicit()) {
                return unsupported("CREATE VIEW projection alias");
            }
            let (TursoExpr::Name(name) | TursoExpr::Id(name)) = expr.as_ref() else {
                return unsupported("CREATE VIEW projection");
            };
            Ok(render_mysql_name(name))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if columns.is_empty() {
        return unsupported("CREATE VIEW without projections");
    }
    Ok(format!(
        "SELECT {} FROM {}",
        columns.join(", "),
        render_mysql_name(&table_name.name)
    ))
}

fn render_mysql_qualified_name(name: &QualifiedName) -> Result<String, ParseError> {
    if name.alias.is_some() {
        return unsupported("table name alias");
    }
    match &name.db_name {
        Some(database) => Ok(format!(
            "{}.{}",
            render_mysql_name(database),
            render_mysql_name(&name.name)
        )),
        None => Ok(render_mysql_name(&name.name)),
    }
}

fn render_mysql_column(
    column: &turso_parser::ast::ColumnDefinition,
    mode: SessionSqlMode,
) -> Result<String, ParseError> {
    let data_type = render_mysql_type(column.col_type.as_ref())?;
    reject_duplicate_nullable_column_constraints(&column.constraints)?;
    let constraints = column
        .constraints
        .iter()
        .map(|constraint| render_mysql_column_constraint(constraint, mode))
        .collect::<Result<Vec<_>, _>>()?;
    let mut definition = format!("{} {data_type}", render_mysql_name(&column.col_name));
    if !constraints.is_empty() {
        definition.push(' ');
        definition.push_str(&constraints.join(" "));
    }
    Ok(definition)
}

fn reject_duplicate_nullable_column_constraints(
    constraints: &[NamedColumnConstraint],
) -> Result<(), ParseError> {
    let mut nullable_constraints = 0;
    for constraint in constraints {
        if matches!(
            &constraint.constraint,
            TursoColumnConstraint::NotNull { .. }
        ) {
            nullable_constraints += 1;
        }
    }
    if nullable_constraints > 1 {
        return unsupported("multiple column NULL constraints");
    }
    Ok(())
}

fn render_mysql_type(data_type: Option<&TursoType>) -> Result<String, ParseError> {
    let Some(data_type) = data_type else {
        return unsupported("column without type");
    };
    if data_type.array_dimensions != 0 {
        return unsupported("column type modifier");
    }
    for sized in ["VARCHAR", "CHAR"] {
        if data_type.name.eq_ignore_ascii_case(sized) {
            return Ok(format!("{sized}({})", stored_character_length(data_type)?));
        }
    }
    if data_type.name.eq_ignore_ascii_case("DECIMAL") {
        let (precision, scale) = stored_decimal_size(data_type)?;
        return Ok(format!("DECIMAL({precision},{scale})"));
    }
    if data_type.size.is_some() {
        return unsupported("column type modifier");
    }
    let name = if data_type.name.eq_ignore_ascii_case("TINYINT") {
        "TINYINT"
    } else if data_type.name.eq_ignore_ascii_case("SMALLINT") {
        "SMALLINT"
    } else if data_type.name.eq_ignore_ascii_case("MEDIUMINT") {
        "MEDIUMINT"
    } else if data_type.name.eq_ignore_ascii_case("BIGINT") {
        "BIGINT"
    } else if data_type.name.eq_ignore_ascii_case("INT") {
        "INT"
    } else if data_type.name.eq_ignore_ascii_case("INTEGER") {
        "INTEGER"
    } else if data_type.name.eq_ignore_ascii_case("TEXT") {
        "TEXT"
    } else if data_type.name.eq_ignore_ascii_case("BLOB") {
        "BLOB"
    } else if data_type.name.eq_ignore_ascii_case("DOUBLE") {
        "DOUBLE"
    } else if data_type.name.eq_ignore_ascii_case("BOOLEAN") {
        "BOOLEAN"
    } else if data_type.name.eq_ignore_ascii_case("DATETIME") {
        "DATETIME"
    } else if data_type.name.eq_ignore_ascii_case("TIMESTAMP") {
        "TIMESTAMP"
    } else {
        return unsupported("column type");
    };
    Ok(name.to_owned())
}

/// Reads the character count from a sized text type already stored as SQLite
/// DDL.
///
/// The engine keeps the declared size as an expression, so this accepts only
/// the one shape this frontend writes: a single integer literal.
pub fn stored_character_length(data_type: &TursoType) -> Result<u32, ParseError> {
    let Some(TursoTypeSize::MaxSize(length)) = data_type.size.as_ref() else {
        return unsupported("VARCHAR without a length");
    };
    let TursoExpr::Literal(TursoLiteral::Numeric(text)) = length.as_ref() else {
        return unsupported("VARCHAR length");
    };
    let length = text.parse::<u64>().map_err(|_| ParseError::Unsupported {
        feature: "VARCHAR length",
    })?;
    if length == 0 || length > MAX_VARCHAR_CHARACTERS {
        return unsupported("VARCHAR length");
    }
    u32::try_from(length).map_err(|_| ParseError::Unsupported {
        feature: "VARCHAR length",
    })
}

fn render_mysql_column_constraint(
    constraint: &NamedColumnConstraint,
    mode: SessionSqlMode,
) -> Result<String, ParseError> {
    let name = render_mysql_constraint_name(constraint.name.as_ref());
    match &constraint.constraint {
        TursoColumnConstraint::NotNull {
            nullable: true,
            conflict_clause: None,
        } if constraint.name.is_none() => Ok("NULL".to_owned()),
        TursoColumnConstraint::NotNull {
            nullable: false,
            conflict_clause: None,
        } if constraint.name.is_none() => Ok("NOT NULL".to_owned()),
        TursoColumnConstraint::Unique(None) => Ok(format!("{name}UNIQUE")),
        TursoColumnConstraint::Default(expr) if constraint.name.is_none() => {
            Ok(format!("DEFAULT {}", render_mysql_default(expr, mode)?))
        }
        TursoColumnConstraint::Check { expr, .. } => {
            Ok(format!("{name}CHECK {}", render_mysql_check(expr, mode)?))
        }
        TursoColumnConstraint::PrimaryKey { .. } => unsupported("PRIMARY KEY"),
        TursoColumnConstraint::ForeignKey { .. } => unsupported("column REFERENCES constraint"),
        TursoColumnConstraint::Default(_) => unsupported("named DEFAULT constraint"),
        _ => unsupported("column attribute"),
    }
}

fn render_mysql_table_constraint(
    constraint: &NamedTableConstraint,
    mode: SessionSqlMode,
) -> Result<String, ParseError> {
    let name = render_mysql_constraint_name(constraint.name.as_ref());
    match &constraint.constraint {
        TursoTableConstraint::Unique {
            columns,
            conflict_clause: None,
        } => Ok(format!(
            "{name}UNIQUE ({})",
            render_mysql_sorted_columns(columns)?
        )),
        TursoTableConstraint::Check { expr, .. } => {
            Ok(format!("{name}CHECK {}", render_mysql_check(expr, mode)?))
        }
        TursoTableConstraint::ForeignKey {
            columns,
            clause,
            defer_clause: None,
        } => Ok(format!(
            "{name}FOREIGN KEY ({}) {}",
            render_mysql_indexed_columns(columns)?,
            render_mysql_foreign_key(clause)?
        )),
        TursoTableConstraint::PrimaryKey { .. } => unsupported("PRIMARY KEY"),
        TursoTableConstraint::Unique { .. } => unsupported("UNIQUE conflict clause"),
        TursoTableConstraint::ForeignKey { .. } => unsupported("FOREIGN KEY attribute"),
    }
}

fn render_mysql_sorted_columns(
    columns: &[turso_parser::ast::SortedColumn],
) -> Result<String, ParseError> {
    if columns.is_empty() {
        return unsupported("empty UNIQUE column list");
    }
    columns
        .iter()
        .map(|column| {
            if column.order.is_some() || column.nulls.is_some() {
                return unsupported("indexed column ordering");
            }
            let TursoExpr::Id(name) = column.expr.as_ref() else {
                return unsupported("indexed column expression");
            };
            Ok(render_mysql_name(name))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|columns| columns.join(", "))
}

fn render_mysql_indexed_columns(
    columns: &[turso_parser::ast::IndexedColumn],
) -> Result<String, ParseError> {
    if columns.is_empty() {
        return unsupported("empty FOREIGN KEY column list");
    }
    columns
        .iter()
        .map(|column| {
            if column.collation_name.is_some() || column.order.is_some() {
                return unsupported("FOREIGN KEY column attribute");
            }
            Ok(render_mysql_name(&column.col_name))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|columns| columns.join(", "))
}

fn render_mysql_foreign_key(
    clause: &turso_parser::ast::ForeignKeyClause,
) -> Result<String, ParseError> {
    let columns = if clause.columns.is_empty() {
        String::new()
    } else {
        format!(" ({})", render_mysql_indexed_columns(&clause.columns)?)
    };
    let mut arguments = Vec::with_capacity(clause.args.len());
    let mut has_delete = false;
    let mut has_update = false;
    for argument in &clause.args {
        match argument {
            RefArg::OnDelete(action) if !has_delete => {
                has_delete = true;
                arguments.push(format!("ON DELETE {}", render_mysql_ref_action(*action)));
            }
            RefArg::OnUpdate(action) if !has_update => {
                has_update = true;
                arguments.push(format!("ON UPDATE {}", render_mysql_ref_action(*action)));
            }
            RefArg::OnInsert(_) | RefArg::Match(_) => return unsupported("FOREIGN KEY attribute"),
            _ => return unsupported("duplicate FOREIGN KEY action"),
        }
    }
    let arguments = if arguments.is_empty() {
        String::new()
    } else {
        format!(" {}", arguments.join(" "))
    };
    Ok(format!(
        "REFERENCES {}{columns}{arguments}",
        render_mysql_name(&clause.tbl_name)
    ))
}

fn render_mysql_ref_action(action: RefAct) -> &'static str {
    match action {
        RefAct::SetNull => "SET NULL",
        RefAct::SetDefault => "SET DEFAULT",
        RefAct::Cascade => "CASCADE",
        RefAct::Restrict => "RESTRICT",
        RefAct::NoAction => "NO ACTION",
    }
}

fn render_mysql_default(expr: &TursoExpr, mode: SessionSqlMode) -> Result<String, ParseError> {
    match expr {
        TursoExpr::Literal(literal) => render_mysql_literal(literal, mode),
        TursoExpr::Unary(operator, expression) => {
            let sign = match operator {
                TursoUnaryOperator::Positive => "+",
                TursoUnaryOperator::Negative => "-",
                _ => return unsupported("DEFAULT integer literal"),
            };
            let TursoExpr::Literal(TursoLiteral::Numeric(value)) = expression.as_ref() else {
                return unsupported("DEFAULT integer literal");
            };
            render_signed_integer_default(sign, value)
        }
        _ => unsupported("non-literal DEFAULT expression"),
    }
}

fn render_mysql_check(expr: &TursoExpr, mode: SessionSqlMode) -> Result<String, ParseError> {
    let expression = render_mysql_expr(expr, mode)?;
    if expression.starts_with('(') && expression.ends_with(')') {
        Ok(expression)
    } else {
        Ok(format!("({expression})"))
    }
}

fn render_mysql_expr(expr: &TursoExpr, mode: SessionSqlMode) -> Result<String, ParseError> {
    match expr {
        TursoExpr::Id(name) => Ok(render_mysql_name(name)),
        TursoExpr::Qualified(table, column) => Ok(format!(
            "{}.{}",
            render_mysql_name(table),
            render_mysql_name(column)
        )),
        TursoExpr::DoublyQualified(database, table, column) => Ok(format!(
            "{}.{}.{}",
            render_mysql_name(database),
            render_mysql_name(table),
            render_mysql_name(column)
        )),
        TursoExpr::Literal(literal) => render_mysql_literal(literal, mode),
        TursoExpr::Parenthesized(expressions) if expressions.len() == 1 => {
            Ok(format!("({})", render_mysql_expr(&expressions[0], mode)?))
        }
        TursoExpr::Unary(operator, expression) => {
            let operator = match operator {
                TursoUnaryOperator::Positive => "+",
                TursoUnaryOperator::Negative => "-",
                _ => return unsupported("CHECK unary operator"),
            };
            Ok(format!(
                "({operator}{})",
                render_mysql_expr(expression, mode)?
            ))
        }
        TursoExpr::Binary(left, operator, right) => Ok(format!(
            "({} {} {})",
            render_mysql_expr(left, mode)?,
            render_mysql_operator(*operator)?,
            render_mysql_expr(right, mode)?
        )),
        TursoExpr::IsNull(expression) => Ok(format!(
            "({} IS NULL)",
            render_mysql_expr(expression, mode)?
        )),
        TursoExpr::NotNull(expression) => Ok(format!(
            "({} IS NOT NULL)",
            render_mysql_expr(expression, mode)?
        )),
        TursoExpr::Between {
            lhs,
            not: false,
            start,
            end,
        } => Ok(format!(
            "({} BETWEEN {} AND {})",
            render_mysql_expr(lhs, mode)?,
            render_mysql_expr(start, mode)?,
            render_mysql_expr(end, mode)?
        )),
        TursoExpr::InList { lhs, not, rhs, .. } if !rhs.is_empty() => Ok(format!(
            "({} {}IN ({}))",
            render_mysql_expr(lhs, mode)?,
            if *not { "NOT " } else { "" },
            rhs.iter()
                .map(|expression| render_mysql_expr(expression, mode))
                .collect::<Result<Vec<_>, _>>()?
                .join(", ")
        )),
        _ => unsupported("CHECK expression"),
    }
}

fn render_mysql_operator(operator: TursoOperator) -> Result<&'static str, ParseError> {
    match operator {
        TursoOperator::Add => Ok("+"),
        TursoOperator::Subtract => Ok("-"),
        TursoOperator::Multiply => Ok("*"),
        TursoOperator::Modulus => Ok("%"),
        TursoOperator::Greater => Ok(">"),
        TursoOperator::Less => Ok("<"),
        TursoOperator::GreaterEquals => Ok(">="),
        TursoOperator::LessEquals => Ok("<="),
        TursoOperator::Equals => Ok("="),
        TursoOperator::NotEquals => Ok("<>"),
        TursoOperator::And => Ok("AND"),
        TursoOperator::Or => Ok("OR"),
        _ => unsupported("CHECK binary operator"),
    }
}

fn render_mysql_literal(
    literal: &TursoLiteral,
    mode: SessionSqlMode,
) -> Result<String, ParseError> {
    match literal {
        TursoLiteral::Numeric(value) => Ok(value.clone()),
        TursoLiteral::String(value) => render_mysql_string_literal(value, mode),
        TursoLiteral::Null => Ok("NULL".to_string()),
        TursoLiteral::True => Ok("TRUE".to_string()),
        TursoLiteral::False => Ok("FALSE".to_string()),
        _ => unsupported("literal"),
    }
}

fn render_mysql_string_literal(value: &str, mode: SessionSqlMode) -> Result<String, ParseError> {
    let Some(content) = value
        .strip_prefix('\'')
        .and_then(|value| value.strip_suffix('\''))
    else {
        return unsupported("non-single-quoted string literal");
    };
    let mut normalized = String::with_capacity(value.len());
    let mut chars = content.chars();
    while let Some(ch) = chars.next() {
        if ch == '\'' && chars.next() != Some('\'') {
            return unsupported("malformed string literal");
        }
        normalized.push(ch);
    }
    let normalized = if mode.no_backslash_escapes {
        normalized
    } else {
        normalized.replace('\\', "\\\\")
    };
    Ok(format!("'{}'", normalized.replace('\'', "''")))
}

fn render_mysql_constraint_name(name: Option<&TursoName>) -> String {
    name.map(|name| format!("CONSTRAINT {} ", render_mysql_name(name)))
        .unwrap_or_default()
}

fn render_mysql_name(name: &TursoName) -> String {
    format!("`{}`", name.as_str().replace('`', "``"))
}

fn unsupported<T>(feature: &'static str) -> Result<T, ParseError> {
    Err(ParseError::Unsupported { feature })
}

#[cfg(test)]
mod tests {
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
                render_create_table_mysql_with_mode(&statement, SessionSqlMode::default())
                    .unwrap(),
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
                // MySQL takes these; this frontend does not read them yet.
                format!("SELECT id FROM users WHERE 1 {operator} id"),
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
            let sql =
                format!("SELECT u.id AS ranked FROM Users u ORDER BY ranked DESC, u.id {suffix}");
            let translated = parse_select(&sql, SessionSqlMode::default()).unwrap();
            assert_eq!(translated.source_table(), Some("users"));
            assert_eq!(
                translated.as_sql(),
                format!("SELECT \"u\".\"id\" AS \"ranked\" FROM \"Users\" AS \"u\" ORDER BY \"ranked\" DESC, \"u\".\"id\" ASC {normalized}")
            );
            assert!(translated.parse_ast().is_ok());
        }
    }

    #[test]
    fn select_rejects_unchecked_order_and_limit_options() {
        for suffix in [
            "ORDER BY 1",
            "ORDER BY id + 1",
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
        assert!(
            self_assignment.checked_update().unwrap().assignments()[0].assigns_column_to_itself()
        );

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
        ] {
            assert!(parse_dml(sql, SessionSqlMode::default()).is_ok(), "{sql}");
        }
        for sql in [
            "UPDATE t SET value = 1 WHERE 1 = value",
            "DELETE FROM t WHERE value BETWEEN 1 AND 2",
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
        let translated =
            parse_select(r"SELECT 'a\nb' AS value", SessionSqlMode::default()).unwrap();
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
        let literal =
            parse_select(r#"SELECT "value" AS result"#, SessionSqlMode::default()).unwrap();
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
            "SELECT 3 / 2",
            "SELECT 1 + 2",
            "SELECT 1 = 1",
            "SELECT DISTINCT id FROM users",
            "SELECT id FROM users JOIN accounts ON users.id = accounts.id",
            "SELECT id FROM app.users",
            "SELECT id FROM users WHERE id = 1.0",
            "SELECT 9223372036854775808",
            "SELECT -9223372036854775809",
            "SELECT id <=> NULL FROM users",
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
        let minimum =
            parse_select("SELECT -9223372036854775808", SessionSqlMode::default()).unwrap();
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

        let wildcard =
            parse_select("SELECT *, 0001 AS literal FROM users", SessionSqlMode::default())
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
        let sql = "CREATE TABLE users (label TEXT NOT NULL, id INTEGER NOT NULL AUTO_INCREMENT PRIMARY KEY)";
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
            "REPLACE INTO users (name) VALUES ('a')",
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
        assert!(
            checked
                .inject_reserved_range(i64::from(i32::MAX) as u64)
                .is_err()
        );
        assert!(checked.inject_reserved_range(u64::MAX).is_err());

        let one_row = parse_auto_increment_insert(
            "INSERT INTO users (name) VALUES ('Ada')",
            SessionSqlMode::default(),
        )
        .unwrap()
        .bind_allocator_table(&table)
        .unwrap();
        assert!(
            one_row
                .inject_reserved_range(i64::from(i32::MAX) as u64)
                .is_ok()
        );
        assert!(
            one_row
                .inject_reserved_range(i64::from(i32::MAX) as u64 + 1)
                .is_err()
        );
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
                parse_show_create_table(sql, mode)
                    .map(|command| command.table().as_str().to_owned()),
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
        let ndbinfo =
            parse_show_variables(r"SHOW VARIABLES LIKE 'ndbinfo\_version'", mode).unwrap();
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
                parse_show_create_table(sql, mode)
                    .map(|command| command.table().as_str().to_owned()),
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
                parse_show_create_table(sql, mode).is_err()
                    && parse_show_columns(sql, mode).is_err(),
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
}
