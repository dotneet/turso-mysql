//! Conservative MySQL parsing for the SQLite-compatible path.

use std::any::TypeId;
use std::{fmt, num::NonZeroUsize};

use sqlparser::{
    ast::{
        AlterTable, AlterTableOperation, BinaryOperator, ColumnDef, ColumnOption, CreateIndex,
        CreateTable, CreateTableOptions, CreateTrigger, CreateView, DataType, Delete, Expr,
        FromTable, FunctionArguments, HiveDistributionStyle, Ident, IndexColumn, Insert,
        ObjectName, ObjectNamePart, RenameTableNameKind, SelectFlavor, SelectItem, SetExpr,
        Statement, TableConstraint, TableFactor, TableObject, TriggerEvent as SqlTriggerEvent,
        TriggerObject, TriggerObjectKind, TriggerPeriod, UnaryOperator, Update, Value,
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
        TableConstraint as TursoTableConstraint, Type as TursoType,
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

    /// Returns checked UPDATE target information when this is an UPDATE.
    pub fn checked_update(&self) -> Option<&CheckedUpdate> {
        self.checked_update.as_ref()
    }
}

/// The signed integer range associated with one MySQL table column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MySqlSignedInteger {
    TinyInt,
    Int,
}

impl MySqlSignedInteger {
    /// Returns the inclusive i64 bounds used by the first strict assignment slice.
    pub const fn bounds(self) -> (i64, i64) {
        match self {
            Self::TinyInt => (-128, 127),
            Self::Int => (-2_147_483_648, 2_147_483_647),
        }
    }
}

/// Private MySQL numeric metadata rebuilt from durable normalized table DDL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlNumericSpec {
    columns: Vec<Option<MySqlSignedInteger>>,
}

impl MySqlNumericSpec {
    /// Returns the signed range for a stored column position, if this slice owns it.
    pub fn column(&self, index: usize) -> Option<MySqlSignedInteger> {
        self.columns.get(index).copied().flatten()
    }

    /// Returns the number of columns represented by the durable table DDL.
    pub fn len(&self) -> usize {
        self.columns.len()
    }

