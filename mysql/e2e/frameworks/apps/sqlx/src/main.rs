//! sqlx 0.8 used the way a Rust service uses it: reversible migrations run
//! with the `sqlx` CLI (`migrate run`, `info`, `revert`, `database reset`),
//! and runtime-checked `query()` / `query_as()` calls over prepared
//! statements, which decode every column strictly by its reported type. Every
//! step is recorded in $E2E_OUT/steps.jsonl and a failing step never stops
//! the run.
use std::error::Error;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::process::Command;

use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use futures_util::TryStreamExt;
use rust_decimal::Decimal;
use serde_json::{json, Value};
use sqlx::mysql::{MySqlConnectOptions, MySqlPool, MySqlPoolOptions};
use sqlx::types::Json;
use sqlx::{Acquire, Connection, Executor, FromRow, QueryBuilder, Row};

type Res = Result<(), Box<dyn Error + Send + Sync>>;

macro_rules! ensure {
    ($cond:expr, $($arg:tt)+) => {
        if !$cond {
            return Err(format!($($arg)+).into());
        }
    };
}

#[derive(FromRow, Debug)]
struct User {
    id: u64,
    email: String,
    name: String,
    balance: Decimal,
    is_active: bool,
    profile: Option<Json<Value>>,
    avatar: Option<Vec<u8>>,
    created_at: NaiveDateTime,
    updated_at: DateTime<Utc>,
}

#[tokio::main]
async fn main() {
    let url = database_url();
    let pool = pool(&url);
    step("connect", connect(&pool)).await;
    step("migrate", migrate(&pool, &url)).await;
    step("migrate-info", migrate_info(&url)).await;
    step("migrate-again", migrate_again(&pool, &url)).await;
    step("types", types(&pool)).await;
    step("insert", insert(&pool)).await;
    step("relations", relations(&pool)).await;
    step("update", update(&pool)).await;
    step("delete", delete(&pool)).await;
    step("pagination", pagination(&pool)).await;
    step("aggregate", aggregate(&pool)).await;
    step("transaction-commit", transaction_commit(&pool)).await;
    step("transaction-rollback", transaction_rollback(&pool)).await;
    step("savepoint", savepoint(&pool)).await;
    step("isolation-lock", isolation_lock(&pool)).await;
    step("json", json_functions(&pool)).await;
    step("upsert", upsert(&pool)).await;
    step("raw-multi-statement", raw_multi_statement(&pool)).await;
    step("alter-migration", alter_migration(&pool, &url)).await;
    step("revert-migration", revert_migration(&pool, &url)).await;
    step("revert-all", revert_all(&pool, &url)).await;
    pool.close().await;
    step("database-reset", database_reset(&url)).await;
}

fn database_url() -> String {
    let env = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set"));
    format!(
        "mysql://{}:{}@{}:{}/{}?ssl-mode=VERIFY_IDENTITY&ssl-ca={}",
        env("E2E_USER"),
        env("E2E_PASSWORD"),
        env("E2E_HOST"),
        env("E2E_PORT"),
        env("E2E_APP"),
        env("E2E_CA")
    )
}

fn pool(url: &str) -> MySqlPool {
    let options: MySqlConnectOptions = url.parse().expect("database URL");
    MySqlPoolOptions::new()
        .max_connections(4)
        .connect_lazy_with(options)
}

async fn connect(pool: &MySqlPool) -> Res {
    let (version, database): (String, Option<String>) =
        sqlx::query_as("SELECT VERSION(), DATABASE()")
            .fetch_one(pool)
            .await?;
    println!("{version} {database:?}");
    ensure!(
        database.as_deref() == std::env::var("E2E_APP").ok().as_deref(),
        "connected to {database:?}"
    );
    pool.acquire().await?.ping().await?;
    let (mode,): (String,) = sqlx::query_as("SELECT @@SESSION.sql_mode")
        .fetch_one(pool)
        .await?;
    ensure!(
        mode.contains("PIPES_AS_CONCAT"),
        "sqlx's session setup did not stick: sql_mode is {mode}"
    );
    Ok(())
}

