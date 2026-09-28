// Drizzle ORM used the way a typical Node service uses it: drizzle-kit
// migrations generated from the schema, drizzle-kit pull and push for
// introspection, the relational query API and the SQL-like query builder,
// nested transactions (savepoints) and a push back to the older schema.
// Every step is recorded in $E2E_OUT/steps.jsonl and a failing step never
// stops the run.
'use strict';

const fs = require('node:fs');
const path = require('node:path');
const { execFileSync } = require('node:child_process');
const mysql = require('mysql2/promise');
const { drizzle } = require('drizzle-orm/mysql2');
const { and, asc, count, desc, eq, gt, gte, inArray, isNull, lt, sql, sum, max, avg } = require('drizzle-orm');
const schema = require('./src/schema');

const { users, posts, tags, postTags } = schema;
const env = process.env;
const database = env.E2E_APP;
const pool = mysql.createPool({
  host: env.E2E_HOST,
  port: Number(env.E2E_PORT),
  user: env.E2E_USER,
  password: env.E2E_PASSWORD,
  database,
  ssl: { ca: fs.readFileSync(env.E2E_CA, 'utf8') },
  connectionLimit: 4,
});
const logger = { logQuery: (query, params) => console.log(`query: ${query}${params.length ? ` -- ${JSON.stringify(params)}` : ''}`) };
const db = drizzle(pool, { schema, mode: 'default', logger });

main().finally(() => pool.end());

