use std::collections::HashMap;
use std::time::Duration;

use turso_mysql_parser::{
    parse_optional_select_database, parse_optional_session_settings,
    parse_optional_session_sql_notes, parse_optional_show_variables,
    parse_optional_system_variable_query, parse_optional_user_variable_assignment,
    parse_optional_user_variable_query, MySqlSelectDatabaseQuery, MySqlSessionSetting,
    MySqlShowVariablesCommand, MySqlSystemVariableQuery, MySqlSystemVariableRead,
    MySqlUserVariableQuery, MySqlUserVariableValue, MySqlVariableScope, SessionSqlMode,
};

use turso_mysql::MySqlIsolationLevel;

use crate::{
    dispatcher::{SERVER_STATUS_AUTOCOMMIT, SERVER_STATUS_IN_TRANS},
    frontend_adapter::{
        MySqlBootstrapSettings, MYSQL_BINARY_COLLATION, MYSQL_BINARY_FLAG, MYSQL_NOT_NULL_FLAG,
        MYSQL_NO_DEFAULT_VALUE_FLAG, MYSQL_NUM_FLAG, MYSQL_UNSIGNED_FLAG, NOT_FIXED_DECIMALS,
    },
    handshake::{SERVER_VERSION, SERVER_VERSION_COMMENT},
    statement_execute::{
        MYSQL_TYPE_LONGLONG, MYSQL_TYPE_LONG_BLOB, MYSQL_TYPE_MEDIUM_BLOB, MYSQL_TYPE_NEWDECIMAL,
        MYSQL_TYPE_VAR_STRING,
    },
    ColumnDefinitionConfig, CommandExecutionResult, CommandOkResult, FrontendErrorKind,
    TextResultSet, DEFAULT_UTF8MB4_COLLATION,
};

/// The character set this server speaks, and the only one it takes.
const SERVER_CHARACTER_SET: &str = "utf8mb4";

/// The collation the connection runs on, which is the one the handshake sends.
const SERVER_CONNECTION_COLLATION: &str = "utf8mb4_general_ci";

/// The collation a table this server writes is declared with, which is the one
/// `SHOW CREATE TABLE` and every `information_schema` reading already report.
const SERVER_DECLARED_COLLATION: &str = "utf8mb4_0900_ai_ci";

/// The zone this server runs in.
const SERVER_SYSTEM_TIME_ZONE: &str = "UTC";

/// The zone a session starts in, which is MySQL's own default and means the
/// system's — UTC.
const SERVER_TIME_ZONE_AT_THE_START: &str = "SYSTEM";

/// The licence this repository carries. MySQL's own answer is `GPL`.
const SERVER_LICENSE: &str = "MIT";

#[derive(Debug)]
pub(crate) struct MySqlSessionVariables {
    sql_notes: bool,
    /// Whether a row this session writes has to name a parent that is there.
    foreign_key_checks: bool,
    raw_character_set_results: bool,
    binary_character_set_results: bool,
    /// A foreign-key switch this session asked for and the caller has not
    /// applied yet.
    pending_foreign_key_checks: Option<bool>,
    /// What `SET @name = value` left behind, by lowercased name.
    ///
    /// These belong to the connection: another connection never sees them, and
    /// `COM_RESET_CONNECTION` takes them away, which it does here by replacing
    /// the whole of this.
    user_variables: HashMap<String, MySqlUserVariableValue>,
    /// A lock wait this session asked for and the caller has not applied yet.
    lock_wait_timeout: Option<Duration>,
    /// The zone the client last named, as MySQL reads it back.
    ///
    /// Fixed offsets affect TIMESTAMP values in the current session.
    time_zone: String,
    /// The level this session's transactions run at.
    transaction_isolation: MySqlIsolationLevel,
    /// A level the client set for the next transaction alone.
    next_transaction_isolation: Option<MySqlIsolationLevel>,
    /// The modes this session named beyond the ones the server always runs.
    sql_mode_choices: SqlModeChoices,
    /// The idle time the session asked for in place of the server's own.
    wait_timeout: Option<Duration>,
}

/// The `sql_mode` flags a session may turn on or off.
///
/// The rest of the modes a session names describe what this server does
/// anyway, and are checked against that rather than kept.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct SqlModeChoices {
    /// Also refuse bad values on a table without transactions, which every
    /// table here already has, so it changes nothing but what is read back.
    strict_all_tables: bool,
    /// Store a written 0 in a counted column rather than taking the next
    /// number.
    no_auto_value_on_zero: bool,
}

impl SqlModeChoices {
    fn named_in(modes: &[String]) -> Self {
        let named = |wanted: &str| modes.iter().any(|mode| mode.eq_ignore_ascii_case(wanted));
        Self {
            strict_all_tables: named("STRICT_ALL_TABLES"),
            no_auto_value_on_zero: named("NO_AUTO_VALUE_ON_ZERO"),
        }
    }
}

impl Default for MySqlSessionVariables {
    fn default() -> Self {
        Self {
            sql_notes: true,
            foreign_key_checks: true,
            raw_character_set_results: false,
            binary_character_set_results: false,
            pending_foreign_key_checks: None,
            user_variables: HashMap::new(),
            lock_wait_timeout: None,
            time_zone: SERVER_TIME_ZONE_AT_THE_START.to_owned(),
            transaction_isolation: MySqlIsolationLevel::default(),
            next_transaction_isolation: None,
            sql_mode_choices: SqlModeChoices::default(),
            wait_timeout: None,
        }
    }
}

impl MySqlSessionVariables {
    pub(crate) const fn sql_notes(&self) -> bool {
        self.sql_notes
    }

    pub(crate) const fn raw_character_set_results(&self) -> bool {
        self.raw_character_set_results
    }

    pub(crate) const fn binary_character_set_results(&self) -> bool {
        self.binary_character_set_results
    }

    /// The level a transaction beginning now would run at.
    pub(crate) fn isolation_for_next_transaction(&self) -> MySqlIsolationLevel {
        self.next_transaction_isolation
            .unwrap_or(self.transaction_isolation)
    }

    /// Forgets a level set for the next transaction alone, once one began.
    pub(crate) fn use_up_next_transaction_isolation(&mut self) {
        self.next_transaction_isolation = None;
    }

    pub(crate) fn time_zone_offset_seconds(&self) -> i32 {
        parse_time_zone_offset(&self.time_zone).unwrap_or(0)
    }

    /// Takes the lock wait this session last asked for, if it asked since this
    /// was last read.
    ///
    /// The setting has to reach the engine connection, which this does not
    /// hold, so it is left here for the caller that does.
    pub(crate) fn take_lock_wait_timeout(&mut self) -> Option<Duration> {
        self.lock_wait_timeout.take()
    }

    /// Takes the foreign-key switch this session last asked for, if it asked
    /// since this was last read.
    ///
    /// It reaches the engine connection the way the lock wait does, and for
    /// the same reason.
    pub(crate) fn take_foreign_key_checks(&mut self) -> Option<bool> {
        self.pending_foreign_key_checks.take()
    }

    pub(crate) fn execute_query(
        &mut self,
        sql: &str,
        settings: MySqlBootstrapSettings,
        selected_database: Option<&str>,
        session_sql_mode: SessionSqlMode,
        status_flags: u16,
    ) -> Result<Option<CommandExecutionResult>, FrontendErrorKind> {
        let parsed = match parse_optional_session_settings(sql, session_sql_mode) {
            Ok(parsed) => parsed,
            Err(turso_mysql_parser::ParseError::Unsupported { .. }) => {
                return Err(FrontendErrorKind::Unsupported);
            }
            Err(_) => return Err(FrontendErrorKind::Syntax),
        };
        if let Some(assignments) = parsed {
            // Measured on MySQL 8.4.11: when one assignment of a `SET` fails,
            // none of them takes effect, so every one is checked before any is
            // applied.
            let assignments = assignments
                .into_iter()
                .map(|setting| {
                    let setting = self.resolve_dump_session_setting(setting)?;
                    let setting = self.resolve_sql_mode_expression(setting, session_sql_mode);
                    accept_session_setting(&setting, session_sql_mode, status_flags)?;
                    Ok(setting)
                })
                .collect::<Result<Vec<_>, FrontendErrorKind>>()?;
            for setting in assignments {
                self.apply_session_setting(setting);
            }
            return Ok(Some(CommandExecutionResult::Ok(CommandOkResult {
                status_flags,
                ..CommandOkResult::default()
            })));
        }
        if let Some(assignments) = parse_optional_user_variable_assignment(sql, session_sql_mode)
            .map_err(|_| FrontendErrorKind::Syntax)?
        {
            for assignment in assignments {
                let value = if let Some(name) = assignment.system_variable() {
                    if ![
                        "character_set_client",
                        "character_set_results",
                        "collation_connection",
                        "sql_mode",
                    ]
                    .contains(&name)
                    {
                        return Err(FrontendErrorKind::UnknownSystemVariable);
                    }
                    if name == "character_set_results" && self.raw_character_set_results {
                        MySqlUserVariableValue::Null
                    } else if name == "character_set_results" && self.binary_character_set_results {
                        MySqlUserVariableValue::Text("binary".to_owned())
                    } else {
                        MySqlUserVariableValue::Text(
                            worded_system_variable(name, session_sql_mode, self)
                                .ok_or(FrontendErrorKind::UnknownSystemVariable)?,
                        )
                    }
                } else if let Some(name) = assignment.user_variable() {
                    self.user_variables
                        .get(name)
                        .cloned()
                        .unwrap_or(MySqlUserVariableValue::Null)
                } else {
                    assignment
                        .literal_value()
                        .expect("assignment source is one of literal, system or user variable")
                        .clone()
                };
                self.user_variables
                    .insert(assignment.name().to_owned(), value);
            }
            return Ok(Some(CommandExecutionResult::Ok(CommandOkResult {
                status_flags,
                ..CommandOkResult::default()
            })));
        }
        if let Some(query) = parse_optional_select_database(sql, SessionSqlMode::default())
            .map_err(|_| FrontendErrorKind::Syntax)?
        {
            return Ok(Some(select_database_result(
                &query,
                selected_database,
                status_flags,
            )));
        }
        if let Some(query) = parse_optional_system_variable_query(sql, session_sql_mode)
            .map_err(|_| FrontendErrorKind::Unsupported)?
        {
            // The statement reads variables and nothing else, so a name this
            // server has no answer for is one it does not have. Measured on
            // MySQL 8.4.11: that is 1193, not a refusal of the statement's
            // shape.
            return system_variable_result(&query, session_sql_mode, settings, status_flags, self)
                .map(Some)
                .ok_or(FrontendErrorKind::UnknownSystemVariable);
        }
        if let Some(query) = parse_optional_user_variable_query(sql, session_sql_mode)
            .map_err(|_| FrontendErrorKind::Syntax)?
        {
            return Ok(Some(self.user_variable_result(&query, status_flags)));
        }
        if let Some(command) = parse_optional_show_variables(sql, SessionSqlMode::default())
            .map_err(|_| FrontendErrorKind::Syntax)?
        {
            return Ok(Some(self.show_variables(
                &command,
                settings,
                session_sql_mode,
                status_flags,
            )));
        }
        let enabled = match parse_optional_session_sql_notes(sql, SessionSqlMode::default()) {
            Ok(Some(enabled)) => enabled,
            Err(turso_mysql_parser::ParseError::Unsupported { .. }) => {
                return Err(FrontendErrorKind::Unsupported);
            }
            Ok(None) | Err(_) => return Ok(None),
        };
        self.sql_notes = enabled;
        Ok(Some(CommandExecutionResult::Ok(CommandOkResult {
            status_flags,
            ..CommandOkResult::default()
        })))
    }

