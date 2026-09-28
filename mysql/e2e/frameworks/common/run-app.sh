#!/bin/sh
# Entry point of every app container: starts the SQL-logging proxy on
# 127.0.0.1:3306 in front of the target server, runs the app's own ./run.sh,
# and stops the proxy. The app always connects to 127.0.0.1:3306 over TLS.
set -u
: "${E2E_APP:?}" "${E2E_TARGET:?}" "${E2E_UPSTREAM:?}"
out="/results/${E2E_APP}/${E2E_TARGET}"
mkdir -p "${out}"
rm -f "${out}/wire.jsonl" "${out}/steps.jsonl" "${out}/proxy.ready"
: > "${out}/steps.jsonl"
export E2E_OUT="${out}"
export E2E_HOST=127.0.0.1
export E2E_PORT=3306
export E2E_CA=/e2e/tls/ca.pem

# E2E_PROXY_DUMP=1 also writes every packet, in hex, to packets.txt.
dump=""
rm -f "${out}/packets.txt"
[ "${E2E_PROXY_DUMP:-0}" = 1 ] && dump="-dump ${out}/packets.txt"
# shellcheck disable=SC2086
/e2e/bin/sqlproxy ${dump} \
  -listen 127.0.0.1:3306 \
  -upstream "${E2E_UPSTREAM}" \
  -cert /e2e/proxy-tls/server-chain.pem \
  -key /e2e/proxy-tls/server-key.pem \
  -ca /e2e/tls/ca.pem \
  -log "${out}/wire.jsonl" \
  -ready "${out}/proxy.ready" 2>"${out}/proxy.log" &
proxy=$!
i=0
while [ ! -f "${out}/proxy.ready" ]; do
  i=$((i + 1))
  if [ "${i}" -gt 100 ] || ! kill -0 "${proxy}" 2>/dev/null; then
    echo "sqlproxy did not start" >&2
    cat "${out}/proxy.log" >&2
    exit 1
  fi
  sleep 0.1
done

[ "$#" -gt 0 ] || set -- ./run.sh
start=$(date +%s)
"$@" >"${out}/app.log" 2>&1
status=$?
end=$(date +%s)
echo "{\"app\":\"${E2E_APP}\",\"target\":\"${E2E_TARGET}\",\"exit\":${status},\"seconds\":$((end - start))}" >"${out}/exit.json"
kill "${proxy}" 2>/dev/null
wait "${proxy}" 2>/dev/null
exit 0
