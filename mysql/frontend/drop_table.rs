use turso_core::LimboError;

/// Failure while dropping one checked MySQL table.
#[derive(Debug)]
pub enum MySqlDropTableError {
    /// The command named no stored table.
    MissingTable,
    /// Core rejected or failed to execute the translated drop statement.
    Engine(LimboError),
}

impl std::fmt::Display for MySqlDropTableError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingTable => formatter.write_str("unknown table"),
            Self::Engine(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for MySqlDropTableError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Engine(error) => Some(error),
            Self::MissingTable => None,
        }
    }
}

/// Outcome of one checked `DROP TABLE` command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MySqlDropTableResult {
    /// Whether a stored base table was removed.
    pub dropped: bool,
}
