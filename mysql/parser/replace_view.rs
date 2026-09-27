use super::*;

/// A `CREATE OR REPLACE VIEW` or an `ALTER VIEW`: the view it names and the
/// `CREATE VIEW` that writes the view again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlViewReplacement {
    view: MySqlTableName,
    create_view: String,
    requires_the_view: bool,
}

impl MySqlViewReplacement {
    /// Returns the view the statement names.
    pub fn view(&self) -> &MySqlTableName {
        &self.view
    }

    /// Returns the `CREATE VIEW` that writes the view again, which goes
    /// through every check a `CREATE VIEW` written by the client does.
    pub fn create_view(&self) -> &str {
        &self.create_view
    }

    /// Reports whether the view has to be there already, which is what
    /// `ALTER VIEW` asks and `CREATE OR REPLACE VIEW` does not.
    pub const fn requires_the_view(&self) -> bool {
        self.requires_the_view
    }
}

/// Reads a `CREATE OR REPLACE VIEW` or an `ALTER VIEW`, or nothing when the
/// statement is neither.
pub fn parse_optional_view_replacement(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlViewReplacement>, ParseError> {
    if !names_a_view_replacement(sql, mode)? {
        return Ok(None);
    }
    match parse_one_statement(sql, mode)? {
        Statement::CreateView(mut view) if view.or_replace => {
            let name = view_name(&view.name)?;
            view.or_replace = false;
            Ok(Some(MySqlViewReplacement {
                view: name,
                create_view: Statement::CreateView(view).to_string(),
                requires_the_view: false,
            }))
        }
        Statement::AlterView {
            name,
            columns,
            query,
            with_options,
        } => {
            if !columns.is_empty() || !with_options.is_empty() {
                return unsupported("ALTER VIEW option");
            }
            Ok(Some(MySqlViewReplacement {
                view: view_name(&name)?,
                create_view: format!("CREATE VIEW {name} AS {query}"),
                requires_the_view: true,
            }))
        }
        _ => unsupported("CREATE OR REPLACE VIEW or ALTER VIEW form"),
    }
}

/// Reports whether a statement begins `CREATE OR REPLACE VIEW` or
/// `ALTER VIEW`, before it is parsed whole.
fn names_a_view_replacement(sql: &str, mode: SessionSqlMode) -> Result<bool, ParseError> {
    let dialect = SessionMySqlDialect::without_executable_comments(mode);
    let tokens = Tokenizer::new(&dialect, sql)
        .tokenize()
        .map_err(|error| ParseError::Sqlparser(error.to_string()))?;
    let words = tokens
        .iter()
        .filter(|token| !matches!(token, Token::Whitespace(_)))
        .take(4)
        .collect::<Vec<_>>();
    let starts_with = |expected: &[&str]| {
        words.len() >= expected.len()
            && expected
                .iter()
                .zip(&words)
                .all(|(word, token)| is_unquoted_word(token, word))
    };
    Ok(starts_with(&["CREATE", "OR", "REPLACE", "VIEW"]) || starts_with(&["ALTER", "VIEW"]))
}

fn view_name(name: &ObjectName) -> Result<MySqlTableName, ParseError> {
    let [ObjectNamePart::Identifier(ident)] = name.0.as_slice() else {
        return unsupported("qualified view name");
    };
    MySqlTableName::parse(&ident.value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_replacement_is_written_as_the_create_view_it_makes() {
        let replacement = parse_optional_view_replacement(
            "CREATE OR REPLACE VIEW `V3` AS SELECT id FROM plain",
            SessionSqlMode::default(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(replacement.view().as_str(), "v3");
        assert_eq!(
            replacement.create_view(),
            "CREATE VIEW `V3` AS SELECT id FROM plain"
        );
        assert!(!replacement.requires_the_view());

        let replacement = parse_optional_view_replacement(
            "alter view v3 as select name from plain",
            SessionSqlMode::default(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(replacement.view().as_str(), "v3");
        assert_eq!(
            replacement.create_view(),
            "CREATE VIEW v3 AS SELECT name FROM plain"
        );
        assert!(replacement.requires_the_view());

        for sql in [
            "CREATE VIEW v3 AS SELECT id FROM plain",
            "SELECT 'ALTER VIEW v'",
            "ALTER TABLE plain ADD COLUMN n INT",
        ] {
            assert_eq!(
                parse_optional_view_replacement(sql, SessionSqlMode::default()).unwrap(),
                None,
                "{sql}"
            );
        }
        for sql in [
            "ALTER VIEW db.v3 AS SELECT id FROM plain",
            "ALTER VIEW v3 (a) AS SELECT id FROM plain",
            "CREATE OR REPLACE VIEW v3",
        ] {
            assert!(
                parse_optional_view_replacement(sql, SessionSqlMode::default()).is_err(),
                "{sql}"
            );
        }
    }
}
