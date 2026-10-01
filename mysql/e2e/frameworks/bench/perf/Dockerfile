# perf, run privileged in the turso container's PID namespace, to sample where
# the server spends its time during a benchmark.
FROM debian:bookworm-20260918-slim@sha256:3783cc01769c7b2b1b83a5c5ad96c815348e28ed7da68e2e3687004faa906251
RUN apt-get update && apt-get install -y --no-install-recommends linux-perf procps python3 && rm -rf /var/lib/apt/lists/*
