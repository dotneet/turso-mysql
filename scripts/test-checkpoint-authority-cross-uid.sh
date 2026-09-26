#!/usr/bin/env bash

set -euo pipefail

readonly image="${TURSO_MYSQL_CROSS_UID_IMAGE:-ubuntu:24.04@sha256:33ceb71981b602c1a7443a53469e4dba065f7503eab3078a2d7a57a2ab987517}"
readonly mysql_cli_required="${TURSO_MYSQL_CLI_REQUIRED:-0}"
readonly dump_required="${TURSO_MYSQL_DUMP_REQUIRED:-0}"
readonly dump_oracle_host="${TURSO_MYSQL_DUMP_ORACLE_HOST:-}"
readonly dump_oracle_password="${TURSO_MYSQL_DUMP_ORACLE_PASSWORD:-}"
readonly dump_oracle_ca="${TURSO_MYSQL_DUMP_ORACLE_CA:-}"
readonly gate_network="${TURSO_MYSQL_GATE_NETWORK:-none}"
readonly drivers_required="${TURSO_MYSQL_DRIVERS_REQUIRED:-0}"
readonly orm_required="${TURSO_MYSQL_ORM_REQUIRED:-0}"
readonly p0_required="${TURSO_MYSQL_P0_REQUIRED:-0}"
readonly gorm_trace="${TURSO_MYSQL_GORM_TRACE:-0}"
readonly runtime_trace="${TURSO_MYSQL_RUNTIME_TRACE:-0}"
readonly repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
artifact_dir="${CROSS_UID_ARTIFACT_DIR:-$(pwd)/target/debug}"
test_binary="${1:?pass the compiled privileged_cross_uid test binary}"
runtime_test_binary="${2:?pass the compiled runtime Unix E2E test binary}"
tcp_test_binary="${3:?pass the compiled runtime TCP E2E test binary}"
authority_binary="${artifact_dir}/turso-mysql-checkpoint-authority"
provision_binary="${artifact_dir}/turso-mysql-offline-provision"
runtime_binary="${artifact_dir}/turso-mysql-server"
conformance_binary="${artifact_dir}/mysql-conformance"
create_db_binary="${artifact_dir}/mysql-conformance-create-db"
parity_probe_binary="${artifact_dir}/mysql-parity-probe"
readonly completion_marker='checkpoint authority cross-UID gate: complete'

fail() {
  printf '%s\n' "checkpoint authority cross-UID gate: $*" >&2
  exit 1
}

command -v docker >/dev/null || fail "requires Docker"
if docker info --format '{{.OSType}}' 2>/dev/null | grep -qx linux; then
  run_docker() {
    docker "$@"
  }
elif command -v sudo >/dev/null \
  && sudo -n docker info --format '{{.OSType}}' 2>/dev/null | grep -qx linux; then
  run_docker() {
    sudo -n docker "$@"
  }
else
  fail "requires a Linux Docker daemon accessible directly or through passwordless sudo"
fi

command -v file >/dev/null || fail "requires file"
[[ "${mysql_cli_required}" == 0 || "${mysql_cli_required}" == 1 ]] \
  || fail "TURSO_MYSQL_CLI_REQUIRED must be 0 or 1"
[[ "${dump_required}" == 0 || "${dump_required}" == 1 ]] \
  || fail "TURSO_MYSQL_DUMP_REQUIRED must be 0 or 1"
if [[ "${gate_network}" != none ]]; then
  [[ "${dump_required}" == 1 && -n "${dump_oracle_host}" && -n "${dump_oracle_password}" && -f "${dump_oracle_ca}" ]] \
    || fail "a Docker network requires the dedicated mysqldump oracle fixture"
fi
oracle_mount=()
oracle_ca_path=''
if [[ "${gate_network}" != none ]]; then
  oracle_mount=(--mount "type=bind,src=${dump_oracle_ca},dst=/oracle-ca.pem,readonly")
  oracle_ca_path='/oracle-ca.pem'
fi
[[ "${drivers_required}" == 0 || "${drivers_required}" == 1 ]] \
  || fail "TURSO_MYSQL_DRIVERS_REQUIRED must be 0 or 1"
[[ "${orm_required}" == 0 || "${orm_required}" == 1 ]] \
  || fail "TURSO_MYSQL_ORM_REQUIRED must be 0 or 1"
[[ "${p0_required}" == 0 || "${p0_required}" == 1 ]] \
  || fail "TURSO_MYSQL_P0_REQUIRED must be 0 or 1"

for artifact in "${authority_binary}" "${provision_binary}" "${runtime_binary}" "${test_binary}" "${runtime_test_binary}" "${tcp_test_binary}"; do
  [[ -f "${artifact}" && -x "${artifact}" ]] || fail "missing executable artifact"
  file -Lb "${artifact}" | grep -q 'ELF' || fail "requires Linux executable artifacts"