    /// Applies one assignment that [`accept_session_setting`] has taken.
    fn apply_session_setting(&mut self, setting: MySqlSessionSetting) {
        match setting {
            MySqlSessionSetting::SqlMode(named) => {
                self.sql_mode_choices = SqlModeChoices::named_in(&named);
            }
            MySqlSessionSetting::LockWaitTimeout(seconds) => {
                self.lock_wait_timeout = Some(Duration::from_secs(seconds));
            }
            MySqlSessionSetting::WaitTimeout(seconds) => {
                self.wait_timeout = seconds.map(Duration::from_secs);
            }
            MySqlSessionSetting::ForeignKeyChecks(enabled) => {
                self.foreign_key_checks = enabled;
                self.pending_foreign_key_checks = Some(enabled);
            }
            MySqlSessionSetting::CharacterSetResultsNull => {
                self.raw_character_set_results = true;
                self.binary_character_set_results = false;
            }
            MySqlSessionSetting::CharacterSetResults(value) => {
                self.raw_character_set_results = false;
                self.binary_character_set_results = value.eq_ignore_ascii_case("binary");
            }
            MySqlSessionSetting::Names { .. } => {
                self.raw_character_set_results = false;
                self.binary_character_set_results = false;
            }
            MySqlSessionSetting::TimeZone(zone) => {
                self.time_zone = the_zone_read_back(&zone);
            }
            MySqlSessionSetting::TransactionIsolationLevel {
                level,
                next_transaction_only,
            } => {
                let level = MySqlIsolationLevel::from_name(&level)
                    .expect("an accepted isolation level has a name this server keeps");
                if next_transaction_only {
                    self.next_transaction_isolation = Some(level);
                } else {
                    self.transaction_isolation = level;
                }
            }
            MySqlSessionSetting::SqlModeFromUserVariable(_)
            | MySqlSessionSetting::SqlModeExpression(_) => {
                unreachable!("a sql_mode is worked out before it is applied")
            }
            MySqlSessionSetting::InformationSchemaStatsExpiry(_)
            | MySqlSessionSetting::SqlAutoIsNull(_)
            | MySqlSessionSetting::SqlSafeUpdates(_)
            | MySqlSessionSetting::CharacterSetClient(_)
            | MySqlSessionSetting::CollationConnection(_)
            | MySqlSessionSetting::SqlQuoteShowCreate(_) => {}
        }
    }

    /// Works out a `sql_mode` written as an expression over the one in force.
    fn resolve_sql_mode_expression(
        &self,
        setting: MySqlSessionSetting,
        session_sql_mode: SessionSqlMode,
    ) -> MySqlSessionSetting {
        let MySqlSessionSetting::SqlModeExpression(value) = setting else {
            return setting;
        };
        MySqlSessionSetting::SqlMode(value.named_modes(
            &self.reported_sql_mode(session_sql_mode),
            &reported_sql_mode(SessionSqlMode::default()),
        ))
    }

    /// The `sql_mode` this session reads back.
    pub(crate) fn reported_sql_mode(&self, session_sql_mode: SessionSqlMode) -> String {
        reported_sql_mode_with(session_sql_mode, self.sql_mode_choices)
    }

    /// Whether a written 0 is stored as 0 rather than asking the counter.
    pub(crate) const fn no_auto_value_on_zero(&self) -> bool {
        self.sql_mode_choices.no_auto_value_on_zero
    }

    /// The idle time this session asked for, if it asked for one.
    pub(crate) const fn wait_timeout(&self) -> Option<Duration> {
        self.wait_timeout
    }

    fn resolve_dump_session_setting(
        &self,
        setting: MySqlSessionSetting,
    ) -> Result<MySqlSessionSetting, FrontendErrorKind> {
        match setting {
            MySqlSessionSetting::SqlModeFromUserVariable(name) => {
                let Some(MySqlUserVariableValue::Text(value)) = self.user_variables.get(&name)
                else {
                    return Err(FrontendErrorKind::Unsupported);
                };
                Ok(MySqlSessionSetting::SqlMode(
                    value
                        .split(',')
                        .map(str::trim)
                        .filter(|mode| !mode.is_empty())
                        .map(str::to_owned)
                        .collect(),
                ))
            }
            MySqlSessionSetting::CharacterSetClient(value) => Ok(
                MySqlSessionSetting::CharacterSetClient(self.resolve_dump_word(value)?),
            ),
            MySqlSessionSetting::CharacterSetResults(value) => Ok(
                MySqlSessionSetting::CharacterSetResults(self.resolve_dump_word(value)?),
            ),
            MySqlSessionSetting::CollationConnection(value) => Ok(
                MySqlSessionSetting::CollationConnection(self.resolve_dump_word(value)?),
            ),
            other => Ok(other),
        }
    }

    fn resolve_dump_word(&self, value: String) -> Result<String, FrontendErrorKind> {
        let Some(name) = value.strip_prefix('@') else {
            return Ok(value);
        };
        let Some(MySqlUserVariableValue::Text(value)) = self.user_variables.get(name) else {
            return Err(FrontendErrorKind::Unsupported);
        };
        Ok(value.clone())
    }

    /// Answers `SELECT @name` from what the connection holds.
    ///
    /// Measured on MySQL 8.4.11, and the reason each kind is kept apart rather
    /// than flattened into text: an integer answers a LONGLONG of length 21
    /// with decimals 0 and the binary and NUM flags; a decimal a NEWDECIMAL of
    /// length 67 and decimals 30 with the same flags; a string a MEDIUM_BLOB of
    /// length 268,435,440 with decimals 31, the connection's own utf8mb4
    /// collation and no flags at all; a NULL a MEDIUM_BLOB of 16,777,215 with
    /// the binary collation and flag; and a variable never set a VAR_STRING of
    /// length 65,532 with decimals 31 and the binary collation and flag,
    /// answering NULL rather than an error.
    fn user_variable_result(
        &self,
        query: &MySqlUserVariableQuery,
        status_flags: u16,
    ) -> CommandExecutionResult {
        let mut columns = Vec::with_capacity(query.reads().len());
        let mut row = Vec::with_capacity(query.reads().len());
        for read in query.reads() {
            let held = self.user_variables.get(read.name());
            let mut column = ColumnDefinitionConfig::new(
                read.column_name(),
                match held {
                    Some(MySqlUserVariableValue::Integer(_)) => MYSQL_TYPE_LONGLONG,
                    Some(MySqlUserVariableValue::Decimal(_)) => MYSQL_TYPE_NEWDECIMAL,
                    Some(MySqlUserVariableValue::Text(_)) => MYSQL_TYPE_LONG_BLOB,
                    Some(MySqlUserVariableValue::Null) => MYSQL_TYPE_MEDIUM_BLOB,
                    None => MYSQL_TYPE_VAR_STRING,
                },
            );
            match held {
                Some(MySqlUserVariableValue::Integer(_)) => {
                    column.character_set = MYSQL_BINARY_COLLATION;
                    column.column_length = 21;
                    column.decimals = 0;
                    column.flags = MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG;
                }
                Some(MySqlUserVariableValue::Decimal(_)) => {
                    column.character_set = MYSQL_BINARY_COLLATION;
                    column.column_length = 67;
                    column.decimals = 30;
                    column.flags = MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG;
                }
                Some(MySqlUserVariableValue::Text(_)) => {
                    column.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
                    column.column_length = 268_435_440;
                    column.decimals = NOT_FIXED_DECIMALS;
                }
                Some(MySqlUserVariableValue::Null) => {
                    column.character_set = MYSQL_BINARY_COLLATION;
                    column.column_length = 16_777_215;
                    column.decimals = NOT_FIXED_DECIMALS;
                    column.flags = MYSQL_BINARY_FLAG;
                }
                None => {
                    column.character_set = MYSQL_BINARY_COLLATION;
                    column.column_length = 65_532;
                    column.decimals = NOT_FIXED_DECIMALS;
                    column.flags = MYSQL_BINARY_FLAG;
                }
            }
            columns.push(column);
            row.push(match held {
                Some(MySqlUserVariableValue::Integer(value)) => {
                    Some(value.to_string().into_bytes())
                }
                Some(
                    MySqlUserVariableValue::Decimal(value) | MySqlUserVariableValue::Text(value),
                ) => Some(value.as_bytes().to_vec()),
                Some(MySqlUserVariableValue::Null) | None => None,
            });
        }
        CommandExecutionResult::ResultSet(TextResultSet {
            columns,
            rows: vec![row],
            warnings: 0,
            status_flags,
        })
    }

    /// Answers `SHOW VARIABLES` for the variables this server actually has.
    ///
    /// MySQL 8.4.11 returns 647 rows for an unfiltered `SHOW VARIABLES`. This
    /// server reports the ones it has and returns no row for any other name.
    /// That is what MySQL itself does for a variable its build leaves out:
    /// `SHOW VARIABLES LIKE 'ndbinfo\\_version'` returns the two columns and
    /// zero rows rather than an error.
    ///
    /// The names are the ones `SELECT @@name` answers, read through the same
    /// two readers so the two cannot drift apart, and MySQL writes them in
    /// name order.
    fn show_variables(
        &self,
        command: &MySqlShowVariablesCommand,
        settings: MySqlBootstrapSettings,
        session_sql_mode: SessionSqlMode,
        status_flags: u16,
    ) -> CommandExecutionResult {
        // Nothing can change a global value on this server, so the global scope
        // reports the values a new session would start from.
        let at_the_start = Self::default();
        let (session_sql_mode, status_flags_read, read_from) = match command.scope() {
            MySqlVariableScope::Session => (session_sql_mode, status_flags, self),
            MySqlVariableScope::Global => (
                SessionSqlMode::default(),
                SERVER_STATUS_AUTOCOMMIT,
                &at_the_start,
            ),
        };
        let rows = SHOWN_VARIABLES
            .iter()
            .filter(|name| command.selects(name))
            .filter_map(|name| {
                shown_variable_value(
                    name,
                    session_sql_mode,
                    settings,
                    status_flags_read,
                    read_from,
                )
                .map(|value| {
                    let value = if command.scope() == MySqlVariableScope::Session
                        && self.raw_character_set_results
                        && name.eq_ignore_ascii_case("character_set_results")
                    {
                        String::new()
                    } else if command.scope() == MySqlVariableScope::Session
                        && self.binary_character_set_results
                        && name.eq_ignore_ascii_case("character_set_results")
                    {
                        "binary".to_owned()
                    } else {
                        value
                    };
                    vec![Some(name.as_bytes().to_vec()), Some(value.into_bytes())]
                })
            })
            .collect();
        CommandExecutionResult::ResultSet(TextResultSet {
            columns: show_variables_columns(command.scope()),
            rows,
            warnings: 0,
            status_flags,
        })
    }
}

/// The variables `SHOW VARIABLES` reports, in the order MySQL writes them.
///
/// These are the names `SELECT @@name` answers. MySQL writes them in name
/// order, measured on 8.4.11.
const SHOWN_VARIABLES: [&str; 31] = [
    "auto_increment_increment",
    "auto_increment_offset",
    "autocommit",
    "character_set_client",
    "character_set_connection",
    "character_set_database",
    "character_set_results",
    "character_set_server",
    "collation_connection",
    "collation_database",
    "collation_server",
    "default_storage_engine",
    "foreign_key_checks",
    "init_connect",
    "interactive_timeout",
    "license",
    "lower_case_table_names",
    "max_allowed_packet",
    "net_write_timeout",
    "performance_schema",
    "sql_auto_is_null",
    "sql_mode",
    "sql_notes",
    "sql_safe_updates",
    "system_time_zone",
    "time_zone",
    "transaction_isolation",
    "transaction_read_only",
    "version",
    "version_comment",
    "wait_timeout",
];

