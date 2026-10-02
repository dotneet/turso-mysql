use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use turso_core::io::FileSyncType;
use turso_core::storage::auto_increment::{
    AllocatorDatabaseIdentity, AllocatorOpenMode, AutoIncrementKey, DurableRangeAllocator,
};
use turso_core::{Database, DatabaseOpts, IOExt as _, LimboError, OpenFlags, SqliteDialect, IO};

use crate::unreliable_io::UnreliableIo;

const DATABASE: [u8; 16] = *b"counter-marks-db";
const KEY_T: [u8; 16] = *b"counter-table-t1";
const KEY_U: [u8; 16] = *b"counter-table-u1";

#[test]
fn a_committed_number_is_not_taken_again_after_a_power_loss_drops_the_sidecar_records(
) -> anyhow::Result<()> {
    let names = Names::new("carried-mark-committed");
    let io = Arc::new(UnreliableIo::new());
    let (db, counter) = open_mvcc_with_counter(io.clone(), &names, AllocatorOpenMode::Create)?;
    let conn = db.connect()?;
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")?;

    assert_eq!(reserve(io.as_ref(), &counter, KEY_T, 3)?, 3);
    assert!(
        io.has_unsynced_writes(&names.sidecar),
        "the sidecar synced the record itself"
    );
    conn.execute("INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c')")?;
    conn.execute("DELETE FROM t WHERE id = 3")?;

    let recovered = recover_after_power_loss(io.as_ref(), &names)?;
    let after = recovered.path("after");
    let io = Arc::new(PlatformIo::new()?);
    let lost_sidecar = open_counter(io.as_ref(), &after, AllocatorOpenMode::Reopen)?;
    assert_eq!(peek(io.as_ref(), &lost_sidecar, KEY_T)?, 0);
    drop(lost_sidecar);

    let (_db, counter) = open_mvcc_with_counter(io.clone(), &after, AllocatorOpenMode::Reopen)?;
    assert_eq!(reserve(io.as_ref(), &counter, KEY_T, 1)?, 4);
    Ok(())
}

#[test]
fn a_checkpoint_syncs_the_sidecar_before_the_log_drops_the_logged_marks() -> anyhow::Result<()> {
    let names = Names::new("carried-mark-checkpoint");
    let io = Arc::new(UnreliableIo::new());
    let (db, counter) = open_mvcc_with_counter(io.clone(), &names, AllocatorOpenMode::Create)?;
    let conn = db.connect()?;
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")?;
    assert_eq!(reserve(io.as_ref(), &counter, KEY_T, 2)?, 2);
    conn.execute("INSERT INTO t VALUES (1, 'a'), (2, 'b')")?;
    conn.execute("DELETE FROM t WHERE id = 2")?;
    conn.execute("PRAGMA wal_checkpoint(TRUNCATE)")?;
    assert!(!io.has_unsynced_writes(&names.sidecar));

    let recovered = recover_after_power_loss(io.as_ref(), &names)?;
    let io = Arc::new(PlatformIo::new()?);
    let after = recovered.path("after");
    let (_db, counter) = open_mvcc_with_counter(io.clone(), &after, AllocatorOpenMode::Reopen)?;
    assert_eq!(reserve(io.as_ref(), &counter, KEY_T, 1)?, 3);
    Ok(())
}

#[test]
fn a_later_commit_logs_the_mark_of_a_number_whose_transaction_never_committed() -> anyhow::Result<()>
{
    let names = Names::new("carried-mark-other-table");
    let io = Arc::new(UnreliableIo::new());
    let (db, counter) = open_mvcc_with_counter(io.clone(), &names, AllocatorOpenMode::Create)?;
    let conn = db.connect()?;
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")?;
    conn.execute("CREATE TABLE u (id INTEGER PRIMARY KEY, v TEXT)")?;
    let open = db.connect()?;
    open.execute("BEGIN CONCURRENT")?;
    assert_eq!(reserve(io.as_ref(), &counter, KEY_T, 1)?, 1);
    open.execute("INSERT INTO t VALUES (1, 'never committed')")?;
    assert_eq!(reserve(io.as_ref(), &counter, KEY_U, 1)?, 1);
    conn.execute("INSERT INTO u VALUES (1, 'committed')")?;

    let recovered = recover_after_power_loss(io.as_ref(), &names)?;
    let io = Arc::new(PlatformIo::new()?);
    let after = recovered.path("after");
    let (db, counter) = open_mvcc_with_counter(io.clone(), &after, AllocatorOpenMode::Reopen)?;
    assert_eq!(reserve(io.as_ref(), &counter, KEY_T, 1)?, 2);
    assert_eq!(reserve(io.as_ref(), &counter, KEY_U, 1)?, 2);
    assert_eq!(count_rows(&db.connect()?, "SELECT id FROM t")?, 0);
    Ok(())
}