done
if [[ "${p0_required}" == 1 ]]; then
  for artifact in "${conformance_binary}" "${create_db_binary}" "${parity_probe_binary}"; do
    [[ -f "${artifact}" && -x "${artifact}" ]] || fail "missing P0 conformance executable artifact"
    file -Lb "${artifact}" | grep -q 'ELF' || fail "requires Linux P0 executable artifacts"
  done
fi
report_mount=()
if [[ "${p0_required}" == 1 ]]; then
  report_dir="${CROSS_UID_REPORT_DIR:-${repo_root}/target/mysql-turso-p0-report}"
  mkdir -p "${report_dir}"
  report_dir="$(cd "${report_dir}" && pwd -P)"
  report_mount=(--mount "type=bind,src=${report_dir},dst=/probe-report")
fi

artifact_dir="$(cd "${artifact_dir}" && pwd -P)"
test_binary="$(cd "$(dirname "${test_binary}")" && pwd -P)/$(basename "${test_binary}")"
runtime_test_binary="$(cd "$(dirname "${runtime_test_binary}")" && pwd -P)/$(basename "${runtime_test_binary}")"
tcp_test_binary="$(cd "$(dirname "${tcp_test_binary}")" && pwd -P)/$(basename "${tcp_test_binary}")"
authority_binary="${artifact_dir}/turso-mysql-checkpoint-authority"
provision_binary="${artifact_dir}/turso-mysql-offline-provision"
runtime_binary="${artifact_dir}/turso-mysql-server"
readonly artifact_dir
readonly test_binary
readonly runtime_test_binary
readonly tcp_test_binary
readonly authority_binary
readonly provision_binary
readonly runtime_binary
[[ "${test_binary}" == "${artifact_dir}/deps/"* ]] \
  || fail "test artifact must be below the artifact deps directory"
[[ "${runtime_test_binary}" == "${artifact_dir}/deps/"* ]] \
  || fail "runtime test artifact must be below the artifact deps directory"
[[ "${tcp_test_binary}" == "${artifact_dir}/deps/"* ]] \
  || fail "TCP runtime test artifact must be below the artifact deps directory"

gate_log="$(mktemp)"
if run_docker run --rm --interactive --user 0:0 --network "${gate_network}" --read-only \
  --tmpfs /run:rw,mode=755 \
  --mount "type=bind,src=${artifact_dir},dst=/artifacts,readonly" \
  "${oracle_mount[@]+"${oracle_mount[@]}"}" \
  --mount "type=bind,src=${repo_root}/mysql/conformance,dst=/mysql-conformance,readonly" \
  "${report_mount[@]+"${report_mount[@]}"}" \
  -e "TURSO_MYSQL_CROSS_UID_TEST=$(basename "${test_binary}")" \
  -e "TURSO_MYSQL_RUNTIME_TEST=$(basename "${runtime_test_binary}")" \
  -e "TURSO_MYSQL_TCP_TEST=$(basename "${tcp_test_binary}")" \
  -e "TURSO_MYSQL_CLI_REQUIRED=${mysql_cli_required}" \
  -e "TURSO_MYSQL_DUMP_REQUIRED=${dump_required}" \
  -e "TURSO_MYSQL_DUMP_ORACLE_HOST=${dump_oracle_host}" \
  -e "TURSO_MYSQL_DUMP_ORACLE_PASSWORD=${dump_oracle_password}" \
  -e "TURSO_MYSQL_DUMP_ORACLE_CA=${oracle_ca_path}" \
  -e "TURSO_MYSQL_DRIVERS_REQUIRED=${drivers_required}" \
  -e "TURSO_MYSQL_ORM_REQUIRED=${orm_required}" \
  -e "TURSO_MYSQL_P0_REQUIRED=${p0_required}" \
  -e "TURSO_MYSQL_GORM_TRACE=${gorm_trace}" \
  -e "TURSO_MYSQL_RUNTIME_TRACE=${runtime_trace}" \
  "${image}" bash -s >"${gate_log}" 2>&1 <<'INNER'
set -euo pipefail

export LC_ALL=C

