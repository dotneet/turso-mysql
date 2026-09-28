-- Flyway Community has no undo migrations; a release is reverted by a new one.
DROP INDEX uk_posts_slug ON posts;
ALTER TABLE posts RENAME COLUMN view_count TO views, DROP COLUMN slug;
