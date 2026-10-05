use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;

use turso_core::io::FileSyncType;
use turso_core::storage::auto_increment::{
    AllocatorDatabaseIdentity, AllocatorOpenMode, AutoIncrementKey, DurableRangeAllocator,
};
use turso_core::{
    Connection, Database, DatabaseOpts, IOExt as _, MvccCheckpointWritingRowsFirst, OpenFlags,
    SqliteDialect, IO,
};

use crate::unreliable_io::UnreliableIo;

const COUNTER_DATABASE: [u8; 16] = *b"rows-first-db-01";
const COUNTER_KEY: [u8; 16] = *b"rows-first-key-1";

#[test]
fn a_power_loss_while_rows_wait_for_the_lock_keeps_every_committed_row() -> anyhow::Result<()> {
    let names = Names::new("rows-first-before-lock");
    let io = Arc::new(UnreliableIo::new());
    let db = open_mvcc(io.clone(), &names.database)?;
    let mut rows = Rows::create(&db.connect()?)?;
    let writer = db.connect()?;
    rows.change_some(&writer, 0)?;
    let checkpoint = db.connect()?.begin_mvcc_checkpoint_writing_rows_first()?;
    assert!(checkpoint.rows_written() > 0);
    rows.change_some(&writer, 1)?;

    let recovered = recover(&io.durable_files(), &names)?;
    rows.assert_held_by(&recovered.connect()?)?;
    drop(checkpoint);
    rows.assert_held_by(&writer)?;
    Ok(())
}

#[test]
fn a_power_loss_at_any_sync_of_the_finishing_checkpoint_keeps_every_committed_row(
) -> anyhow::Result<()> {
    for (case, crashed_file) in ["-wal", "", "-log"].into_iter().enumerate() {
        let names = Names::new(&format!("rows-first-finish-{case}"));
        let io = Arc::new(UnreliableIo::new());
        let db = open_mvcc(io.clone(), &names.database)?;
        let mut rows = Rows::create(&db.connect()?)?;
        let writer = db.connect()?;
        rows.change_some(&writer, 0)?;
        let mut checkpoint = db.connect()?.begin_mvcc_checkpoint_writing_rows_first()?;
        rows.change_some(&writer, 1)?;
        checkpoint.write_the_rows_committed_since()?;
        rows.change_some(&writer, 2)?;

        io.arm_crash_on_sync(&format!("{}{crashed_file}", names.database));
        finish(&mut checkpoint)?;
        let snapshot = io
            .take_crash_snapshot()
            .unwrap_or_else(|| panic!("the checkpoint never synced {crashed_file:?}"));
        let recovered = recover(&snapshot.files, &names)?;
        rows.assert_held_by(&recovered.connect()?)?;
        rows.assert_held_by(&writer)?;
    }
    Ok(())
}

#[test]
fn a_number_taken_while_rows_wait_for_the_lock_is_not_taken_again_after_a_power_loss(
) -> anyhow::Result<()> {
    let names = Names::new("rows-first-counter");
    let io = Arc::new(UnreliableIo::new());
    let db = open_mvcc(io.clone(), &names.database)?;
    let counter = open_counter(io.as_ref(), &names.sidecar, AllocatorOpenMode::Create)?;
    let mut initialize = counter.initialize()?;
    io.block(|| initialize.step())?;
    db.get_mv_store()
        .as_ref()
        .expect("the database is in MVCC")
        .log_counter_marks_in_commits(&counter, io.as_ref())?;
    let conn = db.connect()?;
    conn.execute("CREATE TABLE c (id INTEGER PRIMARY KEY, v TEXT)")?;
    let first = reserve(io.as_ref(), &counter, 1)?;
    conn.execute(format!("INSERT INTO c VALUES ({first}, 'before')"))?;
    let mut checkpoint = db.connect()?.begin_mvcc_checkpoint_writing_rows_first()?;
    let second = reserve(io.as_ref(), &counter, 1)?;
    conn.execute(format!("INSERT INTO c VALUES ({second}, 'while waiting')"))?;
    finish(&mut checkpoint)?;
    drop(checkpoint);

    let recovered = recover(&io.durable_files(), &names)?;
    let io = Arc::new(turso_core::PlatformIO::new()?);
    let db = &recovered.db;
    let counter = open_counter(
        io.as_ref(),
        &recovered.names.sidecar,
        AllocatorOpenMode::Reopen,
    )?;
    db.get_mv_store()
        .as_ref()
        .expect("the database is in MVCC")
        .log_counter_marks_in_commits(&counter, io.as_ref())?;
    assert_eq!(reserve(io.as_ref(), &counter, 1)?, second + 1);
    assert_eq!(
        query(&db.connect()?, "SELECT id FROM c ORDER BY id")?,
        vec![first.to_string(), second.to_string()]
    );
    Ok(())
}