readonly service_uid=41001
readonly client_uid=41002
readonly foreign_uid=41004
readonly shared_gid=41003
readonly authority_id='cross-uid-gate'
readonly root='/run/turso-mysql-cross-uid'
readonly state_root="${root}/state"
readonly socket_root="${root}/socket"
readonly account_root="${root}/accounts"
export TURSO_MYSQL_RUNTIME_TEST_ROOT="${root}/runtime"
readonly socket_path="${socket_root}/authority.sock"
readonly service_log="${root}/authority.log"
readonly authority_binary='/artifacts/turso-mysql-checkpoint-authority'
readonly provision_binary='/artifacts/turso-mysql-offline-provision'
readonly runtime_binary='/artifacts/turso-mysql-server'
readonly runtime_test_binary="/artifacts/deps/${TURSO_MYSQL_RUNTIME_TEST:?}"
readonly tcp_test_binary="/artifacts/deps/${TURSO_MYSQL_TCP_TEST:?}"
readonly test_binary="/artifacts/deps/${TURSO_MYSQL_CROSS_UID_TEST:?}"
readonly runtime_test_name='mysql_async_0_37_1_bootstrap_authenticates_and_serves_prepared_queries_and_pool_reset_over_a_unix_socket'
readonly runtime_mediumint_test_name='mysql_async_0_37_1_mediumint_result_metadata_and_boundaries_over_a_unix_socket'
readonly runtime_prepared_quota_test_name='mysql_async_0_37_1_prepared_statement_quota_and_reset_over_a_unix_socket'
readonly runtime_table_test_name='mysql_async_0_37_1_table_grants_authorize_records_and_deny_other_over_a_unix_socket'
readonly tcp_test_name='mysql_async_0_37_1_over_tls_tcp_validates_localhost_and_releases_port'
readonly value_test_name='mysql_value_regressions_over_tls_tcp'
readonly sql_admin_test_name='sql_account_administration_persists_and_reauthorizes_over_tls_tcp'
readonly auto_increment_test_name='mixed_auto_increment_ids_over_tls_tcp'
readonly mysql_cli_test_name='mysql_cli_8_0_46_over_tls_tcp_exercises_schema_data_transactions_and_reconnect'
readonly dump_test_name='mysqldump_8_0_46_lock_tables_over_tls_tcp_roundtrips_schema_and_data'
readonly go_driver_test_name='go_sql_driver_1_9_3_over_tls_tcp_exercises_prepared_crud_and_migration'
readonly jdbc_driver_test_name='connector_j_9_6_0_over_tls_tcp_exercises_prepared_crud_and_migration'
readonly gorm_test_name='gorm_1_31_2_over_tls_tcp_exercises_schema_migration_and_relations'
readonly hibernate_test_name='hibernate_6_6_0_over_tls_tcp_exercises_schema_migration_and_relations'

fail() {
  printf '%s\n' "checkpoint authority cross-UID fixture: $*" >&2
  exit 1
}

command -v setpriv >/dev/null || fail "container image does not provide setpriv"
command -v timeout >/dev/null || fail "container image does not provide timeout"
[[ "$(id -u)" == 0 && "$(id -g)" == 0 ]] || fail "container entrypoint is not root"

run_as() {
  local uid="$1"
  shift
  setpriv --reuid="${uid}" --regid="${shared_gid}" --clear-groups "$@"
}

runtime_failures=0
run_runtime_test() {
  local binary="$1"
  local selector="$2"
  local test_status=0
  printf 'runtime E2E: %s\n' "${selector}"
  run_as "${client_uid}" "${binary}" --ignored --exact "${selector}" || test_status=$?
  printf 'runtime E2E exit: %s %s\n' "${selector}" "${test_status}"
  if [[ "${test_status}" -ne 0 ]]; then
    runtime_failures=$((runtime_failures + 1))
  fi
}

assert_runtime_test_name() {
  local expected="$1"
  local listed_tests count
  local binary="$2"
  if ! listed_tests="$(run_as "${client_uid}" "${binary}" --list 2>/dev/null)"; then
    fail "could not list the runtime E2E tests"
  fi
  count="$(awk -v expected="${expected}" '$0 == expected ": test" { count += 1 } END { print count + 0 }' <<<"${listed_tests}")"
  if [[ "${count}" -ne 1 ]]; then
    fail "expected one runtime E2E test named ${expected}, found ${count}"
  fi
}

assert_identity() {
  local uid="$1"
  [[ "$(run_as "${uid}" id -u)" == "${uid}" ]] || fail "effective UID setup failed"
  [[ "$(run_as "${uid}" id -g)" == "${shared_gid}" ]] || fail "effective GID setup failed"
  [[ "$(run_as "${uid}" id -G)" == "${shared_gid}" ]] || fail "supplementary groups were retained"
}

assert_metadata() {
  local path="$1"
  local expected="$2"
  [[ "$(stat -c '%u:%g %a %F' "${path}")" == "${expected}" ]] \
    || fail "unexpected ownership or mode"
}

service_pid=''
p0_pid=''
stop_p0_runtime() {
  if [[ -z "${p0_pid}" ]]; then
    return 0
  fi
  kill -TERM "${p0_pid}" 2>/dev/null || true
  if ! timeout 5s tail --pid="${p0_pid}" -f /dev/null; then
    kill -KILL "${p0_pid}" 2>/dev/null || true
    wait "${p0_pid}" 2>/dev/null || true
    p0_pid=''
    return 1
  fi
  local wait_status=0
  wait "${p0_pid}" || wait_status=$?
  p0_pid=''
  return "${wait_status}"
}

