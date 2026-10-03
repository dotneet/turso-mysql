# Benchmarks: turso-mysql against MySQL 8.4

sysbench 1.0.20 runs the same workloads against a live `turso-mysql-server` and
MySQL 8.4.11, each in its own container on the same Docker host, both over TLS
with the same account and both with their data on tmpfs unless
`E2E_DATA_ON_DISK=1`. sysbench talks to each server directly; the logging proxy
the framework apps use is not in the path.

The server is built with the `bench-profile` cargo profile (release with debug
info, so `perf` can name functions). That build lives only in its own Docker
volume; nothing optimized is built into the checkout's `target/`.

```bash
cd mysql/e2e/frameworks
export COMPOSE_PROJECT_NAME=turso-e2e-bench \
  E2E_CARGO_TARGET_VOLUME=turso-e2e-bench-cargo-target \
  E2E_CARGO_PROFILE=bench-profile
./run.sh --up          # optimized build of the checkout, both servers up
./run.sh --bench       # every workload, 1 and 8 threads, 30 s each
./run.sh --down
```

Knobs: `BENCH_TARGETS` (`turso mysql`), `BENCH_WORKLOADS` (sysbench Lua
scripts: `oltp_point_select oltp_read_only oltp_write_only oltp_read_write
oltp_insert`), `BENCH_THREADS` (`1 8`), `BENCH_TIME` (seconds, 30) and
`BENCH_TABLE_SIZE` (rows, 100000). The raw sysbench outputs and `summary.md`
are in `.run/results/bench/`.

Both servers keep their data on tmpfs by default, where an fsync costs nothing.
`E2E_DATA_ON_DISK=1` on `run.sh --up` puts each server's data directory on a
Docker volume of its own instead (`<project>-turso-data`, `<project>-mysql-data`),
so every fsync reaches the Docker VM's disk; on Docker Desktop for Mac one
36-byte append and fsync there took about 0.7 ms (tmpfs: 0.5 µs). Every `--up`
and `--down` removes both volumes, so each run starts from empty databases.

The turso server opens every database in WAL, its default, where writers take
one write lock over the whole database. `TURSO_MYSQL_JOURNAL_MODE=mvcc` on
`run.sh --up` starts it with every database in MVCC instead, where writers run
side by side with InnoDB's row locks, to compare the two; `--up` again without
it restarts the server on the same build.

## Profiling the server under load

While a benchmark runs, sample the server from a privileged container in its
PID namespace:

```bash
docker build -q -t turso-e2e-bench-perf:local bench/perf
docker run --rm --privileged --pid=container:turso-e2e-bench-turso-1 \
  -v turso-e2e-bench-cargo-target:/target:ro -v "$PWD/.run/results:/results" \
  turso-e2e-bench-perf:local sh -c \
  'perf record -F 999 -g -p "$(pgrep -f turso-mysql-server | head -1)" -o /results/perf.data -- sleep 20 &&
   perf report -i /results/perf.data --no-children --percent-limit 1 --stdio > /results/perf.txt'
```
