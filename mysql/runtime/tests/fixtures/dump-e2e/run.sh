#!/usr/bin/env bash

set -euo pipefail

[[ "$#" -eq 3 ]] || {
  printf '%s\n' 'usage: run.sh PRIVILEGED_CROSS_UID_TEST UNIX_E2E_TEST TCP_E2E_TEST' >&2
  exit 2
}
: "${TURSO_MYSQL_CROSS_UID_IMAGE:?set the built cross-UID fixture image}"

repo_root="$(cd "$(dirname "$0")/../../../../.." && pwd -P)"
readonly repo_root
nonce="$(date +%s)-$$"
readonly nonce
readonly network="turso-mysqldump-${nonce}"
readonly container="turso-mysqldump-oracle-${nonce}"
readonly oracle_root_password='cross-uid-dump-oracle-password'
readonly oracle_password='cross-uid-gate-password'
readonly oracle_image='mysql@sha256:b3b90af2a6552ae30c266fdb7d5dd55f3afb72404bb78d37fe8a23eb857fd3fb'
temp_root="$(mktemp -d)"
readonly temp_root
readonly oracle_ca="${temp_root}/oracle-ca.pem"
network_created=0
container_created=0

cleanup() {
  local status=$?
  trap - EXIT
  if [[ "${container_created}" == 1 ]]; then
    docker rm --force "${container}" >/dev/null 2>&1 || true
  fi
  if [[ "${network_created}" == 1 ]]; then
    docker network rm "${network}" >/dev/null 2>&1 || true
  fi
  rm -rf "${temp_root}"
  exit "${status}"
}
trap cleanup EXIT

docker network create "${network}" >/dev/null
network_created=1
docker run --rm --detach \
  --network "${network}" --network-alias mysql-oracle --name "${container}" \
  -e "MYSQL_ROOT_PASSWORD=${oracle_root_password}" \
  -e MYSQL_DATABASE=oracle_source \
  "${oracle_image}" --require-secure-transport=ON \
  --log-bin-trust-function-creators=ON >/dev/null
container_created=1

ready=0
for _ in $(seq 1 100); do
  if docker exec -e "MYSQL_PWD=${oracle_root_password}" "${container}" \
    mysqladmin --protocol=tcp --host=127.0.0.1 --user=root ping --silent >/dev/null 2>&1; then
    ready=1
    break
  fi
  sleep 0.2
done
[[ "${ready}" == 1 ]] || {
  docker logs "${container}" >&2
  printf '%s\n' 'mysqldump oracle did not become ready' >&2
  exit 1
}
docker exec -i -e "MYSQL_PWD=${oracle_root_password}" "${container}" \
  mysql --user=root <<'SQL'
CREATE DATABASE reports;
CREATE USER 'gateadmin'@'%' IDENTIFIED BY 'cross-uid-gate-password';
GRANT ALL PRIVILEGES ON reports.* TO 'gateadmin'@'%';
GRANT ALL PRIVILEGES ON oracle_source.* TO 'gateadmin'@'%';
SQL
docker cp "${container}:/var/lib/mysql/ca.pem" "${oracle_ca}" >/dev/null
chmod 0644 "${oracle_ca}"

TURSO_MYSQL_DUMP_REQUIRED=1 \
TURSO_MYSQL_GATE_NETWORK="${network}" \
TURSO_MYSQL_DUMP_ORACLE_HOST=mysql-oracle \
TURSO_MYSQL_DUMP_ORACLE_PASSWORD="${oracle_password}" \
TURSO_MYSQL_DUMP_ORACLE_CA="${oracle_ca}" \
  "${repo_root}/scripts/test-checkpoint-authority-cross-uid.sh" "$@"