/// Answers one variable the way `SHOW VARIABLES` writes it.
///
/// Measured on MySQL 8.4.11: `SHOW VARIABLES` writes a switch as `ON` or `OFF`
/// where `SELECT @@name` answers 1 or 0, and writes every other value as that
/// reading does. A switch is exactly a variable whose column is one digit
/// wide, which is how this tells the two apart.
fn shown_variable_value(
    name: &str,
    session_sql_mode: SessionSqlMode,
    settings: MySqlBootstrapSettings,
    status_flags: u16,
    session_variables: &MySqlSessionVariables,
) -> Option<String> {
    if let Some((value, width, _)) =
        counted_system_variable(name, settings, status_flags, session_variables)
    {
        if width == 1 {
            return Some(switch_value(value == "1").to_owned());
        }
        return Some(value);
    }
    worded_system_variable(name, session_sql_mode, session_variables)
}

/// Takes a session setting only when the server can keep the state it asks for.
///
/// Every real client opens with a handful of these, and refusing them all ends
/// the connection before any work starts. Each is checked against what the
/// server can actually do and then applied by the caller.
fn accept_session_setting(
    setting: &MySqlSessionSetting,
    session_sql_mode: SessionSqlMode,
    status_flags: u16,
) -> Result<(), FrontendErrorKind> {
    match setting {
        MySqlSessionSetting::SqlMode(named) => {
            for mode in named {
                if !session_names_the_mode_already(mode, session_sql_mode) {
                    return Err(FrontendErrorKind::Unsupported);
                }
            }
            Ok(())
        }
        MySqlSessionSetting::SqlModeFromUserVariable(_)
        | MySqlSessionSetting::SqlModeExpression(_) => Err(FrontendErrorKind::Unsupported),
        // Measured on MySQL 8.4.11: from one second to a year. The idle time
        // is the caller's to keep, and a value past either end is refused
        // rather than clamped with MySQL's warning.
        MySqlSessionSetting::WaitTimeout(None) => Ok(()),
        MySqlSessionSetting::WaitTimeout(Some(seconds)) => {
            if (1..=31_536_000).contains(seconds) {
                Ok(())
            } else {
                Err(FrontendErrorKind::Unsupported)
            }
        }
        // A comparison with NULL never finds the row just inserted here, which
        // is what 0 says; 1 asks for a rule this server does not have.
        MySqlSessionSetting::SqlAutoIsNull(enabled) => {
            if *enabled {
                Err(FrontendErrorKind::Unsupported)
            } else {
                Ok(())
            }
        }
        // An UPDATE or DELETE with no key in its WHERE runs here, which is what
        // 0 says; 1 asks for a refusal this server does not make.
        MySqlSessionSetting::SqlSafeUpdates(enabled) => {
            if *enabled {
                Err(FrontendErrorKind::Unsupported)
            } else {
                Ok(())
            }
        }
        MySqlSessionSetting::TimeZone(zone) => {
            if parse_time_zone_offset(zone).is_some() {
                Ok(())
            } else {
                Err(FrontendErrorKind::Unsupported)
            }
        }
        // This is how long MySQL caches `information_schema` statistics. There
        // are none here, so every value describes what this server does.
        MySqlSessionSetting::InformationSchemaStatsExpiry(_) => Ok(()),
        // Whether a row has to name a parent that is there is the caller's to
        // apply, and the engine's own switch says exactly what MySQL's does,
        // so both values are taken.
        MySqlSessionSetting::ForeignKeyChecks(_) => Ok(()),
        MySqlSessionSetting::CharacterSetResultsNull => Ok(()),
        MySqlSessionSetting::CharacterSetClient(value) => {
            if value.eq_ignore_ascii_case(SERVER_CHARACTER_SET) {
                Ok(())
            } else {
                Err(FrontendErrorKind::Unsupported)
            }
        }
        MySqlSessionSetting::CharacterSetResults(value) => {
            if value.eq_ignore_ascii_case(SERVER_CHARACTER_SET)
                || value.eq_ignore_ascii_case("binary")
            {
                Ok(())
            } else {
                Err(FrontendErrorKind::Unsupported)
            }
        }
        MySqlSessionSetting::SqlQuoteShowCreate(true) => Ok(()),
        MySqlSessionSetting::SqlQuoteShowCreate(false) => Err(FrontendErrorKind::Unsupported),
        MySqlSessionSetting::CollationConnection(value) => {
            if value.eq_ignore_ascii_case(SERVER_CONNECTION_COLLATION) {
                Ok(())
            } else {
                Err(FrontendErrorKind::Unsupported)
            }
        }
        // How long to wait for a lock is the caller's to apply. MySQL takes a
        // whole number of seconds from one to 1073741824 and answers 1231 for
        // anything else, which is what this refuses.
        MySqlSessionSetting::LockWaitTimeout(seconds) => {
            if (1..=1_073_741_824).contains(seconds) {
                Ok(())
            } else {
                Err(FrontendErrorKind::Unsupported)
            }
        }
        MySqlSessionSetting::Names {
            character_set,
            collation,
        } => {
            if !character_set.eq_ignore_ascii_case("utf8mb4") {
                return Err(FrontendErrorKind::Unsupported);
            }
            match collation {
                None => Ok(()),
                Some(collation) if collation.eq_ignore_ascii_case("utf8mb4_general_ci") => Ok(()),
                Some(_) => Err(FrontendErrorKind::Unsupported),
            }
        }
        // `READ COMMITTED` and `REPEATABLE READ` are the two levels this
        // server keeps. The other two are refused rather than accepted and
        // ignored, because a client that asked for `SERIALIZABLE` and was told
        // yes would be reasoning about a guarantee it does not have.
        MySqlSessionSetting::TransactionIsolationLevel {
            level,
            next_transaction_only,
        } => {
            if MySqlIsolationLevel::from_name(level).is_none() {
                return Err(FrontendErrorKind::Unsupported);
            }
            // Measured on MySQL 8.4.11: 1568 for the next-transaction form
            // inside a transaction, while the SESSION form is taken there and
            // holds from the next transaction on.
            if *next_transaction_only && status_flags & SERVER_STATUS_IN_TRANS != 0 {
                return Err(FrontendErrorKind::TransactionCharacteristicsInProgress);
            }
            Ok(())
        }
    }
}

/// Reports whether this server already behaves as one named `sql_mode` asks.
///
/// The two lexer modes have to match the session, because they change what a
/// double quote and a backslash mean. The rest of MySQL 8.4's default
/// `sql_mode` describes behavior this server already has, so a client that
/// reads the variable and writes it back is taken: writes are refused rather
/// than truncated, an impossible date is refused, `InnoDB` is the only engine
/// and is what `SHOW CREATE TABLE` reports, and division by zero never reaches
/// a write. Every other mode is refused rather than silently ignored.
fn session_names_the_mode_already(mode: &str, session_sql_mode: SessionSqlMode) -> bool {
    if mode.eq_ignore_ascii_case("ANSI_QUOTES") {
        return session_sql_mode.ansi_quotes;
    }
    if mode.eq_ignore_ascii_case("NO_BACKSLASH_ESCAPES") {
        return session_sql_mode.no_backslash_escapes;
    }
    [
        "ONLY_FULL_GROUP_BY",
        "STRICT_TRANS_TABLES",
        "STRICT_ALL_TABLES",
        "NO_ZERO_IN_DATE",
        "NO_ZERO_DATE",
        "ERROR_FOR_DIVISION_BY_ZERO",
        "NO_ENGINE_SUBSTITUTION",
    ]
    .iter()
    .any(|known| mode.eq_ignore_ascii_case(known))
        || mode.eq_ignore_ascii_case("NO_AUTO_VALUE_ON_ZERO")
}

/// Answers `SELECT @@name` and `SELECT VERSION()` from what this server is.
///
/// Only the variables this server has an honest answer for are read; every
/// other name is refused rather than answered with a value it does not have.
///
/// Nothing can change a global value on this server, so `@@global.name` answers
/// what a new session would start from rather than what this session is using.
/// Measured on MySQL 8.4.11: after `SET SESSION autocommit = 0` and
/// `SET SESSION foreign_key_checks = 0`, `@@global.autocommit` and
/// `@@global.foreign_key_checks` both still answer 1, and a session that adds
/// `ANSI_QUOTES` to its `sql_mode` reads `@@global.sql_mode` back without it.
///
/// Measured on MySQL 8.4.11: `@@version` is a `VAR_STRING` of length 87380 with
/// no flags and `decimals` 31, while `VERSION()` is a `VAR_STRING` of length 24
/// and is NOT NULL. The lengths are the ones MySQL reports under utf8mb4.
fn system_variable_result(
    query: &MySqlSystemVariableQuery,
    session_sql_mode: SessionSqlMode,
    settings: MySqlBootstrapSettings,
    status_flags: u16,
    session_variables: &MySqlSessionVariables,
) -> Option<CommandExecutionResult> {
    let mut columns = Vec::with_capacity(query.reads().len());
    let mut row = Vec::with_capacity(query.reads().len());
    for read in query.reads() {
        // One name this server cannot answer leaves the whole statement to the
        // caller, which refuses it rather than answering the rest.
        let (column, value) = system_variable_column(
            read,
            session_sql_mode,
            settings,
            status_flags,
            session_variables,
        )?;
        columns.push(column);
        if session_variables.raw_character_set_results
            && read.scope() == MySqlVariableScope::Session
            && read.name().eq_ignore_ascii_case("character_set_results")
        {
            row.push(None);
        } else if session_variables.binary_character_set_results
            && read.scope() == MySqlVariableScope::Session
            && read.name().eq_ignore_ascii_case("character_set_results")
        {
            row.push(Some(b"binary".to_vec()));
        } else {
            row.push(Some(value.into_bytes()));
        }
    }
    Some(CommandExecutionResult::ResultSet(TextResultSet {
        columns,
        rows: vec![row],
        warnings: 0,
        status_flags,
    }))
}

/// Answers one variable a `SELECT` reads, with the column MySQL reports it in.
///
/// Measured on MySQL 8.4.11: a number answers a LONGLONG with the binary and
/// numeric flags — a switch is one digit wide, a counter 21 and unsigned —
/// where a word answers the same VAR_STRING `@@version` does.
fn system_variable_column(
    read: &MySqlSystemVariableRead,
    session_sql_mode: SessionSqlMode,
    settings: MySqlBootstrapSettings,
    status_flags: u16,
    session_variables: &MySqlSessionVariables,
) -> Option<(ColumnDefinitionConfig, String)> {
    if let Some((from, to)) = read.zone_conversion() {
        return Some(zone_conversion_column(read, from, to));
    }
    let at_the_start = MySqlSessionVariables::default();
    let (session_sql_mode, status_flags, session_variables) = match read.scope() {
        MySqlVariableScope::Session => (session_sql_mode, status_flags, session_variables),
        MySqlVariableScope::Global => (
            SessionSqlMode::default(),
            SERVER_STATUS_AUTOCOMMIT,
            &at_the_start,
        ),
    };
    if let Some((value, length, unsigned)) =
        counted_system_variable(read.name(), settings, status_flags, session_variables)
    {
        let mut column =
            ColumnDefinitionConfig::new(read.column_name().to_owned(), MYSQL_TYPE_LONGLONG);
        column.catalog = "def".into();
        column.character_set = MYSQL_BINARY_COLLATION;
        column.column_length = length;
        column.decimals = 0;
        column.flags =
            MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG | if unsigned { MYSQL_UNSIGNED_FLAG } else { 0 };
        return Some((column, value));
    }
    let value = worded_system_variable(read.name(), session_sql_mode, session_variables)?;
    // A call is NOT NULL and a variable is not, and their reported widths
    // differ; both measured.
    let called = read.called();
    let mut column =
        ColumnDefinitionConfig::new(read.column_name().to_owned(), MYSQL_TYPE_VAR_STRING);
    column.catalog = "def".into();
    column.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
    column.column_length = if called { 24 } else { 87_380 };
    column.decimals = NOT_FIXED_DECIMALS;
    column.flags = if called { MYSQL_NOT_NULL_FLAG } else { 0 };
    Some((column, value))
}

