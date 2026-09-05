use turso_mysql_parser::{parse_optional_session_sql_notes, MySqlSessionSqlNotes, SessionSqlMode};

use crate::{
    ColumnDefinitionConfig, CommandExecutionResult, CommandOkResult, FrontendErrorKind,
    TextResultSet,
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
        status_flags: u16,
    ) -> Result<Option<CommandExecutionResult>, FrontendErrorKind> {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notes(session: &mut MySqlSessionVariables) -> Vec<u8> {
        let Some(CommandExecutionResult::ResultSet(result)) =
            session.execute_query("SELECT @@sql_notes", 3).unwrap()
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
            .execute_query("SELECT @@SeSsIoN.SQL_NOTES", 2)
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
            .execute_query("SELECT 'unterminated", 2)
            .unwrap()
            .is_none());
    }

    #[test]
    fn sql_notes_is_session_local_and_invalid_statements_do_not_change_it() {
        let mut first = MySqlSessionVariables::default();
        let mut second = MySqlSessionVariables::default();
        assert_eq!(notes(&mut first), b"1");
        let Some(CommandExecutionResult::Ok(result)) =
            first.execute_query("SET SESSION sql_notes=0", 3).unwrap()
        else {
            panic!("expected SET success");
        };
        assert_eq!(result.status_flags, 3);
        assert_eq!(notes(&mut first), b"0");
        assert_eq!(notes(&mut second), b"1");
        assert!(first.execute_query("SET sql_notes=2", 3).is_err());
        assert!(first
            .execute_query("SET sql_notes=1; SELECT 1", 3)
            .unwrap()
            .is_none());
        assert_eq!(notes(&mut first), b"0");
        first.execute_query("SET sql_notes=1", 3).unwrap();
        assert_eq!(notes(&mut first), b"1");
    }
}
