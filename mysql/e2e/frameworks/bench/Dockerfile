# sysbench, the usual MySQL benchmark, with the MariaDB client library
# Debian builds it against (caching_sha2_password and TLS both work there).
FROM debian:bookworm-20260918-slim@sha256:3783cc01769c7b2b1b83a5c5ad96c815348e28ed7da68e2e3687004faa906251
RUN apt-get update && apt-get install -y --no-install-recommends sysbench python3 ca-certificates default-mysql-client openssl && rm -rf /var/lib/apt/lists/*