/// Answers whether `CONVERT_TZ` can convert between two zones, which is
/// whether this server knows both.
///
/// It knows `UTC`, `SYSTEM` and fixed offsets and no named zone, which is a
/// MySQL whose zone tables are empty: there `CONVERT_TZ` answers NULL for a
/// named zone. Measured on MySQL 8.4.11, the column is a LONGLONG of length 1
/// that is NOT NULL, with the binary and numeric flags.
fn zone_conversion_column(
    read: &MySqlSystemVariableRead,
    from: &str,
    to: &str,
) -> (ColumnDefinitionConfig, String) {
    let mut column =
        ColumnDefinitionConfig::new(read.column_name().to_owned(), MYSQL_TYPE_LONGLONG);
    column.catalog = "def".into();
    column.character_set = MYSQL_BINARY_COLLATION;
    column.column_length = 1;
    column.decimals = 0;
    column.flags = MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG;
    let known = parse_time_zone_offset(from).is_some() && parse_time_zone_offset(to).is_some();
    (column, u8::from(known).to_string())
}

/// The system variables this server answers with a number, with the width
/// MySQL reports for each and whether it is unsigned.
///
/// Measured on MySQL 8.4.11: `@@autocommit` is a LONGLONG of length 1 carrying
/// the binary and numeric flags, and `@@max_allowed_packet` and
/// `@@wait_timeout` are LONGLONGs of length 21 carrying those and the unsigned
/// flag as well.
fn counted_system_variable(
    name: &str,
    settings: MySqlBootstrapSettings,
    status_flags: u16,
    session_variables: &MySqlSessionVariables,
) -> Option<(String, u32, bool)> {
    if name.eq_ignore_ascii_case("autocommit") {
        let on = status_flags & SERVER_STATUS_AUTOCOMMIT != 0;
        return Some((u8::from(on).to_string(), 1, false));
    }
    if name.eq_ignore_ascii_case("foreign_key_checks") {
        return Some((
            u8::from(session_variables.foreign_key_checks).to_string(),
            1,
            false,
        ));
    }
    if name.eq_ignore_ascii_case("sql_notes") {
        return Some((u8::from(session_variables.sql_notes).to_string(), 1, false));
    }
    // A READ ONLY transaction leaves MySQL's session default unchanged. This
    // server does not accept a change to that default, so it remains off.
    if name.eq_ignore_ascii_case("transaction_read_only") {
        return Some(("0".to_owned(), 1, false));
    }
    // This server has no performance schema, which is a thing a client can see
    // for itself and act on rather than a claim about how it behaves.
    if name.eq_ignore_ascii_case("performance_schema") {
        return Some(("0".to_owned(), 1, false));
    }
    if name.eq_ignore_ascii_case("max_allowed_packet") {
        return Some((settings.max_allowed_packet().to_string(), 21, true));
    }
    if name.eq_ignore_ascii_case("net_write_timeout") {
        return Some((settings.net_write_timeout_seconds().to_string(), 21, true));
    }
    if name.eq_ignore_ascii_case("wait_timeout") {
        let seconds = session_variables
            .wait_timeout
            .map_or(settings.wait_timeout_seconds(), |wait| wait.as_secs());
        return Some((seconds.to_string(), 21, true));
    }
    // Neither rule is one this server has, and a session is refused both.
    if name.eq_ignore_ascii_case("sql_auto_is_null")
        || name.eq_ignore_ascii_case("sql_safe_updates")
    {
        return Some(("0".to_owned(), 1, false));
    }
    // MySQL keeps an idle connection a client called interactive for
    // `interactive_timeout` instead. This server keeps every connection for the
    // same time, so that is the answer for both.
    if name.eq_ignore_ascii_case("interactive_timeout") {
        return Some((settings.wait_timeout_seconds().to_string(), 21, true));
    }
    // The counter numbers a row one past the last, from one, whatever the
    // session. Measured on MySQL 8.4.11: both read 1 there too.
    if name.eq_ignore_ascii_case("auto_increment_increment")
        || name.eq_ignore_ascii_case("auto_increment_offset")
    {
        return Some(("1".to_owned(), 21, true));
    }
    // A table this server writes is found again whatever case its name is
    // asked for, and `SHOW TABLES` reads it back lowercased — measured against
    // this server, and what MySQL's 1 means. MySQL on Linux reads 0 here.
    if name.eq_ignore_ascii_case("lower_case_table_names") {
        return Some(("1".to_owned(), 21, true));
    }
    None
}

/// The system variables this server answers with a word.
///
/// Each is something this server decides rather than a default copied from
/// MySQL: it speaks utf8mb4 and nothing else, it runs in UTC, it runs a
/// session at the level the session asked for, it runs nothing when a
/// connection opens, and it is under the licence this repository carries.
///
/// Measured on MySQL 8.4.11: every one of these answers the same `VAR_STRING`
/// of length 87380 with 31 decimals and no flags that `@@version` does.
fn worded_system_variable(
    name: &str,
    session_sql_mode: SessionSqlMode,
    session_variables: &MySqlSessionVariables,
) -> Option<String> {
    if name.eq_ignore_ascii_case("version") {
        return Some(SERVER_VERSION.to_owned());
    }
    if name.eq_ignore_ascii_case("version_comment") {
        return Some(SERVER_VERSION_COMMENT.to_owned());
    }
    if name.eq_ignore_ascii_case("sql_mode") {
        return Some(session_variables.reported_sql_mode(session_sql_mode));
    }
    // InnoDB is the engine `SHOW CREATE TABLE` and `SHOW ENGINES` name.
    if name.eq_ignore_ascii_case("default_storage_engine") {
        return Some("InnoDB".to_owned());
    }
    // A client that asks for any other character set is refused, so every one
    // of these is utf8mb4 and stays that way.
    if [
        "character_set_client",
        "character_set_connection",
        "character_set_results",
        "character_set_server",
        "character_set_database",
    ]
    .iter()
    .any(|known| name.eq_ignore_ascii_case(known))
    {
        return Some(SERVER_CHARACTER_SET.to_owned());
    }
    // The handshake sends collation 45, which is what the connection runs on,
    // while a table this server writes is declared with the collation MySQL
    // declares one with. Both are what this server already tells a client
    // elsewhere: 45 in the handshake, and `utf8mb4_0900_ai_ci` in every
    // `SHOW CREATE TABLE` and `information_schema` reading.
    if name.eq_ignore_ascii_case("collation_connection") {
        return Some(SERVER_CONNECTION_COLLATION.to_owned());
    }
    if name.eq_ignore_ascii_case("collation_server")
        || name.eq_ignore_ascii_case("collation_database")
    {
        return Some(SERVER_DECLARED_COLLATION.to_owned());
    }
    // Nothing here converts a moment between zones, which is the same as
    // running in UTC, and every zone a client may name means UTC.
    if name.eq_ignore_ascii_case("system_time_zone") {
        return Some(SERVER_SYSTEM_TIME_ZONE.to_owned());
    }
    if name.eq_ignore_ascii_case("time_zone") {
        return Some(session_variables.time_zone.clone());
    }
    // Measured on MySQL 8.4.11: this reads the session's level, not one set
    // for the next transaction alone, even inside the transaction using it.
    if name.eq_ignore_ascii_case("transaction_isolation") {
        return Some(
            session_variables
                .transaction_isolation
                .variable_value()
                .to_owned(),
        );
    }
    // Nothing runs when a connection opens.
    if name.eq_ignore_ascii_case("init_connect") {
        return Some(String::new());
    }
    // MySQL answers `GPL`. This is not MySQL, and saying so is the honest
    // answer rather than the compatible-looking one.
    if name.eq_ignore_ascii_case("license") {
        return Some(SERVER_LICENSE.to_owned());
    }
    None
}

/// Returns a zone the way MySQL reads it back after taking it.
///
/// Measured on MySQL 8.4.11: `SYSTEM` is a keyword and reads back upper-cased
/// whatever case it was written in, and an offset reads back as `+HH:MM`, so
/// `'-00:00'` reads back `+00:00`.
///
/// A named zone reads back as the statement wrote it. MySQL keeps the first
/// spelling a whole *server* saw for a name and answers that to every session
/// afterwards — measured: on a server started fresh, `SET time_zone = 'UTC'`
/// makes a later `'utc'` read back `UTC`, while a server that saw `'utc'`
/// first answers `utc` to both. That is history, not a rule about the zone,
/// and this server keeps none: it answers what the session wrote, which is
/// what a MySQL that has not seen the name before answers.
fn the_zone_read_back(zone: &str) -> String {
    if zone.eq_ignore_ascii_case("SYSTEM") {
        return "SYSTEM".to_owned();
    }
    if zone.eq_ignore_ascii_case("UTC") {
        return zone.to_owned();
    }
    if parse_time_zone_offset(zone) == Some(0) {
        return "+00:00".to_owned();
    }
    zone.to_owned()
}

fn parse_time_zone_offset(zone: &str) -> Option<i32> {
    if zone.eq_ignore_ascii_case("SYSTEM") || zone.eq_ignore_ascii_case("UTC") {
        return Some(0);
    }
    let bytes = zone.as_bytes();
    if bytes.len() != 6
        || !matches!(bytes[0], b'+' | b'-')
        || bytes[3] != b':'
        || ![1, 2, 4, 5]
            .into_iter()
            .all(|index| bytes[index].is_ascii_digit())
    {
        return None;
    }
    let hours = i32::from(bytes[1] - b'0') * 10 + i32::from(bytes[2] - b'0');
    let minutes = i32::from(bytes[4] - b'0') * 10 + i32::from(bytes[5] - b'0');
    if minutes > 59
        || hours * 60 + minutes > 14 * 60
        || (bytes[0] == b'+' && hours == 14 && minutes > 0)
    {
        return None;
    }
    if bytes[0] == b'-' && hours == 14 {
        return None;
    }
    let seconds = (hours * 60 + minutes) * 60;
    Some(if bytes[0] == b'-' { -seconds } else { seconds })
}

/// The `sql_mode` this server runs in.
///
/// It is not a setting: the modes MySQL's own default names are the ones this
/// enforces, and a client asking for any other is refused rather than told it
/// took effect. What varies is the two a session may be opened with, so those
/// are reported when they are on.
///
/// MySQL writes the modes in an order of its own rather than the order they
/// were set in — measured on 8.4.11, setting all eight reads back as
/// `ANSI_QUOTES,ONLY_FULL_GROUP_BY,NO_BACKSLASH_ESCAPES,STRICT_TRANS_TABLES,...`
/// — so they are written in that order here.
pub(crate) fn reported_sql_mode(session_sql_mode: SessionSqlMode) -> String {
    reported_sql_mode_with(session_sql_mode, SqlModeChoices::default())
}