stop_service() {
  if [[ -z "${service_pid}" ]]; then
    return 0
  fi
  kill -TERM "${service_pid}" 2>/dev/null || true
  if ! timeout 5s tail --pid="${service_pid}" -f /dev/null; then
    kill -KILL "${service_pid}" 2>/dev/null || true
    wait "${service_pid}" 2>/dev/null || true
    service_pid=''
    return 1
  fi
  local wait_status=0
  wait "${service_pid}" || wait_status=$?
  service_pid=''
  return "${wait_status}"
}

cleanup() {
  local status=$?
  trap - EXIT
  if ! stop_p0_runtime; then
    status=1
  fi
  if ! stop_service; then
    status=1
  fi
  if [[ "${status}" -ne 0 && -f "${service_log}" ]]; then
    cat "${service_log}" >&2
  fi
  if [[ "${status}" -ne 0 && -f "${TURSO_MYSQL_RUNTIME_TEST_ROOT}/p0/server.log" ]]; then
    cat "${TURSO_MYSQL_RUNTIME_TEST_ROOT}/p0/server.log" >&2
  fi
  exit "${status}"
}
trap cleanup EXIT

assert_runtime_test_name "${runtime_test_name}" "${runtime_test_binary}"
assert_runtime_test_name "${runtime_mediumint_test_name}" "${runtime_test_binary}"
assert_runtime_test_name "${runtime_prepared_quota_test_name}" "${runtime_test_binary}"
assert_runtime_test_name "${runtime_table_test_name}" "${runtime_test_binary}"
assert_runtime_test_name "${tcp_test_name}" "${tcp_test_binary}"
assert_runtime_test_name "${value_test_name}" "${tcp_test_binary}"
assert_runtime_test_name "${sql_admin_test_name}" "${tcp_test_binary}"
assert_runtime_test_name "${auto_increment_test_name}" "${tcp_test_binary}"
if [[ "${TURSO_MYSQL_CLI_REQUIRED}" == 1 ]]; then
  command -v mysql >/dev/null || fail "fixture image has no MySQL CLI"
  mysql_client_version="$(mysql --version)" || fail "MySQL CLI version probe failed"
  [[ "${mysql_client_version}" == *'Ver 8.0.46'* ]] \
    || fail "fixture requires MySQL CLI 8.0.46"
  printf 'MySQL CLI version: %s\n' "${mysql_client_version}"
  assert_runtime_test_name "${mysql_cli_test_name}" "${tcp_test_binary}"
fi
if [[ "${TURSO_MYSQL_DUMP_REQUIRED}" == 1 ]]; then
  command -v mysqldump >/dev/null || fail "fixture image has no mysqldump"
  command -v mysql >/dev/null || fail "fixture image has no MySQL CLI"
  dump_version="$(mysqldump --version)" || fail "mysqldump version probe failed"
  [[ "${dump_version}" == *'Ver 8.0.46'* ]] \
    || fail "fixture requires mysqldump 8.0.46"
  printf 'mysqldump version: %s\n' "${dump_version}"
  assert_runtime_test_name "${dump_test_name}" "${tcp_test_binary}"
fi
if [[ "${TURSO_MYSQL_DRIVERS_REQUIRED}" == 1 ]]; then
  [[ -x /usr/local/bin/mysql-go-driver-e2e ]] || fail "fixture has no Go driver E2E"
  command -v java >/dev/null || fail "fixture has no Java runtime"
  [[ -f /opt/mysql-drivers/JdbcDriver.class && -f /opt/mysql-drivers/mysql-connector-j-9.6.0.jar ]] \
    || fail "fixture has no Connector/J E2E"
  assert_runtime_test_name "${go_driver_test_name}" "${tcp_test_binary}"
  assert_runtime_test_name "${jdbc_driver_test_name}" "${tcp_test_binary}"
fi
if [[ "${TURSO_MYSQL_ORM_REQUIRED}" == 1 ]]; then
  [[ -x /usr/local/bin/mysql-gorm-e2e ]] || fail "fixture has no GORM E2E"
  command -v java >/dev/null || fail "fixture has no Java runtime"
  [[ -f /opt/mysql-drivers/hibernate-e2e-1.0.0.jar ]] || fail "fixture has no Hibernate E2E"
  assert_runtime_test_name "${gorm_test_name}" "${tcp_test_binary}"
  assert_runtime_test_name "${hibernate_test_name}" "${tcp_test_binary}"
fi

assert_identity "${service_uid}"
assert_identity "${client_uid}"
assert_identity "${foreign_uid}"

