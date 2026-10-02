use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

use turso_core::{Connection, Database, LimboError, PlatformIO, SqliteDialect, Value, IO};

use crate::unreliable_io::{SyncFailure, UnreliableIo};

const BOTH_FAILURES: [SyncFailure; 2] = [
    SyncFailure::ReturnedBySync,
    SyncFailure::ReportedByCompletion,
];

#[test]
fn a_commit_whose_wal_sync_fails_panics_when_data_sync_retry_is_off() {
    for failure in BOTH_FAILURES {
        let scenario = Scenario::new(JournalMode::Wal);
        scenario.execute("INSERT INTO t VALUES (1)");

        scenario.fail_next_sync(&scenario.wal_path(), failure);

        assert!(
            scenario.panics("INSERT INTO t VALUES (2)"),
            "{failure:?}: a failed WAL sync must stop the process"
        );
        assert_eq!(scenario.rows_after_power_loss(), vec![1], "{failure:?}");
    }
}

#[test]
fn with_data_sync_retry_a_commit_whose_wal_sync_fails_answers_an_error_and_the_next_commit_is_durable(
) {
    for failure in BOTH_FAILURES {
        let scenario = Scenario::new(JournalMode::Wal);
        scenario.execute("PRAGMA data_sync_retry = 1");
        scenario.execute("INSERT INTO t VALUES (1)");

        scenario.fail_next_sync(&scenario.wal_path(), failure);

        assert_io_error(scenario.try_execute("INSERT INTO t VALUES (2)"), failure);
        scenario.execute("INSERT INTO t VALUES (3)");
        assert_eq!(scenario.rows(), vec![1, 3], "{failure:?}");
        assert_eq!(scenario.rows_after_power_loss(), vec![1, 3], "{failure:?}");
    }
}

#[test]
fn a_checkpoint_whose_database_sync_fails_panics_when_data_sync_retry_is_off() {
    for mode in ["PASSIVE", "FULL", "RESTART", "TRUNCATE"] {
        for failure in BOTH_FAILURES {
            let scenario = Scenario::new(JournalMode::Wal);
            scenario.insert_rows(1..=3);

            scenario.fail_next_sync(&scenario.database, failure);

            assert!(
                scenario.panics(&format!("PRAGMA wal_checkpoint({mode})")),
                "{mode} {failure:?}: a failed database sync must stop the process"
            );
            assert_eq!(
                scenario.rows_after_power_loss(),
                vec![1, 2, 3],
                "{mode} {failure:?}"
            );
        }
    }
}

#[test]
fn with_data_sync_retry_the_next_checkpoint_writes_again_what_a_failed_passive_checkpoint_wrote() {
    for mode in ["PASSIVE", "FULL"] {
        for failure in BOTH_FAILURES {
            let scenario = Scenario::new(JournalMode::Wal);
            scenario.execute("PRAGMA data_sync_retry = 1");
            scenario.insert_rows(1..=3);

            scenario.fail_next_sync(&scenario.database, failure);

            assert!(
                !scenario.checkpoint_succeeds(mode),
                "{mode} {failure:?}: a checkpoint whose sync failed must not report success"
            );
            assert!(
                scenario.checkpoint_succeeds("TRUNCATE"),
                "{mode} {failure:?}"
            );
            assert_eq!(
                scenario.rows_after_power_loss(),
                vec![1, 2, 3],
                "{mode} {failure:?}"
            );
        }
    }
}

#[test]
fn a_checkpoint_that_restarts_the_wal_panics_when_its_database_sync_fails_even_with_data_sync_retry(
) {
    for mode in ["RESTART", "TRUNCATE"] {
        for failure in BOTH_FAILURES {
            let scenario = Scenario::new(JournalMode::Wal);
            scenario.execute("PRAGMA data_sync_retry = 1");
            scenario.insert_rows(1..=3);

            scenario.fail_next_sync(&scenario.database, failure);

            assert!(
                scenario.panics(&format!("PRAGMA wal_checkpoint({mode})")),
                "{mode} {failure:?}: the WAL was restarted, so its frames are no longer kept"
            );
            assert_eq!(
                scenario.rows_after_power_loss(),
                vec![1, 2, 3],
                "{mode} {failure:?}"
            );
        }
    }
}

#[test]
fn an_mvcc_commit_whose_logical_log_sync_fails_panics_even_with_data_sync_retry() {
    for data_sync_retry in [false, true] {
        for failure in BOTH_FAILURES {
            let scenario = Scenario::new(JournalMode::Mvcc);
            scenario.set_data_sync_retry(data_sync_retry);
            scenario.execute("INSERT INTO t VALUES (1)");

            scenario.fail_next_sync(&scenario.log_path(), failure);

            assert!(
                scenario.panics("INSERT INTO t VALUES (2)"),
                "data_sync_retry={data_sync_retry} {failure:?}: the record is already in the log, \
                 so the commit can neither be undone nor retried"
            );
            assert_eq!(
                scenario.rows_after_power_loss(),
                vec![1],
                "data_sync_retry={data_sync_retry} {failure:?}"
            );
        }
    }
}

