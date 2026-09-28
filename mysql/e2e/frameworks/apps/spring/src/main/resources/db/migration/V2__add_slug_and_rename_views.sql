ALTER TABLE posts ADD COLUMN slug VARCHAR(220) NULL AFTER title;
ALTER TABLE posts RENAME COLUMN views TO view_count;
CREATE UNIQUE INDEX uk_posts_slug ON posts (slug);
UPDATE posts SET slug = CONCAT('post-', id);
