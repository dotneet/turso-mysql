use turso_core::LimboError;

/// Failure while truncating one checked MySQL table.
#[derive(Debug)]
pub enum MySqlTruncateTableError {
    /// The command named no stored base table.
    MissingTable,
    /// The table allocates `AUTO_INCREMENT` values.
    ///
    /// MySQL restarts the counter at 1, and the durable allocator behind this
    /// frontend only ever moves its high water forward, so taking the statement
    /// would silently hand out different keys than MySQL does.
    AutoIncrementTable,
    /// Core rejected or failed to execute the translated statement.
    Engine(LimboError),
}

impl std::fmt::Display for MySqlTruncateTableError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingTable => formatter.write_str("unknown table"),
            Self::AutoIncrementTable => {
                formatter.write_str("TRUNCATE TABLE on an AUTO_INCREMENT table")
            }
            Self::Engine(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for MySqlTruncateTableError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Engine(error) => Some(error),
            Self::MissingTable | Self::AutoIncrementTable => None,
        }
    }
}
