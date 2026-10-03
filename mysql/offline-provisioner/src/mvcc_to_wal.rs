// Copyright 2026 the Turso authors. All rights reserved. MIT license.

//! Offline switch of one MySQL database from MVCC back to WAL.

#[cfg(unix)]
use std::{fmt, path::PathBuf, process::ExitCode};

#[cfg(unix)]
use clap::{error::ErrorKind, Parser};
#[cfg(unix)]
use turso_mysql::{
    convert_database_from_mvcc_to_wal, journal_mode_from_environment, JournalMode,
    JournalModeError, MySqlDatabaseError, MySqlMvccToWal, MySqlMvccToWalError,
};

#[cfg(unix)]
#[derive(Debug, Parser)]
#[command(name = "turso-mysql-offline-mvcc-to-wal")]
#[command(
    about = "Turn a database that was opened in MVCC back to WAL while the server is stopped",
    after_help = "Run it, and the server afterwards, with TURSO_MYSQL_JOURNAL_MODE=wal: \
                  a server started without it opens the database in MVCC again."
)]
struct Arguments {
    /// Existing private directory holding MySQL database data.
    #[arg(long)]
    data_root: PathBuf,

    /// Name of the database to turn back to WAL.
    #[arg(long)]
    database: String,
}

#[cfg(unix)]
fn main() -> ExitCode {
    let arguments = match Arguments::try_parse() {
        Ok(arguments) => arguments,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            let _ = error.print();
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            let _ = error.print();
            return ExitCode::from(CommandError::Input.exit_code());
        }
    };

    match run(&arguments, journal_mode_from_environment()) {
        Ok(MySqlMvccToWal::Converted) => {
            println!("database {} now opens in WAL", arguments.database);
            ExitCode::SUCCESS
        }
        Ok(MySqlMvccToWal::AlreadyWal) => {
            println!("database {} already opens in WAL", arguments.database);
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(error.exit_code())
        }
    }
}

#[cfg(not(unix))]
fn main() -> std::process::ExitCode {
    eprintln!("turso-mysql-offline-mvcc-to-wal is unsupported on this platform");
    std::process::ExitCode::from(3)
}

#[cfg(unix)]
fn run(
    arguments: &Arguments,
    journal_mode: Result<JournalMode, JournalModeError>,
) -> Result<MySqlMvccToWal, CommandError> {
    match journal_mode {
        Err(error) => return Err(CommandError::JournalMode(error)),
        Ok(JournalMode::Mvcc) => return Err(CommandError::MvccSelected),
        Ok(JournalMode::Wal) => {}
    }
    convert_database_from_mvcc_to_wal(&arguments.data_root, &arguments.database).map_err(|error| {
        match error {
            MySqlMvccToWalError::DataRootInUse => CommandError::DataRootInUse,
            MySqlMvccToWalError::Database(
                MySqlDatabaseError::InvalidDatabaseName | MySqlDatabaseError::DatabaseNotFound(_),
            ) => CommandError::Input,
            MySqlMvccToWalError::Database(error) => CommandError::Database(error),
        }
    })
}

#[cfg(unix)]
#[derive(Clone, Debug, PartialEq, Eq)]
enum CommandError {
    Input,
    JournalMode(JournalModeError),
    MvccSelected,
    DataRootInUse,
    Database(MySqlDatabaseError),
}

#[cfg(unix)]
impl CommandError {
    const fn exit_code(&self) -> u8 {
        match self {
            Self::Input | Self::JournalMode(_) | Self::MvccSelected => 2,
            Self::DataRootInUse | Self::Database(_) => 3,
        }
    }
}

#[cfg(unix)]
impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Input => f.write_str("the data root or the database name is invalid"),
            Self::JournalMode(error) => write!(f, "{error}"),
            Self::MvccSelected => f.write_str(
                "databases open in MVCC unless TURSO_MYSQL_JOURNAL_MODE=wal is set; set it here \
                 and for the server, or the server opens the database in MVCC again",
            ),
            Self::DataRootInUse => f.write_str(
                "the data root is open in another process; stop the server and run this again",
            ),
            Self::Database(error) => write!(f, "the database was not turned back to WAL: {error}"),
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use clap::Parser;
    use turso_mysql::{JournalMode, JournalModeError, MySqlDatabaseCatalog};

    use super::{run, Arguments, CommandError};

    fn arguments(data_root: &std::path::Path, database: &str) -> Arguments {
        Arguments::try_parse_from([
            "turso-mysql-offline-mvcc-to-wal",
            "--data-root",
            data_root.to_str().unwrap(),
            "--database",
            database,
        ])
        .unwrap()
    }

    fn private_data_root() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        directory
    }

    #[test]
    fn the_data_root_and_the_database_are_both_required() {
        for missing in [
            ["--data-root", "/var/lib/turso/data"],
            ["--database", "reports"],
        ] {
            let mut given = vec!["turso-mysql-offline-mvcc-to-wal"];
            given.extend(missing);
            assert!(Arguments::try_parse_from(given).is_err());
        }
    }

    #[test]
    fn nothing_is_touched_unless_wal_is_selected() {
        let directory = private_data_root();
        for (journal_mode, refusal) in [
            (Ok(JournalMode::Mvcc), CommandError::MvccSelected),
            (
                Err(JournalModeError::UnknownMode),
                CommandError::JournalMode(JournalModeError::UnknownMode),
            ),
            (
                Err(JournalModeError::RemovedMvccVariable),
                CommandError::JournalMode(JournalModeError::RemovedMvccVariable),
            ),
        ] {
            let outcome = run(&arguments(directory.path(), "reports"), journal_mode);
            assert_eq!(outcome, Err(refusal));
            assert_eq!(outcome.unwrap_err().exit_code(), 2);
        }
        assert!(CommandError::MvccSelected
            .to_string()
            .contains("TURSO_MYSQL_JOURNAL_MODE=wal"));
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_running_server_keeps_the_database_as_it_is() {
        let directory = private_data_root();
        let catalog = MySqlDatabaseCatalog::open(directory.path()).unwrap();
        catalog.create("reports").unwrap();
        let outcome = run(
            &arguments(directory.path(), "reports"),
            Ok(JournalMode::Wal),
        );
        assert_eq!(outcome, Err(CommandError::DataRootInUse));
        assert_eq!(outcome.unwrap_err().exit_code(), 3);
        drop(catalog);

        assert_eq!(
            run(
                &arguments(directory.path(), "missing"),
                Ok(JournalMode::Wal)
            ),
            Err(CommandError::Input)
        );
    }
}
