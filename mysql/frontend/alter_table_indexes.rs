use turso_core::LimboError;

/// Failure while adding or dropping indexes with one `ALTER TABLE`.
#[derive(Debug)]
pub enum MySqlAlterTableIndexError {
    /// The statement named no stored base table.
    MissingTable,
    /// A `DROP INDEX` named an index the table does not carry.
    MissingIndex,
    /// An `ADD INDEX` named an index the table already carries.
    DuplicateIndex,
    /// Core rejected or failed to execute one of the translated statements.
    Engine(LimboError),
}

impl std::fmt::Display for MySqlAlterTableIndexError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingTable => formatter.write_str("unknown table"),
            Self::MissingIndex => formatter.write_str("unknown index"),
            Self::DuplicateIndex => formatter.write_str("duplicate key name"),
            Self::Engine(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for MySqlAlterTableIndexError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Engine(error) => Some(error),
            Self::MissingTable | Self::MissingIndex | Self::DuplicateIndex => None,
        }
    }
}
