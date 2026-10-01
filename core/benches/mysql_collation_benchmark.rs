#[cfg(feature = "codspeed")]
use codspeed_criterion_compat::{
    black_box, criterion_group, criterion_main, Criterion, Throughput,
};
#[cfg(not(feature = "codspeed"))]
use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};

use std::sync::Arc;
use std::time::Duration;
use turso_core::{CollationSeq, Database, MemoryIO, SqliteDialect, Statement, StepResult};

#[cfg(not(target_family = "wasm"))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const TEXTS: usize = 1024;
const ROWS: i64 = 10_000;
const RANGE: i64 = 100;
const COLLATIONS: &[(&str, &str)] = &[
    ("uca9", "utf8mb4_0900_ai_ci"),
    ("uca400", "utf8mb4_unicode_ci"),
];
const QUERIES: &[(&str, &str)] = &[
    (
        "distinct_order_by",
        "SELECT DISTINCT c FROM sbtest1 WHERE id BETWEEN ?1 AND ?1 + 99 ORDER BY c",
    ),
    (
        "order_by",
        "SELECT c FROM sbtest1 WHERE id BETWEEN ?1 AND ?1 + 99 ORDER BY c",
    ),
];

#[turso_macros::codspeed_criterion_benchmark]
fn bench_mysql_collation(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("mysql_collation");
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(3));
    group.throughput(Throughput::Elements(TEXTS as u64));

    let mut random = Random(0x9e37_79b9_7f4a_7c15);
    let texts = [
        (
            "sysbench_c",
            (0..TEXTS).map(|_| random.sysbench_c()).collect(),
        ),
        ("accented", (0..TEXTS).map(|_| random.accented()).collect()),
    ];
    for (collation_label, collation_name) in COLLATIONS {
        let collation = CollationSeq::new(collation_name).unwrap();
        for (text_label, texts) in &texts {
            let texts: &Vec<String> = texts;
            group.bench_function(format!("compare/{collation_label}/{text_label}"), |b| {
                b.iter(|| {
                    texts
                        .iter()
                        .zip(texts.iter().skip(1))
                        .filter(|(left, right)| collation.compare_strings(left, right).is_lt())
                        .count()
                })
            });
            group.bench_function(format!("hash_key/{collation_label}/{text_label}"), |b| {
                b.iter(|| {
                    texts
                        .iter()
                        .map(|text| collation.hash_key(text).len())
                        .sum::<usize>()
                })
            });
        }
    }
    group.finish();

    let mut group = criterion.benchmark_group("mysql_collation_sql");
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(3));
    for (collation_label, collation_name) in COLLATIONS {
        let io = Arc::new(MemoryIO::new());
        let db = Database::open_file(io, ":memory:", Arc::new(SqliteDialect)).unwrap();
        let conn = db.connect().unwrap();
        conn.execute(format!(
            "CREATE TABLE sbtest1(id INTEGER PRIMARY KEY, c TEXT COLLATE {collation_name})"
        ))
        .unwrap();
        conn.execute("BEGIN").unwrap();
        for id in 1..=ROWS {
            conn.execute(format!(
                "INSERT INTO sbtest1 VALUES ({id}, '{}')",
                random.sysbench_c()
            ))
            .unwrap();
        }
        conn.execute("COMMIT").unwrap();
        for (query_label, sql) in QUERIES {
            let mut stmt = conn.prepare(sql).unwrap();
            let mut start = 1;
            group.bench_function(format!("{query_label}/{collation_label}"), |b| {
                b.iter(|| {
                    start = (start + 37) % (ROWS - RANGE) + 1;
                    let rows = run(&db, &mut stmt, start);
                    assert_eq!(rows, RANGE as usize);
                    rows
                })
            });
        }
    }
    group.finish();
}

fn run(db: &Database, stmt: &mut Statement, start: i64) -> usize {
    stmt.bind_at(1.try_into().unwrap(), turso_core::Value::from_i64(start))
        .unwrap();
    let mut rows = 0;
    loop {
        match stmt.step().unwrap() {
            StepResult::Row => {
                black_box(stmt.row());
                rows += 1;
            }
            StepResult::IO | StepResult::Yield | StepResult::Sleep { .. } => {
                db.io.step().unwrap();
            }
            StepResult::Done => break,
            StepResult::Interrupt | StepResult::Busy => unreachable!(),
        }
    }
    stmt.reset().unwrap();
    rows
}

struct Random(u64);

impl Random {
    fn sysbench_c(&mut self) -> String {
        (0..10)
            .map(|_| (0..11).map(|_| self.digit()).collect::<String>())
            .collect::<Vec<_>>()
            .join("-")
    }

    fn accented(&mut self) -> String {
        const WORDS: &[&str] = &["Café", "crème", "brûlée", "Straße", "naïve", "Ångström"];
        (0..6)
            .map(|_| WORDS[(self.next() % WORDS.len() as u64) as usize])
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn digit(&mut self) -> char {
        char::from(b'0' + (self.next() % 10) as u8)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

criterion_group!(benches, bench_mysql_collation);
criterion_main!(benches);
