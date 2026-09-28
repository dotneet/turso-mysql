// Sequelize 6 used the way a typical Express app uses it: sequelize-cli
// migrations, models with associations, findAndCountAll paging, managed and
// nested transactions, optimistic locking and model sync. Every step is
// recorded in $E2E_OUT/steps.jsonl and a failing step never stops the run.
'use strict';

const fs = require('node:fs');
const path = require('node:path');
const { execFileSync } = require('node:child_process');
const { Sequelize, DataTypes, Op, QueryTypes, Transaction, OptimisticLockError } = require('sequelize');
const config = require('./config/config.js').e2e;

const env = process.env;
const sequelize = new Sequelize(config.database, config.username, config.password, config);
const queryInterface = sequelize.getQueryInterface();

const User = sequelize.define('User', {
  id: { type: DataTypes.BIGINT, autoIncrement: true, primaryKey: true },
  email: { type: DataTypes.STRING(191), allowNull: false, unique: true },
  name: { type: DataTypes.STRING(100), allowNull: false },
  balance: { type: DataTypes.DECIMAL(10, 2), allowNull: false, defaultValue: 0 },
  isActive: { type: DataTypes.BOOLEAN, allowNull: false, defaultValue: true },
  profile: { type: DataTypes.JSON, allowNull: true },
}, { tableName: 'users', version: true });

const Post = sequelize.define('Post', {
  id: { type: DataTypes.BIGINT, autoIncrement: true, primaryKey: true },
  title: { type: DataTypes.STRING(200), allowNull: false },
  body: { type: DataTypes.TEXT, allowNull: true },
  publishedAt: { type: DataTypes.DATE, allowNull: true },
  views: { type: DataTypes.INTEGER, allowNull: false, defaultValue: 0 },
}, { tableName: 'posts', indexes: [{ name: 'posts_user_published', fields: ['user_id', 'published_at'] }] });

const Tag = sequelize.define('Tag', {
  id: { type: DataTypes.BIGINT, autoIncrement: true, primaryKey: true },
  name: { type: DataTypes.STRING(100), allowNull: false, unique: true },
}, { tableName: 'tags', timestamps: false });

const PostTag = sequelize.define('PostTag', {}, { tableName: 'post_tags', timestamps: false });

User.hasMany(Post, { foreignKey: { name: 'userId', allowNull: false }, as: 'posts', onDelete: 'CASCADE' });
Post.belongsTo(User, { foreignKey: { name: 'userId', allowNull: false }, as: 'user' });
Post.belongsToMany(Tag, { through: PostTag, foreignKey: 'postId', otherKey: 'tagId', as: 'tags' });
Tag.belongsToMany(Post, { through: PostTag, foreignKey: 'tagId', otherKey: 'postId', as: 'posts' });

// Created by sync(), not by a migration, the way many apps keep side tables.
const AuditLog = sequelize.define('AuditLog', {
  action: { type: DataTypes.STRING(50), allowNull: false },
  payload: { type: DataTypes.JSON, allowNull: true },
}, { tableName: 'audit_logs', updatedAt: false });

main().finally(() => sequelize.close());

