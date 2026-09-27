#!/bin/sh
# Laravel 12 used the way a developer does: artisan migrations and schema
# commands, Eloquent through `php artisan e2e:step <name>`, the database cache
# and queue drivers, then migrate:rollback, migrate:fresh and db:seed.
set -u
. /e2e/src/common/steps.sh

export DB_CONNECTION=mysql
export DB_HOST="${E2E_HOST}"
export DB_PORT="${E2E_PORT}"
export DB_DATABASE="${E2E_APP}"
export DB_USERNAME="${E2E_USER}"
export DB_PASSWORD="${E2E_PASSWORD}"
export MYSQL_ATTR_SSL_CA="${E2E_CA}"
export CACHE_STORE=database
export QUEUE_CONNECTION=database

artisan() {
  php artisan --no-ansi --no-interaction "$@"
}

e2e() {
  artisan e2e:step "$1"
}

migrate_is_a_no_op() {
  output="$(artisan migrate --force 2>&1)"
  status=$?
  printf '%s\n' "${output}"
  [ "${status}" -eq 0 ] && printf '%s' "${output}" | grep -q 'Nothing to migrate'
}

every_migration_ran() {
  output="$(artisan migrate:status 2>&1)"
  status=$?
  printf '%s\n' "${output}"
  [ "${status}" -eq 0 ] && ! printf '%s' "${output}" | grep -q 'Pending'
}

run_queued_job() {
  e2e queue-dispatch && artisan queue:work --stop-when-empty --tries=1 && e2e queue-check
}

add_second_migration() {
  cp database/later/*.php database/migrations/ && artisan migrate --force && e2e check-alter
}

# artisan_then_check CHECK ARTISAN_ARGS...: run an artisan command, then e2e:step CHECK.
artisan_then_check() {
  check="$1"
  shift
  artisan "$@" && e2e "${check}"
}

step connect e2e connect
step session-charset e2e session-charset
step migrate artisan migrate --force
step migrate-again migrate_is_a_no_op
step migrate-status every_migration_ran
step introspect e2e introspect
step db-show artisan db:show
step db-table artisan db:table users
step insert e2e insert
step unique-violation e2e unique-violation
step relations e2e relations
step update e2e update
step delete e2e delete
step pagination e2e pagination
step aggregate e2e aggregate
step transaction-commit e2e transaction-commit
step transaction-rollback e2e transaction-rollback
step transaction-savepoint e2e transaction-savepoint
step json e2e json
step upsert e2e upsert
step cache e2e cache
step queue run_queued_job
step alter-migration add_second_migration
step rollback-migration artisan_then_check check-rollback migrate:rollback --force
step migrate-fresh artisan_then_check check-fresh migrate:fresh --force
step db-seed artisan_then_check check-seed db:seed --force
