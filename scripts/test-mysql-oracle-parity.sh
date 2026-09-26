#!/usr/bin/env bash

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
compose_file="${root}/mysql/conformance/compose.yaml"
tls_compose_file="${root}/mysql/conformance/compose.tls-e2e.yaml"
work="$(mktemp -d)"
export COMPOSE_PROJECT_NAME="mysql-parity-${GITHUB_RUN_ID:-$$}"
export MYSQL_CONFORMANCE_PASSWORD="$(openssl rand -hex 24)"
export MYSQL_CONFORMANCE_ROOT_PASSWORD="$(openssl rand -hex 24)"
fixture_password="$(openssl rand -hex 24)"
export MYSQL_ORACLE_PORT="${MYSQL_ORACLE_PORT:-3307}"
export MYSQL_ORACLE_DSN="mysql://turso_oracle:${MYSQL_CONFORMANCE_PASSWORD}@127.0.0.1:${MYSQL_ORACLE_PORT}/turso_oracle"
export MYSQL_ORACLE_TLS_DIR="${work}/tls"
fixture_image="turso-mysql-oracle-fixtures:${GITHUB_RUN_ID:-local}"
report_dir="${MYSQL_PARITY_REPORT_DIR:-${work}/report}"
mkdir -p "${report_dir}"

cleanup() {
  local status=$?
  trap - EXIT
  if [[ "${status}" -ne 0 ]]; then
    docker compose -f "${compose_file}" -f "${tls_compose_file}" logs --no-color mysql \
      >"${report_dir}/mysql-server.log" 2>&1 || true
  fi
  docker compose -f "${compose_file}" -f "${tls_compose_file}" down --volumes --remove-orphans >/dev/null 2>&1 || status=1
  rm -rf "${work}"
  exit "${status}"
}
trap cleanup EXIT

command -v docker >/dev/null
command -v openssl >/dev/null
[[ "$(uname -s)" == Linux ]] || {
  echo 'Oracle parity requires a Linux host for Docker host networking' >&2
  exit 1
}
[[ "$(docker info --format '{{.OSType}}')" == linux ]] || {
  echo 'Oracle parity requires a Linux Docker daemon' >&2
  exit 1
}

cd "${root}"
docker build --file mysql/runtime/tests/fixtures/cross-uid.Dockerfile \
  --tag "${fixture_image}" mysql/runtime/tests/fixtures

make -C mysql/conformance verify-p0 2>&1 | tee "${report_dir}/p0.log"
cargo run --locked -p turso_mysql_conformance --bin mysql-parity-probe -- \
  MYSQL_ORACLE_DSN "${report_dir}/oracle-probe.json"

mkdir -m 755 "${MYSQL_ORACLE_TLS_DIR}"
openssl req -x509 -newkey rsa:2048 -noenc -days 1 \
  -subj '/CN=Turso MySQL fixture CA' \
  -keyout "${MYSQL_ORACLE_TLS_DIR}/ca-key.pem" \
  -out "${MYSQL_ORACLE_TLS_DIR}/ca.pem" >/dev/null 2>&1
openssl req -newkey rsa:2048 -noenc \
  -subj '/CN=localhost' \
  -addext 'subjectAltName=DNS:localhost,IP:127.0.0.1' \
  -keyout "${MYSQL_ORACLE_TLS_DIR}/server-key.pem" \
  -out "${work}/server.csr" >/dev/null 2>&1
printf '%s\n' 'subjectAltName=DNS:localhost,IP:127.0.0.1' >"${work}/server.ext"
openssl x509 -req -days 1 -in "${work}/server.csr" \
  -CA "${MYSQL_ORACLE_TLS_DIR}/ca.pem" \
  -CAkey "${MYSQL_ORACLE_TLS_DIR}/ca-key.pem" -CAcreateserial \
  -extfile "${work}/server.ext" \
  -out "${MYSQL_ORACLE_TLS_DIR}/server.pem" >/dev/null 2>&1
chmod 644 "${MYSQL_ORACLE_TLS_DIR}/ca.pem" "${MYSQL_ORACLE_TLS_DIR}/server.pem"
docker run --rm --user 0:0 \
  --mount "type=bind,src=${MYSQL_ORACLE_TLS_DIR},dst=/oracle-tls" \
  --entrypoint sh \
  'mysql@sha256:b3b90af2a6552ae30c266fdb7d5dd55f3afb72404bb78d37fe8a23eb857fd3fb' \
  -ec 'chown mysql:mysql /oracle-tls/server-key.pem && chmod 600 /oracle-tls/server-key.pem'

docker compose -f "${compose_file}" -f "${tls_compose_file}" up -d --wait
docker compose -f "${compose_file}" -f "${tls_compose_file}" exec -T \
  -e "MYSQL_FIXTURE_PASSWORD=${fixture_password}" mysql \
  sh -ec 'MYSQL_PWD="$MYSQL_ROOT_PASSWORD" mysql -uroot <<SQL
CREATE DATABASE reports;
CREATE USER "gateadmin"@"%" IDENTIFIED BY "$MYSQL_FIXTURE_PASSWORD";
GRANT ALL PRIVILEGES ON reports.* TO "gateadmin"@"%";
SQL'

run_fixture() {
  local stage="$1"
  shift
  docker run --rm --network host \
    --mount "type=bind,src=${MYSQL_ORACLE_TLS_DIR}/ca.pem,dst=/oracle-ca.pem,readonly" \
    -e "TURSO_MYSQL_DRIVER_ENDPOINT=localhost:${MYSQL_ORACLE_PORT}" \
    -e 'TURSO_MYSQL_DRIVER_CA=/oracle-ca.pem' \
    -e "TURSO_MYSQL_DRIVER_PASSWORD=${fixture_password}" \
    "${fixture_image}" "$@" 2>&1 | tee "${report_dir}/${stage}.log"
}

run_fixture go /usr/local/bin/mysql-go-driver-e2e
run_fixture jdbc java -cp '/opt/mysql-drivers/*' JdbcDriver
run_fixture gorm /usr/local/bin/mysql-gorm-e2e
run_fixture hibernate-prepare java -jar /opt/mysql-drivers/hibernate-e2e-1.0.0.jar prepare
docker compose -f "${compose_file}" -f "${tls_compose_file}" restart mysql
docker compose -f "${compose_file}" -f "${tls_compose_file}" up -d --wait
run_fixture hibernate-verify java -jar /opt/mysql-drivers/hibernate-e2e-1.0.0.jar verify
printf '%s\n' 'Driver/ORM comparison: shared fixture assertions on both servers; no exported field-by-field diff'
printf '%s\n' 'mysql oracle parity: P0 and four client fixtures complete'
oracle_case_count="$(find mysql/conformance/cases/p0 -maxdepth 1 -name '*.json' | wc -l)"
printf 'p0-oracle=%d golden checks\n' "${oracle_case_count}" >"${report_dir}/result.txt"
printf '%s\n' \
  'p0-turso=2 direct comparisons in cross-UID job' \
  "p0-oracle-only=$((oracle_case_count - 2))" \
  'drivers=shared assertions, no exported field-level diff' \
  'go=passed' 'jdbc=passed' 'gorm=passed' 'hibernate-restart=passed' \
  >>"${report_dir}/result.txt"