async function main() {
  await step('connect', async () => {
    await sequelize.authenticate();
    const [row] = await sequelize.query('SELECT VERSION() AS version, DATABASE() AS db', { type: QueryTypes.SELECT });
    console.log(row);
    expect(row.db === config.database, `connected to ${row.db}`);
  });

  await step('migrate', async () => {
    cli('db:migrate');
    const tables = await tableNames();
    for (const t of ['SequelizeMeta', 'users', 'posts', 'tags', 'post_tags']) expect(tables.includes(t), `table ${t} is missing from ${tables}`);
  });

  await step('migrate-status', async () => {
    const out = cli('db:migrate:status');
    expect(/up\s+20260101000000-create-blog\.js/.test(out), 'the migration is not reported as up');
    expect(!/\bdown\b/.test(out), 'a migration is reported as down');
  });

  await step('introspect', async () => {
    const users = await queryInterface.describeTable('users');
    console.log(users);
    expect(users.id.primaryKey && users.id.autoIncrement && users.id.type === 'BIGINT', `users.id is ${JSON.stringify(users.id)}`);
    expect(users.email.type === 'VARCHAR(191)' && !users.email.allowNull, `users.email is ${users.email.type}`);
    expect(users.balance.type === 'DECIMAL(10,2)' && users.balance.defaultValue === '0.00', `users.balance is ${users.balance.type} default ${users.balance.defaultValue}`);
    expect(users.profile.type === 'JSON' && users.profile.allowNull, `users.profile is ${users.profile.type}`);
    expect(users.is_active.type === 'TINYINT(1)', `users.is_active is ${users.is_active.type}`);
    const indexes = await queryInterface.showIndex('users');
    expect(indexes.some((i) => i.unique && i.fields.map((f) => f.attribute).join() === 'email'), 'users.email has no unique index');
    const postIndexes = await queryInterface.showIndex('posts');
    const composite = postIndexes.find((i) => i.name === 'posts_user_published');
    expect(composite && composite.fields.map((f) => f.attribute).join() === 'user_id,published_at', 'posts_user_published is wrong');
    const fks = await queryInterface.getForeignKeyReferencesForTable('posts');
    console.log(fks);
    expect(fks.some((f) => f.columnName === 'user_id' && f.referencedTableName === 'users' && f.referencedColumnName === 'id'), 'posts.user_id foreign key is missing');
    const joinFks = await queryInterface.getForeignKeyReferencesForTable('post_tags');
    expect(joinFks.length === 2, `post_tags has ${joinFks.length} foreign keys`);
  });

  await step('sync', async () => {
    await AuditLog.sync();
    await AuditLog.create({ action: 'created', payload: { by: 'sync' } });
    // The model grows a column and widens another; sync({ alter }) compares
    // the table with the model and alters it.
    const Wider = sequelize.define('AuditLogV2', {
      action: { type: DataTypes.STRING(80), allowNull: false },
      actor: { type: DataTypes.STRING(100), allowNull: true },
      payload: { type: DataTypes.JSON, allowNull: true },
    }, { tableName: 'audit_logs', updatedAt: false });
    await Wider.sync({ alter: true });
    const table = await queryInterface.describeTable('audit_logs');
    expect(table.actor && table.action.type === 'VARCHAR(80)', `audit_logs is ${JSON.stringify(table)}`);
    expect((await Wider.count()) === 1, 'sync({ alter }) lost the row');
    sequelize.modelManager.removeModel(Wider);
  });

  await step('insert', async () => {
    const [news, rust, sql] = await Tag.bulkCreate([{ name: 'news' }, { name: 'rust' }, { name: 'sql' }]);
    expect(news.id !== null && sql.id !== null, 'bulkCreate did not return ids');
    const alice = await User.create({
      email: 'alice@example.com',
      name: 'Alice',
      balance: '100.50',
      profile: { city: 'Tokyo', tags: ['a', 'b'] },
      posts: [
        { title: 'Hello', body: 'First post', publishedAt: new Date('2024-01-02T03:04:05Z'), views: 10 },
        { title: 'Draft', body: 'Not yet', views: 0 },
      ],
    }, { include: [{ model: Post, as: 'posts' }] });
    expect(alice.posts.length === 2 && alice.posts.every((p) => p.id), 'nested create did not assign ids');
    await alice.posts[0].setTags([news, rust]);
    const bob = await User.create({ email: 'bob@example.com', name: 'Bob', balance: '20.25', isActive: false, profile: { city: 'Osaka', tags: ['c'] } });
    await User.create({ email: 'carol@example.com', name: 'Carol', balance: '5.00' });
    const bobPost = await Post.create({ title: 'Bob writes', views: 3, userId: bob.id });
    await bobPost.addTag(sql);
    expect((await Post.count()) === 3, 'expected 3 posts');
    expect((await PostTag.count()) === 3, 'expected 3 post_tags rows');
  });

  await step('relations', async () => {
    const users = await User.findAll({
      include: [{ model: Post, as: 'posts', include: [{ model: Tag, as: 'tags' }] }],
      order: [['id', 'ASC'], [{ model: Post, as: 'posts' }, 'id', 'ASC']],
    });
    expect(users.length === 3, `expected 3 users, got ${users.length}`);
    const names = users[0].posts[0].tags.map((t) => t.name).sort().join();
    expect(names === 'news,rust', `unexpected tags ${names}`);
    const tagged = await Post.findAll({
      include: [
        { model: User, as: 'user', where: { isActive: true } },
        { model: Tag, as: 'tags', where: { name: 'rust' } },
      ],
    });
    expect(tagged.length === 1 && tagged[0].user.email === 'alice@example.com', 'filtered include returned the wrong posts');
    const withoutPosts = await User.findAll({
      include: [{ model: Post, as: 'posts', required: false, attributes: [] }],
      where: { '$posts.id$': null },
    });
    expect(withoutPosts.length === 1 && withoutPosts[0].name === 'Carol', 'anti-join returned the wrong users');
    const bob = await User.findOne({ where: { email: 'bob@example.com' } });
    const bobPosts = await bob.getPosts({ include: [{ model: Tag, as: 'tags' }] });
    expect(bobPosts.length === 1 && bobPosts[0].tags[0].name === 'sql', 'lazy association returned the wrong posts');
    expect((await bob.countPosts()) === 1, 'countPosts is wrong');
  });

  await step('update', async () => {
    const before = await User.findOne({ where: { email: 'alice@example.com' } });
    await new Promise((r) => setTimeout(r, 1100));
    await User.increment({ balance: 9.5 }, { where: { email: 'alice@example.com' } });
    const alice = await User.findOne({ where: { email: 'alice@example.com' } });
    alice.name = 'Alice A.';
    await alice.save();
    await alice.reload();
    expect(Number(alice.balance) === 110 && alice.name === 'Alice A.', `alice is ${alice.name} with ${alice.balance}`);
    expect(alice.updatedAt > before.updatedAt, 'updated_at did not move');
    expect(alice.version > before.version, `version went from ${before.version} to ${alice.version}`);
    const [affected] = await Post.update({ views: sequelize.literal('views + 1') }, { where: { views: { [Op.lt]: 5 } } });
    expect(affected === 2, `bulk update touched ${affected} rows`);
  });

  await step('optimistic-lock', async () => {
    const first = await User.findOne({ where: { email: 'bob@example.com' } });
    const second = await User.findOne({ where: { email: 'bob@example.com' } });
    first.name = 'Bob One';
    await first.save();
    second.name = 'Bob Two';
    let caught;
    try {
      await second.save();
    } catch (e) {
      caught = e;
    }
    expect(caught instanceof OptimisticLockError, `stale save gave ${caught}`);
    const bob = await User.findOne({ where: { email: 'bob@example.com' } });
    expect(bob.name === 'Bob One', `bob is ${bob.name}`);
  });

  await step('delete', async () => {
    const dave = await User.create({ email: 'dave@example.com', name: 'Dave', posts: [{ title: 'Bye' }] }, { include: [{ model: Post, as: 'posts' }] });
    await dave.destroy();
    expect((await Post.count({ where: { userId: dave.id } })) === 0, 'ON DELETE CASCADE left posts behind');
    await Tag.create({ name: 'temp' });
    const removed = await Tag.destroy({ where: { name: { [Op.like]: 'te%' } } });
    expect(removed === 1, `destroy removed ${removed} tags`);
  });

  await step('pagination', async () => {
    const page = await Post.findAndCountAll({ order: [['id', 'ASC']], offset: 1, limit: 2 });
    expect(page.rows.length === 2 && page.count === 3, `page ${page.rows.length} of ${page.count}`);
    // With a joined collection Sequelize pages in a subquery and counts DISTINCT ids.
    const withTags = await Post.findAndCountAll({
      include: [{ model: Tag, as: 'tags' }, { model: User, as: 'user', attributes: ['name'] }],
      distinct: true,
      order: [['id', 'ASC']],
      offset: 0,
      limit: 1,
    });
    expect(withTags.rows.length === 1 && withTags.rows[0].tags.length === 2 && withTags.count === 3,
      `paginated include returned ${withTags.rows.length} rows, ${withTags.rows[0] && withTags.rows[0].tags.length} tags, count ${withTags.count}`);
  });

  await step('aggregate', async () => {
    const rows = await User.findAll({
      attributes: [
        'isActive',
        [sequelize.fn('COUNT', sequelize.col('id')), 'count'],
        [sequelize.fn('SUM', sequelize.col('balance')), 'total'],
        [sequelize.fn('AVG', sequelize.col('balance')), 'average'],
        [sequelize.fn('MAX', sequelize.col('created_at')), 'latest'],
      ],
      group: ['isActive'],
      having: sequelize.where(sequelize.fn('COUNT', sequelize.col('id')), Op.gt, 0),
      order: [['isActive', 'ASC']],
      raw: true,
    });
    console.log(rows);
    expect(rows.length === 2 && Number(rows[1].count) === 2 && Number(rows[1].total) === 115, `groups ${JSON.stringify(rows)}`);
    expect((await User.count()) === 3, 'count is wrong');
    expect(Number(await Post.sum('views')) === 15, 'sum is wrong');
    expect(Number(await Post.max('views', { include: [{ model: User, as: 'user', where: { email: 'alice@example.com' } }] })) === 10, 'max over an include is wrong');
    const perUser = await User.count({ include: [{ model: Post, as: 'posts', required: true }], group: ['User.id'], distinct: true });
    expect(perUser.length === 2, `grouped count returned ${JSON.stringify(perUser)}`);
    const busy = await Post.findAll({
      attributes: ['userId', [sequelize.fn('SUM', sequelize.col('Post.views')), 'views']],
      group: ['userId'],
      having: sequelize.literal('SUM(`Post`.`views`) > 5'),
      raw: true,
    });
    expect(busy.length === 1, `having on a sum returned ${busy.length} groups`);
  });

  await step('transaction-commit', async () => {
    await sequelize.transaction(async (transaction) => {
      await User.decrement({ balance: 10 }, { where: { email: 'alice@example.com' }, transaction });
      await User.increment({ balance: 10 }, { where: { email: 'bob@example.com' }, transaction });
      const bob = await User.findOne({ where: { email: 'bob@example.com' }, transaction });
      await Post.create({ title: 'In a transaction', userId: bob.id }, { transaction });
    });
    const bob = await User.findOne({ where: { email: 'bob@example.com' } });
    expect(Number(bob.balance) === 30.25, `bob has ${bob.balance}`);
    expect((await Post.count()) === 4, 'transaction did not commit');
  });

  await step('transaction-rollback', async () => {
    const marker = new Error('roll back on purpose');
    let caught;
    try {
      await sequelize.transaction(async (transaction) => {
        await User.update({ balance: 0 }, { where: { email: 'alice@example.com' }, transaction });
        await Post.destroy({ where: {}, transaction });
        throw marker;
      });
    } catch (e) {
      caught = e;
    }
    expect(caught === marker, `unexpected error ${caught}`);
    const unmanaged = await sequelize.transaction();
    await Tag.update({ name: 'breaking' }, { where: { name: 'news' }, transaction: unmanaged });
    await unmanaged.rollback();
    const alice = await User.findOne({ where: { email: 'alice@example.com' } });
    expect(Number(alice.balance) === 100, `alice has ${alice.balance} after rollback`);
    expect((await Post.count()) === 4, 'rolled back deletes are visible');
    expect((await Tag.count({ where: { name: 'news' } })) === 1, 'rolled back tag rename is visible');
  });

  await step('savepoint', async () => {
    await sequelize.transaction(async (outer) => {
      await Tag.create({ name: 'kept' }, { transaction: outer });
      try {
        // A transaction started inside another one is a SAVEPOINT.
        await sequelize.transaction({ transaction: outer }, async (inner) => {
          await Tag.create({ name: 'dropped' }, { transaction: inner });
          await Tag.create({ name: 'kept' }, { transaction: inner });
        });
      } catch (e) {
        expect(e.name === 'SequelizeUniqueConstraintError', `inner failed with ${e.name}: ${e.message}`);
      }
      await Tag.create({ name: 'after' }, { transaction: outer });
    });
    const names = (await Tag.findAll({ where: { name: ['kept', 'dropped', 'after'] }, order: [['name', 'ASC']] })).map((t) => t.name).join();
    expect(names === 'after,kept', `tags after the savepoint rollback: ${names}`);
  });

  await step('isolation-lock', async () => {
    await sequelize.transaction({ isolationLevel: Transaction.ISOLATION_LEVELS.READ_COMMITTED }, async (transaction) => {
      const carol = await User.findOne({ where: { email: 'carol@example.com' }, lock: transaction.LOCK.UPDATE, transaction });
      carol.balance = Number(carol.balance) + 1;
      await carol.save({ transaction });
      const shared = await Post.findAll({ where: { views: { [Op.gte]: 0 } }, lock: transaction.LOCK.SHARE, transaction });
      expect(shared.length === 4, `FOR SHARE read ${shared.length} posts`);
    });
    const carol = await User.findOne({ where: { email: 'carol@example.com' } });
    expect(Number(carol.balance) === 6, `carol has ${carol.balance}`);
  });

  await step('json', async () => {
    const inTokyo = await User.findAll({ where: { profile: { city: 'Tokyo' } } });
    expect(inTokyo.length === 1 && inTokyo[0].email === 'alice@example.com', 'nested JSON filter returned the wrong users');
    const osaka = await User.findAll({ where: sequelize.where(sequelize.json('profile.city'), 'Osaka') });
    expect(osaka.length === 1 && osaka[0].email === 'bob@example.com', 'sequelize.json filter returned the wrong users');
    // A '$' inside a sequelize.fn() string argument is escaped to '$$', so the path goes in a literal.
    const tagged = await User.count({ where: sequelize.literal(`JSON_CONTAINS(\`profile\`, '"a"', '$.tags')`) });
    expect(tagged === 1, `JSON_CONTAINS matched ${tagged} users`);
    const bob = await User.findOne({ where: { email: 'bob@example.com' } });
    expect(bob.profile.city === 'Osaka' && bob.profile.tags[0] === 'c', 'json column did not round-trip');
    bob.profile = { ...bob.profile, city: 'Kyoto' };
    await bob.save();
    await bob.reload();
    expect(bob.profile.city === 'Kyoto', 'json update did not stick');
  });

  await step('upsert', async () => {
    await Tag.bulkCreate([{ name: 'go' }, { name: 'news' }], { updateOnDuplicate: ['name'] });
    expect((await Tag.count({ where: { name: 'go' } })) === 1 && (await Tag.count({ where: { name: 'news' } })) === 1, 'bulkCreate updateOnDuplicate is wrong');
    const [alice] = await User.upsert({ email: 'alice@example.com', name: 'Alice Upserted', balance: '100.00' });
    console.log(alice.toJSON());
    await User.upsert({ email: 'erin@example.com', name: 'Erin' });
    const reread = await User.findOne({ where: { email: 'alice@example.com' } });
    expect(reread.name === 'Alice Upserted', 'upsert did not update the existing row');
    expect((await User.count()) === 4, 'upsert did not insert the new row');
    const [sqlTag, created] = await Tag.findOrCreate({ where: { name: 'sql' } });
    expect(!created && sqlTag.id, 'findOrCreate created an existing tag');
    const [fresh, made] = await Tag.findOrCreate({ where: { name: 'fresh' } });
    expect(made && fresh.id, 'findOrCreate did not create a new tag');
  });

  await step('raw-query', async () => {
    const named = await sequelize.query('SELECT name FROM users WHERE email = :email', { replacements: { email: 'erin@example.com' }, type: QueryTypes.SELECT });
    expect(named.length === 1 && named[0].name === 'Erin', 'replacements query is wrong');
    // bind parameters go to the server as a prepared statement.
    const bound = await sequelize.query('SELECT COUNT(*) AS n FROM posts WHERE views >= $1 AND title <> $2', { bind: [1, 'x'], type: QueryTypes.SELECT });
    expect(Number(bound[0].n) === 3, `bound query counted ${bound[0].n}`);
    const [, meta] = await sequelize.query('UPDATE posts SET body = CONCAT(COALESCE(body, \'\'), \'!\') WHERE user_id = ?', { replacements: [1] });
    expect(meta.affectedRows === 2, `raw update touched ${meta.affectedRows}`);
  });

  await step('alter-migration', async () => {
    fs.copyFileSync(path.join(__dirname, 'later', '20260201000000-add-slug-and-rename-views.js'),
      path.join(__dirname, 'migrations', '20260201000000-add-slug-and-rename-views.js'));
    cli('db:migrate');
    const posts = await queryInterface.describeTable('posts');
    expect(posts.slug && posts.slug.allowNull, 'posts.slug is missing');
    expect(posts.view_count && !posts.views, 'views was not renamed');
    expect(posts.title.type === 'VARCHAR(255)', `posts.title is ${posts.title.type}`);
    expect((await queryInterface.showIndex('posts')).some((i) => i.name === 'posts_slug' && i.unique), 'posts_slug is missing');
    await sequelize.query("UPDATE posts SET slug = CONCAT('post-', id)");
    const [row] = await sequelize.query('SELECT SUM(view_count) AS v FROM posts', { type: QueryTypes.SELECT });
    expect(Number(row.v) === 15, `view_count sum is ${row.v}`);
  });

  await step('rollback-migration', async () => {
    cli('db:migrate:undo');
    const posts = await queryInterface.describeTable('posts');
    expect(!posts.slug, 'posts.slug is still there');
    expect(posts.views && !posts.view_count, 'view_count was not renamed back');
    expect(posts.title.type === 'VARCHAR(200)', `posts.title is ${posts.title.type}`);
    expect(!(await queryInterface.showIndex('posts')).some((i) => i.name === 'posts_slug'), 'posts_slug is still there');
    expect(/down\s+20260201000000/.test(cli('db:migrate:status')), 'the undone migration is not pending again');
  });

  await step('undo-all', async () => {
    cli('db:migrate:undo:all');
    const tables = await tableNames();
    expect(tables.sort().join() === 'SequelizeMeta,audit_logs', `tables left: ${tables}`);
  });

  await step('sync-force', async () => {
    await sequelize.sync({ force: true });
    const tables = await tableNames();
    for (const t of ['users', 'posts', 'tags', 'post_tags', 'audit_logs']) expect(tables.includes(t), `sync did not create ${t}`);
    const user = await User.create({ email: 'x@example.com', name: 'X', posts: [{ title: 'x' }] }, { include: [{ model: Post, as: 'posts' }] });
    expect(user.posts[0].id, 'insert after sync failed');
    await sequelize.sync({ force: true });
    expect((await User.count()) === 0, 'sync({ force }) kept the old rows');
    await queryInterface.dropAllTables();
    expect((await tableNames()).length === 0, 'dropAllTables left tables');
  });
}

function cli(...args) {
  console.log(`$ sequelize-cli ${args.join(' ')}`);
  try {
    const out = execFileSync(path.join(__dirname, 'node_modules', '.bin', 'sequelize-cli'), [...args, '--env', 'e2e'], {
      cwd: __dirname,
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    console.log(out);
    return out;
  } catch (e) {
    throw new Error(`sequelize-cli ${args.join(' ')} failed:\n${e.stdout}\n${e.stderr}`);
  }
}

async function tableNames() {
  const rows = await sequelize.query('SELECT TABLE_NAME AS name FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE()', { type: QueryTypes.SELECT });
  return rows.map((r) => r.name);
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
  const original = e && (e.original || e.parent);
  if (original) parts.push(`code: ${original.code} errno: ${original.errno} sqlMessage: ${original.sqlMessage}`);
  if (e && e.sql) parts.push(`sql: ${e.sql}`);
  return parts.join('\n');
}

function expect(condition, message) {
  if (!condition) throw new Error(message);
}
