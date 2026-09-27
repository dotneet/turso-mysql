#!/bin/sh
# Schema steps run Rails' own rake db:* tasks; model steps run in one Rails
# process (script/steps.rb), which records its own steps.
set -u
. /e2e/src/common/steps.sh
cd /app

step_at() {
  echo "=== step $1 at $(date -u +%Y-%m-%dT%H:%M:%S.%NZ)"
  step "$@"
}

ruby_steps() {
  bundle exec ruby script/steps.rb "$@"
}

migrate_changes_nothing() {
  out="$(bundle exec rake db:migrate 2>&1)"
  status=$?
  printf '%s\n' "${out}"
  [ "${status}" -eq 0 ] || return 1
  if printf '%s\n' "${out}" | grep -Eq 'migrating|\)  (CREATE|ALTER|DROP|RENAME) '; then
    echo "the second db:migrate changed the schema"
    return 1
  fi
}

all_migrations_up() {
  out="$(bundle exec rake db:migrate:status 2>&1)"
  status=$?
  printf '%s\n' "${out}"
  [ "${status}" -eq 0 ] || return 1
  printf '%s\n' "${out}" | grep -Eq '^ +up +20260101000004' || { echo "migrations are not all up"; return 1; }
  ! printf '%s\n' "${out}" | grep -Eq '^ +down '
}

dump_and_inspect_schema() {
  bundle exec rake db:schema:dump || return 1
  cat db/schema.rb
  ruby_steps --check introspect
}

second_migration() {
  cp db/second_migration/*.rb db/migrate/ || return 1
  bundle exec rake db:migrate || return 1
  ruby_steps --check after-alter
}

roll_back_second_migration() {
  bundle exec rake db:rollback || return 1
  ruby_steps --check after-rollback
}

load_schema() {
  bundle exec rake db:schema:load || return 1
  ruby_steps --check after-schema-load
}

ruby_steps connect-mysql2-default-encoding connect
step_at migrate bundle exec rake db:migrate
step_at migrate-again migrate_changes_nothing
step_at migrate-status all_migrations_up
step_at introspect dump_and_inspect_schema
ruby_steps insert relations update delete pagination aggregate transaction-commit transaction-rollback \
  savepoint json upsert insert-all
step_at alter-migration second_migration
step_at rollback-migration roll_back_second_migration
ruby_steps drop-tables
step_at schema-load load_schema
