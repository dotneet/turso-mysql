#!/bin/sh
# Drops and recreates the named databases on one server, straight to the
# server (no proxy). Usage: reset-db.sh HOST DB...
set -eu
host="$1"
shift
run() {
  MYSQL_PWD="${E2E_PASSWORD}" mysql --no-defaults --protocol=tcp --host="${host}" --port=3306 \
    --user=e2e --ssl-mode=VERIFY_IDENTITY --ssl-ca=/e2e/tls/ca.pem \
    --default-character-set=utf8mb4 -e "$1"
}
for db in "$@"; do
  run "DROP DATABASE IF EXISTS \`${db}\`" 2>/dev/null || run "DROP DATABASE \`${db}\`" 2>/dev/null || true
  run "CREATE DATABASE \`${db}\`"
done
