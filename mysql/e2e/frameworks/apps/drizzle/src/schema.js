// The blog schema as first released. drizzle-v1/ holds the migration that
// `drizzle-kit generate` wrote for it.
const { relations } = require('drizzle-orm');
const {
  bigint, boolean, datetime, decimal, index, int, json, mysqlTable, primaryKey, text, timestamp, varchar,
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
  title: varchar('title', { length: 200 }).notNull(),
  body: text('body'),
  publishedAt: datetime('published_at'),
  views: int('views').notNull().default(0),
}, (t) => [index('posts_user_published').on(t.userId, t.publishedAt)]);

const tags = mysqlTable('tags', {
  id: id(),
  name: varchar('name', { length: 100 }).notNull().unique(),
});

const postTags = mysqlTable('post_tags', {
  postId: bigint('post_id', { mode: 'number', unsigned: true }).notNull().references(() => posts.id, { onDelete: 'cascade' }),
  tagId: bigint('tag_id', { mode: 'number', unsigned: true }).notNull().references(() => tags.id, { onDelete: 'cascade' }),
}, (t) => [primaryKey({ name: 'post_tags_post_id_tag_id', columns: [t.postId, t.tagId] })]);

const usersRelations = relations(users, ({ many }) => ({ posts: many(posts) }));
const postsRelations = relations(posts, ({ one, many }) => ({
  user: one(users, { fields: [posts.userId], references: [users.id] }),
  postTags: many(postTags),
}));
const tagsRelations = relations(tags, ({ many }) => ({ postTags: many(postTags) }));
const postTagsRelations = relations(postTags, ({ one }) => ({
  post: one(posts, { fields: [postTags.postId], references: [posts.id] }),
  tag: one(tags, { fields: [postTags.tagId], references: [tags.id] }),
}));

module.exports = { users, posts, tags, postTags, usersRelations, postsRelations, tagsRelations, postTagsRelations };