async function main() {
  await step('connect', async () => {
    const [rows] = await pool.query('SELECT VERSION() AS version, DATABASE() AS db');
    console.log(rows);
    expect(rows[0].db === database, `connected to ${rows[0].db}`);
    const [{ one }] = await db.select({ one: sql`1`.mapWith(Number) }).from(sql`dual`);
    expect(one === 1, 'SELECT 1 FROM dual is wrong');
  });

  await step('migrate', async () => {
    kit(['migrate'], { DRIZZLE_OUT: './drizzle-v1' });
    const tables = await tableNames();
    for (const t of ['__drizzle_migrations', 'users', 'posts', 'tags', 'post_tags']) expect(tables.includes(t), `table ${t} is missing from ${tables}`);
  });

  await step('migrate-again', async () => {
    kit(['migrate'], { DRIZZLE_OUT: './drizzle-v1' });
    const [rows] = await pool.query('SELECT COUNT(*) AS n FROM __drizzle_migrations');
    expect(Number(rows[0].n) === 1, `__drizzle_migrations has ${rows[0].n} rows`);
  });

  await step('introspect-push', async () => {
    // push compares the live schema with src/schema.js; they agree after migrate.
    pushFindsNoChanges();
  });

  await step('introspect-pull', async () => {
    const out = path.join('/tmp', 'drizzle-pull');
    fs.rmSync(out, { recursive: true, force: true });
    kit(['pull'], { DRIZZLE_OUT: out });
    const pulled = fs.readFileSync(path.join(out, 'schema.ts'), 'utf8');
    console.log(pulled);
    for (const expected of [
      'export const users = mysqlTable("users"',
      'email: varchar({ length: 191 }).notNull()',
      'balance: decimal({ precision: 10, scale: 2 }).default(\'0.00\').notNull()',
      'profile: json()',
      'unique("users_email_unique").on(table.email)',
      '.references(() => users.id, { onDelete: "cascade" } )',
      'index("posts_user_published").on(table.userId, table.publishedAt)',
      'primaryKey({ columns: [table.postId, table.tagId], name: "post_tags_post_id_tag_id"})',
    ]) {
      expect(pulled.includes(expected), `pulled schema lacks ${expected}`);
    }
  });

  await step('insert', async () => {
    const tagIds = await db.insert(tags).values([{ name: 'news' }, { name: 'rust' }, { name: 'sql' }]).$returningId();
    expect(tagIds.length === 3 && tagIds.every((t) => t.id > 0), `returned ids ${JSON.stringify(tagIds)}`);
    const [news, rust, sqlTag] = tagIds.map((t) => t.id);
    const [{ id: alice }] = await db.insert(users).values({ email: 'alice@example.com', name: 'Alice', balance: '100.50', profile: { city: 'Tokyo', tags: ['a', 'b'] } }).$returningId();
    const [{ id: bob }] = await db.insert(users).values({ email: 'bob@example.com', name: 'Bob', balance: '20.25', isActive: false, profile: { city: 'Osaka', tags: ['c'] } }).$returningId();
    await db.insert(users).values({ email: 'carol@example.com', name: 'Carol', balance: '5.00' });
    const postIds = await db.insert(posts).values([
      { userId: alice, title: 'Hello', body: 'First post', publishedAt: new Date('2024-01-02T03:04:05Z'), views: 10 },
      { userId: alice, title: 'Draft', body: 'Not yet', views: 0 },
      { userId: bob, title: 'Bob writes', views: 3 },
    ]).$returningId();
    await db.insert(postTags).values([
      { postId: postIds[0].id, tagId: news },
      { postId: postIds[0].id, tagId: rust },
      { postId: postIds[2].id, tagId: sqlTag },
    ]);
    expect((await db.$count(posts)) === 3, 'expected 3 posts');
    expect((await db.$count(postTags)) === 3, 'expected 3 post_tags rows');
  });

  await step('relations', async () => {
    const all = await db.query.users.findMany({
      with: { posts: { with: { postTags: { with: { tag: true } } }, orderBy: [asc(posts.id)] } },
      orderBy: [asc(users.id)],
    });
    expect(all.length === 3, `expected 3 users, got ${all.length}`);
    const names = all[0].posts[0].postTags.map((pt) => pt.tag.name).sort().join();
    expect(names === 'news,rust', `unexpected tags ${names}`);
    const one = await db.query.posts.findFirst({ where: eq(posts.title, 'Bob writes'), with: { user: { columns: { name: true } } } });
    expect(one && one.user.name === 'Bob', 'findFirst with a one relation is wrong');
    const tagged = await db.select({ email: users.email, title: posts.title })
      .from(posts)
      .innerJoin(users, eq(posts.userId, users.id))
      .innerJoin(postTags, eq(postTags.postId, posts.id))
      .innerJoin(tags, eq(tags.id, postTags.tagId))
      .where(and(eq(tags.name, 'rust'), eq(users.isActive, true)));
    expect(tagged.length === 1 && tagged[0].email === 'alice@example.com', 'join returned the wrong posts');
    const withoutPosts = await db.select({ name: users.name }).from(users).leftJoin(posts, eq(posts.userId, users.id)).where(isNull(posts.id));
    expect(withoutPosts.length === 1 && withoutPosts[0].name === 'Carol', 'anti-join returned the wrong users');
  });

  await step('update', async () => {
    const [before] = await db.select().from(users).where(eq(users.email, 'alice@example.com'));
    await new Promise((r) => setTimeout(r, 1100));
    await db.update(users).set({ balance: sql`${users.balance} + ${'9.50'}` }).where(eq(users.email, 'alice@example.com'));
    await db.update(users).set({ name: 'Alice A.' }).where(eq(users.id, before.id));
    const [after] = await db.select().from(users).where(eq(users.id, before.id));
    expect(Number(after.balance) === 110 && after.name === 'Alice A.', `alice is ${after.name} with ${after.balance}`);
    expect(after.updatedAt > before.updatedAt, `updated_at did not move (ON UPDATE CURRENT_TIMESTAMP): ${before.updatedAt} -> ${after.updatedAt}`);
    const [result] = await db.update(posts).set({ views: sql`${posts.views} + 1` }).where(lt(posts.views, 5));
    expect(result.affectedRows === 2, `bulk update touched ${result.affectedRows} rows`);
  });

  await step('delete', async () => {
    const [{ id: dave }] = await db.insert(users).values({ email: 'dave@example.com', name: 'Dave' }).$returningId();
    await db.insert(posts).values({ userId: dave, title: 'Bye' });
    await db.delete(users).where(eq(users.id, dave));
    expect((await db.$count(posts, eq(posts.userId, dave))) === 0, 'ON DELETE CASCADE left posts behind');
    await db.insert(tags).values({ name: 'temp' });
    const [result] = await db.delete(tags).where(eq(tags.name, 'temp'));
    expect(result.affectedRows === 1, `delete removed ${result.affectedRows} tags`);
  });

  await step('pagination', async () => {
    const page = await db.select().from(posts).orderBy(asc(posts.id)).limit(2).offset(1);
    const total = await db.$count(posts);
    expect(page.length === 2 && total === 3, `page ${page.length} of total ${total}`);
    const withCounts = await db.select({ id: users.id, name: users.name, postCount: db.$count(posts, eq(posts.userId, users.id)) })
      .from(users).orderBy(desc(users.id)).limit(2);
    expect(withCounts.length === 2 && withCounts[1].postCount === 1, `$count subquery gave ${JSON.stringify(withCounts)}`);
    const firstPage = await db.query.posts.findMany({ with: { postTags: true }, orderBy: [asc(posts.id)], limit: 1, offset: 0 });
    expect(firstPage.length === 1 && firstPage[0].postTags.length === 2, 'relational query page is wrong');
  });

  await step('aggregate', async () => {
    const rows = await db.select({
      active: users.isActive,
      count: count(),
      total: sum(users.balance),
      average: avg(users.balance),
      latest: max(users.createdAt),
    }).from(users).groupBy(users.isActive).having(gt(count(), 0)).orderBy(asc(users.isActive));
    console.log(rows);
    expect(rows.length === 2 && rows[1].count === 2 && Number(rows[1].total) === 115, `groups ${JSON.stringify(rows)}`);
    const busy = await db.select({ userId: posts.userId, views: sum(posts.views) }).from(posts)
      .groupBy(posts.userId).having(({ views }) => gt(views, 5));
    expect(busy.length === 1, `having on a sum returned ${busy.length} groups`);
    const perTag = await db.select({ name: tags.name, n: count(postTags.postId) }).from(tags)
      .leftJoin(postTags, eq(postTags.tagId, tags.id)).groupBy(tags.id).orderBy(desc(count(postTags.postId)), asc(tags.name));
    expect(perTag.length === 3 && perTag.every((t) => t.n === 1), `per-tag counts ${JSON.stringify(perTag)}`);
  });

  await step('transaction-commit', async () => {
    await db.transaction(async (tx) => {
      await tx.update(users).set({ balance: sql`${users.balance} - 10` }).where(eq(users.email, 'alice@example.com'));
      await tx.update(users).set({ balance: sql`${users.balance} + 10` }).where(eq(users.email, 'bob@example.com'));
      const [bob] = await tx.select({ id: users.id }).from(users).where(eq(users.email, 'bob@example.com'));
      await tx.insert(posts).values({ userId: bob.id, title: 'In a transaction' });
    });
    const [bob] = await db.select().from(users).where(eq(users.email, 'bob@example.com'));
    expect(Number(bob.balance) === 30.25, `bob has ${bob.balance}`);
    expect((await db.$count(posts)) === 4, 'transaction did not commit');
  });

  await step('transaction-rollback', async () => {
    let caught;
    try {
      await db.transaction(async (tx) => {
        await tx.update(users).set({ balance: '0' }).where(eq(users.email, 'alice@example.com'));
        await tx.delete(posts);
        tx.rollback();
      });
    } catch (e) {
      caught = e;
    }
    expect(caught && caught.constructor.name === 'TransactionRollbackError', `unexpected error ${caught}`);
    const [alice] = await db.select().from(users).where(eq(users.email, 'alice@example.com'));
    expect(Number(alice.balance) === 100, `alice has ${alice.balance} after rollback`);
    expect((await db.$count(posts)) === 4, 'rolled back deletes are visible');
  });

  await step('savepoint', async () => {
    await db.transaction(async (tx) => {
      await tx.insert(tags).values({ name: 'kept' });
      try {
        await tx.transaction(async (inner) => {
          await inner.insert(tags).values({ name: 'dropped' });
          await inner.insert(tags).values({ name: 'kept' });
        });
      } catch (e) {
        const cause = e.cause || e;
        expect(cause.code === 'ER_DUP_ENTRY', `inner transaction failed with ${cause.code}: ${e.message}`);
      }
      await tx.insert(tags).values({ name: 'after' });
    });
    const rows = await db.select({ name: tags.name }).from(tags).where(inArray(tags.name, ['kept', 'dropped', 'after'])).orderBy(asc(tags.name));
    expect(rows.map((r) => r.name).join() === 'after,kept', `tags after the savepoint rollback: ${JSON.stringify(rows)}`);
  });

  await step('isolation-lock', async () => {
    await db.transaction(async (tx) => {
      const [carol] = await tx.select().from(users).where(eq(users.email, 'carol@example.com')).for('update');
      await tx.update(users).set({ balance: sql`${users.balance} + 1` }).where(eq(users.id, carol.id));
      const shared = await tx.select({ id: posts.id }).from(posts).where(gte(posts.views, 0)).for('share');
      expect(shared.length === 4, `FOR SHARE read ${shared.length} posts`);
    }, { isolationLevel: 'read committed', accessMode: 'read write' });
    const [carol] = await db.select().from(users).where(eq(users.email, 'carol@example.com'));
    expect(Number(carol.balance) === 6, `carol has ${carol.balance}`);
  });

  await step('json', async () => {
    const inTokyo = await db.select().from(users).where(sql`${users.profile}->>'$.city' = ${'Tokyo'}`);
    expect(inTokyo.length === 1 && inTokyo[0].email === 'alice@example.com', '->> filter returned the wrong users');
    const osaka = await db.select().from(users).where(sql`JSON_UNQUOTE(JSON_EXTRACT(${users.profile}, '$.city')) = ${'Osaka'}`);
    expect(osaka.length === 1 && osaka[0].name === 'Bob', 'JSON_EXTRACT filter returned the wrong users');
    const tagged = await db.$count(users, sql`JSON_CONTAINS(${users.profile}, ${JSON.stringify('a')}, '$.tags')`);
    expect(tagged === 1, `JSON_CONTAINS matched ${tagged} users`);
    const [bob] = await db.select().from(users).where(eq(users.email, 'bob@example.com'));
    expect(bob.profile.city === 'Osaka' && bob.profile.tags[0] === 'c', `json column came back as ${JSON.stringify(bob.profile)}`);
    await db.update(users).set({ profile: sql`JSON_SET(${users.profile}, '$.city', ${'Kyoto'})` }).where(eq(users.id, bob.id));
    const [moved] = await db.select({ city: sql`${users.profile}->>'$.city'` }).from(users).where(eq(users.id, bob.id));
    expect(moved.city === 'Kyoto', `JSON_SET left ${moved.city}`);
  });

  await step('upsert', async () => {
    await db.insert(tags).values([{ name: 'go' }, { name: 'news' }]).onDuplicateKeyUpdate({ set: { name: sql`values(${tags.name})` } });
    expect((await db.$count(tags, eq(tags.name, 'go'))) === 1 && (await db.$count(tags, eq(tags.name, 'news'))) === 1, 'onDuplicateKeyUpdate is wrong');
    await db.insert(users).values([
      { email: 'alice@example.com', name: 'Alice Upserted', balance: '100.00' },
      { email: 'erin@example.com', name: 'Erin' },
    ]).onDuplicateKeyUpdate({ set: { name: sql`values(${users.name})`, balance: sql`values(${users.balance})` } });
    const [alice] = await db.select().from(users).where(eq(users.email, 'alice@example.com'));
    expect(alice.name === 'Alice Upserted', 'upsert did not update the existing row');
    expect((await db.$count(users)) === 4, 'upsert did not insert the new row');
    const [ignored] = await db.insert(tags).ignore().values({ name: 'go' });
    expect(ignored.affectedRows === 0, `INSERT IGNORE affected ${ignored.affectedRows} rows`);
  });

  await step('prepared', async () => {
    const byEmail = db.select().from(users).where(eq(users.email, sql.placeholder('email'))).prepare();
    const [erin] = await byEmail.execute({ email: 'erin@example.com' });
    const [bob] = await byEmail.execute({ email: 'bob@example.com' });
    expect(erin.name === 'Erin' && bob.name === 'Bob', 'prepared query returned the wrong users');
    const [rows] = await pool.execute('SELECT COUNT(*) AS n FROM posts WHERE views >= ? AND title <> ?', [1, 'x']);
    expect(Number(rows[0].n) === 3, `server-side prepared statement counted ${rows[0].n}`);
  });

  await step('alter-migration', async () => {
    kit(['migrate'], { DRIZZLE_OUT: './drizzle-v2' });
    const columns = await columnsOf('posts');
    expect(columns.slug && columns.slug.nullable === 'YES', 'posts.slug is missing');
    expect(columns.title.type === 'varchar(255)', `posts.title is ${columns.title.type}`);
    expect((await tableNames()).includes('comments'), 'comments is missing');
    await db.execute(sql`UPDATE posts SET slug = CONCAT('post-', id)`);
    const [first] = await db.select({ id: posts.id }).from(posts).orderBy(asc(posts.id)).limit(1);
    await db.execute(sql`INSERT INTO comments (post_id, body) VALUES (${first.id}, ${'Nice'})`);
    const [rows] = await pool.query('SELECT COUNT(*) AS n FROM __drizzle_migrations');
    expect(Number(rows[0].n) === 2, `__drizzle_migrations has ${rows[0].n} rows`);
  });

  await step('revert-migration', async () => {
    // drizzle-kit has no down migrations: going back is a new migration that
    // `drizzle-kit generate` wrote from the older schema.
    kit(['migrate'], { DRIZZLE_OUT: './drizzle-v3' });
    const columns = await columnsOf('posts');
    expect(!columns.slug, 'posts.slug is still there');
    expect(columns.title.type === 'varchar(200)', `posts.title is ${columns.title.type}`);
    expect(!(await tableNames()).includes('comments'), 'comments is still there');
    pushFindsNoChanges();
  });
}

