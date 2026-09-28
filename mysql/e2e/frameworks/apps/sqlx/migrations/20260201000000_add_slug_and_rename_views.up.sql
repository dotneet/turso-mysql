ALTER TABLE posts
  ADD COLUMN slug VARCHAR(220) NULL AFTER title,
  RENAME COLUMN views TO view_count,
  MODIFY title VARCHAR(255) NOT NULL;
CREATE UNIQUE INDEX posts_slug ON posts (slug);