#[test]
fn a_checkpoint_waits_for_the_recovered_marks_to_reach_the_sidecar() -> anyhow::Result<()> {
    let names = Names::new("carried-mark-recovered");
    let io = Arc::new(UnreliableIo::new());
    let (db, counter) = open_mvcc_with_counter(io.clone(), &names, AllocatorOpenMode::Create)?;
    let conn = db.connect()?;
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")?;
    assert_eq!(reserve(io.as_ref(), &counter, KEY_T, 5)?, 5);
    conn.execute("INSERT INTO t VALUES (5, 'e')")?;
    conn.execute("DELETE FROM t")?;

    let recovered = recover_after_power_loss(io.as_ref(), &names)?;
    let after = recovered.path("after");
    let io = Arc::new(PlatformIo::new()?);
    let db = open_mvcc(io.clone(), &after)?;
    let conn = db.connect()?;
    assert!(matches!(
        conn.execute("PRAGMA wal_checkpoint(TRUNCATE)"),
        Err(LimboError::Busy)
    ));

    let counter = open_counter(io.as_ref(), &after, AllocatorOpenMode::Reopen)?;
    db.get_mv_store()
        .as_ref()
        .expect("the database is in MVCC")
        .log_counter_marks_in_commits(&counter, io.as_ref())?;
    conn.execute("PRAGMA wal_checkpoint(TRUNCATE)")?;
    drop(counter);
    let counter = open_counter(io.as_ref(), &after, AllocatorOpenMode::Reopen)?;
    assert_eq!(peek(io.as_ref(), &counter, KEY_T)?, 5);
    Ok(())
}

type PlatformIo = turso_core::PlatformIO;

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

fn open_mvcc_with_counter(
    io: Arc<dyn IO>,
    names: &Names,
    mode: AllocatorOpenMode,
) -> anyhow::Result<(Arc<Database>, DurableRangeAllocator)> {
    let db = open_mvcc(io.clone(), names)?;
    let counter = open_counter(io.as_ref(), names, mode)?;
    if mode == AllocatorOpenMode::Create {
        let mut initialize = counter.initialize()?;
        io.block(|| initialize.step())?;
    }
    db.get_mv_store()
        .as_ref()
        .expect("the database is in MVCC")
        .log_counter_marks_in_commits(&counter, io.as_ref())?;
    Ok((db, counter))
}

fn open_mvcc(io: Arc<dyn IO>, names: &Names) -> anyhow::Result<Arc<Database>> {
    let db = Database::open_file_with_flags(
        io,
        &names.database,
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
    names: &Names,
    mode: AllocatorOpenMode,
) -> anyhow::Result<DurableRangeAllocator> {
    Ok(DurableRangeAllocator::open(
        io,
        &names.sidecar,
        AllocatorDatabaseIdentity::new(DATABASE)?,
        mode,
        FileSyncType::Fsync,
    )?)
}

fn reserve(
    io: &dyn IO,
    counter: &DurableRangeAllocator,
    key: [u8; 16],
    count: u64,
) -> anyhow::Result<u64> {
    let mut reservation = counter.reserve(AutoIncrementKey::new(key)?, count)?;
    Ok(io.block(|| reservation.step())?.last())
}

fn count_rows(conn: &Arc<turso_core::Connection>, sql: &str) -> anyhow::Result<usize> {
    let mut statement = conn.prepare(sql)?;
    let mut rows = 0;
    statement.run_with_row_callback(|_| {
        rows += 1;
        Ok(())
    })?;
    Ok(rows)
}

fn peek(io: &dyn IO, counter: &DurableRangeAllocator, key: [u8; 16]) -> anyhow::Result<u64> {
    let mut query = counter.peek_high_water(AutoIncrementKey::new(key)?)?;
    Ok(io.block(|| query.step())?)
}

struct Recovered {
    directory: tempfile::TempDir,
}

impl Recovered {
    fn path(&self, stem: &str) -> Names {
        let stem = self.directory.path().join(stem);
        Names::new(stem.to_str().expect("the temporary path is UTF-8"))
    }
}

fn recover_after_power_loss(io: &UnreliableIo, names: &Names) -> anyhow::Result<Recovered> {
    let directory = tempfile::TempDir::new()?;
    let after = directory.path().join("after");
    let files: HashMap<String, Vec<u8>> = io.durable_files();
    for (name, bytes) in files {
        let renamed = if let Some(suffix) = name.strip_prefix(&names.database) {
            format!("{}.db{suffix}", after.display())
        } else if name == names.sidecar {
            format!("{}.counter", after.display())
        } else {
            continue;
        };
        std::fs::write(Path::new(&renamed), bytes)?;
    }
    Ok(Recovered { directory })
}