async fn migrate(pool: &MySqlPool, url: &str) -> Res {
    cli(
        url,
        &[
            "migrate",
            "run",
            "--source",
            "migrations",
            "--target-version",
            "20260101000000",
        ],
    )?;
    let tables = table_names(pool).await?;
    for t in ["_sqlx_migrations", "users", "posts", "tags", "post_tags"] {
        ensure!(
            tables.iter().any(|n| n == t),
            "table {t} is missing from {tables:?}"
        );
    }
    Ok(())
}

async fn migrate_info(url: &str) -> Res {
    let out = cli(url, &["migrate", "info", "--source", "migrations"])?;
    ensure!(
        out.contains("20260101000000/installed"),
        "the first migration is not installed"
    );
    ensure!(
        out.contains("20260201000000/pending"),
        "the second migration is not pending"
    );
    Ok(())
}

async fn migrate_again(pool: &MySqlPool, url: &str) -> Res {
    cli(
        url,
        &[
            "migrate",
            "run",
            "--source",
            "migrations",
            "--target-version",
            "20260101000000",
        ],
    )?;
    let (n, ok): (i64, Decimal) =
        sqlx::query_as("SELECT COUNT(*), SUM(success) FROM _sqlx_migrations")
            .fetch_one(pool)
            .await?;
    ensure!(
        n == 1 && ok == Decimal::ONE,
        "_sqlx_migrations has {n} rows, {ok} successful"
    );
    Ok(())
}

/// A row with every column type, written and read back over prepared statements.
async fn types(pool: &MySqlPool) -> Res {
    let avatar: Vec<u8> = (0..=255).collect();
    let profile = json!({"city": "Tokyo", "tags": ["a", "b"], "n": 1.5});
    let result = sqlx::query("INSERT INTO users (email, name, balance, is_active, profile, avatar) VALUES (?, ?, ?, ?, ?, ?)")
        .bind("alice@example.com")
        .bind("Alice")
        .bind(Decimal::new(10050, 2))
        .bind(true)
        .bind(Json(&profile))
        .bind(&avatar[..])
        .execute(pool)
        .await?;
    ensure!(
        result.rows_affected() == 1 && result.last_insert_id() > 0,
        "insert result {result:?}"
    );
    let user: User = sqlx::query_as("SELECT * FROM users WHERE id = ?")
        .bind(result.last_insert_id())
        .fetch_one(pool)
        .await?;
    println!("{user:?}");
    ensure!(
        user.email == "alice@example.com"
            && user.name == "Alice"
            && user.balance == Decimal::new(10050, 2)
            && user.is_active,
        "scalars {user:?}"
    );
    ensure!(
        user.profile.as_ref().map(|p| &p.0) == Some(&profile),
        "json {:?}",
        user.profile
    );
    ensure!(
        user.avatar.as_deref() == Some(&avatar[..]),
        "blob round trip"
    );
    ensure!(
        user.created_at.date() > NaiveDate::from_ymd_opt(2020, 1, 1).unwrap(),
        "created_at default {}",
        user.created_at
    );
    ensure!(
        user.updated_at.timestamp() > 1_600_000_000,
        "updated_at default {}",
        user.updated_at
    );
    let row = sqlx::query("SELECT CAST(? AS SIGNED) AS i, CAST(? AS DECIMAL(10,2)) AS d, ? AS s, CAST(NULL AS CHAR) AS n, NOW(6) AS t")
        .bind(-7_i64)
        .bind("3.25")
        .bind("héllo")
        .fetch_one(pool)
        .await?;
    ensure!(
        row.try_get::<i64, _>("i")? == -7
            && row.try_get::<Decimal, _>("d")? == Decimal::new(325, 2),
        "casts"
    );
    ensure!(
        row.try_get::<String, _>("s")? == "héllo"
            && row.try_get::<Option<String>, _>("n")?.is_none(),
        "strings"
    );
    let _: NaiveDateTime = row.try_get("t")?;
    Ok(())
}