install -d -m 0755 -o 0 -g 0 "${root}"
install -d -m 0700 -o "${service_uid}" -g "${shared_gid}" "${state_root}"
install -d -m 0710 -o "${service_uid}" -g "${shared_gid}" "${socket_root}"
install -d -m 0700 -o "${client_uid}" -g "${shared_gid}" "${account_root}"
install -d -m 0700 -o "${client_uid}" -g "${shared_gid}" "${TURSO_MYSQL_RUNTIME_TEST_ROOT}"
assert_metadata "${root}" '0:0 755 directory'
assert_metadata "${state_root}" "${service_uid}:${shared_gid} 700 directory"
assert_metadata "${socket_root}" "${service_uid}:${shared_gid} 710 directory"
assert_metadata "${account_root}" "${client_uid}:${shared_gid} 700 directory"
assert_metadata "${TURSO_MYSQL_RUNTIME_TEST_ROOT}" "${client_uid}:${shared_gid} 700 directory"

setpriv --reuid="${service_uid}" --regid="${shared_gid}" --clear-groups \
  "${authority_binary}" \
  --authority-id "${authority_id}" \
  --state-root "${state_root}" \
  --socket-directory "${socket_root}" \
  --socket-name authority.sock \
  --socket-gid "${shared_gid}" \
  --client-uid "${client_uid}" \
  --io-timeout-ms 1000 >"${service_log}" 2>&1 &
service_pid=$!

for _ in $(seq 1 100); do
  if [[ -S "${socket_path}" ]]; then
    break
  fi
  kill -0 "${service_pid}" 2>/dev/null || fail "authority exited before binding"
  sleep 0.05
done
[[ -S "${socket_path}" ]] || fail "authority did not bind its socket"
assert_metadata "${socket_path}" "${service_uid}:${shared_gid} 660 socket"

printf '%s' 'cross-uid-gate-password' | run_as "${client_uid}" "${provision_binary}" \
  --account-store-root "${account_root}" \
  --authority-id "${authority_id}" \
  --authority-socket "${socket_path}" \
  --authority-service-uid "${service_uid}" \
  --authority-rpc-timeout-ms 1000 \
  --coordination-timeout-ms 1000 \
  initialize \
  --username gateadmin \
  --global-connect true \
  --global-list false \
  --global-manage-accounts true \
  --disabled false \
  --database-grant reports:connect,query \
  --database-grant turso_oracle:connect,query \
  --password-stdin \
  --password-input-timeout-ms 1000

run_as "${client_uid}" "${provision_binary}" \
  --account-store-root "${account_root}" \
  --authority-id "${authority_id}" \
  --authority-socket "${socket_path}" \
  --authority-service-uid "${service_uid}" \
  --authority-rpc-timeout-ms 1000 \
  --coordination-timeout-ms 1000 \
  reconcile

printf '%s' 'cross-uid-reports-password' | run_as "${client_uid}" "${provision_binary}" \
  --account-store-root "${account_root}" \
  --authority-id "${authority_id}" \
  --authority-socket "${socket_path}" \
  --authority-service-uid "${service_uid}" \
  --authority-rpc-timeout-ms 1000 \
  --coordination-timeout-ms 1000 \
  add-account \
  --username reportreader \
  --global-connect true \
  --global-list false \
  --disabled false \
  --database-grant reports:connect \
  --table-grant reports.records:select \
  --password-stdin \
  --password-input-timeout-ms 1000

run_as "${client_uid}" "${provision_binary}" \
  --account-store-root "${account_root}" \
  --authority-id "${authority_id}" \
  --authority-socket "${socket_path}" \
  --authority-service-uid "${service_uid}" \
  --authority-rpc-timeout-ms 1000 \
  --coordination-timeout-ms 1000 \
  reconcile

TURSO_MYSQL_CROSS_UID_SOCKET="${socket_path}" \
TURSO_MYSQL_CROSS_UID_AUTHORITY="${authority_id}" \
TURSO_MYSQL_CROSS_UID_SERVICE_UID="${service_uid}" \
TURSO_MYSQL_CROSS_UID_FOREIGN_UID="${foreign_uid}" \
  run_as "${foreign_uid}" "${test_binary}" --ignored --exact foreign_client_is_rejected_despite_socket_group_access

TURSO_MYSQL_CROSS_UID_SOCKET="${socket_path}" \
TURSO_MYSQL_CROSS_UID_AUTHORITY="${authority_id}" \
TURSO_MYSQL_CROSS_UID_SERVICE_UID="${service_uid}" \
TURSO_MYSQL_CROSS_UID_CLIENT_UID="${client_uid}" \
TURSO_MYSQL_CROSS_UID_ACCOUNT_STORE_ROOT="${account_root}" \
  run_as "${client_uid}" "${test_binary}" --ignored --exact configured_client_observes_revised_accounts_and_grants

