// TypeORM used the way a typical app uses it: decorated entities kept in
// sync with synchronize(), repositories and the QueryBuilder for queries, and
// a hand-written migration for a later schema change. Every step is recorded
// in $E2E_OUT/steps.jsonl and a failing step never stops the run.
import 'reflect-metadata';
import fs from 'node:fs';
import path from 'node:path';
import { DataSource, Logger } from 'typeorm';
import { Post, Tag, User } from './entities';
import { AddPostSlug1700000000000 } from './migrations/1700000000000-AddPostSlug';

const env = process.env;
const database = env.E2E_APP!;

class RecordingLogger implements Logger {
  queries: string[] = [];

  logQuery(query: string, parameters?: unknown[]) {
    this.queries.push(query);
    console.log(`query: ${query}${parameters && parameters.length ? ` -- ${JSON.stringify(parameters)}` : ''}`);
  }
  logQueryError(error: string | Error, query: string, parameters?: unknown[]) {
    console.log(`query failed: ${query} -- ${JSON.stringify(parameters ?? [])}: ${error}`);
  }
  logQuerySlow(time: number, query: string) {
    console.log(`slow query (${time} ms): ${query}`);
  }
  logSchemaBuild(message: string) {
    console.log(`schema: ${message}`);
  }
  logMigration(message: string) {
    console.log(`migration: ${message}`);
  }
  log(level: 'log' | 'info' | 'warn', message: unknown) {
    console.log(`${level}: ${message}`);
  }
}

const logger = new RecordingLogger();
const dataSource = new DataSource({
  type: 'mysql',
  host: env.E2E_HOST,
  port: Number(env.E2E_PORT),
  username: env.E2E_USER,
  password: env.E2E_PASSWORD,
  database,
  ssl: { ca: fs.readFileSync(env.E2E_CA!) },
  entities: [User, Post, Tag],
  migrations: [AddPostSlug1700000000000],
  logger,
  logging: 'all',
});

const users = dataSource.getRepository(User);
const posts = dataSource.getRepository(Post);
const tags = dataSource.getRepository(Tag);

main().finally(() => (dataSource.isInitialized ? dataSource.destroy() : undefined));