/// The `sql_mode` a session reads back, with the flags it turned on.
///
/// Measured on MySQL 8.4.11: `NO_AUTO_VALUE_ON_ZERO` reads back after
/// `ONLY_FULL_GROUP_BY` and `STRICT_ALL_TABLES` after `STRICT_TRANS_TABLES`.
fn reported_sql_mode_with(session_sql_mode: SessionSqlMode, choices: SqlModeChoices) -> String {
    let mut modes = Vec::with_capacity(10);
    if session_sql_mode.ansi_quotes {
        modes.push("ANSI_QUOTES");
    }
    modes.push("ONLY_FULL_GROUP_BY");
    if choices.no_auto_value_on_zero {
        modes.push("NO_AUTO_VALUE_ON_ZERO");
    }
    if session_sql_mode.no_backslash_escapes {
        modes.push("NO_BACKSLASH_ESCAPES");
    }
    modes.push("STRICT_TRANS_TABLES");
    if choices.strict_all_tables {
        modes.push("STRICT_ALL_TABLES");
    }
    modes.extend([
        "NO_ZERO_IN_DATE",
        "NO_ZERO_DATE",
        "ERROR_FOR_DIVISION_BY_ZERO",
        "NO_ENGINE_SUBSTITUTION",
    ]);
    modes.join(",")
}

/// Answers `SELECT DATABASE()` from the session alone.
///
/// MySQL answers this with no database selected, returning NULL, which is how a
/// client's `USE` gets started: `com_use` asks `SELECT DATABASE()` first, and a
/// server that demands a selected database here can never be given one.
///
/// Measured on MySQL 8.4.11: one `MYSQL_TYPE_VAR_STRING` column, no origin
/// table, `column_length` 256, no flags, and `decimals` 31.
fn select_database_result(
    query: &MySqlSelectDatabaseQuery,
    selected_database: Option<&str>,
    status_flags: u16,
) -> CommandExecutionResult {
    let mut column = ColumnDefinitionConfig::new(query.column_name(), MYSQL_TYPE_VAR_STRING);
    column.catalog = "def".into();
    column.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
    column.column_length = 256;
    column.decimals = NOT_FIXED_DECIMALS;
    CommandExecutionResult::ResultSet(TextResultSet {
        columns: vec![column],
        rows: vec![vec![selected_database.map(|name| name.as_bytes().to_vec())]],
        warnings: 0,
        status_flags,
    })
}

/// Renders a boolean the way `SHOW VARIABLES` renders one.
///
/// `SELECT @@sql_notes` answers `1`, but `SHOW VARIABLES LIKE 'sql_notes'`
/// answers `ON`. Both measured on MySQL 8.4.11.
const fn switch_value(enabled: bool) -> &'static str {
    if enabled {
        "ON"
    } else {
        "OFF"
    }
}