TURSO_MYSQL_CROSS_UID_SOCKET="${socket_path}" \
TURSO_MYSQL_CROSS_UID_AUTHORITY="${authority_id}" \
TURSO_MYSQL_CROSS_UID_SERVICE_UID="${service_uid}" \
TURSO_MYSQL_CROSS_UID_CLIENT_UID="${client_uid}" \
TURSO_MYSQL_CROSS_UID_ACCOUNT_STORE_ROOT="${account_root}" \
TURSO_MYSQL_CROSS_UID_RUNTIME_BINARY='/artifacts/turso-mysql-server' \
  run_runtime_test "${runtime_test_binary}" "${runtime_test_name}"

TURSO_MYSQL_CROSS_UID_SOCKET="${socket_path}" \
TURSO_MYSQL_CROSS_UID_AUTHORITY="${authority_id}" \
TURSO_MYSQL_CROSS_UID_SERVICE_UID="${service_uid}" \
TURSO_MYSQL_CROSS_UID_CLIENT_UID="${client_uid}" \
TURSO_MYSQL_CROSS_UID_ACCOUNT_STORE_ROOT="${account_root}" \
TURSO_MYSQL_CROSS_UID_RUNTIME_BINARY='/artifacts/turso-mysql-server' \
  run_runtime_test "${runtime_test_binary}" "${runtime_mediumint_test_name}"

TURSO_MYSQL_CROSS_UID_SOCKET="${socket_path}" \
TURSO_MYSQL_CROSS_UID_AUTHORITY="${authority_id}" \
TURSO_MYSQL_CROSS_UID_SERVICE_UID="${service_uid}" \
TURSO_MYSQL_CROSS_UID_CLIENT_UID="${client_uid}" \
TURSO_MYSQL_CROSS_UID_ACCOUNT_STORE_ROOT="${account_root}" \
TURSO_MYSQL_CROSS_UID_RUNTIME_BINARY='/artifacts/turso-mysql-server' \
  run_runtime_test "${runtime_test_binary}" "${runtime_prepared_quota_test_name}"

TURSO_MYSQL_CROSS_UID_SOCKET="${socket_path}" \
TURSO_MYSQL_CROSS_UID_AUTHORITY="${authority_id}" \
TURSO_MYSQL_CROSS_UID_SERVICE_UID="${service_uid}" \
TURSO_MYSQL_CROSS_UID_CLIENT_UID="${client_uid}" \
TURSO_MYSQL_CROSS_UID_ACCOUNT_STORE_ROOT="${account_root}" \
TURSO_MYSQL_CROSS_UID_RUNTIME_BINARY='/artifacts/turso-mysql-server' \
  run_runtime_test "${runtime_test_binary}" "${runtime_table_test_name}"

TURSO_MYSQL_CROSS_UID_SOCKET="${socket_path}" \
TURSO_MYSQL_CROSS_UID_AUTHORITY="${authority_id}" \
TURSO_MYSQL_CROSS_UID_SERVICE_UID="${service_uid}" \
TURSO_MYSQL_CROSS_UID_CLIENT_UID="${client_uid}" \
TURSO_MYSQL_CROSS_UID_ACCOUNT_STORE_ROOT="${account_root}" \
TURSO_MYSQL_CROSS_UID_RUNTIME_BINARY='/artifacts/turso-mysql-server' \
  run_runtime_test "${tcp_test_binary}" "${tcp_test_name}"

TURSO_MYSQL_CROSS_UID_SOCKET="${socket_path}" \
TURSO_MYSQL_CROSS_UID_AUTHORITY="${authority_id}" \
TURSO_MYSQL_CROSS_UID_SERVICE_UID="${service_uid}" \
TURSO_MYSQL_CROSS_UID_CLIENT_UID="${client_uid}" \
TURSO_MYSQL_CROSS_UID_ACCOUNT_STORE_ROOT="${account_root}" \
TURSO_MYSQL_CROSS_UID_RUNTIME_BINARY='/artifacts/turso-mysql-server' \
  run_runtime_test "${tcp_test_binary}" "${value_test_name}"

TURSO_MYSQL_CROSS_UID_SOCKET="${socket_path}" \
TURSO_MYSQL_CROSS_UID_AUTHORITY="${authority_id}" \
TURSO_MYSQL_CROSS_UID_SERVICE_UID="${service_uid}" \
TURSO_MYSQL_CROSS_UID_CLIENT_UID="${client_uid}" \
TURSO_MYSQL_CROSS_UID_ACCOUNT_STORE_ROOT="${account_root}" \
TURSO_MYSQL_CROSS_UID_RUNTIME_BINARY='/artifacts/turso-mysql-server' \
  run_runtime_test "${tcp_test_binary}" "${sql_admin_test_name}"

