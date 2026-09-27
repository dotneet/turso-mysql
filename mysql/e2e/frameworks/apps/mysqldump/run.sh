#!/bin/sh
# mysqldump with its default options and with the usual backup options, then
# the dump replayed into a second database on the same server.
set -u
. /e2e/src/common/steps.sh
seed=/e2e/src/apps/mysqlcli
work=/tmp/dump
mkdir -p "${work}"

dump() {
  MYSQL_PWD="${E2E_PASSWORD}" mysqldump --no-defaults --protocol=tcp \
    --host="${E2E_HOST}" --port="${E2E_PORT}" --user="${E2E_USER}" \
    --ssl-mode=VERIFY_IDENTITY --ssl-ca="${E2E_CA}" "$@"
}

dump_default() {
  dump dump_src >"${work}/default.sql" && cp "${work}/default.sql" "${E2E_OUT}/default.sql"
}

dump_backup() {
  dump --single-transaction --routines --triggers --hex-blob dump_src >"${work}/backup.sql" \
    && cp "${work}/backup.sql" "${E2E_OUT}/backup.sql"
}

replay() {
  sql dump_dst <"$1"
}

rows_of() {
  sql --batch --skip-column-names "$1" -e "
    SELECT id, email, name, balance, is_active, profile, created_at FROM users ORDER BY id;
    SELECT id, user_id, title, body, status, views, published_at FROM posts ORDER BY id;
    SELECT id, name FROM tags ORDER BY id;
    SELECT post_id, tag_id FROM post_tag ORDER BY post_id, tag_id"
}

same_rows() {
  src="$(rows_of dump_src)" && dst="$(rows_of dump_dst)" || return 1
  if [ "${src}" != "${dst}" ]; then
    printf 'rows differ\n--- dump_src\n%s\n--- dump_dst\n%s\n' "${src}" "${dst}"
    return 1
  fi
}

step seed-schema sql --force dump_src <"${seed}/schema.sql"
step seed-data sql --force dump_src <"${seed}/data.sql"
step dump-default dump_default
step dump-no-data dump --no-data dump_src
step dump-backup-options dump_backup
step replay-default replay "${work}/default.sql"
step same-rows-after-replay same_rows
step replay-backup replay "${work}/backup.sql"
step same-rows-after-second-replay same_rows