/// Writers move amounts between accounts, readers keep snapshots open
/// across statements and one moves its snapshot forward between them, while
/// checkpoints write rows before holding new transactions back, again and
/// again. Every snapshot must add up to the same total.
#[test]
fn checkpoints_writing_rows_first_beside_writers_and_long_readers_keep_every_snapshot_whole(
) -> anyhow::Result<()> {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    const ACCOUNTS: i64 = 200;
    const TOTAL: i64 = ACCOUNTS * 1000;
    let directory = tempfile::TempDir::new()?;
    let path = directory.path().join("transfers.db");
    let path = path
        .to_str()
        .expect("the temporary path is UTF-8")
        .to_string();
    let db = open_mvcc(Arc::new(turso_core::PlatformIO::new()?), &path)?;
    let store = db.get_mv_store().clone().expect("the database is in MVCC");
    store.leave_checkpoints_to_the_caller();
    let setup = db.connect()?;
    setup.execute("CREATE TABLE acct (id INTEGER PRIMARY KEY, bal INTEGER, tag INTEGER)")?;
    setup.execute("CREATE INDEX acct_tag ON acct (tag)")?;
    setup.execute("BEGIN")?;
    for id in 0..ACCOUNTS {
        setup.execute(format!("INSERT INTO acct VALUES ({id}, 1000, {id})"))?;
    }
    setup.execute("COMMIT")?;

    let stop = Arc::new(AtomicBool::new(false));
    let transfers = Arc::new(AtomicUsize::new(0));
    let snapshots_read = Arc::new(AtomicUsize::new(0));
    let connect = |db: &Arc<Database>| -> anyhow::Result<Arc<Connection>> {
        let conn = db.connect()?;
        conn.set_busy_timeout(Duration::from_secs(30));
        Ok(conn)
    };
    let mut workers = Vec::new();
    for writer in 0..4i64 {
        let conn = connect(&db)?;
        let (stop, transfers) = (stop.clone(), transfers.clone());
        workers.push(std::thread::spawn(move || -> anyhow::Result<()> {
            let mut n = writer;
            while !stop.load(Ordering::SeqCst) {
                n = (n * 7919 + 13) % 1_000_003;
                let (from, to) = (n % ACCOUNTS, (n / ACCOUNTS) % ACCOUNTS);
                if from == to {
                    continue;
                }
                let moved = (|| -> turso_core::Result<()> {
                    conn.execute("BEGIN CONCURRENT")?;
                    conn.execute(format!(
                        "UPDATE acct SET bal = bal - 7, tag = {n} WHERE id = {from}"
                    ))?;
                    conn.execute(format!(
                        "UPDATE acct SET bal = bal + 7, tag = {} WHERE id = {to}",
                        n + 1
                    ))?;
                    conn.execute("COMMIT")
                })();
                match moved {
                    Ok(()) => {
                        transfers.fetch_add(1, Ordering::SeqCst);
                    }
                    Err(_) => {
                        let _ = conn.execute("ROLLBACK");
                    }
                }
            }
            Ok(())
        }));
    }
    for reader in 0..3u64 {
        let conn = connect(&db)?;
        let (stop, snapshots_read) = (stop.clone(), snapshots_read.clone());
        workers.push(std::thread::spawn(move || -> anyhow::Result<()> {
            while !stop.load(Ordering::SeqCst) {
                conn.execute("BEGIN CONCURRENT")?;
                for _ in 0..5 {
                    if reader == 2 {
                        conn.refresh_read_snapshot()?;
                    }
                    assert_eq!(
                        query(&conn, "SELECT sum(bal) FROM acct")?,
                        vec![TOTAL.to_string()]
                    );
                    std::thread::sleep(Duration::from_millis(2 * reader));
                    assert_eq!(
                        query(
                            &conn,
                            "SELECT sum(bal), count(*) FROM acct INDEXED BY acct_tag WHERE tag >= 0"
                        )?,
                        vec![format!("{TOTAL}|{ACCOUNTS}")]
                    );
                }
                conn.execute("COMMIT")?;
                snapshots_read.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        }));
    }

    let checkpointing = db.connect()?;
    let started = Instant::now();
    let mut checkpoints = 0;
    let mut rows_written_first = 0;
    while started.elapsed() < Duration::from_secs(4) {
        std::thread::sleep(Duration::from_millis(20));
        let mut checkpoint = match checkpointing.begin_mvcc_checkpoint_writing_rows_first() {
            Ok(checkpoint) => checkpoint,
            Err(turso_core::LimboError::Busy) => continue,
            Err(error) => return Err(error.into()),
        };
        checkpoint.write_the_rows_committed_since()?;
        rows_written_first += checkpoint.rows_written();
        store.hold_new_transactions();
        let held_until = Instant::now() + Duration::from_millis(200);
        let finished = loop {
            if checkpoint.finish_once_no_transaction_runs()?.is_some() {
                break true;
            }
            if Instant::now() >= held_until {
                break false;
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        store.let_new_transactions_begin();
        drop(checkpoint);
        if finished {
            store.sweep_what_the_checkpoint_left();
            checkpoints += 1;
        }
    }
    stop.store(true, Ordering::SeqCst);
    for worker in workers {
        worker.join().expect("a worker panicked")?;
    }
    assert!(checkpoints >= 5, "only {checkpoints} checkpoints finished");
    assert!(rows_written_first > 0);
    assert!(transfers.load(Ordering::SeqCst) > 100);
    assert!(snapshots_read.load(Ordering::SeqCst) > 10);

    let check = |conn: &Arc<Connection>| -> anyhow::Result<()> {
        assert_eq!(
            query(conn, "SELECT sum(bal), count(*) FROM acct")?,
            vec![format!("{TOTAL}|{ACCOUNTS}")]
        );
        assert_eq!(
            query(
                conn,
                "SELECT sum(bal), count(*) FROM acct INDEXED BY acct_tag WHERE tag >= 0"
            )?,
            vec![format!("{TOTAL}|{ACCOUNTS}")]
        );
        assert_eq!(
            query(conn, "PRAGMA integrity_check")?,
            vec!["ok".to_string()]
        );
        Ok(())
    };
    check(&setup)?;
    drop((setup, checkpointing, store));
    drop(db);
    let reopened = open_mvcc(Arc::new(turso_core::PlatformIO::new()?), &path)?;
    check(&reopened.connect()?)?;
    Ok(())
}

/// Rows of `t (id INTEGER PRIMARY KEY, k INTEGER)`, indexed on `k`, with the
/// contents every committed change gave them.
struct Rows {
    expected: BTreeMap<i64, i64>,
}

impl Rows {
    fn create(conn: &Arc<Connection>) -> anyhow::Result<Self> {
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, k INTEGER, pad TEXT)")?;
        conn.execute("CREATE INDEX t_k ON t (k)")?;
        conn.execute("BEGIN")?;
        for id in 0..300 {
            conn.execute(format!(
                "INSERT INTO t VALUES ({id}, {id}, '{}')",
                "p".repeat(100)
            ))?;
        }
        conn.execute("COMMIT")?;
        conn.execute("PRAGMA wal_checkpoint(TRUNCATE)")?;
        Ok(Self {
            expected: (0..300).map(|id| (id, id)).collect(),
        })
    }

    /// Updates, deletes and inserts rows, some of them rows an earlier
    /// round changed, in transactions of their own.
    fn change_some(&mut self, conn: &Arc<Connection>, round: i64) -> anyhow::Result<()> {
        for id in (round..300).step_by(7) {
            let k = 1000 * (round + 1) + id;
            conn.execute(format!("UPDATE t SET k = {k} WHERE id = {id}"))?;
            if let Some(kept) = self.expected.get_mut(&id) {
                *kept = k;
            }
        }
        for id in (round * 3..300).step_by(11) {
            conn.execute(format!("DELETE FROM t WHERE id = {id}"))?;
            self.expected.remove(&id);
        }
        for n in 0..20 {
            let id = 300 + round * 20 + n;
            let k = -id;
            conn.execute(format!("INSERT INTO t VALUES ({id}, {k}, 'new')"))?;
            self.expected.insert(id, k);
        }
        if round > 0 {
            let id = 300 + (round - 1) * 20;
            conn.execute(format!("DELETE FROM t WHERE id = {id}"))?;
            self.expected.remove(&id);
        }
        Ok(())
    }

    fn assert_held_by(&self, conn: &Arc<Connection>) -> anyhow::Result<()> {
        let expected: Vec<String> = self
            .expected
            .iter()
            .map(|(id, k)| format!("{id}|{k}"))
            .collect();
        assert_eq!(query(conn, "SELECT id, k FROM t ORDER BY id")?, expected);
        assert_eq!(
            query(conn, "SELECT id, k FROM t INDEXED BY t_k ORDER BY id")?,
            expected
        );
        for (id, k) in self.expected.iter().step_by(13) {
            assert_eq!(
                query(conn, &format!("SELECT id FROM t WHERE k = {k}"))?,
                vec![id.to_string()]
            );
        }
        assert_eq!(
            query(conn, "PRAGMA integrity_check")?,
            vec!["ok".to_string()]
        );
        Ok(())
    }
}

fn finish(checkpoint: &mut MvccCheckpointWritingRowsFirst) -> anyhow::Result<()> {
    assert!(
        checkpoint.finish_once_no_transaction_runs()?.is_some(),
        "no transaction runs, so the checkpoint finishes"
    );
    Ok(())
}

struct Names {
    database: String,
    sidecar: String,
}

impl Names {
    fn new(stem: &str) -> Self {
        Self {
            database: format!("{stem}.db"),
            sidecar: format!("{stem}.counter"),
        }
    }
}

struct Recovered {
    _directory: tempfile::TempDir,
    names: Names,
    db: Arc<Database>,
}

impl Recovered {
    fn connect(&self) -> anyhow::Result<Arc<Connection>> {
        Ok(self.db.connect()?)
    }
}

/// Opens, from a fresh process's point of view, the files a power loss left.
fn recover(files: &HashMap<String, Vec<u8>>, names: &Names) -> anyhow::Result<Recovered> {
    let directory = tempfile::TempDir::new()?;
    let stem = directory.path().join("after");
    let after = Names::new(stem.to_str().expect("the temporary path is UTF-8"));
    for (name, bytes) in files {
        let renamed = if let Some(suffix) = name.strip_prefix(&names.database) {
            format!("{}{suffix}", after.database)
        } else if *name == names.sidecar {
            after.sidecar.clone()
        } else {
            continue;
        };
        std::fs::write(Path::new(&renamed), bytes)?;
    }
    let db = open_mvcc(Arc::new(turso_core::PlatformIO::new()?), &after.database)?;
    Ok(Recovered {
        _directory: directory,
        names: after,
        db,
    })
}

fn open_mvcc(io: Arc<dyn IO>, path: &str) -> anyhow::Result<Arc<Database>> {
    let db = Database::open_file_with_flags(
        io,
        path,
        OpenFlags::default(),
        DatabaseOpts::new(),
        None,
        Arc::new(SqliteDialect),
    )?;
    if !db.mvcc_enabled() {
        db.connect()?.pragma_update("journal_mode", "'mvcc'")?;
    }
    assert!(db.mvcc_enabled());
    Ok(db)
}

fn open_counter(
    io: &dyn IO,
    path: &str,
    mode: AllocatorOpenMode,
) -> anyhow::Result<DurableRangeAllocator> {
    Ok(DurableRangeAllocator::open(
        io,
        path,
        AllocatorDatabaseIdentity::new(COUNTER_DATABASE)?,
        mode,
        FileSyncType::Fsync,
    )?)
}

fn reserve(io: &dyn IO, counter: &DurableRangeAllocator, count: u64) -> anyhow::Result<u64> {
    let mut reservation = counter.reserve(AutoIncrementKey::new(COUNTER_KEY)?, count)?;
    Ok(io.block(|| reservation.step())?.last())
}

fn query(conn: &Arc<Connection>, sql: &str) -> anyhow::Result<Vec<String>> {
    let mut statement = conn.prepare(sql)?;
    let mut rows = Vec::new();
    statement.run_with_row_callback(|row| {
        let values: Vec<String> = row.get_values().map(|value| format!("{value}")).collect();
        rows.push(values.join("|"));
        Ok(())
    })?;
    Ok(rows)
}
