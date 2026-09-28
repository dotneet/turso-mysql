DROP INDEX posts_slug ON posts;
ALTER TABLE posts
  DROP COLUMN slug,
  RENAME COLUMN view_count TO views,
  MODIFY title VARCHAR(200) NOT NULL;
