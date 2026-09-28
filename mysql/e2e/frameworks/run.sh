#!/usr/bin/env bash
# Runs framework apps against turso-mysql and against MySQL 8.4, then writes
# .run/results/summary.md.
#
#   mysql/e2e/frameworks/run.sh                 # every app, then stop the servers
#   mysql/e2e/frameworks/run.sh laravel gorm    # some apps
#   mysql/e2e/frameworks/run.sh --up            # build and start the servers only
#   mysql/e2e/frameworks/run.sh --dev laravel   # rerun one app on running servers
#   mysql/e2e/frameworks/run.sh --down          # stop the servers
#
# E2E_KEEP=1 leaves the servers running after a full run.
#
# To run a second copy of the harness next to this one (another checkout or
# worktree), give it its own names so neither touches the other's containers,
# network or build cache:
#
#   COMPOSE_PROJECT_NAME=turso-e2e-more E2E_CARGO_TARGET_VOLUME=turso-e2e-more-cargo-target run.sh ...
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
all_apps=(mysqlcli mysqldump laravel prisma typeorm django rails sqlalchemy gorm sequelize drizzle spring efcore sqlx)
rust_image=rust:1.88-bookworm@sha256:af306cfa71d987911a781c37b59d7d67d934f49684058f96cf72079c3626bfe0
python_image=python:3.12.12-slim-bookworm@sha256:593bd06efe90efa80dc4eee3948be7c0fde4134606dd40d8dd8dbcade98e669c

export E2E_RUN_DIR="${E2E_RUN_DIR:-${here}/.run}"
# Prefix of every container, network and app image of this copy of the harness.
export COMPOSE_PROJECT_NAME="${COMPOSE_PROJECT_NAME:-turso-e2e-fw}"
prefix="${COMPOSE_PROJECT_NAME}"
# The build cache records which source it was built from, so a copy built from
# another checkout must not share it (cargo would skip rebuilding changed crates).
export E2E_CARGO_TARGET_VOLUME="${E2E_CARGO_TARGET_VOLUME:-turso-e2e-cargo-target}"
# One database per app, plus the extra ones some apps need.
export E2E_DATABASES="mysqlcli laravel prisma prisma_shadow typeorm django rails sqlalchemy gorm dump_src dump_dst sequelize drizzle spring spring_ssp efcore sqlx"

main() {
  case "${1:-}" in
    --up) up ;;
    --down) down ;;
    --dev)
      shift
      use_running_servers
      mkdir -p "${E2E_RUN_DIR}/results"
      run_apps "$@"
      report
      ;;
    *)
      local started
      started=$(date +%s)
      [[ "${E2E_KEEP:-0}" == 1 ]] || trap down EXIT
      up
      rm -rf "${E2E_RUN_DIR}/results"
      mkdir -p "${E2E_RUN_DIR}/results"
      if [[ "$#" -gt 0 ]]; then run_apps "$@"; else run_apps "${all_apps[@]}"; fi
      report
      log "done in $(($(date +%s) - started))s; summary: ${E2E_RUN_DIR}/results/summary.md"
      ;;
  esac
}

compose() {
  docker compose -f "${here}/compose.yaml" "$@"
}

log() {
  printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*"
}

up() {
  mkdir -p "${E2E_RUN_DIR}"/{bin,tls,tls-mysql,turso-log,results}
  rm -f "${E2E_RUN_DIR}/turso-log/"*
  # Test-only password, new for every server start and kept only in the run directory.
  E2E_PASSWORD="$(od -An -N16 -tx1 /dev/urandom | tr -d ' \n')"
  export E2E_PASSWORD
  (umask 077 && printf '%s' "${E2E_PASSWORD}" >"${E2E_RUN_DIR}/password")

  log "generating throwaway TLS material"
  docker run --rm --name "${prefix}-tls" \
    -v "${here}/server/gen-tls.sh:/gen-tls.sh:ro" -v "${E2E_RUN_DIR}/tls:/out" \
    --entrypoint /gen-tls.sh "${rust_image}"
  cp "${E2E_RUN_DIR}/tls/"{ca.pem,server-chain.pem,server-key.pem} "${E2E_RUN_DIR}/tls-mysql/"

  log "building turso-mysql (debug) and the SQL proxy"
  compose --profile build run --rm --name "${prefix}-build" build >"${E2E_RUN_DIR}/build.log" 2>&1 \
    || { tail -30 "${E2E_RUN_DIR}/build.log"; exit 1; }
  compose --profile build run --rm --name "${prefix}-sqlproxy-build" sqlproxy-build

  log "starting turso and mysql"
  compose --profile servers down --remove-orphans --timeout 10 >/dev/null 2>&1 || true
  compose --profile servers up -d --wait turso mysql
}

down() {
  compose --profile servers --profile apps down --remove-orphans --timeout 10 >/dev/null 2>&1 || true
}

use_running_servers() {
  [[ -f "${E2E_RUN_DIR}/password" ]] || { echo "no running servers; start them with $0 --up" >&2; exit 2; }
  E2E_PASSWORD="$(cat "${E2E_RUN_DIR}/password")"
  export E2E_PASSWORD
}

databases_of() {
  case "$1" in
    prisma) echo "prisma prisma_shadow" ;;
    mysqldump) echo "dump_src dump_dst" ;;
    spring) echo "spring spring_ssp" ;;
    *) echo "$1" ;;
  esac
}

# Each app runs against turso first, then MySQL, each time on freshly created databases.
run_apps() {
  local targets="${E2E_TARGETS:-turso mysql}"
  for app in "$@"; do
    log "building ${app}"
    compose --profile apps build "${app}" >"${E2E_RUN_DIR}/results/${app}.build.log" 2>&1 \
      || { log "${app}: image build failed, see results/${app}.build.log"; continue; }
    for target in ${targets}; do
      log "${app} against ${target}"
      # A server that stopped fails the reset; that is reported and the next
      # target still runs, rather than `set -e` ending the whole run silently.
      # shellcheck disable=SC2046
      compose --profile apps run --rm --name "${prefix}-dbadmin-${app}-${target}" \
        dbadmin "${target}" $(databases_of "${app}") >/dev/null \
        || { log "${app} against ${target}: resetting its databases failed"; server_still_running "${target}"; continue; }
      compose --profile apps run --rm --name "${prefix}-${app}-${target}" \
        -e E2E_APP="${app}" -e E2E_TARGET="${target}" -e E2E_UPSTREAM="${target}:3306" \
        "${app}" >"${E2E_RUN_DIR}/results/${app}-${target}.run.log" 2>&1 \
        || log "${app} against ${target}: container failed, see results/${app}-${target}.run.log"
      server_still_running "${target}"
    done
  done
}

# Says so loudly when a server stopped, with the end of its log: every later
# step would otherwise fail only as a refused connection.
server_still_running() {
  local target="$1"
  local state
  state="$(docker inspect -f '{{.State.Status}}' "${prefix}-${target}-1" 2>/dev/null || echo missing)"
  if [[ "${state}" != "running" ]]; then
    log "SERVER ${target} IS ${state}"
    if [[ "${target}" == "turso" && -f "${E2E_RUN_DIR}/turso-log/server.log" ]]; then
      tail -n 20 "${E2E_RUN_DIR}/turso-log/server.log" >&2
    fi
  fi
}

report() {
  log "writing the report"
  docker run --rm --name "${prefix}-report" \
    -v "${here}/common/report.py:/report.py:ro" -v "${E2E_RUN_DIR}/results:/results" \
    "${python_image}" python /report.py /results
}

main "$@"
