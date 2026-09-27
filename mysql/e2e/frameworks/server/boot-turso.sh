#!/usr/bin/env bash
# Boots turso-mysql-server on 0.0.0.0:3306 with mandatory TLS, the same way
# scripts/test-checkpoint-authority-cross-uid.sh does: the checkpoint
# authority runs as one UID and the server as another, because the authority
# refuses a client with its own UID.
set -euo pipefail

readonly service_uid=41001
readonly client_uid=41002
readonly shared_gid=41003
readonly authority_id='turso-e2e'
readonly bin=/target/debug
# Short paths: Unix socket paths are capped at 103 bytes.
readonly root=/run/t
readonly state="${root}/s"
readonly sockets="${root}/k"
readonly accounts="${root}/a"
readonly data="${root}/d"
readonly tls="${root}/tls"
readonly socket="${sockets}/a.sock"

[[ -n "${E2E_PASSWORD:-}" ]] || { echo "E2E_PASSWORD is empty" >&2; exit 2; }
[[ -n "${E2E_DATABASES:-}" ]] || { echo "E2E_DATABASES is empty" >&2; exit 2; }

run_as() {
  local uid="$1"
  shift
  setpriv --reuid="${uid}" --regid="${shared_gid}" --clear-groups "$@"
}

provision() {
  run_as "${client_uid}" "${bin}/turso-mysql-offline-provision" \
    --account-store-root "${accounts}" \
    --authority-id "${authority_id}" \
    --authority-socket "${socket}" \
    --authority-service-uid "${service_uid}" \
    --authority-rpc-timeout-ms 5000 \
    --coordination-timeout-ms 5000 \
    "$@"
}

install -d -m 0700 -o "${service_uid}" -g "${shared_gid}" "${state}"
install -d -m 0710 -o "${service_uid}" -g "${shared_gid}" "${sockets}"
install -d -m 0700 -o "${client_uid}" -g "${shared_gid}" "${accounts}" "${data}" "${tls}"
install -m 0644 -o "${client_uid}" -g "${shared_gid}" /tls-in/server-chain.pem "${tls}/server-chain.pem"
install -m 0600 -o "${client_uid}" -g "${shared_gid}" /tls-in/server-key.pem "${tls}/server-key.pem"

run_as "${service_uid}" "${bin}/turso-mysql-checkpoint-authority" \
  --authority-id "${authority_id}" \
  --state-root "${state}" \
  --socket-directory "${sockets}" \
  --socket-name a.sock \
  --socket-gid "${shared_gid}" \
  --client-uid "${client_uid}" \
  --io-timeout-ms 5000 >/log/authority.log 2>&1 &
authority_pid=$!
for _ in $(seq 1 200); do
  [[ -S "${socket}" ]] && break
  kill -0 "${authority_pid}" 2>/dev/null || { cat /log/authority.log >&2; exit 1; }
  sleep 0.05
done
[[ -S "${socket}" ]] || { echo "authority did not bind" >&2; exit 1; }

grants=()
for database in ${E2E_DATABASES}; do
  grants+=(--database-grant "${database}:connect,query,create,drop")
done
printf '%s' "${E2E_PASSWORD}" | provision initialize \
  --username e2e \
  --global-connect true \
  --global-list true \
  --global-manage-accounts false \
  --disabled false \
  "${grants[@]}" \
  --password-stdin \
  --password-input-timeout-ms 5000
provision reconcile

run_as "${client_uid}" "${bin}/turso-mysql-server" \
  --data-root "${data}" \
  --account-store-root "${accounts}" \
  --listen 0.0.0.0:3306 \
  --tls-cert "${tls}/server-chain.pem" \
  --tls-key "${tls}/server-key.pem" \
  --authority-id "${authority_id}" \
  --authority-socket "${socket}" \
  --authority-service-uid "${service_uid}" \
  --authority-rpc-timeout-ms 5000 \
  --reload-interval-ms 1000 \
  --max-connections 256 \
  --max-admissions 32 \
  --max-write-bytes 67108864 \
  --max-write-frames 4096 \
  --checkpoint-timeout-ms 5000 \
  --tls-timeout-ms 10000 \
  --authentication-timeout-ms 10000 \
  --idle-timeout-ms 3600000 \
  --query-timeout-ms 60000 \
  --write-timeout-ms 60000 \
  --shutdown-timeout-ms 5000 >/log/server.log 2>&1 &
server_pid=$!

stop() {
  kill -TERM "${server_pid}" "${authority_pid}" 2>/dev/null || true
  wait "${server_pid}" "${authority_pid}" 2>/dev/null || true
  exit 0
}
trap stop TERM INT

for _ in $(seq 1 200); do
  if (exec 3<>/dev/tcp/127.0.0.1/3306) 2>/dev/null; then
    touch "${root}/ready"
    break
  fi
  kill -0 "${server_pid}" 2>/dev/null || { cat /log/server.log >&2; exit 1; }
  sleep 0.05
done
[[ -f "${root}/ready" ]] || { echo "server did not listen" >&2; exit 1; }
echo "turso-mysql-server is listening on 3306"

wait -n "${server_pid}" "${authority_pid}"
echo "a turso process exited" >&2
cat /log/server.log >&2
exit 1