#[test]
fn an_mvcc_checkpoint_whose_database_sync_fails_panics_even_with_data_sync_retry() {
    for data_sync_retry in [false, true] {
        for failure in BOTH_FAILURES {
            let scenario = Scenario::new(JournalMode::Mvcc);
            scenario.set_data_sync_retry(data_sync_retry);
            scenario.insert_rows(1..=3);

            scenario.fail_next_sync(&scenario.database, failure);

            assert!(
                scenario.panics("PRAGMA wal_checkpoint(TRUNCATE)"),
                "data_sync_retry={data_sync_retry} {failure:?}: the WAL was restarted, \
                 so its frames are no longer kept"
            );
            assert_eq!(
                scenario.rows_after_power_loss(),
                vec![1, 2, 3],
                "data_sync_retry={data_sync_retry} {failure:?}"
            );
        }
    }
}

fn assert_io_error(result: turso_core::Result<()>, failure: SyncFailure) {
    assert!(
        matches!(result, Err(LimboError::CompletionError(_))),
        "{failure:?}: expected an I/O error, got {result:?}"
    );
}

#[derive(Clone, Copy)]
enum JournalMode {
    Wal,
    Mvcc,
}

struct Scenario {
    _directory: tempfile::TempDir,
    io: Arc<UnreliableIo>,
    database: String,
    db: Arc<Database>,
    conn: Arc<Connection>,
}

impl Scenario {
    fn new(journal_mode: JournalMode) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let database = directory
            .path()
            .join("failed-sync.db")
            .to_str()
            .unwrap()
            .to_string();
        let io = Arc::new(UnreliableIo::new());
        let db = Database::open_file(io.clone(), &database, Arc::new(SqliteDialect)).unwrap();
        let conn = db.connect().unwrap();
        if let JournalMode::Mvcc = journal_mode {
            conn.pragma_update("journal_mode", "'mvcc'").unwrap();
            assert!(db.mvcc_enabled());
        }
        conn.execute("CREATE TABLE t (x INTEGER PRIMARY KEY)")
            .unwrap();
        Self {
            _directory: directory,
            io,
            database,
            db,
            conn,
        }
    }

    fn wal_path(&self) -> String {
        format!("{}-wal", self.database)
    }

    fn log_path(&self) -> String {
        format!("{}-log", self.database)
    }

    fn set_data_sync_retry(&self, data_sync_retry: bool) {
        self.execute(&format!(
            "PRAGMA data_sync_retry = {}",
            i64::from(data_sync_retry)
        ));
    }

    fn fail_next_sync(&self, path: &str, failure: SyncFailure) {
        self.io.fail_next_sync(path, failure);
    }

    fn execute(&self, sql: &str) {
        self.conn.execute(sql).unwrap();
    }

    fn try_execute(&self, sql: &str) -> turso_core::Result<()> {
        let result = self.conn.execute(sql);
        assert!(
            !self.io.a_sync_failure_is_still_waiting(),
            "{sql} did not sync the file the failure was set for"
        );
        result
    }

    fn insert_rows(&self, values: std::ops::RangeInclusive<i64>) {
        for value in values {
            self.execute(&format!("INSERT INTO t VALUES ({value})"));
        }
    }

    fn panics(&self, sql: &str) -> bool {
        let panicked = catch_unwind(AssertUnwindSafe(|| self.conn.execute(sql))).is_err();
        assert!(
            !self.io.a_sync_failure_is_still_waiting(),
            "{sql} did not sync the file the failure was set for"
        );
        panicked
    }

    fn checkpoint_succeeds(&self, mode: &str) -> bool {
        let rows = self.conn.pragma_query(&format!("wal_checkpoint({mode})"));
        assert!(
            !self.io.a_sync_failure_is_still_waiting(),
            "the {mode} checkpoint did not sync the file the failure was set for"
        );
        match rows {
            Ok(rows) => rows[0][0] == Value::from_i64(0),
            Err(_) => false,
        }
    }

    fn rows(&self) -> Vec<i64> {
        rows_of(&self.conn)
    }

    fn rows_after_power_loss(self) -> Vec<i64> {
        let durable = self.io.durable_files();
        std::mem::forget(self.conn);
        std::mem::forget(self.db);
        let directory = tempfile::tempdir().unwrap();
        let reopened = directory.path().join("reopened.db");
        let reopened = reopened.to_str().unwrap();
        for (path, bytes) in durable {
            if let Some(suffix) = path.strip_prefix(&self.database) {
                std::fs::write(format!("{reopened}{suffix}"), bytes).unwrap();
            }
        }
        let io: Arc<dyn IO> = Arc::new(PlatformIO::new().unwrap());
        let db = Database::open_file(io, reopened, Arc::new(SqliteDialect)).unwrap();
        let conn = db.connect().unwrap();
        assert_eq!(
            conn.pragma_query("integrity_check").unwrap(),
            vec![vec![Value::build_text("ok")]]
        );
        rows_of(&conn)
    }
}

fn rows_of(conn: &Arc<Connection>) -> Vec<i64> {
    let mut statement = conn.prepare("SELECT x FROM t ORDER BY x").unwrap();
    let mut rows = Vec::new();
    statement
        .run_with_row_callback(|row| {
            rows.push(row.get::<i64>(0)?);
            Ok(())
        })
        .unwrap();
    rows
}