TURSO_MYSQL_CROSS_UID_SOCKET="${socket_path}" \
TURSO_MYSQL_CROSS_UID_AUTHORITY="${authority_id}" \
TURSO_MYSQL_CROSS_UID_SERVICE_UID="${service_uid}" \
TURSO_MYSQL_CROSS_UID_CLIENT_UID="${client_uid}" \
TURSO_MYSQL_CROSS_UID_ACCOUNT_STORE_ROOT="${account_root}" \
TURSO_MYSQL_CROSS_UID_RUNTIME_BINARY='/artifacts/turso-mysql-server' \
  run_runtime_test "${tcp_test_binary}" "${auto_increment_test_name}"

if [[ "${TURSO_MYSQL_CLI_REQUIRED}" == 1 ]]; then
  TURSO_MYSQL_CROSS_UID_SOCKET="${socket_path}" \
  TURSO_MYSQL_CROSS_UID_AUTHORITY="${authority_id}" \
  TURSO_MYSQL_CROSS_UID_SERVICE_UID="${service_uid}" \
  TURSO_MYSQL_CROSS_UID_CLIENT_UID="${client_uid}" \
  TURSO_MYSQL_CROSS_UID_ACCOUNT_STORE_ROOT="${account_root}" \
  TURSO_MYSQL_CROSS_UID_RUNTIME_BINARY='/artifacts/turso-mysql-server' \
    run_runtime_test "${tcp_test_binary}" "${mysql_cli_test_name}"
fi

if [[ "${TURSO_MYSQL_DUMP_REQUIRED}" == 1 ]]; then
  TURSO_MYSQL_CROSS_UID_SOCKET="${socket_path}" \
  TURSO_MYSQL_CROSS_UID_AUTHORITY="${authority_id}" \
  TURSO_MYSQL_CROSS_UID_SERVICE_UID="${service_uid}" \
  TURSO_MYSQL_CROSS_UID_CLIENT_UID="${client_uid}" \
  TURSO_MYSQL_CROSS_UID_ACCOUNT_STORE_ROOT="${account_root}" \
  TURSO_MYSQL_CROSS_UID_RUNTIME_BINARY='/artifacts/turso-mysql-server' \
    run_runtime_test "${tcp_test_binary}" "${dump_test_name}"
fi

if [[ "${TURSO_MYSQL_DRIVERS_REQUIRED}" == 1 ]]; then
  for driver_test in "${go_driver_test_name}" "${jdbc_driver_test_name}"; do
    TURSO_MYSQL_CROSS_UID_SOCKET="${socket_path}" \
    TURSO_MYSQL_CROSS_UID_AUTHORITY="${authority_id}" \
    TURSO_MYSQL_CROSS_UID_SERVICE_UID="${service_uid}" \
    TURSO_MYSQL_CROSS_UID_CLIENT_UID="${client_uid}" \
    TURSO_MYSQL_CROSS_UID_ACCOUNT_STORE_ROOT="${account_root}" \
    TURSO_MYSQL_CROSS_UID_RUNTIME_BINARY='/artifacts/turso-mysql-server' \
      run_runtime_test "${tcp_test_binary}" "${driver_test}"
  done
fi

if [[ "${TURSO_MYSQL_ORM_REQUIRED}" == 1 ]]; then
  for orm_test in "${gorm_test_name}" "${hibernate_test_name}"; do
    TURSO_MYSQL_CROSS_UID_SOCKET="${socket_path}" \
    TURSO_MYSQL_CROSS_UID_AUTHORITY="${authority_id}" \
    TURSO_MYSQL_CROSS_UID_SERVICE_UID="${service_uid}" \
    TURSO_MYSQL_CROSS_UID_CLIENT_UID="${client_uid}" \
    TURSO_MYSQL_CROSS_UID_ACCOUNT_STORE_ROOT="${account_root}" \
    TURSO_MYSQL_CROSS_UID_RUNTIME_BINARY='/artifacts/turso-mysql-server' \
      run_runtime_test "${tcp_test_binary}" "${orm_test}"
  done
fi

if [[ "${TURSO_MYSQL_DRIVERS_REQUIRED}" == 1 && "${TURSO_MYSQL_ORM_REQUIRED}" == 1 ]]; then
  printf '%s\n' 'Driver/ORM comparison: shared fixture assertions on both servers; no exported field-by-field diff'
fi