async fn insert(pool: &MySqlPool) -> Res {
    let mut tags = QueryBuilder::new("INSERT INTO tags (name) ");
    tags.push_values(["news", "rust", "sql"], |mut b, name| {
        b.push_bind(name);
    });
    let result = tags.build().execute(pool).await?;
    ensure!(
        result.rows_affected() == 3,
        "multi-row insert affected {}",
        result.rows_affected()
    );
    let alice: u64 = sqlx::query_scalar("SELECT id FROM users WHERE email = ?")
        .bind("alice@example.com")
        .fetch_one(pool)
        .await?;
    let bob = sqlx::query(
        "INSERT INTO users (email, name, balance, is_active, profile) VALUES (?, ?, ?, ?, ?)",
    )
    .bind("bob@example.com")
    .bind("Bob")
    .bind(Decimal::new(2025, 2))
    .bind(false)
    .bind(Json(json!({"city": "Osaka", "tags": ["c"]})))
    .execute(pool)
    .await?
    .last_insert_id();
    sqlx::query("INSERT INTO users (email, name, balance) VALUES (?, ?, ?)")
        .bind("carol@example.com")
        .bind("Carol")
        .bind(Decimal::new(500, 2))
        .execute(pool)
        .await?;
    let published = NaiveDate::from_ymd_opt(2024, 1, 2)
        .unwrap()
        .and_hms_opt(3, 4, 5)
        .unwrap();
    let hello = sqlx::query(
        "INSERT INTO posts (user_id, title, body, published_at, views) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(alice)
    .bind("Hello")
    .bind("First post")
    .bind(published)
    .bind(10)
    .execute(pool)
    .await?
    .last_insert_id();
    sqlx::query("INSERT INTO posts (user_id, title, body, views) VALUES (?, ?, ?, ?)")
        .bind(alice)
        .bind("Draft")
        .bind("Not yet")
        .bind(0)
        .execute(pool)
        .await?;
    let bobs = sqlx::query("INSERT INTO posts (user_id, title, views) VALUES (?, ?, ?)")
        .bind(bob)
        .bind("Bob writes")
        .bind(3)
        .execute(pool)
        .await?
        .last_insert_id();
    sqlx::query(
        "INSERT INTO post_tags (post_id, tag_id) SELECT ?, id FROM tags WHERE name IN (?, ?)",
    )
    .bind(hello)
    .bind("news")
    .bind("rust")
    .execute(pool)
    .await?;
    sqlx::query("INSERT INTO post_tags (post_id, tag_id) SELECT ?, id FROM tags WHERE name = ?")
        .bind(bobs)
        .bind("sql")
        .execute(pool)
        .await?;
    let counts: (i64, i64) =
        sqlx::query_as("SELECT (SELECT COUNT(*) FROM posts), (SELECT COUNT(*) FROM post_tags)")
            .fetch_one(pool)
            .await?;
    ensure!(counts == (3, 3), "posts and post_tags counts {counts:?}");
    Ok(())
}

async fn relations(pool: &MySqlPool) -> Res {
    let rows: Vec<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT u.email, p.title, GROUP_CONCAT(t.name ORDER BY t.name SEPARATOR ',') \
         FROM users u JOIN posts p ON p.user_id = u.id \
         LEFT JOIN post_tags pt ON pt.post_id = p.id LEFT JOIN tags t ON t.id = pt.tag_id \
         GROUP BY u.email, p.id, p.title ORDER BY p.id",
    )
    .fetch_all(pool)
    .await?;
    ensure!(
        rows.len() == 3 && rows[0].2.as_deref() == Some("news,rust") && rows[1].2.is_none(),
        "joined rows {rows:?}"
    );
    let tagged: Vec<String> = sqlx::query_scalar(
        "SELECT u.email FROM posts p JOIN users u ON u.id = p.user_id JOIN post_tags pt ON pt.post_id = p.id \
         JOIN tags t ON t.id = pt.tag_id WHERE t.name = ? AND u.is_active = ?",
    )
    .bind("rust")
    .bind(true)
    .fetch_all(pool)
    .await?;
    ensure!(tagged == ["alice@example.com"], "tag filter {tagged:?}");
    let lonely: Vec<String> = sqlx::query_scalar(
        "SELECT u.name FROM users u LEFT JOIN posts p ON p.user_id = u.id WHERE p.id IS NULL",
    )
    .fetch(pool)
    .try_collect()
    .await?;
    ensure!(lonely == ["Carol"], "anti-join {lonely:?}");
    let users: Vec<User> = sqlx::query_as("SELECT * FROM users ORDER BY id")
        .fetch_all(pool)
        .await?;
    ensure!(
        users.len() == 3 && !users[1].is_active && users[2].profile.is_none(),
        "users {users:?}"
    );
    Ok(())
}

