#!/usr/bin/env bash

set -euo pipefail

readonly image="${TURSO_MYSQL_CROSS_UID_IMAGE:-ubuntu:24.04@sha256:33ceb71981b602c1a7443a53469e4dba065f7503eab3078a2d7a57a2ab987517}"
artifact_dir="${CROSS_UID_ARTIFACT_DIR:-$(pwd)/target/debug}"
test_binary="${1:?pass the compiled privileged_cross_uid test binary}"
authority_binary="${artifact_dir}/turso-mysql-checkpoint-authority"
provision_binary="${artifact_dir}/turso-mysql-offline-provision"

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

for artifact in "${authority_binary}" "${provision_binary}" "${test_binary}"; do
  [[ -f "${artifact}" && -x "${artifact}" ]] || fail "missing executable artifact"
  file -Lb "${artifact}" | grep -q 'ELF' || fail "requires Linux executable artifacts"
done

artifact_dir="$(cd "${artifact_dir}" && pwd -P)"
test_binary="$(cd "$(dirname "${test_binary}")" && pwd -P)/$(basename "${test_binary}")"
authority_binary="${artifact_dir}/turso-mysql-checkpoint-authority"
provision_binary="${artifact_dir}/turso-mysql-offline-provision"
readonly artifact_dir
readonly test_binary
readonly authority_binary
readonly provision_binary
[[ "${test_binary}" == "${artifact_dir}/deps/"* ]] \
  || fail "test artifact must be below the artifact deps directory"

run_docker run --rm --user 0:0 --network none --read-only \
  --tmpfs /run:rw,mode=755 \
  --mount "type=bind,src=${artifact_dir},dst=/artifacts,readonly" \
  -e "TURSO_MYSQL_CROSS_UID_TEST=$(basename "${test_binary}")" \
  "${image}" bash -s <<'INNER'
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
readonly socket_path="${socket_root}/authority.sock"
readonly service_log="${root}/authority.log"
readonly authority_binary='/artifacts/turso-mysql-checkpoint-authority'
readonly provision_binary='/artifacts/turso-mysql-offline-provision'
readonly test_binary="/artifacts/deps/${TURSO_MYSQL_CROSS_UID_TEST:?}"

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
  wait "${service_pid}"
  service_pid=''
}

cleanup() {
  local status=$?
  trap - EXIT
  if ! stop_service; then
    status=1
  fi
  if [[ "${status}" -ne 0 && -f "${service_log}" ]]; then
    cat "${service_log}" >&2
  fi
  exit "${status}"
}
trap cleanup EXIT

assert_identity "${service_uid}"
assert_identity "${client_uid}"
assert_identity "${foreign_uid}"

install -d -m 0755 -o 0 -g 0 "${root}"
install -d -m 0700 -o "${service_uid}" -g "${shared_gid}" "${state_root}"
install -d -m 0710 -o "${service_uid}" -g "${shared_gid}" "${socket_root}"
install -d -m 0700 -o "${client_uid}" -g "${shared_gid}" "${account_root}"
assert_metadata "${root}" '0:0 755 directory'
assert_metadata "${state_root}" "${service_uid}:${shared_gid} 700 directory"
assert_metadata "${socket_root}" "${service_uid}:${shared_gid} 710 directory"
assert_metadata "${account_root}" "${client_uid}:${shared_gid} 700 directory"

run_as "${service_uid}" "${authority_binary}" \
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
  --disabled false \
  --database-grant reports:connect,query \
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
  --database-grant reports:connect,query \
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
TURSO_MYSQL_CROSS_UID_CLIENT_UID="${client_uid}" \
TURSO_MYSQL_CROSS_UID_ACCOUNT_STORE_ROOT="${account_root}" \
  run_as "${client_uid}" "${test_binary}" --ignored --exact configured_client_observes_revised_accounts_and_grants

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

stop_service || fail "authority did not stop after SIGTERM"
[[ ! -e "${socket_path}" && ! -L "${socket_path}" ]] \
  || fail "authority left its socket after shutdown"
INNER
