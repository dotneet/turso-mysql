# Benchmarks: turso-mysql against MySQL 8.4

sysbench 1.0.20 runs the same workloads against a live `turso-mysql-server` and
MySQL 8.4.11, each in its own container on the same Docker host, both over TLS
with the same account and both with their data on tmpfs. sysbench talks to
each server directly; the logging proxy the framework apps use is not in the
path.

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

`TURSO_MYSQL_EXPERIMENTAL_MVCC=1` on `run.sh --up` starts the turso server with
every database in the engine's MVCC mode, where writers run side by side
instead of under one write lock. It is experimental and off by default: MVCC
gives snapshot isolation with row-level write conflicts answered 1213, which is
not yet what the server's isolation levels, `SELECT ... FOR UPDATE` and `LOCK
TABLES` promise. `--up` again without it restarts the server on the same build.

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