async fn update(pool: &MySqlPool) -> Res {
    let before: DateTime<Utc> = sqlx::query_scalar("SELECT updated_at FROM users WHERE email = ?")
        .bind("alice@example.com")
        .fetch_one(pool)
        .await?;
    let result = sqlx::query("UPDATE users SET balance = balance + ?, name = ? WHERE email = ?")
        .bind(Decimal::new(950, 2))
        .bind("Alice A.")
        .bind("alice@example.com")
        .execute(pool)
        .await?;
    ensure!(
        result.rows_affected() == 1,
        "update affected {}",
        result.rows_affected()
    );
    let (balance, name, after): (Decimal, String, DateTime<Utc>) =
        sqlx::query_as("SELECT balance, name, updated_at FROM users WHERE email = ?")
            .bind("alice@example.com")
            .fetch_one(pool)
            .await?;
    ensure!(
        balance == Decimal::new(11000, 2) && name == "Alice A.",
        "alice is {name} with {balance}"
    );
    ensure!(
        after > before,
        "ON UPDATE CURRENT_TIMESTAMP(6) did not move updated_at: {before} -> {after}"
    );
    let bumped = sqlx::query("UPDATE posts SET views = views + 1 WHERE views < ?")
        .bind(5)
        .execute(pool)
        .await?;
    ensure!(
        bumped.rows_affected() == 2,
        "bulk update affected {}",
        bumped.rows_affected()
    );
    Ok(())
}

async fn delete(pool: &MySqlPool) -> Res {
    let dave = sqlx::query("INSERT INTO users (email, name) VALUES (?, ?)")
        .bind("dave@example.com")
        .bind("Dave")
        .execute(pool)
        .await?
        .last_insert_id();
    sqlx::query("INSERT INTO posts (user_id, title) VALUES (?, ?)")
        .bind(dave)
        .bind("Bye")
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM users WHERE id = ?")
        .bind(dave)
        .execute(pool)
        .await?;
    let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM posts WHERE user_id = ?")
        .bind(dave)
        .fetch_one(pool)
        .await?;
    ensure!(left == 0, "ON DELETE CASCADE left {left} posts");
    Ok(())
}

async fn pagination(pool: &MySqlPool) -> Res {
    let page: Vec<(u64, String)> =
        sqlx::query_as("SELECT id, title FROM posts ORDER BY id LIMIT ? OFFSET ?")
            .bind(2_i64)
            .bind(1_i64)
            .fetch_all(pool)
            .await?;
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM posts")
        .fetch_one(pool)
        .await?;
    ensure!(
        page.len() == 2 && page[0].1 == "Draft" && total == 3,
        "page {page:?} of {total}"
    );
    Ok(())
}

async fn aggregate(pool: &MySqlPool) -> Res {
    let rows: Vec<(bool, i64, Decimal, Decimal, NaiveDateTime)> = sqlx::query_as(
        "SELECT is_active, COUNT(*), SUM(balance), AVG(balance), MAX(created_at) FROM users \
         GROUP BY is_active HAVING COUNT(*) > ? ORDER BY is_active",
    )
    .bind(0)
    .fetch_all(pool)
    .await?;
    println!("{rows:?}");
    ensure!(
        rows.len() == 2 && rows[1].1 == 2 && rows[1].2 == Decimal::new(11500, 2),
        "groups {rows:?}"
    );
    let busy: Vec<(u64, Decimal)> = sqlx::query_as(
        "SELECT user_id, SUM(views) FROM posts GROUP BY user_id HAVING SUM(views) > ?",
    )
    .bind(5)
    .fetch_all(pool)
    .await?;
    ensure!(
        busy.len() == 1 && busy[0].1 == Decimal::new(11, 0),
        "HAVING SUM {busy:?}"
    );
    Ok(())
}

