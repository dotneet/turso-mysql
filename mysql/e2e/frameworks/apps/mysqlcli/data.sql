INSERT INTO users (email, name, balance, is_active, profile, created_at) VALUES
  ('alice@example.com', 'Alice', 120.50, 1, '{"city": "Oslo", "tags": ["a", "b"]}', '2026-01-02 03:04:05'),
  ('bob@example.com', 'Bob', 0.00, 0, NULL, '2026-02-03 04:05:06'),
  ('carol@example.com', 'Carol', 99999.99, 1, '{"city": "Lima"}', NOW());

INSERT INTO posts (user_id, title, body, status, views, published_at) VALUES
  (1, 'Hello', 'First post', 'published', 10, '2026-03-01 10:00:00.123456'),
  (1, 'Second', 'More text', 'draft', 0, NULL),
  (3, 'Carol''s post', 'It''s quoted', 'published', 5, NOW(6));

INSERT INTO tags (name) VALUES ('news'), ('rust'), ('sql');
INSERT INTO post_tag (post_id, tag_id) VALUES (1, 1), (1, 2), (3, 3);
