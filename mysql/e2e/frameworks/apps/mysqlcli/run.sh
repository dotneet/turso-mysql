#!/bin/sh
# The mysql command-line client, used the way people use it by hand: load a
# schema file, load data, look at the schema, query, and run a transaction.
# --force keeps going past a refused statement so the wire log shows them all.
set -u
. /e2e/src/common/steps.sh
db=mysqlcli
here=/e2e/src/apps/mysqlcli

# Without --default-character-set the 8.4 client derives the charset from the
# locale; this container has none, so it asks for latin1 in the handshake.
connect_with_locale_charset() {
  MYSQL_PWD="${E2E_PASSWORD}" mysql --no-defaults --protocol=tcp --host="${E2E_HOST}" \
    --port="${E2E_PORT}" --user="${E2E_USER}" --ssl-mode=VERIFY_IDENTITY --ssl-ca="${E2E_CA}" \
    -e "SELECT @@character_set_client"
}
step connect-latin1-handshake connect_with_locale_charset
step connect sql -e "SELECT VERSION(), @@version_comment, DATABASE(), CURRENT_USER(), @@sql_mode"
step status sql -e "status"
step schema-file sql --force "${db}" <"${here}/schema.sql"
step data-file sql --force "${db}" <"${here}/data.sql"
step show-tables sql "${db}" -e "SHOW TABLES; SHOW FULL TABLES"
step describe sql "${db}" -e "DESCRIBE users; SHOW COLUMNS FROM posts; SHOW FULL COLUMNS FROM users"
step show-create sql "${db}" -e "SHOW CREATE TABLE users\G SHOW CREATE TABLE posts\G"
step show-index sql "${db}" -e "SHOW INDEX FROM posts; SHOW KEYS FROM users"
step show-table-status sql "${db}" -e "SHOW TABLE STATUS"
step join sql "${db}" -e "SELECT u.name, p.title, GROUP_CONCAT(t.name ORDER BY t.name SEPARATOR ',') AS tags
  FROM users u JOIN posts p ON p.user_id = u.id
  LEFT JOIN post_tag pt ON pt.post_id = p.id LEFT JOIN tags t ON t.id = pt.tag_id
  GROUP BY u.id, p.id ORDER BY p.id"
step aggregate sql "${db}" -e "SELECT is_active, COUNT(*), SUM(balance), AVG(balance), MAX(created_at)
  FROM users GROUP BY is_active HAVING COUNT(*) > 0 ORDER BY is_active"
step json sql "${db}" -e "SELECT name, JSON_EXTRACT(profile, '\$.city'), profile->>'\$.city'
  FROM users WHERE JSON_CONTAINS(profile, '\"a\"', '\$.tags')"
step pagination sql "${db}" -e "SELECT SQL_CALC_FOUND_ROWS id, title FROM posts ORDER BY id LIMIT 2 OFFSET 1; SELECT FOUND_ROWS()"
step dates sql "${db}" -e "SELECT DATE_FORMAT(created_at, '%Y-%m'), DATE_SUB(NOW(), INTERVAL 1 DAY) < created_at,
  TIMESTAMPDIFF(DAY, created_at, NOW()) >= 0 FROM users ORDER BY id"
step transaction sql "${db}" -e "START TRANSACTION; UPDATE users SET balance = balance - 10 WHERE id = 1;
  UPDATE users SET balance = balance + 10 WHERE id = 2; COMMIT;
  BEGIN; DELETE FROM posts WHERE id = 2; ROLLBACK; SELECT COUNT(*) FROM posts"
step fk-cascade sql "${db}" -e "DELETE FROM users WHERE id = 3; SELECT COUNT(*) FROM posts WHERE user_id = 3"
fk_violation_is_refused() {
  ! sql "${db}" -e "INSERT INTO posts (user_id, title) VALUES (999, 'x')"
}
step fk-violation-refused fk_violation_is_refused
step upsert sql "${db}" -e "INSERT INTO tags (name) VALUES ('news') ON DUPLICATE KEY UPDATE name = VALUES(name);
  INSERT IGNORE INTO tags (name) VALUES ('rust'); REPLACE INTO tags (id, name) VALUES (3, 'sql2')"
step alter sql --force "${db}" -e "ALTER TABLE posts ADD COLUMN slug VARCHAR(200) NULL AFTER title;
  CREATE INDEX posts_slug_index ON posts (slug); ALTER TABLE posts DROP INDEX posts_slug_index;
  ALTER TABLE posts MODIFY views BIGINT NOT NULL DEFAULT 0; ALTER TABLE posts RENAME COLUMN slug TO handle;
  ALTER TABLE posts DROP COLUMN handle"
step information-schema sql "${db}" -e "SELECT TABLE_NAME, TABLE_TYPE, ENGINE FROM information_schema.TABLES
  WHERE TABLE_SCHEMA = DATABASE() ORDER BY TABLE_NAME;
  SELECT COLUMN_NAME, DATA_TYPE, COLUMN_TYPE, IS_NULLABLE, COLUMN_DEFAULT, EXTRA FROM information_schema.COLUMNS
  WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'users' ORDER BY ORDINAL_POSITION"
step show-databases sql -e "SHOW DATABASES"