async fn transaction_commit(pool: &MySqlPool) -> Res {
    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE users SET balance = balance - 10 WHERE email = ?")
        .bind("alice@example.com")
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE users SET balance = balance + 10 WHERE email = ?")
        .bind("bob@example.com")
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO posts (user_id, title) SELECT id, ? FROM users WHERE email = ?")
        .bind("In a transaction")
        .bind("bob@example.com")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    let bob: Decimal = sqlx::query_scalar("SELECT balance FROM users WHERE email = ?")
        .bind("bob@example.com")
        .fetch_one(pool)
        .await?;
    let posts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM posts")
        .fetch_one(pool)
        .await?;
    ensure!(
        bob == Decimal::new(3025, 2) && posts == 4,
        "bob {bob}, posts {posts}"
    );
    Ok(())
}

async fn transaction_rollback(pool: &MySqlPool) -> Res {
    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE users SET balance = 0 WHERE email = ?")
        .bind("alice@example.com")
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM posts").execute(&mut *tx).await?;
    tx.rollback().await?;
    {
        let mut dropped = pool.begin().await?;
        sqlx::query("UPDATE tags SET name = 'breaking' WHERE name = 'news'")
            .execute(&mut *dropped)
            .await?;
        // Dropping a transaction without commit rolls it back.
    }
    let alice: Decimal = sqlx::query_scalar("SELECT balance FROM users WHERE email = ?")
        .bind("alice@example.com")
        .fetch_one(pool)
        .await?;
    let posts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM posts")
        .fetch_one(pool)
        .await?;
    let news: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tags WHERE name = 'news'")
        .fetch_one(pool)
        .await?;
    ensure!(
        alice == Decimal::new(10000, 2) && posts == 4 && news == 1,
        "after rollback: alice {alice}, posts {posts}, news {news}"
    );
    Ok(())
}

async fn savepoint(pool: &MySqlPool) -> Res {
    let mut tx = pool.begin().await?;
    sqlx::query("INSERT INTO tags (name) VALUES (?)")
        .bind("kept")
        .execute(&mut *tx)
        .await?;
    {
        let mut inner = tx.begin().await?;
        sqlx::query("INSERT INTO tags (name) VALUES (?)")
            .bind("dropped")
            .execute(&mut *inner)
            .await?;
        let duplicate = sqlx::query("INSERT INTO tags (name) VALUES (?)")
            .bind("kept")
            .execute(&mut *inner)
            .await;
        match duplicate {
            Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
                println!("duplicate refused: {e}")
            }
            other => return Err(format!("duplicate insert gave {other:?}").into()),
        }
        inner.rollback().await?;
    }
    sqlx::query("INSERT INTO tags (name) VALUES (?)")
        .bind("after")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    let names: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM tags WHERE name IN ('kept', 'dropped', 'after') ORDER BY name",
    )
    .fetch_all(pool)
    .await?;
    ensure!(
        names == ["after", "kept"],
        "tags after the savepoint rollback: {names:?}"
    );
    Ok(())
}

