use turso_core::LimboError;

/// Failure while running one `CREATE TABLE ... AS SELECT`.
#[derive(Debug)]
pub enum MySqlCreateTableAsSelectError {
    /// The `SELECT` named no stored base table.
    MissingTable,
    /// The projection named a column the source table does not carry.
    MissingColumn,
    /// A source column carries something the new table's DDL cannot be written
    /// from — today, a string `DEFAULT`, whose escaping this does not decide.
    UnsupportedColumn,
    /// The statement is one this frontend does not take.
    Query(crate::MySqlQueryError),
    /// Core rejected or failed to execute one of the translated statements.
    Engine(LimboError),
}

impl std::fmt::Display for MySqlCreateTableAsSelectError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingTable => formatter.write_str("unknown table"),
            Self::MissingColumn => formatter.write_str("unknown column"),
            Self::UnsupportedColumn => {
                formatter.write_str("CREATE TABLE AS SELECT over this column")
            }
            Self::Query(error) => error.fmt(formatter),
            Self::Engine(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for MySqlCreateTableAsSelectError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Engine(error) => Some(error),
            Self::MissingTable | Self::MissingColumn | Self::UnsupportedColumn | Self::Query(_) => {
                None
            }
        }
    }
}
