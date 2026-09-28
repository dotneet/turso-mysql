// The next release of the blog schema: posts gain a unique slug and a longer
// title, and comments are new. drizzle-v2/ holds both migrations.
const {
  bigint, boolean, datetime, decimal, index, int, json, mysqlTable, primaryKey, text, timestamp, uniqueIndex, varchar,
} = require('drizzle-orm/mysql-core');

const id = () => bigint('id', { mode: 'number', unsigned: true }).autoincrement().primaryKey();

const users = mysqlTable('users', {
  id: id(),
  email: varchar('email', { length: 191 }).notNull().unique(),
  name: varchar('name', { length: 100 }).notNull(),
  balance: decimal('balance', { precision: 10, scale: 2 }).notNull().default('0.00'),
  isActive: boolean('is_active').notNull().default(true),
  profile: json('profile'),
  createdAt: timestamp('created_at').notNull().defaultNow(),
  updatedAt: timestamp('updated_at').notNull().defaultNow().onUpdateNow(),
});

const posts = mysqlTable('posts', {
  id: id(),
  userId: bigint('user_id', { mode: 'number', unsigned: true }).notNull().references(() => users.id, { onDelete: 'cascade' }),
  title: varchar('title', { length: 255 }).notNull(),
  slug: varchar('slug', { length: 220 }),
  body: text('body'),
  publishedAt: datetime('published_at'),
  views: int('views').notNull().default(0),
}, (t) => [index('posts_user_published').on(t.userId, t.publishedAt), uniqueIndex('posts_slug').on(t.slug)]);

const tags = mysqlTable('tags', {
  id: id(),
  name: varchar('name', { length: 100 }).notNull().unique(),
});

const postTags = mysqlTable('post_tags', {
  postId: bigint('post_id', { mode: 'number', unsigned: true }).notNull().references(() => posts.id, { onDelete: 'cascade' }),
  tagId: bigint('tag_id', { mode: 'number', unsigned: true }).notNull().references(() => tags.id, { onDelete: 'cascade' }),
}, (t) => [primaryKey({ name: 'post_tags_post_id_tag_id', columns: [t.postId, t.tagId] })]);

const comments = mysqlTable('comments', {
  id: id(),
  postId: bigint('post_id', { mode: 'number', unsigned: true }).notNull().references(() => posts.id, { onDelete: 'cascade' }),
  body: text('body').notNull(),
  createdAt: timestamp('created_at').notNull().defaultNow(),
});

module.exports = { users, posts, tags, postTags, comments };