async function main() {
  await step('connect', async () => {
    await dataSource.initialize();
    const rows = await dataSource.query('SELECT VERSION() AS version, DATABASE() AS db, @@character_set_client AS cs');
    console.log(rows);
    expect(rows[0].db === database, `connected to ${rows[0].db}`);
  });

  await step('migrate', async () => {
    await dataSource.synchronize();
    const tables = await tableNames();
    for (const t of ['users', 'posts', 'tags', 'post_tag']) expect(tables.includes(t), `table ${t} is missing`);
  });

  await step('migrate-again', async () => {
    logger.queries = [];
    await dataSource.synchronize();
    const ddl = logger.queries.filter((q) => /^\s*(CREATE|ALTER|DROP|RENAME)\b/i.test(q));
    expect(ddl.length === 0, `second synchronize issued DDL:\n${ddl.join('\n')}`);
  });

  await step('introspect', async () => {
    const queryRunner = dataSource.createQueryRunner();
    try {
      const all = await queryRunner.getTables(['users', 'posts', 'tags', 'post_tag']);
      expect(all.length === 4, `getTables found ${all.length} tables`);
      const usersTable = (await queryRunner.getTable('users'))!;
      const column = (name: string) => usersTable.findColumnByName(name)!;
      expect(column('id').isPrimary && column('id').isGenerated && column('id').type === 'bigint', 'users.id is wrong');
      expect(column('balance').type === 'decimal' && column('balance').precision === 10 && column('balance').scale === 2,
        `users.balance is ${column('balance').type}(${column('balance').precision}, ${column('balance').scale})`);
      expect(column('profile').type === 'json' && column('profile').isNullable, 'users.profile is not a nullable json');
      expect(column('email').length === '191', `users.email length is ${column('email').length}`);
      const unique = usersTable.indices.some((i) => i.isUnique && i.columnNames.join() === 'email') ||
        usersTable.uniques.some((u) => u.columnNames.join() === 'email');
      expect(unique, 'users.email is not unique');
      const postsTable = (await queryRunner.getTable('posts'))!;
      const fk = postsTable.foreignKeys.find((f) => f.columnNames.join() === 'user_id');
      expect(fk !== undefined && fk.referencedTableName === 'users' && fk.onDelete === 'CASCADE', 'posts.user_id foreign key is wrong');
      const joinTable = (await queryRunner.getTable('post_tag'))!;
      expect(joinTable.primaryColumns.map((c) => c.name).sort().join() === 'post_id,tag_id', 'post_tag primary key is wrong');
      expect(joinTable.foreignKeys.length === 2, `post_tag has ${joinTable.foreignKeys.length} foreign keys`);
    } finally {
      await queryRunner.release();
    }
    // What `typeorm migration:generate` would write: nothing, when entities and tables agree.
    const pending = await dataSource.driver.createSchemaBuilder().log();
    const up = pending.upQueries.map((q) => q.query);
    expect(up.length === 0, `schema diff is not empty:\n${up.join('\n')}`);
  });

  await step('insert', async () => {
    const [news, rust, sql] = await tags.save([tags.create({ name: 'news' }), tags.create({ name: 'rust' }), tags.create({ name: 'sql' })]);
    const alice = await users.save(users.create({
      email: 'alice@example.com',
      name: 'Alice',
      balance: '100.50',
      profile: { city: 'Tokyo', tags: ['a', 'b'] },
      posts: [
        posts.create({ title: 'Hello', body: 'First post', publishedAt: new Date('2024-01-02T03:04:05Z'), views: 10, tags: [news, rust] }),
        posts.create({ title: 'Draft', body: 'Not yet', views: 0 }),
      ],
    }));
    expect(alice.id !== undefined && alice.posts.every((p) => p.id !== undefined), 'cascade insert did not assign ids');
    const bob = await users.save(users.create({ email: 'bob@example.com', name: 'Bob', balance: '20.25', isActive: false, profile: { city: 'Osaka', tags: ['c'] } }));
    await users.insert({ email: 'carol@example.com', name: 'Carol', balance: '5.00' });
    await posts.save(posts.create({ title: 'Bob writes', views: 3, user: bob, tags: [sql] }));
    expect((await posts.count()) === 3, 'expected 3 posts');
    expect((await dataSource.query('SELECT COUNT(*) AS n FROM post_tag'))[0].n == 3, 'expected 3 post_tag rows');
  });

  await step('relations', async () => {
    const all = await users.find({
      relations: { posts: { tags: true } },
      order: { id: 'ASC', posts: { id: 'ASC' } },
    });
    expect(all.length === 3, `expected 3 users, got ${all.length}`);
    const names = all[0].posts[0].tags.map((t) => t.name).sort().join();
    expect(names === 'news,rust', `unexpected tags ${names}`);
    const tagged = await posts.createQueryBuilder('post')
      .leftJoinAndSelect('post.user', 'user')
      .leftJoinAndSelect('post.tags', 'tag')
      .where('tag.name = :name', { name: 'rust' })
      .andWhere('user.isActive = :active', { active: true })
      .getMany();
    expect(tagged.length === 1 && tagged[0].user.email === 'alice@example.com', 'QueryBuilder join returned the wrong posts');
    const withoutPosts = await users.createQueryBuilder('user')
      .leftJoin('user.posts', 'post')
      .where('post.id IS NULL')
      .getMany();
    expect(withoutPosts.length === 1 && withoutPosts[0].name === 'Carol', 'anti-join returned the wrong users');
  });

  await step('update', async () => {
    const before = await users.findOneByOrFail({ email: 'alice@example.com' });
    await new Promise((r) => setTimeout(r, 1100));
    await users.increment({ email: 'alice@example.com' }, 'balance', 9.5);
    const alice = await users.findOneByOrFail({ email: 'alice@example.com' });
    alice.name = 'Alice A.';
    await users.save(alice);
    const after = await users.findOneByOrFail({ email: 'alice@example.com' });
    expect(Number(after.balance) === 110 && after.name === 'Alice A.', `alice is ${after.name} with ${after.balance}`);
    expect(after.updatedAt > before.updatedAt, 'updated_at did not move');
    const res = await posts.createQueryBuilder()
      .update(Post)
      .set({ views: () => 'views + 1' })
      .where('views < :limit', { limit: 5 })
      .execute();
    expect(res.affected === 2, `bulk update touched ${res.affected} rows`);
  });

  await step('delete', async () => {
    const dave = await users.save(users.create({ email: 'dave@example.com', name: 'Dave', posts: [posts.create({ title: 'Bye' })] }));
    await users.delete(dave.id);
    expect((await posts.countBy({ userId: dave.id })) === 0, 'ON DELETE CASCADE left posts behind');
    const temp = await tags.save(tags.create({ name: 'temp' }));
    await tags.remove(temp);
    expect((await tags.countBy({ name: 'temp' })) === 0, 'remove did not delete the tag');
  });

  await step('pagination', async () => {
    const [page, total] = await posts.findAndCount({ order: { id: 'ASC' }, skip: 1, take: 2 });
    expect(page.length === 2 && total === 3, `page ${page.length} of total ${total}`);
    // With a joined collection TypeORM pages over a DISTINCT subquery of ids.
    const [withTags, count] = await posts.findAndCount({ relations: { tags: true }, order: { id: 'ASC' }, skip: 0, take: 1 });
    expect(withTags.length === 1 && withTags[0].tags.length === 2 && count === 3, 'paginated join returned the wrong rows');
  });

  await step('aggregate', async () => {
    const rows = await users.createQueryBuilder('user')
      .select('user.isActive', 'active')
      .addSelect('COUNT(*)', 'count')
      .addSelect('SUM(user.balance)', 'total')
      .addSelect('AVG(user.balance)', 'average')
      .addSelect('MAX(user.createdAt)', 'latest')
      .groupBy('user.isActive')
      .having('COUNT(*) > :min', { min: 0 })
      .orderBy('user.isActive', 'ASC')
      .getRawMany();
    console.log(rows);
    expect(rows.length === 2 && Number(rows[1].count) === 2 && Number(rows[1].total) === 115, `groups ${JSON.stringify(rows)}`);
    expect((await users.count()) === 3, 'repository count is wrong');
    expect(Number(await posts.sum('views')) === 15, 'repository sum is wrong');
    expect(Number(await posts.maximum('views', { user: { email: 'alice@example.com' } })) === 10, 'repository maximum is wrong');
    const busy = await posts.createQueryBuilder('post')
      .select('post.userId', 'userId')
      .addSelect('SUM(post.views)', 'views')
      .groupBy('post.userId')
      .having('SUM(post.views) > :min', { min: 5 })
      .getRawMany();
    expect(busy.length === 1, `having on a sum returned ${busy.length} groups`);
  });

  await step('transaction-commit', async () => {
    await dataSource.transaction(async (manager) => {
      await manager.decrement(User, { email: 'alice@example.com' }, 'balance', 10);
      await manager.increment(User, { email: 'bob@example.com' }, 'balance', 10);
      const bob = await manager.findOneByOrFail(User, { email: 'bob@example.com' });
      await manager.save(manager.create(Post, { title: 'In a transaction', user: bob }));
    });
    const bob = await users.findOneByOrFail({ email: 'bob@example.com' });
    expect(Number(bob.balance) === 30.25, `bob has ${bob.balance}`);
    expect((await posts.count()) === 4, 'transaction did not commit');
  });

  await step('transaction-rollback', async () => {
    const marker = new Error('roll back on purpose');
    let caught: unknown;
    try {
      await dataSource.transaction(async (manager) => {
        await manager.update(User, { email: 'alice@example.com' }, { balance: '0' });
        await manager.createQueryBuilder().delete().from(Post).execute();
        throw marker;
      });
    } catch (e) {
      caught = e;
    }
    expect(caught === marker, `unexpected error ${caught}`);
    const queryRunner = dataSource.createQueryRunner();
    try {
      await queryRunner.startTransaction();
      await queryRunner.manager.update(Tag, { name: 'news' }, { name: 'breaking' });
      await queryRunner.rollbackTransaction();
    } finally {
      await queryRunner.release();
    }
    const alice = await users.findOneByOrFail({ email: 'alice@example.com' });
    expect(Number(alice.balance) === 100, `alice has ${alice.balance} after rollback`);
    expect((await posts.count()) === 4, 'rolled back deletes are visible');
    expect((await tags.countBy({ name: 'news' })) === 1, 'rolled back tag rename is visible');
  });

  await step('json', async () => {
    const inTokyo = await users.createQueryBuilder('user')
      .where("JSON_UNQUOTE(JSON_EXTRACT(user.profile, '$.city')) = :city", { city: 'Tokyo' })
      .getMany();
    expect(inTokyo.length === 1 && inTokyo[0].email === 'alice@example.com', 'JSON_EXTRACT filter returned the wrong users');
    const arrow = await users.createQueryBuilder('user').where("user.profile->>'$.city' = :city", { city: 'Osaka' }).getMany();
    expect(arrow.length === 1 && arrow[0].name === 'Bob', '->> filter returned the wrong users');
    const tagged = await users.createQueryBuilder('user')
      .where("JSON_CONTAINS(user.profile, :tag, '$.tags')", { tag: JSON.stringify('a') })
      .getCount();
    expect(tagged === 1, `JSON_CONTAINS matched ${tagged} users`);
    const bob = await users.findOneByOrFail({ email: 'bob@example.com' });
    expect((bob.profile as { city: string }).city === 'Osaka', 'json column did not round-trip');
  });

  await step('upsert', async () => {
    await tags.upsert([{ name: 'go' }, { name: 'news' }], ['name']);
    expect((await tags.countBy({ name: 'go' })) === 1, 'upsert did not insert');
    const result = await users.upsert(
      [{ email: 'alice@example.com', name: 'Alice Upserted', balance: '100.00' }, { email: 'erin@example.com', name: 'Erin' }],
      { conflictPaths: ['email'] },
    );
    console.log(result);
    const alice = await users.findOneByOrFail({ email: 'alice@example.com' });
    expect(alice.name === 'Alice Upserted', 'upsert did not update the existing row');
    expect((await users.count()) === 4, 'upsert did not insert the new row');
  });

  await step('alter-migration', async () => {
    const ran = await dataSource.runMigrations({ transaction: 'each' });
    expect(ran.length === 1 && ran[0].name === 'AddPostSlug1700000000000', `ran ${ran.map((m) => m.name)}`);
    const table = (await dataSource.createQueryRunner().getTable('posts'))!;
    expect(table.findColumnByName('slug')?.isNullable === true, 'posts.slug is missing');
    expect(table.findColumnByName('view_count') !== undefined && table.findColumnByName('views') === undefined, 'views was not renamed');
    expect(table.indices.some((i) => i.name === 'IDX_posts_slug'), 'IDX_posts_slug is missing');
    await dataSource.query("UPDATE posts SET slug = CONCAT('post-', id)");
    const rows = await dataSource.query('SELECT SUM(view_count) AS v FROM posts');
    expect(Number(rows[0].v) === 15, `view_count sum is ${rows[0].v}`);
    expect(!(await dataSource.showMigrations()), 'a migration is still pending');
  });

  await step('rollback-migration', async () => {
    await dataSource.undoLastMigration({ transaction: 'each' });
    const table = (await dataSource.createQueryRunner().getTable('posts'))!;
    expect(table.findColumnByName('slug') === undefined, 'posts.slug is still there');
    expect(table.findColumnByName('views') !== undefined, 'view_count was not renamed back');
    expect(!table.indices.some((i) => i.name === 'IDX_posts_slug'), 'IDX_posts_slug is still there');
    expect(await dataSource.showMigrations(), 'the undone migration is not pending again');
    const pending = await dataSource.driver.createSchemaBuilder().log();
    expect(pending.upQueries.length === 0, `schema diff after rollback:\n${pending.upQueries.map((q) => q.query).join('\n')}`);
  });

  await step('drop-database', async () => {
    await dataSource.dropDatabase();
    const left = await tableNames();
    expect(left.length === 0, `tables left: ${left.join()}`);
  });

  await step('synchronize-drop', async () => {
    await dataSource.synchronize();
    await users.insert({ email: 'x@example.com', name: 'X' });
    await dataSource.synchronize(true);
    expect((await users.count()) === 0, 'synchronize(true) kept the old rows');
    const pending = await dataSource.driver.createSchemaBuilder().log();
    expect(pending.upQueries.length === 0, 'schema diff after synchronize(true) is not empty');
  });
}

async function tableNames(): Promise<string[]> {
  const rows: { name: string }[] = await dataSource.query(
    'SELECT TABLE_NAME AS name FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE()',
  );
  return rows.map((r) => r.name);
}

async function step(name: string, fn: () => Promise<void>) {
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
  fs.appendFileSync(path.join(env.E2E_OUT!, 'steps.jsonl'), `${JSON.stringify(entry)}\n`);
}

function errorText(e: unknown): string {
  const err = e as { stack?: string; code?: string; errno?: number; sqlMessage?: string; query?: string; parameters?: unknown };
  const parts = [err && err.stack ? err.stack : String(e)];
  if (err && err.code) parts.push(`code: ${err.code} errno: ${err.errno}`);
  if (err && err.query) parts.push(`query: ${err.query} -- ${JSON.stringify(err.parameters ?? [])}`);
  return parts.join('\n');
}

function expect(condition: boolean, message: string) {
  if (!condition) throw new Error(message);
}