    /// Returns whether no columns need strict signed-width validation.
    pub fn is_empty(&self) -> bool {
        self.columns.iter().all(Option::is_none)
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
    Semicolon,
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
        tokens.push(AdminToken::Other);
        cursor += 1;
    }
    Ok(tokens)
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
        AdminToken::Semicolon | AdminToken::Comment | AdminToken::Other => {
            return Err(ParseError::ExpectedAdminCommand);
        }
    };
    *cursor += 1;
    MySqlDatabaseName::parse(name)
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
    let reads_table = match query.body.as_ref() {
        SetExpr::Select(select) => !select.from.is_empty(),
        _ => false,
    };
    Ok(TranslatedSelect {
        sqlite_sql: translate_select_query(&query)?,
        reads_table,
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
    let (sqlite_sql, checked_update) = match statement {
        Statement::Insert(insert) => (translate_insert(&insert)?, None),
        Statement::Update(update) => (translate_update(&update)?, Some(checked_update(&update)?)),
        Statement::Delete(delete) => (translate_delete(&delete)?, None),
        _ => return Err(ParseError::ExpectedDml),
    };
    Ok(TranslatedDml {
        sqlite_sql,
        checked_update,
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
            Value::Number(value, false) => value.parse::<i64>().is_ok(),
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
    let statement = parse_one_statement(sql, mode)?;
    let Statement::CreateTable(table) = statement else {
        return Err(ParseError::ExpectedCreateTable);
    };
    if let Err(error) = translate_create_table(&table) {
        parse_auto_increment_create_table(sql, mode).map_err(|_| error)?;
    }
    Ok(MySqlNumericSpec {
        columns: table
            .columns
            .iter()
            .map(|column| match column.data_type {
                DataType::TinyInt(None) => Some(MySqlSignedInteger::TinyInt),
                DataType::Int(None) | DataType::Integer(None) => Some(MySqlSignedInteger::Int),
                _ => None,
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
    let translated = parse_create_table(sql, mode)?;
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
    let statement = parse_one_statement(sql, mode)?;
    match statement {
        Statement::CreateTable(table) => {
            let translated = translate_create_table(&table)?;
            parse_normalized_create_table(translated.as_sql())
        }
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

fn translate_select_query(query: &sqlparser::ast::Query) -> Result<String, ParseError> {
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

    let projection = select
        .projection
        .iter()
        .map(render_select_item)
        .collect::<Result<Vec<_>, _>>()?;
    if projection.is_empty() {
        return unsupported("SELECT without projections");
    }

    let from = match select.from.as_slice() {
        [] => None,
        [from] if from.joins.is_empty() => Some(render_select_table(&from.relation)?),
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
        normalized.push_str(&render_select_predicate(selection)?);
    }
    Ok(normalized)
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
    if insert.columns.is_empty() {
        return unsupported("INSERT without an explicit column list");
    }
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

fn translate_update(update: &Update) -> Result<String, ParseError> {
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
        normalized.push_str(&render_dml_predicate(selection)?);
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

fn translate_delete(delete: &Delete) -> Result<String, ParseError> {
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
        normalized.push_str(&render_dml_predicate(selection)?);
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
            Value::Number(value, false) => value
                .parse::<i64>()
                .map(|value| value.to_string())
                .map_err(|_| ParseError::Unsupported {
                    feature: "DML numeric literal outside signed 64-bit integer range",
                }),
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
            let magnitude = value.parse::<u64>().map_err(|_| ParseError::Unsupported {
                feature: "DML numeric literal outside signed 64-bit integer range",
            })?;
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

fn render_dml_predicate(expr: &Expr) -> Result<String, ParseError> {
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
                render_dml_predicate(left)?,
                render_dml_predicate(right)?
            ))
        }
        Expr::IsNull(expr) => Ok(format!("({} IS NULL)", render_dml_expr(expr)?)),
        Expr::IsNotNull(expr) => Ok(format!("({} IS NOT NULL)", render_dml_expr(expr)?)),
        Expr::UnaryOp {
            op: UnaryOperator::Not,
            expr,
        } => Ok(format!("(NOT {})", render_dml_predicate(expr)?)),
        Expr::Nested(expr) => Ok(format!("({})", render_dml_predicate(expr)?)),
        Expr::Value(value) if matches!(&value.value, Value::Boolean(_)) => render_dml_expr(expr),
        _ => unsupported("DML WHERE predicate"),
    }
}

fn render_select_item(item: &SelectItem) -> Result<String, ParseError> {
    match item {
        SelectItem::UnnamedExpr(expr) => render_select_expr(expr),
        SelectItem::ExprWithAlias { expr, alias } => Ok(format!(
            "{} AS {}",
            render_select_expr(expr)?,
            render_ident(alias)
        )),
        SelectItem::Wildcard(options) if wildcard_options_are_empty(options) => Ok("*".to_string()),
        SelectItem::Wildcard(_) => unsupported("SELECT wildcard option"),
        _ => unsupported("SELECT projection"),
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

fn render_select_table(table: &TableFactor) -> Result<String, ParseError> {
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
    let mut rendered = render_unqualified_name(name)?;
    if let Some(alias) = alias {
        if !alias.columns.is_empty() || alias.at.is_some() {
            return unsupported("SELECT table alias option");
        }
        rendered.push_str(" AS ");
        rendered.push_str(&render_ident(&alias.name));
    }
    Ok(rendered)
}

fn render_select_expr(expr: &Expr) -> Result<String, ParseError> {
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
            Value::Placeholder(marker) if marker == "?" => Ok("?".to_string()),
            _ => unsupported("SELECT literal"),
        },
        Expr::IsNull(expr) => Ok(format!("({} IS NULL)", render_select_expr(expr)?)),
        Expr::IsNotNull(expr) => Ok(format!("({} IS NOT NULL)", render_select_expr(expr)?)),
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
            Ok(format!("(+{})", render_select_expr(expr)?))
        }
        Expr::Nested(expr) => Ok(format!("({})", render_select_expr(expr)?)),
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

fn render_select_predicate(expr: &Expr) -> Result<String, ParseError> {
    match expr {
        Expr::IsNull(expr) => Ok(format!("({} IS NULL)", render_select_expr(expr)?)),
        Expr::IsNotNull(expr) => Ok(format!("({} IS NOT NULL)", render_select_expr(expr)?)),
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
                render_select_predicate(left)?,
                render_select_predicate(right)?
            ))
        }
        Expr::UnaryOp {
            op: UnaryOperator::Not,
            expr,
        } => Ok(format!("(NOT {})", render_select_predicate(expr)?)),
        Expr::Nested(expr) => Ok(format!("({})", render_select_predicate(expr)?)),
        Expr::Value(value) if matches!(&value.value, Value::Boolean(_)) => render_select_expr(expr),
        _ => unsupported("SELECT WHERE predicate before coercion calibration"),
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
    let data_type = match column.data_type {
        DataType::TinyInt(None) => "TINYINT",
        DataType::Int(None) => "INT",
        DataType::Integer(None) => "INTEGER",
        DataType::Text => "TEXT",
        DataType::Blob(None) => "BLOB",
        _ => return unsupported("column type"),
    };
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

fn render_column_option(option: &sqlparser::ast::ColumnOptionDef) -> Result<String, ParseError> {
    let name = render_constraint_name(option.name.as_ref());
    match &option.option {
        ColumnOption::NotNull => Ok(format!("{name}NOT NULL")),
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
    let Expr::Value(value) = expr else {
        return unsupported("non-literal DEFAULT expression");
    };
    match &value.value {
        Value::Number(value, _) => Ok(value.clone()),
        Value::SingleQuotedString(value) => Ok(format!("'{}'", value.replace('\'', "''"))),
        Value::Boolean(value) => Ok(if *value { "TRUE" } else { "FALSE" }.to_string()),
        Value::Null => Ok("NULL".to_string()),
        _ => unsupported("DEFAULT literal"),
    }
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

fn render_mysql_type(data_type: Option<&TursoType>) -> Result<&'static str, ParseError> {
    let Some(data_type) = data_type else {
        return unsupported("column without type");
    };
    if data_type.size.is_some() || data_type.array_dimensions != 0 {
        return unsupported("column type modifier");
    }
    if data_type.name.eq_ignore_ascii_case("TINYINT") {
        Ok("TINYINT")
    } else if data_type.name.eq_ignore_ascii_case("INT") {
        Ok("INT")
    } else if data_type.name.eq_ignore_ascii_case("INTEGER") {
        Ok("INTEGER")
    } else if data_type.name.eq_ignore_ascii_case("TEXT") {
        Ok("TEXT")
    } else if data_type.name.eq_ignore_ascii_case("BLOB") {
        Ok("BLOB")
    } else {
        unsupported("column type")
    }
}

fn render_mysql_column_constraint(
    constraint: &NamedColumnConstraint,
    mode: SessionSqlMode,
) -> Result<String, ParseError> {
    let name = render_mysql_constraint_name(constraint.name.as_ref());
    match &constraint.constraint {
        TursoColumnConstraint::NotNull {
            nullable: false,
            conflict_clause: None,
        } => Ok(format!("{name}NOT NULL")),
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
    fn select_without_from_does_not_read_a_table() {
        let translated = parse_select("SELECT 1", SessionSqlMode::default()).unwrap();

        assert!(!translated.reads_table());
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
            "CREATE TABLE `numbers` (`tiny` TINYINT, `wide` INT, `legacy` INTEGER, `label` TEXT)";
        let statement = parse_create_table_ast(create, SessionSqlMode::default()).unwrap();
        assert_eq!(
            render_create_table_mysql(&statement).unwrap(),
            "CREATE TABLE `numbers` (`tiny` TINYINT, `wide` INT, `legacy` INTEGER, `label` TEXT)"
        );
        let spec = parse_mysql_numeric_spec(create, SessionSqlMode::default()).unwrap();
        assert_eq!(spec.column(0), Some(MySqlSignedInteger::TinyInt));
        assert_eq!(spec.column(1), Some(MySqlSignedInteger::Int));
        assert_eq!(spec.column(2), Some(MySqlSignedInteger::Int));
        assert_eq!(spec.column(3), None);

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
    fn rejects_dml_and_numeric_forms_outside_the_strict_signed_slice() {
        for sql in [
            "INSERT IGNORE INTO t (value) VALUES (1)",
            "INSERT INTO t VALUES (1)",
            "INSERT INTO t (value) SELECT 1",
            "UPDATE t SET value = 1 ORDER BY value",
            "UPDATE t SET value = 1 WHERE value = 1",
            "UPDATE t SET value = value + 1 WHERE TRUE",
            "UPDATE t SET value = CONCAT('1', '2')",
        ] {
            assert!(matches!(
                parse_dml(sql, SessionSqlMode::default()),
                Err(ParseError::Unsupported { .. })
            ));
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
            "CREATE TABLE t (value INT UNSIGNED)",
            "CREATE TABLE t (value DECIMAL(4, 1))",
            "CREATE TABLE t (value TINYINT(3))",
        ] {
            assert!(matches!(
                parse_create_table(sql, SessionSqlMode::default()),
                Err(ParseError::Unsupported { .. })
            ));
        }
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
    fn rejects_select_features_with_unproven_mysql_semantics() {
        for sql in [
            "SELECT 3 / 2",
            "SELECT 1 + 2",
            "SELECT 1 = 1",
            "SELECT id FROM users ORDER BY id",
            "SELECT id FROM users LIMIT 1",
            "SELECT DISTINCT id FROM users",
            "SELECT COUNT(*) FROM users",
            "SELECT id FROM users JOIN accounts ON users.id = accounts.id",
            "SELECT id FROM app.users",
            "SELECT id FROM users WHERE id = ?",
            "SELECT 9223372036854775808",
            "SELECT -9223372036854775809",
            "SELECT id <=> NULL FROM users",
            "SELECT id FROM users WHERE name LIKE 'a%'",
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
    fn rejects_mysql_attributes_instead_of_dropping_them() {
        for sql in [
            "CREATE TABLE t (id INTEGER AUTO_INCREMENT)",
            "CREATE TABLE t (id INTEGER UNSIGNED)",
            "CREATE TABLE t (id INTEGER DEFAULT CURRENT_TIMESTAMP)",
            "CREATE TABLE t (id INTEGER) ENGINE=InnoDB",
            "CREATE TABLE t (id INTEGER, UNIQUE KEY uq_id (id))",
            "CREATE TABLE t (id INTEGER, CHECK (RAND() > 0))",
            "CREATE TABLE t (id INTEGER REFERENCES parent (id))",
            "CREATE TABLE t (id INTEGER PRIMARY KEY)",
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
            "INSERT INTO users (name) VALUES (1.5)",
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
            "SHOW TABLES",
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
