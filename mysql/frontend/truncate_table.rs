use turso_core::LimboError;

/// Failure while truncating one checked MySQL table.
#[derive(Debug)]
pub enum MySqlTruncateTableError {
    /// The command named no stored base table.
    MissingTable,
    /// Another table's foreign key names this one.
    ///
    /// Measured on MySQL 8.4.11: the statement answers 1701 whatever the table
    /// holds, the emptying not being something a child row can be checked
    /// against.
    ReferencedByForeignKey,
    /// Core rejected or failed to execute the translated statement.
    Engine(LimboError),
}

impl std::fmt::Display for MySqlTruncateTableError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingTable => formatter.write_str("unknown table"),
            Self::ReferencedByForeignKey => formatter
                .write_str("cannot truncate a table referenced in a foreign key constraint"),
            Self::Engine(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for MySqlTruncateTableError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Engine(error) => Some(error),
            Self::MissingTable | Self::ReferencedByForeignKey => None,
        }
    }
}
