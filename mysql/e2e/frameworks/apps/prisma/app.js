// Prisma used the way a typical app uses it: the migrate CLI for the schema
// and the generated client for queries. Every step is recorded in
// $E2E_OUT/steps.jsonl and a failing step never stops the run.
'use strict';

const fs = require('node:fs');
const path = require('node:path');
const { spawnSync } = require('node:child_process');

const env = process.env;
const database = env.E2E_APP;
const databaseUrl = mysqlUrl(database);
process.env.DATABASE_URL = databaseUrl;
process.env.SHADOW_DATABASE_URL = mysqlUrl(`${database}_shadow`);

const schemaPath = path.join(__dirname, 'prisma', 'schema.prisma');
const firstSchema = fs.readFileSync(schemaPath, 'utf8');
const nextSchemaPath = path.join(__dirname, 'next', 'schema.prisma');
const firstSchemaCopy = '/tmp/schema-v1.prisma';
fs.writeFileSync(firstSchemaCopy, firstSchema);

const { PrismaClient, Prisma } = require('@prisma/client');
const prisma = new PrismaClient({ log: [{ emit: 'stdout', level: 'query' }] });

main().finally(() => prisma.$disconnect().catch(() => {}));

async function main() {
  await step('connect', async () => {
    await prisma.$connect();
    const rows = await prisma.$queryRaw`SELECT VERSION() AS version, DATABASE() AS db, @@character_set_client AS cs`;
    console.log(rows);
    expect(rows[0].db === database, `connected to ${rows[0].db}`);
  });

  await step('migrate', () => {
    const out = prismaCli(['migrate', 'deploy']);
    expect(/1 migration found/.test(out), 'expected one migration to be found');
  });

  await step('migrate-again', () => {
    const out = prismaCli(['migrate', 'deploy']);
    expect(/No pending migrations to apply/.test(out), 'second deploy was not a no-op');
  });

  await step('migrate-status', () => {
    const out = prismaCli(['migrate', 'status']);
    expect(/Database schema is up to date/.test(out), 'status did not report an up-to-date schema');
  });

  await step('introspect', () => {
    const out = prismaCli(['db', 'pull', '--print']);
    for (const table of ['users', 'posts', 'tags', 'post_tag']) {
      expect(out.includes(`@@map("${table}")`), `db pull did not print the model of ${table}`);
    }
    expect(/@@id\(\[postId, tagId\]\)/.test(out), 'the composite primary key is missing');
    expect(/profile\s+Json\?/.test(out), 'profile is not introspected as Json?');
    expect(/balance\s+Decimal\s+@default\(0\.00\)\s+@db\.Decimal\(10, 2\)/.test(out) ||
      /balance\s+Decimal\s+@default\(0\)\s+@db\.Decimal\(10, 2\)/.test(out), 'balance is not Decimal(10, 2)');
    expect(/onDelete: Cascade/.test(out), 'the cascading foreign key is missing');
  });

  // The live schema must match the datamodel exactly, the check CI pipelines run for drift.
  await step('migrate-diff-empty', () => {
    prismaCli(['migrate', 'diff', '--from-schema-datasource', schemaPath,
      '--to-schema-datamodel', schemaPath, '--script', '--exit-code']);
  });

  await step('insert', async () => {
    const [news, rust] = await Promise.all([
      prisma.tag.create({ data: { name: 'news' } }),
      prisma.tag.create({ data: { name: 'rust' } }),
    ]);
    const alice = await prisma.user.create({
      data: {
        email: 'alice@example.com',
        name: 'Alice',
        balance: new Prisma.Decimal('100.50'),
        profile: { city: 'Tokyo', tags: ['a', 'b'] },
        posts: {
          create: [
            {
              title: 'Hello',
              body: 'First post',
              publishedAt: new Date('2024-01-02T03:04:05Z'),
              views: 10,
              tags: { create: [{ tag: { connect: { id: news.id } } }, { tag: { connect: { id: rust.id } } }] },
            },
            { title: 'Draft', body: 'Not yet', views: 0 },
          ],
        },
      },
      include: { posts: { include: { tags: true } } },
    });
    expect(alice.posts.length === 2, 'nested create did not make two posts');
    const bob = await prisma.user.create({
      data: { email: 'bob@example.com', name: 'Bob', balance: '20.25', isActive: false, profile: { city: 'Osaka', tags: ['c'] } },
    });
    await prisma.user.create({ data: { email: 'carol@example.com', name: 'Carol', balance: '5.00' } });
    await prisma.post.create({
      data: {
        title: 'Bob writes',
        views: 3,
        user: { connect: { id: bob.id } },
        tags: { create: [{ tag: { connectOrCreate: { where: { name: 'sql' }, create: { name: 'sql' } } } }] },
      },
    });
    const count = await prisma.post.count();
    expect(count === 3, `expected 3 posts, got ${count}`);
  });

  await step('relations', async () => {
    const users = await prisma.user.findMany({
      orderBy: { id: 'asc' },
      include: { posts: { orderBy: { id: 'asc' }, include: { tags: { include: { tag: true } } } }, _count: { select: { posts: true } } },
    });
    expect(users.length === 3, `expected 3 users, got ${users.length}`);
    const tagNames = users[0].posts[0].tags.map((t) => t.tag.name).sort();
    expect(JSON.stringify(tagNames) === '["news","rust"]', `unexpected tags ${JSON.stringify(tagNames)}`);
    expect(users[0]._count.posts === 2, 'relation count is wrong');
    const tagged = await prisma.post.findMany({
      where: { tags: { some: { tag: { name: 'rust' } } }, user: { isActive: true } },
      include: { user: true },
    });
    expect(tagged.length === 1 && tagged[0].user.email === 'alice@example.com', 'relation filter returned the wrong posts');
    const writers = await prisma.user.findMany({ where: { posts: { none: {} } } });
    expect(writers.length === 1 && writers[0].name === 'Carol', 'posts: none filter returned the wrong users');
  });

  await step('update', async () => {
    const before = await prisma.user.findUniqueOrThrow({ where: { email: 'alice@example.com' } });
    const after = await prisma.user.update({
      where: { email: 'alice@example.com' },
      data: { balance: { increment: 9.5 }, name: 'Alice A.' },
    });
    expect(after.balance.toString() === '110', `balance is ${after.balance}`);
    expect(after.updatedAt >= before.updatedAt, 'updatedAt went backwards');
    const res = await prisma.post.updateMany({ where: { views: { lt: 5 } }, data: { views: { increment: 1 } } });
    expect(res.count === 2, `updateMany touched ${res.count} rows`);
  });

  await step('delete', async () => {
    const dave = await prisma.user.create({
      data: { email: 'dave@example.com', name: 'Dave', posts: { create: [{ title: 'Bye' }] } },
    });
    await prisma.user.delete({ where: { id: dave.id } });
    const orphaned = await prisma.post.count({ where: { userId: dave.id } });
    expect(orphaned === 0, 'ON DELETE CASCADE left posts behind');
    const res = await prisma.tag.deleteMany({ where: { name: { startsWith: 'zzz' } } });
    expect(res.count === 0, 'deleteMany deleted something');
  });

  await step('pagination', async () => {
    const [page, total] = await prisma.$transaction([
      prisma.post.findMany({ orderBy: { id: 'asc' }, skip: 1, take: 2, select: { id: true, title: true } }),
      prisma.post.count(),
    ]);
    expect(page.length === 2 && total === 3, `page ${page.length} of total ${total}`);
    const cursorPage = await prisma.post.findMany({ cursor: { id: page[0].id }, skip: 1, take: 1, orderBy: { id: 'asc' } });
    expect(cursorPage.length === 1 && cursorPage[0].id === page[1].id, 'cursor pagination returned the wrong row');
  });

  await step('aggregate', async () => {
    const agg = await prisma.user.aggregate({
      _count: { _all: true },
      _sum: { balance: true },
      _avg: { balance: true },
      _max: { createdAt: true },
    });
    expect(agg._count._all === 3, `count ${agg._count._all}`);
    expect(agg._sum.balance.toString() === '135.25', `sum ${agg._sum.balance}`);
    const groups = await prisma.user.groupBy({
      by: ['isActive'],
      _count: { id: true },
      _sum: { balance: true },
      having: { id: { _count: { gt: 0 } } },
      orderBy: { isActive: 'asc' },
    });
    expect(groups.length === 2 && groups[1]._count.id === 2, `groups ${JSON.stringify(groups)}`);
    const views = await prisma.post.groupBy({ by: ['userId'], _sum: { views: true }, having: { views: { _sum: { gt: 5 } } } });
    expect(views.length === 1, `having on a sum returned ${views.length} groups`);
  });

  await step('transaction-commit', async () => {
    await prisma.$transaction([
      prisma.user.update({ where: { email: 'alice@example.com' }, data: { balance: { decrement: 10 } } }),
      prisma.user.update({ where: { email: 'bob@example.com' }, data: { balance: { increment: 10 } } }),
    ]);
    await prisma.$transaction(async (tx) => {
      const bob = await tx.user.findUniqueOrThrow({ where: { email: 'bob@example.com' } });
      await tx.post.create({ data: { title: 'In a transaction', userId: bob.id } });
    });
    const bob = await prisma.user.findUniqueOrThrow({ where: { email: 'bob@example.com' } });
    expect(bob.balance.toString() === '30.25', `bob has ${bob.balance}`);
    expect((await prisma.post.count()) === 4, 'interactive transaction did not commit');
  });

  await step('transaction-rollback', async () => {
    const marker = new Error('roll back on purpose');
    let caught;
    try {
      await prisma.$transaction(async (tx) => {
        await tx.user.update({ where: { email: 'alice@example.com' }, data: { balance: 0 } });
        await tx.post.deleteMany({});
        throw marker;
      });
    } catch (e) {
      caught = e;
    }
    expect(caught === marker, `unexpected error ${caught}`);
    const alice = await prisma.user.findUniqueOrThrow({ where: { email: 'alice@example.com' } });
    expect(alice.balance.toString() === '100', `alice has ${alice.balance} after rollback`);
    expect((await prisma.post.count()) === 4, 'rolled back deletes are visible');
  });

  await step('json', async () => {
    const inTokyo = await prisma.user.findMany({ where: { profile: { path: '$.city', equals: 'Tokyo' } } });
    expect(inTokyo.length === 1 && inTokyo[0].email === 'alice@example.com', 'JSON path equals returned the wrong users');
    const tagged = await prisma.user.findMany({ where: { profile: { path: '$.tags', array_contains: 'a' } } });
    expect(tagged.length === 1, `array_contains returned ${tagged.length} users`);
    const prefixed = await prisma.user.findMany({ where: { profile: { path: '$.city', string_starts_with: 'Osa' } } });
    expect(prefixed.length === 1, `string_starts_with returned ${prefixed.length} users`);
    const noProfile = await prisma.user.count({ where: { profile: { equals: Prisma.DbNull } } });
    expect(noProfile === 1, `${noProfile} users without a profile`);
    await prisma.user.update({ where: { email: 'carol@example.com' }, data: { profile: { city: 'Kyoto', tags: [] } } });
  });

  await step('upsert', async () => {
    const created = await prisma.tag.upsert({ where: { name: 'go' }, create: { name: 'go' }, update: {} });
    const again = await prisma.tag.upsert({ where: { name: 'go' }, create: { name: 'go' }, update: { name: 'golang' } });
    expect(created.id === again.id && again.name === 'golang', 'upsert did not update the existing row');
  });

  await step('create-many', async () => {
    const res = await prisma.tag.createMany({ data: [{ name: 'news' }, { name: 'db' }, { name: 'web' }], skipDuplicates: true });
    expect(res.count === 2, `createMany inserted ${res.count} rows`);
    const many = await prisma.user.createMany({
      data: [1, 2, 3].map((i) => ({ email: `bulk${i}@example.com`, name: `Bulk ${i}`, balance: `${i}.00` })),
    });
    expect(many.count === 3, `createMany inserted ${many.count} users`);
  });

  await step('alter-migration-create', () => {
    fs.copyFileSync(nextSchemaPath, schemaPath);
    prismaCli(['migrate', 'dev', '--create-only', '--name', 'add_post_slug', '--skip-generate']);
    const dirs = fs.readdirSync(path.join(__dirname, 'prisma', 'migrations')).filter((d) => d.endsWith('_add_post_slug'));
    expect(dirs.length === 1, 'migrate dev did not write the migration');
    const sql = fs.readFileSync(path.join(__dirname, 'prisma', 'migrations', dirs[0], 'migration.sql'), 'utf8');
    console.log(sql);
    expect(/ADD COLUMN `slug`/.test(sql) && /CREATE INDEX `posts_slug_idx`/.test(sql), 'unexpected migration SQL');
  });

  await step('alter-migration', async () => {
    const out = prismaCli(['migrate', 'deploy']);
    expect(/add_post_slug/.test(out), 'the second migration was not applied');
    prismaCli(['migrate', 'diff', '--from-schema-datasource', schemaPath,
      '--to-schema-datamodel', schemaPath, '--script', '--exit-code']);
    await prisma.$executeRaw`UPDATE posts SET slug = CONCAT('post-', id)`;
    const rows = await prisma.$queryRaw`SELECT id FROM posts WHERE slug = 'post-1'`;
    expect(rows.length === 1, 'the new column is not usable');
  });

  // Prisma has no down migrations; its documented way back is a diff from the
  // live database to the previous datamodel, applied with db execute.
  await step('rollback-migration', () => {
    const down = prismaCli(['migrate', 'diff', '--from-schema-datasource', schemaPath,
      '--to-schema-datamodel', firstSchemaCopy, '--script']);
    fs.writeFileSync('/tmp/down.sql', down);
    console.log(down);
    prismaCli(['db', 'execute', '--file', '/tmp/down.sql', '--schema', schemaPath]);
    prismaCli(['migrate', 'diff', '--from-schema-datasource', schemaPath,
      '--to-schema-datamodel', firstSchemaCopy, '--script', '--exit-code']);
  });

  await step('reset', () => {
    const out = prismaCli(['migrate', 'reset', '--force', '--skip-seed', '--skip-generate']);
    expect(/add_post_slug/.test(out), 'reset did not reapply every migration');
    prismaCli(['migrate', 'status']);
  });

  await step('db-push', () => {
    fs.writeFileSync(schemaPath, firstSchema);
    prismaCli(['db', 'push', '--force-reset', '--skip-generate']);
    const again = prismaCli(['db', 'push', '--skip-generate']);
    expect(/already in sync/.test(again), 'second db push was not a no-op');
  });
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
  if (e && e.code) parts.push(`code: ${e.code}`);
  if (e && e.meta) parts.push(`meta: ${JSON.stringify(e.meta)}`);
  return parts.join('\n');
}

function prismaCli(args) {
  console.log(`$ prisma ${args.join(' ')}`);
  const res = spawnSync(path.join(__dirname, 'node_modules', '.bin', 'prisma'), args, {
    cwd: __dirname,
    env: process.env,
    encoding: 'utf8',
  });
  const out = `${res.stdout || ''}${res.stderr || ''}`;
  console.log(out);
  if (res.error) throw res.error;
  if (res.status !== 0) {
    throw new Error(`prisma ${args.join(' ')} exited with ${res.status}\n${out}`);
  }
  return res.stdout;
}

function expect(condition, message) {
  if (!condition) throw new Error(message);
}

function mysqlUrl(db) {
  const user = encodeURIComponent(env.E2E_USER);
  const password = encodeURIComponent(env.E2E_PASSWORD);
  const params = new URLSearchParams({ sslaccept: 'strict', sslcert: env.E2E_CA });
  return `mysql://${user}:${password}@${env.E2E_HOST}:${env.E2E_PORT}/${db}?${params}`;
}
