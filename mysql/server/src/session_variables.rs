use turso_mysql_parser::{
    parse_optional_select_database, parse_optional_session_setting,
    parse_optional_session_sql_notes, parse_optional_show_variables, MySqlSelectDatabaseQuery,
    MySqlSessionSetting, MySqlSessionSqlNotes, MySqlShowVariablesCommand, MySqlVariableScope,
    SessionSqlMode,
};

use crate::{
    frontend_adapter::{
        MySqlBootstrapSettings, MYSQL_NOT_NULL_FLAG, MYSQL_NO_DEFAULT_VALUE_FLAG,
        NOT_FIXED_DECIMALS,
    },
    statement_execute::MYSQL_TYPE_VAR_STRING,
    ColumnDefinitionConfig, CommandExecutionResult, CommandOkResult, FrontendErrorKind,
    TextResultSet, DEFAULT_UTF8MB4_COLLATION,
};

#[derive(Debug)]
pub(crate) struct MySqlSessionVariables {
    sql_notes: bool,
}

impl Default for MySqlSessionVariables {
    fn default() -> Self {
        Self { sql_notes: true }
    }
}

impl MySqlSessionVariables {
    pub(crate) const fn sql_notes(&self) -> bool {
        self.sql_notes
    }

    pub(crate) fn execute_query(
        &mut self,
        sql: &str,
        settings: MySqlBootstrapSettings,
        selected_database: Option<&str>,
        session_sql_mode: SessionSqlMode,
        status_flags: u16,
    ) -> Result<Option<CommandExecutionResult>, FrontendErrorKind> {
        if let Some(setting) = parse_optional_session_setting(sql, session_sql_mode)
            .map_err(|_| FrontendErrorKind::Syntax)?
        {
            accept_session_setting(&setting, session_sql_mode)?;
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
        if let Some(command) = parse_optional_show_variables(sql, SessionSqlMode::default())
            .map_err(|_| FrontendErrorKind::Syntax)?
        {
            return Ok(Some(self.show_variables(&command, settings, status_flags)));
        }
        let command = match parse_optional_session_sql_notes(sql, SessionSqlMode::default()) {
            Ok(Some(command)) => command,
            Err(turso_mysql_parser::ParseError::Unsupported { .. }) => {
                return Err(FrontendErrorKind::Unsupported);
            }
            Ok(None) | Err(_) => return Ok(None),
        };
        match command {
            MySqlSessionSqlNotes::Set(enabled) => {
                self.sql_notes = enabled;
                Ok(Some(CommandExecutionResult::Ok(CommandOkResult {
                    status_flags,
                    ..CommandOkResult::default()
                })))
            }
            MySqlSessionSqlNotes::Select { column_name } => {
                let mut column = ColumnDefinitionConfig::new(column_name, 8);
                column.character_set = 63;
                column.column_length = 1;
                column.flags = 128;
                Ok(Some(CommandExecutionResult::ResultSet(TextResultSet {
                    columns: vec![column],
                    rows: vec![vec![Some(
                        if self.sql_notes { b"1" } else { b"0" }.to_vec(),
                    )]],
                    warnings: 0,
                    status_flags,
                })))
            }
        }
    }

    /// Answers `SHOW VARIABLES` for the variables this server actually has.
    ///
    /// MySQL 8.4.11 returns 647 rows for an unfiltered `SHOW VARIABLES`. This
    /// server has three of those variables, so it reports those and returns no
    /// row for any other name. That is what MySQL itself does for a variable
    /// its build leaves out: `SHOW VARIABLES LIKE 'ndbinfo\\_version'` returns
    /// the two columns and zero rows rather than an error.
    fn show_variables(
        &self,
        command: &MySqlShowVariablesCommand,
        settings: MySqlBootstrapSettings,
        status_flags: u16,
    ) -> CommandExecutionResult {
        // Nothing can change a global value on this server, so the global scope
        // reports the value a new session would start from.
        let sql_notes = match command.scope() {
            MySqlVariableScope::Session => self.sql_notes,
            MySqlVariableScope::Global => Self::default().sql_notes,
        };
        let rows = [
            (
                "max_allowed_packet",
                settings.max_allowed_packet().to_string(),
            ),
            ("sql_notes", switch_value(sql_notes).to_owned()),
            ("wait_timeout", settings.wait_timeout_seconds().to_string()),
        ]
        .into_iter()
        .filter(|(name, _)| command.selects(name))
        .map(|(name, value)| vec![Some(name.as_bytes().to_vec()), Some(value.into_bytes())])
        .collect();
        CommandExecutionResult::ResultSet(TextResultSet {
            columns: show_variables_columns(command.scope()),
            rows,
            warnings: 0,
            status_flags,
        })
    }
}

/// Takes a session setting only when the server is already in the state it
/// asks for.
///
/// Every real client opens with a handful of these, and refusing them all ends
/// the connection before any work starts. Accepting one that would change how
/// the server behaves is worse: the client would go on believing a setting took
/// effect. So each is checked against what this server actually does.
fn accept_session_setting(
    setting: &MySqlSessionSetting,
    session_sql_mode: SessionSqlMode,
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
        // Nothing here converts a moment between zones, which is the same as
        // running in UTC. Any other zone would be a claim this cannot keep.
        MySqlSessionSetting::TimeZone(zone) => {
            if ["+00:00", "-00:00", "UTC", "SYSTEM"]
                .iter()
                .any(|known| zone.eq_ignore_ascii_case(known))
            {
                Ok(())
            } else {
                Err(FrontendErrorKind::Unsupported)
            }
        }
        // This is how long MySQL caches `information_schema` statistics. There
        // are none here, so every value describes what this server does.
        MySqlSessionSetting::InformationSchemaStatsExpiry(_) => Ok(()),
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
            "SET time_zone = '+09:00'",
        ] {
            assert_eq!(run(sql), Err(FrontendErrorKind::Unsupported), "{sql}");
        }
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
        assert_eq!(column.flags, 128);
        assert_eq!(column.decimals, 0);
        assert_eq!(result.warnings, 0);
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
        assert_eq!(
            named(&all),
            vec![
                ("max_allowed_packet".to_owned(), "67108864".to_owned()),
                ("sql_notes".to_owned(), "ON".to_owned()),
                ("wait_timeout".to_owned(), "28800".to_owned()),
            ]
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
