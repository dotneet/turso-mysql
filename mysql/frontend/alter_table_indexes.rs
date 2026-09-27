use turso_core::LimboError;

/// Failure while adding or dropping indexes with one `ALTER TABLE`.
#[derive(Debug)]
pub enum MySqlAlterTableIndexError {
    /// The statement named no stored base table.
    MissingTable,
    /// A `DROP INDEX` named an index the table does not carry.
    MissingIndex,
    /// A `RENAME INDEX` named an index the table does not carry, which MySQL
    /// answers with an error of its own rather than the one a `DROP INDEX`
    /// gets.
    MissingIndexToRename,
    /// An `ADD INDEX` named an index the table already carries.
    DuplicateIndex,
    /// A `RENAME INDEX` named an index the engine made for a `UNIQUE` written
    /// on a column, which has no statement of its own to write again under
    /// another name.
    RenamingAColumnsOwnKey,
    /// An index names a JSON column directly.
    JsonIndex,
    /// Dropping this index would leave a child foreign key without a leading index.
    RequiredByForeignKey,
    /// Core rejected or failed to execute one of the translated statements.
    Engine(LimboError),
}

impl std::fmt::Display for MySqlAlterTableIndexError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingTable => formatter.write_str("unknown table"),
            Self::MissingIndex | Self::MissingIndexToRename => formatter.write_str("unknown index"),
            Self::DuplicateIndex => formatter.write_str("duplicate key name"),
            Self::RenamingAColumnsOwnKey => {
                formatter.write_str("renaming the key a column declares for itself")
            }
            Self::JsonIndex => {
                formatter.write_str("JSON column supports indexing only via generated columns")
            }
            Self::RequiredByForeignKey => {
                formatter.write_str("cannot drop index needed by a foreign key")
            }
            Self::Engine(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for MySqlAlterTableIndexError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Engine(error) => Some(error),
            Self::MissingTable
            | Self::MissingIndex
            | Self::MissingIndexToRename
            | Self::DuplicateIndex
            | Self::RenamingAColumnsOwnKey
            | Self::JsonIndex
            | Self::RequiredByForeignKey => None,
        }
    }
}
