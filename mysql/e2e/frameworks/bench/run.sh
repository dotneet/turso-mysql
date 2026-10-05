#!/bin/sh
# sysbench against each target straight (no proxy), over TLS, with the same
# account the apps use. Writes results/bench/<target>-<workload>-<threads>.txt
# and results/bench/summary.md.
set -u
out=/results/bench
mkdir -p "${out}"
# sysbench's --mysql-ssl=on hands the client library these three file names
# in the working directory. MySQL asks for a client certificate (it is not
# required) and turns away one not issued for client authentication, so one
# is issued from the run's throwaway CA.
mkdir -p /tmp/sysbench
cd /tmp/sysbench
cp /e2e/tls/ca.pem cacert.pem
openssl req -newkey rsa:2048 -nodes -keyout client-key.pem -subj /CN=sysbench -out client.csr 2>/dev/null
printf 'extendedKeyUsage=clientAuth\n' >client.ext
openssl x509 -req -in client.csr -CA /e2e/tls/ca.pem -CAkey /e2e/tls/ca-key.pem -CAserial ca.srl -CAcreateserial \
  -days 2 -extfile client.ext -out client-cert.pem 2>/dev/null
purge_mysql_binary_logs() {
  mysql --host=mysql --port=3306 --user=e2e --password="${E2E_PASSWORD}" --ssl --ssl-ca=/e2e/tls/ca.pem \
    --execute='FLUSH BINARY LOGS; PURGE BINARY LOGS BEFORE NOW()' 2>/dev/null \
    || echo "mysql: could not purge binary logs"
}
workloads="${BENCH_WORKLOADS:-oltp_point_select oltp_read_only oltp_write_only oltp_read_write oltp_insert}"
threads="${BENCH_THREADS:-1 8}"
seconds="${BENCH_TIME:-30}"
rows="${BENCH_TABLE_SIZE:-100000}"
for target in ${BENCH_TARGETS:-turso mysql}; do
  common="--db-driver=mysql --mysql-host=${target} --mysql-port=3306 --mysql-user=e2e \
    --mysql-password=${E2E_PASSWORD} --mysql-db=sbtest --mysql-ssl=on --tables=1 --table-size=${rows} \
    ${BENCH_SYSBENCH_ARGS:-}"
  # shellcheck disable=SC2086
  sysbench oltp_common ${common} prepare >"${out}/${target}-prepare.txt" 2>&1 \
    || { echo "${target}: prepare failed"; tail -5 "${out}/${target}-prepare.txt"; continue; }
  for workload in ${workloads}; do
    for t in ${threads}; do
      echo "${target} ${workload} threads=${t}"
      [ "${target}" = mysql ] && purge_mysql_binary_logs
      # shellcheck disable=SC2086
      sysbench "${workload}" ${common} --threads="${t}" --time="${seconds}" --report-interval=0 \
        --db-ps-mode=auto --mysql-ignore-errors=1213,1205 run >"${out}/${target}-${workload}-${t}.txt" 2>&1 \
        || tail -5 "${out}/${target}-${workload}-${t}.txt"
    done
  done
done
python3 /e2e/src/bench/summary.py "${out}"
cat "${out}/summary.md"
