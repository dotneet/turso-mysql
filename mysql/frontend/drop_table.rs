use turso_core::LimboError;

/// Failure while dropping one checked MySQL table.
#[derive(Debug)]
pub enum MySqlDropTableError {
    /// The command named no stored table.
    MissingTable,
    /// The command named one table twice.
    NamedTwice,
    /// Core rejected or failed to execute the translated drop statement.
    Engine(LimboError),
}

impl std::fmt::Display for MySqlDropTableError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingTable => formatter.write_str("unknown table"),
            Self::NamedTwice => formatter.write_str("not unique table"),
            Self::Engine(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for MySqlDropTableError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Engine(error) => Some(error),
            Self::MissingTable | Self::NamedTwice => None,
        }
    }
}

/// Outcome of one checked `DROP TABLE` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlDropTableResult {
    /// The tables an `IF EXISTS` named that were not there, in the order it
    /// named them.
    pub missing: Vec<String>,
}