if [[ "${TURSO_MYSQL_P0_REQUIRED}" == 1 ]]; then
  p0_root="${TURSO_MYSQL_RUNTIME_TEST_ROOT}/p0"
  p0_data="${p0_root}/data"
  p0_sockets="${p0_root}/sockets"
  p0_socket="${p0_sockets}/mysql.sock"
  install -d -m 0700 -o "${client_uid}" -g "${shared_gid}" \
    "${p0_root}" "${p0_data}" "${p0_sockets}"
  run_as "${client_uid}" /artifacts/mysql-conformance-create-db "${p0_data}" turso_oracle
  setpriv --reuid="${client_uid}" --regid="${shared_gid}" --clear-groups \
    "${runtime_binary}" \
    --data-root "${p0_data}" \
    --account-store-root "${account_root}" \
    --socket-directory "${p0_sockets}" \
    --socket-name mysql.sock \
    --authority-id "${authority_id}" \
    --authority-socket "${socket_path}" \
    --authority-service-uid "${service_uid}" \
    --authority-rpc-timeout-ms 1000 \
    --reload-interval-ms 1000 \
    --max-connections 8 \
    --max-admissions 4 \
    --max-write-bytes 8192 \
    --max-write-frames 64 \
    --checkpoint-timeout-ms 1000 \
    --tls-timeout-ms 1000 \
    --authentication-timeout-ms 1000 \
    --idle-timeout-ms 1000 \
    --query-timeout-ms 1000 \
    --write-timeout-ms 1000 \
    --shutdown-timeout-ms 1000 \
    >"${p0_root}/server.log" 2>&1 &
  p0_pid=$!
  for _ in $(seq 1 100); do
    [[ -S "${p0_socket}" ]] && break
    kill -0 "${p0_pid}" 2>/dev/null || fail "P0 Turso runtime exited before binding"
    sleep 0.05
  done
  [[ -S "${p0_socket}" ]] || fail "P0 Turso runtime did not bind its socket"

  p0_socket_url="${p0_socket//\//%2F}"
  p0_dsn="mysql://gateadmin:cross-uid-gate-password@localhost/turso_oracle?socket=${p0_socket_url}"
  p0_cases='/mysql-conformance/cases/p0'
  p0_goldens='/mysql-conformance/goldens/mysql-8.4/sha256-b3b90af2a6552ae30c266fdb7d5dd55f3afb72404bb78d37fe8a23eb857fd3fb'
  printf '%s\n' 'Turso P0 differential: transaction-observer'
  TURSO_DSN="${p0_dsn}" run_as "${client_uid}" /artifacts/mysql-conformance \
    compare-turso \
    --case "${p0_cases}/transaction-observer.json" \
    --golden "${p0_goldens}/transaction-observer.json" \
    --acknowledge-disposable-db turso_oracle
  printf '%s\n' 'Turso P0 differential: select-integer-equality'
  TURSO_DSN="${p0_dsn}" run_as "${client_uid}" /artifacts/mysql-conformance \
    compare-turso-integer-equality \
    --case "${p0_cases}/select-integer-equality.json" \
    --golden "${p0_goldens}/select-integer-equality.json" \
    --acknowledge-disposable-db turso_oracle
  TURSO_DSN="${p0_dsn}" run_as "${client_uid}" /artifacts/mysql-parity-probe \
    TURSO_DSN "${p0_root}/turso-probe.json"
  install -m 0644 "${p0_root}/turso-probe.json" /probe-report/turso-probe.json
  stop_p0_runtime || fail "P0 Turso runtime did not stop after SIGTERM"
  p0_manifest_count="$(sed -n 's/^P0_CASES := //p' /mysql-conformance/Makefile | wc -w)"
  p0_file_count="$(find "${p0_cases}" -maxdepth 1 -name '*.json' | wc -l)"
  [[ $((p0_manifest_count + 1)) -eq "${p0_file_count}" ]] \
    || fail "P0 manifest and case files differ"
  for p0_case_file in "${p0_cases}"/*.json; do
    p0_case_name="${p0_case_file##*/}"
    p0_case_name="${p0_case_name%.json}"
    case "${p0_case_name}" in
      transaction-observer|select-integer-equality) ;;
      *) printf 'Turso P0 Oracle-only: %s\n' "${p0_case_name}" ;;
    esac
  done
  printf 'Turso P0 differential: 2 compared, %d Oracle-only, %d total\n' \
    "$((p0_file_count - 2))" "${p0_file_count}"
fi

stop_service || fail "authority did not stop after SIGTERM"
[[ ! -e "${socket_path}" && ! -L "${socket_path}" ]] \
  || fail "authority left its socket after shutdown"
[[ "${runtime_failures}" -eq 0 ]] || fail "${runtime_failures} runtime E2E tests failed"
printf '%s\n' 'checkpoint authority cross-UID gate: complete'
INNER
then
  gate_status=0
else
  gate_status=$?
fi
gate_output="$(cat "${gate_log}")"
rm -f "${gate_log}"
if [[ "${gate_status}" -ne 0 ]]; then
  printf '%s\n' "${gate_output}"
  fail "cross-UID fixture failed"
fi
printf '%s\n' "${gate_output}"
completion_line="${gate_output##*$'\n'}"
[[ "${completion_line}" == "${completion_marker}" ]] \
  || fail "cross-UID fixture did not report completion"
