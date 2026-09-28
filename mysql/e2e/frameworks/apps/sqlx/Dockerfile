FROM rust:1.88-bookworm@sha256:af306cfa71d987911a781c37b59d7d67d934f49684058f96cf72079c3626bfe0 AS build
# The machine running the harness is shared; keep compile parallelism modest.
ENV CARGO_BUILD_JOBS=4
RUN --mount=type=cache,id=turso-e2e-sqlx-registry,target=/usr/local/cargo/registry \
    cargo install sqlx-cli --version 0.8.6 --locked --no-default-features --features mysql,rustls --root /out
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN --mount=type=cache,id=turso-e2e-sqlx-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=turso-e2e-sqlx-target,target=/src/target \
    cargo build --locked && cp target/debug/sqlx-e2e /out/bin/

FROM debian:bookworm-20260918-slim@sha256:3783cc01769c7b2b1b83a5c5ad96c815348e28ed7da68e2e3687004faa906251
COPY --from=build /out/bin/sqlx /out/bin/sqlx-e2e /usr/local/bin/
WORKDIR /app
COPY migrations ./migrations
COPY run.sh ./run.sh
RUN chmod +x run.sh