async fn isolation_lock(pool: &MySqlPool) -> Res {
    let mut conn = pool.acquire().await?;
    (&mut *conn)
        .execute("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .await?;
    let mut tx = conn.begin().await?;
    let (id, balance): (u64, Decimal) =
        sqlx::query_as("SELECT id, balance FROM users WHERE email = ? FOR UPDATE")
            .bind("carol@example.com")
            .fetch_one(&mut *tx)
            .await?;
    sqlx::query("UPDATE users SET balance = ? WHERE id = ?")
        .bind(balance + Decimal::ONE)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    let shared: Vec<u64> = sqlx::query_scalar("SELECT id FROM posts WHERE views >= ? FOR SHARE")
        .bind(0)
        .fetch_all(&mut *tx)
        .await?;
    ensure!(shared.len() == 4, "FOR SHARE read {} posts", shared.len());
    tx.commit().await?;
    let carol: Decimal = sqlx::query_scalar("SELECT balance FROM users WHERE email = ?")
        .bind("carol@example.com")
        .fetch_one(pool)
        .await?;
    ensure!(carol == Decimal::new(600, 2), "carol has {carol}");
    Ok(())
}

async fn json_functions(pool: &MySqlPool) -> Res {
    let tokyo: Vec<String> = sqlx::query_scalar(
        "SELECT email FROM users WHERE JSON_UNQUOTE(JSON_EXTRACT(profile, ?)) = ?",
    )
    .bind("$.city")
    .bind("Tokyo")
    .fetch_all(pool)
    .await?;
    ensure!(
        tokyo == ["alice@example.com"],
        "JSON_EXTRACT with a bound path {tokyo:?}"
    );
    let osaka: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE profile->>'$.city' = ?")
        .bind("Osaka")
        .fetch_one(pool)
        .await?;
    ensure!(osaka == 1, "->> matched {osaka}");
    let tagged: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE JSON_CONTAINS(profile, ?, '$.tags')")
            .bind("\"a\"")
            .fetch_one(pool)
            .await?;
    ensure!(tagged == 1, "JSON_CONTAINS matched {tagged}");
    sqlx::query("UPDATE users SET profile = JSON_SET(profile, '$.city', ?) WHERE email = ?")
        .bind("Kyoto")
        .bind("bob@example.com")
        .execute(pool)
        .await?;
    let bob: Json<Value> = sqlx::query_scalar("SELECT profile FROM users WHERE email = ?")
        .bind("bob@example.com")
        .fetch_one(pool)
        .await?;
    ensure!(
        bob.0 == json!({"city": "Kyoto", "tags": ["c"]}),
        "profile {:?}",
        bob.0
    );
    let built: Json<Value> = sqlx::query_scalar(
        "SELECT JSON_ARRAYAGG(JSON_OBJECT('name', name)) FROM tags WHERE name IN ('news', 'rust')",
    )
    .fetch_one(pool)
    .await?;
    ensure!(
        built.0.as_array().map(|a| a.len()) == Some(2),
        "JSON_ARRAYAGG {:?}",
        built.0
    );
    Ok(())
}

async fn upsert(pool: &MySqlPool) -> Res {
    let inserted = sqlx::query("INSERT INTO users (email, name, balance) VALUES (?, ?, ?) AS new ON DUPLICATE KEY UPDATE name = new.name, balance = new.balance")
        .bind("erin@example.com")
        .bind("Erin")
        .bind(Decimal::ZERO)
        .execute(pool)
        .await?;
    let updated = sqlx::query("INSERT INTO users (email, name, balance) VALUES (?, ?, ?) AS new ON DUPLICATE KEY UPDATE name = new.name, balance = new.balance")
        .bind("alice@example.com")
        .bind("Alice Upserted")
        .bind(Decimal::new(10000, 2))
        .execute(pool)
        .await?;
    ensure!(
        inserted.rows_affected() == 1 && updated.rows_affected() == 2,
        "affected rows {} and {}",
        inserted.rows_affected(),
        updated.rows_affected()
    );
    let tags = sqlx::query(
        "INSERT INTO tags (name) VALUES (?), (?) ON DUPLICATE KEY UPDATE name = VALUES(name)",
    )
    .bind("go")
    .bind("news")
    .execute(pool)
    .await?;
    // One insert plus one unchanged duplicate: sqlx connects with CLIENT_FOUND_ROWS,
    // so MySQL counts the matched duplicate too.
    ensure!(
        tags.rows_affected() == 2,
        "tag upsert affected {}",
        tags.rows_affected()
    );
    let ignored = sqlx::query("INSERT IGNORE INTO tags (name) VALUES (?)")
        .bind("go")
        .execute(pool)
        .await?;
    ensure!(
        ignored.rows_affected() == 0,
        "INSERT IGNORE affected {}",
        ignored.rows_affected()
    );
    let users: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(pool)
        .await?;
    ensure!(users == 4, "users {users}");
    Ok(())
}

/// raw_sql sends the whole string as one COM_QUERY, several statements at once.
async fn raw_multi_statement(pool: &MySqlPool) -> Res {
    let rows = sqlx::raw_sql(
        "UPDATE posts SET body = CONCAT(COALESCE(body, ''), '!') WHERE title = 'Hello'; \
         SELECT COUNT(*) AS n FROM posts WHERE body LIKE '%!'; \
         SELECT name FROM tags ORDER BY name LIMIT 1",
    )
    .fetch_all(pool)
    .await?;
    ensure!(
        rows.len() == 2,
        "multi-statement returned {} rows",
        rows.len()
    );
    ensure!(
        rows[0].try_get::<i64, _>(0)? == 1 && rows[1].try_get::<String, _>(0)? == "after",
        "multi-statement results"
    );
    Ok(())
}

async fn alter_migration(pool: &MySqlPool, url: &str) -> Res {
    cli(url, &["migrate", "run", "--source", "migrations"])?;
    let columns = columns_of(pool, "posts").await?;
    ensure!(
        columns.iter().any(|(n, _)| n == "slug") && columns.iter().any(|(n, _)| n == "view_count"),
        "posts columns {columns:?}"
    );
    ensure!(
        columns
            .iter()
            .any(|(n, t)| n == "title" && t == "varchar(255)"),
        "posts columns {columns:?}"
    );
    sqlx::query("UPDATE posts SET slug = CONCAT('post-', id)")
        .execute(pool)
        .await?;
    let views: Decimal = sqlx::query_scalar("SELECT SUM(view_count) FROM posts")
        .fetch_one(pool)
        .await?;
    ensure!(views == Decimal::new(15, 0), "view_count sum {views}");
    Ok(())
}

async fn revert_migration(pool: &MySqlPool, url: &str) -> Res {
    cli(url, &["migrate", "revert", "--source", "migrations"])?;
    let columns = columns_of(pool, "posts").await?;
    ensure!(
        !columns.iter().any(|(n, _)| n == "slug")
            && columns.iter().any(|(n, t)| n == "views" && t == "int"),
        "posts columns {columns:?}"
    );
    ensure!(
        columns
            .iter()
            .any(|(n, t)| n == "title" && t == "varchar(200)"),
        "posts columns {columns:?}"
    );
    let out = cli(url, &["migrate", "info", "--source", "migrations"])?;
    ensure!(
        out.contains("20260201000000/pending"),
        "the reverted migration is not pending"
    );
    Ok(())
}

async fn revert_all(pool: &MySqlPool, url: &str) -> Res {
    cli(
        url,
        &[
            "migrate",
            "revert",
            "--source",
            "migrations",
            "--target-version",
            "0",
        ],
    )?;
    let tables = table_names(pool).await?;
    ensure!(tables == ["_sqlx_migrations"], "tables left {tables:?}");
    Ok(())
}

async fn database_reset(url: &str) -> Res {
    cli(url, &["database", "reset", "-y", "--source", "migrations"])?;
    let pool = pool(url);
    let tables = table_names(&pool).await?;
    let applied: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _sqlx_migrations")
        .fetch_one(&pool)
        .await?;
    pool.close().await;
    ensure!(
        tables.len() == 5 && applied == 2,
        "after reset: tables {tables:?}, {applied} migrations"
    );
    Ok(())
}

async fn table_names(pool: &MySqlPool) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT CAST(TABLE_NAME AS CHAR) FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE() ORDER BY TABLE_NAME").fetch_all(pool).await
}