/// Builds the two columns MySQL 8.4.11 returns for `SHOW VARIABLES`.
///
/// Measured with the session's default `character_set_results`. The lengths are
/// the utf8mb4 character counts MySQL reports there; after
/// `SET SESSION character_set_results = 'binary'` MySQL reports the byte counts
/// instead, which this server does not yet model.
///
/// One field deliberately differs. MySQL sends collation 255,
/// `utf8mb4_0900_ai_ci`; this sends 45, `utf8mb4_general_ci`, because that is
/// the collation the whole frontend runs on and every other catalog column
/// already reports.
fn show_variables_columns(scope: MySqlVariableScope) -> Vec<ColumnDefinitionConfig> {
    let table = match scope {
        MySqlVariableScope::Session => "session_variables",
        MySqlVariableScope::Global => "global_variables",
    };
    [
        (
            "Variable_name",
            256,
            MYSQL_NOT_NULL_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG,
        ),
        ("Value", 4096, 0),
    ]
    .into_iter()
    .map(|(name, column_length, flags)| {
        let mut column = ColumnDefinitionConfig::new(name, MYSQL_TYPE_VAR_STRING);
        column.catalog = "def".into();
        column.schema = "performance_schema".into();
        column.table = table.into();
        column.original_table = table.into();
        column.original_name = name.into();
        column.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
        column.column_length = column_length;
        column.flags = flags;
        column
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A user variable goes into the connection and comes back out of it, and
    /// the column it comes back in says which kind it holds. Every number here
    /// was measured on MySQL 8.4.11 — the string case in particular reports
    /// latin1_swedish_ci and no flags at all, where every other column this
    /// server builds reports utf8mb4.
    #[test]
    fn a_user_variable_goes_in_and_comes_back_in_the_column_mysql_answers() {
        let mut session = MySqlSessionVariables::default();
        let mut run = |sql: &str| {
            session
                .execute_query(
                    sql,
                    MySqlBootstrapSettings::default(),
                    None,
                    SessionSqlMode::default(),
                    2,
                )
                .unwrap()
                .unwrap()
        };
        assert!(matches!(
            run("SET @x = 1, @s = 'abc', @n = NULL, @f = 1.5"),
            CommandExecutionResult::Ok(_)
        ));

        let CommandExecutionResult::ResultSet(result) = run("SELECT @X, @s, @n, @f, @missing")
        else {
            panic!("SELECT of a user variable must return a result set");
        };
        assert_eq!(
            result.rows,
            vec![vec![
                Some(b"1".to_vec()),
                Some(b"abc".to_vec()),
                None,
                Some(b"1.5".to_vec()),
                None,
            ]]
        );
        assert_eq!(
            result
                .columns
                .iter()
                .map(|column| (
                    column.name.as_str(),
                    column.column_type,
                    column.character_set,
                    column.column_length,
                    column.decimals,
                    column.flags,
                ))
                .collect::<Vec<_>>(),
            [
                ("@X", MYSQL_TYPE_LONGLONG, 63, 21, 0, 128 | 32_768),
                (
                    "@s",
                    MYSQL_TYPE_LONG_BLOB,
                    u16::from(DEFAULT_UTF8MB4_COLLATION),
                    268_435_440,
                    31,
                    0
                ),
                ("@n", MYSQL_TYPE_MEDIUM_BLOB, 63, 16_777_215, 31, 128),
                ("@f", MYSQL_TYPE_NEWDECIMAL, 63, 67, 30, 128 | 32_768),
                ("@missing", MYSQL_TYPE_VAR_STRING, 63, 65_532, 31, 128),
            ]
        );
    }

    /// Measured on MySQL 8.4.11: `COM_RESET_CONNECTION` takes the user
    /// variables away, so `SELECT @x` after one answers NULL rather than what
    /// was set. The adapter resets by replacing this whole value, so a fresh
    /// one must hold nothing.
    #[test]
    fn a_fresh_session_holds_no_user_variable() {
        let mut session = MySqlSessionVariables::default();
        session
            .execute_query(
                "SET @x = 1",
                MySqlBootstrapSettings::default(),
                None,
                SessionSqlMode::default(),
                2,
            )
            .unwrap();
        let mut fresh = MySqlSessionVariables::default();
        let Ok(Some(CommandExecutionResult::ResultSet(result))) = fresh.execute_query(
            "SELECT @x",
            MySqlBootstrapSettings::default(),
            None,
            SessionSqlMode::default(),
            2,
        ) else {
            panic!("SELECT of a user variable must return a result set");
        };
        assert_eq!(result.rows, vec![vec![None]]);
        assert_eq!(result.columns[0].column_type, MYSQL_TYPE_VAR_STRING);
    }

    /// Django reads this row whenever it connects, and its first statement
    /// after is the isolation level it wants.
    #[test]
    fn answers_the_row_django_reads_when_it_connects() {
        let mut session = MySqlSessionVariables::default();
        let Ok(Some(CommandExecutionResult::ResultSet(result))) = session.execute_query(
            "\n                SELECT VERSION(),\n                       @@sql_mode,\n                       @@default_storage_engine,\n                       @@sql_auto_is_null,\n                       @@lower_case_table_names,\n                       CONVERT_TZ('2001-01-01 01:00:00', 'UTC', 'UTC') IS NOT NULL\n            ",
            MySqlBootstrapSettings::default(),
            None,
            SessionSqlMode::default(),
            SERVER_STATUS_AUTOCOMMIT,
        ) else {
            panic!("Django's row must be answered");
        };
        assert_eq!(
            result.rows,
            vec![vec![
                Some(SERVER_VERSION.as_bytes().to_vec()),
                Some(reported_sql_mode(SessionSqlMode::default()).into_bytes()),
                Some(b"InnoDB".to_vec()),
                Some(b"0".to_vec()),
                Some(b"1".to_vec()),
                Some(b"1".to_vec()),
            ]]
        );
        let probe = &result.columns[5];
        assert_eq!(
            probe.name,
            "CONVERT_TZ('2001-01-01 01:00:00', 'UTC', 'UTC') IS NOT NULL"
        );
        assert_eq!(probe.column_type, MYSQL_TYPE_LONGLONG);
        assert_eq!(probe.column_length, 1);
        assert_eq!(
            probe.flags,
            MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG
        );

        // A named zone is one this server does not know, which a MySQL with
        // empty zone tables answers NULL for.
        let Ok(Some(CommandExecutionResult::ResultSet(named))) = session.execute_query(
            "SELECT CONVERT_TZ('2001-01-01 01:00:00', 'UTC', 'Europe/Paris') IS NOT NULL",
            MySqlBootstrapSettings::default(),
            None,
            SessionSqlMode::default(),
            SERVER_STATUS_AUTOCOMMIT,
        ) else {
            panic!("the probe must be answered");
        };
        assert_eq!(named.rows, vec![vec![Some(b"0".to_vec())]]);
    }

    /// Rails opens every connection with this one statement. Measured on
    /// MySQL 8.4.11: it is taken, and the three settings read back as below.
    #[test]
    fn takes_the_statement_rails_opens_with() {
        let mut session = MySqlSessionVariables::default();
        let mut run = |sql: &str| {
            session.execute_query(
                sql,
                MySqlBootstrapSettings::default(),
                None,
                SessionSqlMode::default(),
                SERVER_STATUS_AUTOCOMMIT,
            )
        };
        let read = |run: &mut dyn FnMut(&str) -> _, sql: &str| -> Vec<Option<String>> {
            let Ok(Some(CommandExecutionResult::ResultSet(result))) = run(sql) else {
                panic!("{sql} must return a result set");
            };
            result.rows[0]
                .iter()
                .map(|value| {
                    value
                        .as_ref()
                        .map(|value| String::from_utf8(value.clone()).unwrap())
                })
                .collect()
        };
        assert!(matches!(
            run("SET NAMES utf8mb4,  @@SESSION.sql_mode = CONCAT(CONCAT(@@sql_mode, ',STRICT_ALL_TABLES'), ',NO_AUTO_VALUE_ON_ZERO'),  @@SESSION.wait_timeout = 2147483"),
            Ok(Some(CommandExecutionResult::Ok(_)))
        ));
        assert_eq!(
            read(
                &mut run,
                "SELECT @@sql_mode, @@wait_timeout, @@GLOBAL.wait_timeout, @@sql_auto_is_null, @@sql_safe_updates, @@default_storage_engine"
            ),
            [
                Some(
                    "ONLY_FULL_GROUP_BY,NO_AUTO_VALUE_ON_ZERO,STRICT_TRANS_TABLES,STRICT_ALL_TABLES,\
                     NO_ZERO_IN_DATE,NO_ZERO_DATE,ERROR_FOR_DIVISION_BY_ZERO,NO_ENGINE_SUBSTITUTION"
                        .to_owned()
                ),
                Some("2147483".to_owned()),
                Some(MySqlBootstrapSettings::default().wait_timeout_seconds().to_string()),
                Some("0".to_owned()),
                Some("0".to_owned()),
                Some("InnoDB".to_owned()),
            ]
        );
        assert!(session.no_auto_value_on_zero());
        assert_eq!(session.wait_timeout(), Some(Duration::from_secs(2_147_483)));

        let mut run = |sql: &str| {
            session.execute_query(
                sql,
                MySqlBootstrapSettings::default(),
                None,
                SessionSqlMode::default(),
                SERVER_STATUS_AUTOCOMMIT,
            )
        };
        // Measured on MySQL 8.4.11: DEFAULT goes back to what a new session
        // starts with.
        assert!(matches!(
            run("SET sql_mode = DEFAULT, wait_timeout = DEFAULT"),
            Ok(Some(CommandExecutionResult::Ok(_)))
        ));
        assert!(!session.no_auto_value_on_zero());
        assert_eq!(session.wait_timeout(), None);
    }

    /// Measured on MySQL 8.4.11: when one assignment of a `SET` fails, none of
    /// them takes effect.
    #[test]
    fn a_set_with_one_refused_assignment_changes_nothing() {
        let mut session = MySqlSessionVariables::default();
        let mut run = |sql: &str| {
            session.execute_query(
                sql,
                MySqlBootstrapSettings::default(),
                None,
                SessionSqlMode::default(),
                SERVER_STATUS_AUTOCOMMIT,
            )
        };
        for sql in [
            "SET foreign_key_checks = 0, wait_timeout = 0",
            "SET @@SESSION.sql_mode = CONCAT(@@sql_mode, ',NO_AUTO_VALUE_ON_ZERO'), sql_auto_is_null = 1",
            "SET wait_timeout = 100, sql_safe_updates = 1",
            "SET SESSION wait_timeout = 100, GLOBAL foreign_key_checks = 0",
        ] {
            assert_eq!(run(sql), Err(FrontendErrorKind::Unsupported), "{sql}");
        }
        assert!(session.foreign_key_checks);
        assert!(!session.no_auto_value_on_zero());
        assert_eq!(session.wait_timeout(), None);
    }

    /// The two levels this server keeps are taken, and each reads back as the
    /// session's level. The other two are refused rather than accepted and
    /// ignored — a client told yes to `SERIALIZABLE` would reason about a
    /// guarantee it does not have.
    #[test]
    fn takes_the_isolation_levels_it_keeps_and_no_other() {
        let mut session = MySqlSessionVariables::default();
        let mut run = |sql: &str, status_flags: u16| {
            session.execute_query(
                sql,
                MySqlBootstrapSettings::default(),
                None,
                SessionSqlMode::default(),
                status_flags,
            )
        };
        let read_back = |run: &mut dyn FnMut(&str, u16) -> _| {
            let Ok(Some(CommandExecutionResult::ResultSet(result))) =
                run("SELECT @@transaction_isolation", SERVER_STATUS_AUTOCOMMIT)
            else {
                panic!("the level must read back");
            };
            String::from_utf8(result.rows[0][0].clone().unwrap()).unwrap()
        };
        for (sql, level) in [
            (
                "SET SESSION TRANSACTION ISOLATION LEVEL READ COMMITTED",
                "READ-COMMITTED",
            ),
            (
                "set local transaction isolation level repeatable read",
                "REPEATABLE-READ",
            ),
            (
                "SET SESSION transaction_isolation = 'READ-COMMITTED'",
                "READ-COMMITTED",
            ),
            (
                "SET transaction_isolation = 'REPEATABLE-READ'",
                "REPEATABLE-READ",
            ),
        ] {
            assert!(
                matches!(
                    run(sql, SERVER_STATUS_AUTOCOMMIT),
                    Ok(Some(CommandExecutionResult::Ok(_)))
                ),
                "{sql}"
            );
            assert_eq!(read_back(&mut run), level, "{sql}");
        }

        // Measured on MySQL 8.4.11: the next-transaction forms leave the
        // session's level where it was.
        for sql in [
            "SET TRANSACTION ISOLATION LEVEL READ COMMITTED",
            "SET @@transaction_isolation = 'READ-COMMITTED'",
        ] {
            assert!(
                matches!(
                    run(sql, SERVER_STATUS_AUTOCOMMIT),
                    Ok(Some(CommandExecutionResult::Ok(_)))
                ),
                "{sql}"
            );
            assert_eq!(read_back(&mut run), "REPEATABLE-READ", "{sql}");
        }
        assert_eq!(
            session.isolation_for_next_transaction(),
            MySqlIsolationLevel::ReadCommitted
        );
        session.use_up_next_transaction_isolation();
        assert_eq!(
            session.isolation_for_next_transaction(),
            MySqlIsolationLevel::RepeatableRead
        );

        let mut run = |sql: &str, status_flags: u16| {
            session.execute_query(
                sql,
                MySqlBootstrapSettings::default(),
                None,
                SessionSqlMode::default(),
                status_flags,
            )
        };
        // Inside a transaction the next-transaction form is 1568, while the
        // session form is taken.
        assert_eq!(
            run(
                "SET TRANSACTION ISOLATION LEVEL READ COMMITTED",
                SERVER_STATUS_IN_TRANS
            ),
            Err(FrontendErrorKind::TransactionCharacteristicsInProgress)
        );
        assert!(matches!(
            run(
                "SET SESSION TRANSACTION ISOLATION LEVEL READ COMMITTED",
                SERVER_STATUS_IN_TRANS
            ),
            Ok(Some(CommandExecutionResult::Ok(_)))
        ));

        for sql in [
            "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE",
            "SET SESSION TRANSACTION ISOLATION LEVEL READ UNCOMMITTED",
            "SET SESSION transaction_isolation = 'SERIALIZABLE'",
        ] {
            assert!(
                matches!(
                    run(sql, SERVER_STATUS_AUTOCOMMIT),
                    Err(FrontendErrorKind::Unsupported)
                ),
                "{sql}"
            );
        }
    }

    #[test]
    fn answers_the_version_a_client_asks_for_at_startup() {
        let mut session = MySqlSessionVariables::default();
        let mut run = |sql: &str| {
            session.execute_query(
                sql,
                MySqlBootstrapSettings::default(),
                None,
                SessionSqlMode::default(),
                2,
            )
        };
        // The `mysql` client opens with exactly this, LIMIT and all.
        let Ok(Some(CommandExecutionResult::ResultSet(comment))) =
            run("select @@version_comment limit 1")
        else {
            panic!("expected a version_comment result");
        };
        assert_eq!(comment.columns[0].name, "@@version_comment");
        assert_eq!(comment.columns[0].column_length, 87_380);
        assert_eq!(comment.columns[0].flags, 0);
        assert_eq!(
            comment.rows,
            vec![vec![Some(SERVER_VERSION_COMMENT.as_bytes().to_vec())]]
        );

        // The version has to be the one the handshake announced, since a
        // client compares them.
        for (sql, name, length, flags) in [
            ("SELECT @@version", "@@version", 87_380, 0),
            ("SELECT @@session.version", "@@session.version", 87_380, 0),
            ("SELECT VERSION()", "VERSION()", 24, MYSQL_NOT_NULL_FLAG),
            ("SELECT @@version AS v", "v", 87_380, 0),
            // An alias hides the parentheses, and MySQL still answers the
            // call's own narrower NOT NULL column — measured on 8.4.11.
            ("SELECT VERSION() AS v", "v", 24, MYSQL_NOT_NULL_FLAG),
        ] {
            let Ok(Some(CommandExecutionResult::ResultSet(result))) = run(sql) else {
                panic!("expected a version result for {sql}");
            };
            assert_eq!(result.columns[0].name, name, "{sql}");
            assert_eq!(result.columns[0].column_length, length, "{sql}");
            assert_eq!(result.columns[0].flags, flags, "{sql}");
            assert_eq!(
                result.rows,
                vec![vec![Some(SERVER_VERSION.as_bytes().to_vec())]],
                "{sql}"
            );
        }

        // A variable this has no honest answer for reads as one this build of
        // the server does not have, which is what MySQL answers 1193 for.
        assert_eq!(
            run("SELECT @@innodb_version"),
            Err(FrontendErrorKind::UnknownSystemVariable)
        );
    }

    /// A driver opens the connection by reading a row of these at once, so a
    /// list of them is answered rather than only one. Measured on MySQL
    /// 8.4.11: each column is the one that variable answers on its own, in the
    /// order the statement names them, and a name the server does not have
    /// fails the whole statement rather than leaving a column out.
    #[test]
    fn a_row_of_variables_is_read_at_once() {
        let mut session = MySqlSessionVariables::default();
        let mut run = |sql: &str| {
            session.execute_query(
                sql,
                MySqlBootstrapSettings::default(),
                None,
                SessionSqlMode::default(),
                2,
            )
        };
        // The pinned `mysql_async` driver opens with exactly these bytes.
        let Ok(Some(CommandExecutionResult::ResultSet(result))) =
            run("SELECT @@max_allowed_packet,@@wait_timeout")
        else {
            panic!("expected a settings result");
        };
        assert_eq!(
            result
                .columns
                .iter()
                .map(|column| (
                    column.name.as_str(),
                    column.column_length,
                    column.flags,
                    column.column_type
                ))
                .collect::<Vec<_>>(),
            [
                (
                    "@@max_allowed_packet",
                    21,
                    MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG | MYSQL_UNSIGNED_FLAG,
                    MYSQL_TYPE_LONGLONG
                ),
                (
                    "@@wait_timeout",
                    21,
                    MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG | MYSQL_UNSIGNED_FLAG,
                    MYSQL_TYPE_LONGLONG
                ),
            ]
        );
        assert_eq!(
            result.rows,
            vec![vec![
                Some(
                    MySqlBootstrapSettings::default()
                        .max_allowed_packet()
                        .to_string()
                        .into_bytes()
                ),
                Some(
                    MySqlBootstrapSettings::default()
                        .wait_timeout_seconds()
                        .to_string()
                        .into_bytes()
                ),
            ]]
        );

        // Aliases, scopes, a call and a limit all read the same in a list as
        // they do on their own.
        let Ok(Some(CommandExecutionResult::ResultSet(result))) =
            run("SELECT @@global.autocommit AS a, VERSION(), @@sql_mode m LIMIT 1")
        else {
            panic!("expected a variable result");
        };
        assert_eq!(
            result
                .columns
                .iter()
                .map(|column| column.name.as_str())
                .collect::<Vec<_>>(),
            ["a", "VERSION()", "m"]
        );
        assert_eq!(result.columns[1].column_length, 24);
        assert_eq!(
            result.rows[0][..2],
            [
                Some(b"1".to_vec()),
                Some(SERVER_VERSION.as_bytes().to_vec())
            ]
        );

        // One name this server cannot answer fails the whole statement rather
        // than leaving a column out.
        assert_eq!(
            run("SELECT @@version, @@innodb_version"),
            Err(FrontendErrorKind::UnknownSystemVariable)
        );
    }

    /// A global read answers what a new session would start from, not what
    /// this one is using — measured on MySQL 8.4.11, where a session that
    /// turns `autocommit` and `foreign_key_checks` off and adds `ANSI_QUOTES`
    /// to its `sql_mode` still reads all three back unchanged under
    /// `@@global.`.
    #[test]
    fn a_global_read_answers_what_a_new_session_would_start_from() {
        let mut session = MySqlSessionVariables::default();
        let ansi = SessionSqlMode {
            ansi_quotes: true,
            ..SessionSqlMode::default()
        };
        for sql in ["SET foreign_key_checks = 0", "SET sql_notes = 0"] {
            assert!(
                matches!(
                    session.execute_query(sql, MySqlBootstrapSettings::default(), None, ansi, 0),
                    Ok(Some(CommandExecutionResult::Ok(_)))
                ),
                "{sql}"
            );
        }
        // The session runs with autocommit off, which is what a status flag
        // of zero says, and with a mode the server was not started in.
        let mut read = |sql: &str| {
            let Ok(Some(CommandExecutionResult::ResultSet(result))) =
                session.execute_query(sql, MySqlBootstrapSettings::default(), None, ansi, 0)
            else {
                panic!("expected a variable result for {sql}");
            };
            String::from_utf8(result.rows[0][0].clone().unwrap()).unwrap()
        };
        for (sql, session_value, global_value) in [
            ("autocommit", "0", "1"),
            ("foreign_key_checks", "0", "1"),
            ("sql_notes", "0", "1"),
        ] {
            assert_eq!(read(&format!("SELECT @@{sql}")), session_value, "{sql}");
            assert_eq!(
                read(&format!("SELECT @@session.{sql}")),
                session_value,
                "{sql}"
            );
            assert_eq!(
                read(&format!("SELECT @@local.{sql}")),
                session_value,
                "{sql}"
            );
            assert_eq!(
                read(&format!("SELECT @@global.{sql}")),
                global_value,
                "{sql}"
            );
        }
        assert!(read("SELECT @@sql_mode").starts_with("ANSI_QUOTES,"));
        assert!(read("SELECT @@global.sql_mode").starts_with("ONLY_FULL_GROUP_BY,"));
        // A variable the server keeps one of answers the same either way.
        assert_eq!(read("SELECT @@global.version"), SERVER_VERSION);
        assert_eq!(
            read("SELECT @@global.max_allowed_packet"),
            MySqlBootstrapSettings::default()
                .max_allowed_packet()
                .to_string()
        );
    }

    #[test]
    fn takes_the_settings_a_client_opens_with_and_refuses_the_rest() {
        let mut session = MySqlSessionVariables::default();
        let mut run = |sql: &str| {
            session.execute_query(
                sql,
                MySqlBootstrapSettings::default(),
                None,
                SessionSqlMode::default(),
                2,
            )
        };
        for sql in [
            // What `mysqldump --no-data` sends first, versioned comments and
            // all, plus the `SET NAMES` every driver opens with.
            "/*!40100 SET @@SQL_MODE='' */",
            "/*!40103 SET TIME_ZONE='+00:00' */",
            "/*!80000 SET SESSION information_schema_stats_expiry=0 */",
            "SET NAMES utf8mb4",
            "SET NAMES 'utf8mb4' COLLATE 'utf8mb4_general_ci'",
            // MySQL 8.4's own default, which is what a client that reads the
            // variable and writes it back sends.
            "SET sql_mode = 'ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES,NO_ZERO_IN_DATE,NO_ZERO_DATE,ERROR_FOR_DIVISION_BY_ZERO,NO_ENGINE_SUBSTITUTION'",
            "SET time_zone = '+09:00'",
        ] {
            assert!(
                matches!(run(sql), Ok(Some(CommandExecutionResult::Ok(_)))),
                "{sql}"
            );
        }

        for sql in [
            // Each of these would change how the server behaves, so taking it
            // would leave the client believing something untrue.
            "SET NAMES latin1",
            "SET NAMES utf8mb4 COLLATE utf8mb4_bin",
            "SET sql_mode = 'ANSI_QUOTES'",
            "SET sql_mode = 'NO_BACKSLASH_ESCAPES'",
            "SET sql_mode = 'PIPES_AS_CONCAT'",
            "SET time_zone = '+14:01'",
        ] {
            assert_eq!(run(sql), Err(FrontendErrorKind::Unsupported), "{sql}");
        }
    }

    #[test]
    fn fixed_time_zone_offsets_follow_mysql_limits() {
        let mut session = MySqlSessionVariables::default();
        for (zone, seconds) in [("+14:00", 50_400), ("-13:59", -50_340), ("-00:00", 0)] {
            let sql = format!("SET time_zone = '{zone}'");
            assert!(session
                .execute_query(
                    &sql,
                    MySqlBootstrapSettings::default(),
                    None,
                    SessionSqlMode::default(),
                    2
                )
                .unwrap()
                .is_some());
            assert_eq!(session.time_zone_offset_seconds(), seconds);
        }
        assert_eq!(session.time_zone, "+00:00");
        assert_eq!(
            session.execute_query(
                "SET time_zone = '+14:01'",
                MySqlBootstrapSettings::default(),
                None,
                SessionSqlMode::default(),
                2
            ),
            Err(FrontendErrorKind::Unsupported)
        );
    }

    #[test]
    fn null_result_charset_reads_back_until_set_names_resets_it() {
        let mut session = MySqlSessionVariables::default();
        let mut run = |sql: &str| {
            session
                .execute_query(
                    sql,
                    MySqlBootstrapSettings::default(),
                    None,
                    SessionSqlMode::default(),
                    2,
                )
                .unwrap()
                .unwrap()
        };
        run("SET character_set_results = NULL");
        let CommandExecutionResult::ResultSet(read) = run("SELECT @@character_set_results") else {
            panic!("expected a variable result");
        };
        assert_eq!(read.rows, vec![vec![None]]);
        let CommandExecutionResult::ResultSet(shown) =
            run("SHOW VARIABLES LIKE 'character_set_results'")
        else {
            panic!("expected a SHOW result");
        };
        assert_eq!(
            named(&shown),
            vec![("character_set_results".to_owned(), String::new())]
        );
        run("SET NAMES utf8mb4");
        let CommandExecutionResult::ResultSet(read) = run("SELECT @@character_set_results") else {
            panic!("expected a variable result");
        };
        assert_eq!(read.rows, vec![vec![Some(b"utf8mb4".to_vec())]]);
    }

    #[test]
    fn mysqldump_uses_the_default_quoted_create_and_binary_results() {
        let mut session = MySqlSessionVariables::default();
        let run = |session: &mut MySqlSessionVariables, sql: &str| {
            session.execute_query(
                sql,
                MySqlBootstrapSettings::default(),
                None,
                SessionSqlMode::default(),
                2,
            )
        };
        assert!(run(&mut session, "SET SQL_QUOTE_SHOW_CREATE=1")
            .unwrap()
            .is_some());
        assert_eq!(
            run(&mut session, "SET SQL_QUOTE_SHOW_CREATE=0"),
            Err(FrontendErrorKind::Unsupported)
        );
        run(&mut session, "SET SESSION character_set_results = 'binary'").unwrap();
        assert!(session.binary_character_set_results());
        let CommandExecutionResult::ResultSet(result) =
            run(&mut session, "SHOW VARIABLES LIKE 'character_set_results'")
                .unwrap()
                .unwrap()
        else {
            panic!("expected SHOW VARIABLES result");
        };
        assert_eq!(
            named(&result),
            [("character_set_results".to_owned(), "binary".to_owned())]
        );
        let CommandExecutionResult::ResultSet(result) =
            run(&mut session, "SELECT @@character_set_results")
                .unwrap()
                .unwrap()
        else {
            panic!("expected variable result");
        };
        assert_eq!(result.rows, [vec![Some(b"binary".to_vec())]]);
        run(
            &mut session,
            "SET SESSION character_set_results = 'utf8mb4'",
        )
        .unwrap();
        assert!(!session.binary_character_set_results());
    }

    #[test]
    fn compact_mysqldump_restores_only_the_supported_session_settings() {
        let mut session = MySqlSessionVariables::default();
        let mut run = |sql: &str| {
            session.execute_query(
                sql,
                MySqlBootstrapSettings::default(),
                None,
                SessionSqlMode::default(),
                2,
            )
        };
        for sql in [
            "/*!50003 SET @saved_cs_client = @@character_set_client */",
            "/*!50003 SET @saved_cs_results = @@character_set_results */",
            "/*!50003 SET @saved_col_connection = @@collation_connection */",
            "/*!50003 SET @saved_sql_mode = @@sql_mode */",
            "/*!50003 SET character_set_client = utf8mb4 */",
            "/*!50003 SET character_set_results = utf8mb4 */",
            "/*!50003 SET collation_connection = utf8mb4_general_ci */",
            "/*!50003 SET sql_mode = 'ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES,NO_ZERO_IN_DATE,NO_ZERO_DATE,ERROR_FOR_DIVISION_BY_ZERO,NO_ENGINE_SUBSTITUTION' */",
            "/*!50003 SET sql_mode = @saved_sql_mode */",
            "/*!50003 SET character_set_client = @saved_cs_client */",
            "/*!50003 SET character_set_results = @saved_cs_results */",
            "/*!50003 SET collation_connection = @saved_col_connection */",
        ] {
            assert!(
                matches!(run(sql), Ok(Some(CommandExecutionResult::Ok(_)))),
                "{sql}"
            );
        }
        for sql in [
            "SET character_set_client = latin1",
            "SET character_set_results = latin1",
            "SET collation_connection = latin1_swedish_ci",
            "SET collation_connection = utf8mb4_0900_ai_ci",
            "SET character_set_client = @missing",
            "SET sql_mode = @missing",
        ] {
            assert_eq!(run(sql), Err(FrontendErrorKind::Unsupported), "{sql}");
        }
        assert_eq!(
            run("SET @saved_cs_client = @@unknown_dump_variable"),
            Err(FrontendErrorKind::UnknownSystemVariable)
        );
    }

    #[test]
    fn a_session_in_ansi_quotes_takes_the_mode_it_is_in() {
        let mut session = MySqlSessionVariables::default();
        let ansi = SessionSqlMode {
            ansi_quotes: true,
            no_backslash_escapes: false,
        };
        assert!(matches!(
            session.execute_query(
                "SET sql_mode = 'ANSI_QUOTES'",
                MySqlBootstrapSettings::default(),
                None,
                ansi,
                2,
            ),
            Ok(Some(CommandExecutionResult::Ok(_)))
        ));
        // And refuses one it is not in, in either direction.
        assert_eq!(
            session.execute_query(
                "SET sql_mode = ''",
                MySqlBootstrapSettings::default(),
                None,
                ansi,
                2,
            ),
            Ok(Some(CommandExecutionResult::Ok(CommandOkResult {
                status_flags: 2,
                ..CommandOkResult::default()
            })))
        );
    }

    fn notes(session: &mut MySqlSessionVariables) -> Vec<u8> {
        let Some(CommandExecutionResult::ResultSet(result)) = session
            .execute_query(
                "SELECT @@sql_notes",
                MySqlBootstrapSettings::default(),
                None,
                SessionSqlMode::default(),
                3,
            )
            .unwrap()
        else {
            panic!("expected sql_notes result");
        };
        assert_eq!(result.status_flags, 3);
        result.rows[0][0].clone().unwrap()
    }

    #[test]
    fn read_metadata_matches_mysql_8_4() {
        let mut session = MySqlSessionVariables::default();
        let Some(CommandExecutionResult::ResultSet(result)) = session
            .execute_query(
                "SELECT @@SeSsIoN.SQL_NOTES",
                MySqlBootstrapSettings::default(),
                None,
                SessionSqlMode::default(),
                2,
            )
            .unwrap()
        else {
            panic!("expected sql_notes result");
        };
        let column = &result.columns[0];
        assert_eq!(column.catalog, "def");
        assert_eq!(column.name, "@@SeSsIoN.SQL_NOTES");
        assert_eq!(column.column_type, 8);
        assert_eq!(column.character_set, 63);
        assert_eq!(column.column_length, 1);
        // Measured on MySQL 8.4.11: BINARY beside NUM, as every number-valued
        // system variable carries.
        assert_eq!(column.flags, MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG);
        assert_eq!(column.decimals, 0);
        assert_eq!(result.warnings, 0);
    }

    #[test]
    fn transaction_read_only_reports_the_unchangeable_session_default() {
        let mut session = MySqlSessionVariables::default();
        for sql in [
            "SELECT @@session.transaction_read_only",
            "SELECT @@global.transaction_read_only",
            "SELECT @@transaction_read_only",
        ] {
            let Some(CommandExecutionResult::ResultSet(result)) = session
                .execute_query(
                    sql,
                    MySqlBootstrapSettings::default(),
                    None,
                    SessionSqlMode::default(),
                    2,
                )
                .unwrap()
            else {
                panic!("expected transaction_read_only result for {sql}");
            };
            assert_eq!(result.rows, vec![vec![Some(b"0".to_vec())]], "{sql}");
            let column = &result.columns[0];
            assert_eq!(column.name, &sql["SELECT ".len()..]);
            assert_eq!(column.catalog, "def");
            assert_eq!(column.column_type, MYSQL_TYPE_LONGLONG);
            assert_eq!(column.character_set, MYSQL_BINARY_COLLATION);
            assert_eq!(column.column_length, 1);
            assert_eq!(column.decimals, 0);
            assert_eq!(column.flags, MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG);
        }
        assert_eq!(
            named(&variables(
                &mut session,
                "SHOW SESSION VARIABLES LIKE 'transaction_read_only'"
            )),
            vec![("transaction_read_only".to_owned(), "OFF".to_owned())]
        );
    }

    #[test]
    fn unrelated_lexer_errors_remain_with_the_existing_query_owner() {
        let mut session = MySqlSessionVariables::default();
        assert!(session
            .execute_query(
                "SELECT 'unterminated",
                MySqlBootstrapSettings::default(),
                None,
                SessionSqlMode::default(),
                2
            )
            .unwrap()
            .is_none());
    }

    fn variables(session: &mut MySqlSessionVariables, sql: &str) -> TextResultSet {
        let Some(CommandExecutionResult::ResultSet(result)) = session
            .execute_query(
                sql,
                MySqlBootstrapSettings::default(),
                None,
                SessionSqlMode::default(),
                2,
            )
            .unwrap()
        else {
            panic!("expected a SHOW VARIABLES result for {sql}");
        };
        result
    }

    fn named(result: &TextResultSet) -> Vec<(String, String)> {
        result
            .rows
            .iter()
            .map(|row| {
                let text = |index: usize| {
                    String::from_utf8(
                        row[index]
                            .clone()
                            .expect("SHOW VARIABLES rows are not null"),
                    )
                    .expect("SHOW VARIABLES rows are text")
                };
                (text(0), text(1))
            })
            .collect()
    }

    #[test]
    fn select_database_answers_with_or_without_a_selected_database() {
        let mut session = MySqlSessionVariables::default();
        for (selected, expected) in [(None, None), (Some("app"), Some("app"))] {
            let Some(CommandExecutionResult::ResultSet(result)) = session
                .execute_query(
                    "SELECT DATABASE()",
                    MySqlBootstrapSettings::default(),
                    selected,
                    SessionSqlMode::default(),
                    2,
                )
                .unwrap()
            else {
                panic!("expected a DATABASE() result for {selected:?}");
            };
            assert_eq!(result.status_flags, 2);
            assert_eq!(result.rows.len(), 1);
            assert_eq!(
                result.rows[0][0]
                    .as_ref()
                    .map(|value| String::from_utf8(value.clone()).unwrap()),
                expected.map(str::to_owned)
            );

            let column = &result.columns[0];
            assert_eq!(column.name, "DATABASE()");
            assert_eq!(column.catalog, "def");
            assert_eq!(column.schema, "");
            assert_eq!(column.table, "");
            assert_eq!(column.original_table, "");
            assert_eq!(column.original_name, "");
            assert_eq!(column.column_type, MYSQL_TYPE_VAR_STRING);
            assert_eq!(column.column_length, 256);
            assert_eq!(column.flags, 0);
            assert_eq!(column.decimals, NOT_FIXED_DECIMALS);
        }

        // MySQL names the column after the call as written, or after an alias.
        for (sql, name) in [
            ("select schema()", "schema()"),
            ("SELECT DATABASE() AS db", "db"),
        ] {
            let Some(CommandExecutionResult::ResultSet(result)) = session
                .execute_query(
                    sql,
                    MySqlBootstrapSettings::default(),
                    Some("app"),
                    SessionSqlMode::default(),
                    2,
                )
                .unwrap()
            else {
                panic!("expected a DATABASE() result for {sql}");
            };
            assert_eq!(result.columns[0].name, name, "{sql}");
        }
    }

    #[test]
    fn show_variables_metadata_matches_mysql_8_4_apart_from_the_collation() {
        let mut session = MySqlSessionVariables::default();
        let result = variables(&mut session, "SHOW VARIABLES LIKE 'sql_notes'");
        assert_eq!(result.status_flags, 2);
        assert_eq!(
            result
                .columns
                .iter()
                .map(|column| (
                    column.name.as_str(),
                    column.original_name.as_str(),
                    column.catalog.as_str(),
                    column.schema.as_str(),
                    column.table.as_str(),
                    column.original_table.as_str(),
                    column.column_type,
                    column.character_set,
                    column.column_length,
                    column.flags,
                ))
                .collect::<Vec<_>>(),
            vec![
                (
                    "Variable_name",
                    "Variable_name",
                    "def",
                    "performance_schema",
                    "session_variables",
                    "session_variables",
                    MYSQL_TYPE_VAR_STRING,
                    u16::from(DEFAULT_UTF8MB4_COLLATION),
                    256,
                    MYSQL_NOT_NULL_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG,
                ),
                (
                    "Value",
                    "Value",
                    "def",
                    "performance_schema",
                    "session_variables",
                    "session_variables",
                    MYSQL_TYPE_VAR_STRING,
                    u16::from(DEFAULT_UTF8MB4_COLLATION),
                    4096,
                    0,
                ),
            ]
        );

        let global = variables(&mut session, "SHOW GLOBAL VARIABLES LIKE 'sql_notes'");
        assert!(global
            .columns
            .iter()
            .all(|column| column.table == "global_variables"
                && column.original_table == "global_variables"));
    }

    #[test]
    fn show_variables_reports_only_the_variables_this_server_has() {
        let mut session = MySqlSessionVariables::default();
        let settings =
            MySqlBootstrapSettings::new(67_108_864, std::time::Duration::from_secs(28_800));
        let Some(CommandExecutionResult::ResultSet(all)) = session
            .execute_query(
                "SHOW VARIABLES",
                settings,
                None,
                SessionSqlMode::default(),
                2,
            )
            .unwrap()
        else {
            panic!("expected a SHOW VARIABLES result");
        };
        // The names `SELECT @@name` answers, in the name order MySQL writes
        // them in, with a switch written as MySQL writes one.
        assert_eq!(
            named(&all)
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str()))
                .collect::<Vec<_>>(),
            [
                ("auto_increment_increment", "1"),
                ("auto_increment_offset", "1"),
                ("autocommit", "ON"),
                ("character_set_client", "utf8mb4"),
                ("character_set_connection", "utf8mb4"),
                ("character_set_database", "utf8mb4"),
                ("character_set_results", "utf8mb4"),
                ("character_set_server", "utf8mb4"),
                ("collation_connection", "utf8mb4_general_ci"),
                ("collation_database", "utf8mb4_0900_ai_ci"),
                ("collation_server", "utf8mb4_0900_ai_ci"),
                ("default_storage_engine", "InnoDB"),
                ("foreign_key_checks", "ON"),
                ("init_connect", ""),
                ("interactive_timeout", "28800"),
                ("license", "MIT"),
                ("lower_case_table_names", "1"),
                ("max_allowed_packet", "67108864"),
                ("net_write_timeout", "60"),
                ("performance_schema", "OFF"),
                ("sql_auto_is_null", "OFF"),
                (
                    "sql_mode",
                    "ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES,NO_ZERO_IN_DATE,\
                     NO_ZERO_DATE,ERROR_FOR_DIVISION_BY_ZERO,NO_ENGINE_SUBSTITUTION"
                ),
                ("sql_notes", "ON"),
                ("sql_safe_updates", "OFF"),
                ("system_time_zone", "UTC"),
                ("time_zone", "SYSTEM"),
                ("transaction_isolation", "REPEATABLE-READ"),
                ("transaction_read_only", "OFF"),
                ("version", SERVER_VERSION),
                ("version_comment", SERVER_VERSION_COMMENT),
                ("wait_timeout", "28800"),
            ]
        );

        // One name reads the same here as `SELECT @@name` answers it, since
        // both go through the one reader.
        let one = variables(&mut session, "SHOW VARIABLES LIKE 'time_zone'");
        assert_eq!(
            named(&one),
            vec![("time_zone".to_owned(), "SYSTEM".to_owned())]
        );

        // The two statements a real `mysqldump --no-data` opens with. MySQL
        // 8.4.11 answers the second with zero rows because its build has no
        // `ndbinfo_version`; this server answers both that way.
        for sql in [
            "SHOW VARIABLES LIKE 'gtid_mode'",
            r"SHOW VARIABLES LIKE 'ndbinfo\_version'",
        ] {
            assert!(variables(&mut session, sql).rows.is_empty(), "{sql}");
        }
    }

    #[test]
    fn show_variables_follows_the_session_value_and_the_global_default() {
        let mut session = MySqlSessionVariables::default();
        session
            .execute_query(
                "SET SESSION sql_notes=0",
                MySqlBootstrapSettings::default(),
                None,
                SessionSqlMode::default(),
                2,
            )
            .unwrap();
        assert_eq!(
            named(&variables(&mut session, "SHOW VARIABLES LIKE 'sql_notes'")),
            vec![("sql_notes".to_owned(), "OFF".to_owned())]
        );
        assert_eq!(
            named(&variables(
                &mut session,
                "SHOW GLOBAL VARIABLES LIKE 'sql_notes'"
            )),
            vec![("sql_notes".to_owned(), "ON".to_owned())]
        );
    }

    #[test]
    fn sql_notes_is_session_local_and_invalid_statements_do_not_change_it() {
        let mut first = MySqlSessionVariables::default();
        let mut second = MySqlSessionVariables::default();
        assert_eq!(notes(&mut first), b"1");
        let Some(CommandExecutionResult::Ok(result)) = first
            .execute_query(
                "SET SESSION sql_notes=0",
                MySqlBootstrapSettings::default(),
                None,
                SessionSqlMode::default(),
                3,
            )
            .unwrap()
        else {
            panic!("expected SET success");
        };
        assert_eq!(result.status_flags, 3);
        assert_eq!(notes(&mut first), b"0");
        assert_eq!(notes(&mut second), b"1");
        assert!(first
            .execute_query(
                "SET sql_notes=2",
                MySqlBootstrapSettings::default(),
                None,
                SessionSqlMode::default(),
                3
            )
            .is_err());
        assert!(first
            .execute_query(
                "SET sql_notes=1; SELECT 1",
                MySqlBootstrapSettings::default(),
                None,
                SessionSqlMode::default(),
                3
            )
            .unwrap()
            .is_none());
        assert_eq!(notes(&mut first), b"0");
        first
            .execute_query(
                "SET sql_notes=1",
                MySqlBootstrapSettings::default(),
                None,
                SessionSqlMode::default(),
                3,
            )
            .unwrap();
        assert_eq!(notes(&mut first), b"1");
    }
}