function pushFindsNoChanges() {
  const out = kit(['push', '--force'], { DRIZZLE_SCHEMA: './src/schema.js' });
  expect(/No changes detected/.test(out), 'drizzle-kit push wants to change the schema that migrate wrote');
}

function kit(args, extraEnv) {
  console.log(`$ drizzle-kit ${args.join(' ')} ${JSON.stringify(extraEnv)}`);
  try {
    const out = execFileSync(path.join(__dirname, 'node_modules', '.bin', 'drizzle-kit'), args, {
      cwd: __dirname,
      encoding: 'utf8',
      env: { ...env, ...extraEnv },
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    console.log(out);
    return out;
  } catch (e) {
    throw new Error(`drizzle-kit ${args.join(' ')} failed:\n${e.stdout}\n${e.stderr}`);
  }
}

async function tableNames() {
  const [rows] = await pool.query('SELECT TABLE_NAME AS name FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE()');
  return rows.map((r) => r.name);
}

async function columnsOf(table) {
  const [rows] = await pool.query(
    'SELECT COLUMN_NAME AS name, COLUMN_TYPE AS type, IS_NULLABLE AS nullable FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = ?',
    [table],
  );
  return Object.fromEntries(rows.map((r) => [r.name, r]));
}

async function step(name, fn) {
  console.log(`=== step ${name}`);
  let entry;
  try {
    await fn();
    entry = { step: name, ok: true };
  } catch (e) {
    const text = errorText(e);
    console.log(text);
    entry = { step: name, ok: false, error: text.slice(-2000) };
  }
  fs.appendFileSync(path.join(env.E2E_OUT, 'steps.jsonl'), `${JSON.stringify(entry)}\n`);
}

function errorText(e) {
  const parts = [e && e.stack ? e.stack : String(e)];
  const cause = e && e.cause;
  if (cause) parts.push(`cause: ${cause.code} errno: ${cause.errno} sqlMessage: ${cause.sqlMessage}`);
  if (e && e.code) parts.push(`code: ${e.code} errno: ${e.errno} sqlMessage: ${e.sqlMessage}`);
  return parts.join('\n');
}

function expect(condition, message) {
  if (!condition) throw new Error(message);
}