async fn columns_of(pool: &MySqlPool, table: &str) -> Result<Vec<(String, String)>, sqlx::Error> {
    sqlx::query_as("SELECT CAST(COLUMN_NAME AS CHAR), CAST(COLUMN_TYPE AS CHAR) FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = ?")
        .bind(table)
        .fetch_all(pool)
        .await
}

/// Runs the `sqlx` CLI the way a developer does, with DATABASE_URL set.
fn cli(url: &str, args: &[&str]) -> Result<String, Box<dyn Error + Send + Sync>> {
    println!("$ sqlx {}", args.join(" "));
    let out = Command::new("sqlx")
        .args(args)
        .env("DATABASE_URL", url)
        .output()?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    println!("{text}");
    if !out.status.success() {
        return Err(format!(
            "sqlx {} exited with {}:\n{text}",
            args.join(" "),
            out.status
        )
        .into());
    }
    Ok(text)
}

async fn step(name: &str, body: impl std::future::Future<Output = Res>) {
    println!("=== step {name}");
    let entry = match body.await {
        Ok(()) => json!({"step": name, "ok": true}),
        Err(e) => {
            let text = format!("{e}\n{e:?}");
            println!("{text}");
            let start = text.len().saturating_sub(2000);
            let start = (start..text.len())
                .find(|&i| text.is_char_boundary(i))
                .unwrap_or(text.len());
            json!({"step": name, "ok": false, "error": &text[start..]})
        }
    };
    let path = format!("{}/steps.jsonl", std::env::var("E2E_OUT").expect("E2E_OUT"));
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("steps.jsonl");
    writeln!(file, "{entry}").expect("write steps.jsonl");
}
