ROOT_DIR := $(shell cd ../.. && pwd)
COMPOSE := docker compose -f $(CURDIR)/compose.yaml
CASE ?= $(CURDIR)/cases/p0/smoke.json
OUTPUT ?= /tmp/turso-mysql-smoke.json
GOLDEN ?=
PARSER_REPORT ?= /tmp/turso-mysql-parser-report.json
LIFECYCLE_CASE ?= $(CURDIR)/cases/p0/auto-increment-restart.json
LIFECYCLE_OUTPUT ?= /tmp/turso-mysql-auto-increment-restart.json
LIFECYCLE_GOLDEN ?= $(PINNED_GOLDEN_DIR)/auto-increment-restart.json
PINNED_GOLDEN_DIR := $(CURDIR)/goldens/mysql-8.4/sha256-b3b90af2a6552ae30c266fdb7d5dd55f3afb72404bb78d37fe8a23eb857fd3fb
P0_CASES := smoke parser-ddl parser-quoting transaction-observer numeric-coercion numeric-mediumint collation-utf8mb4-0900-ai-ci auto-increment auto-increment-parallel show-columns information-schema-tables information-schema-columns

.PHONY: require-env require-dsn require-golden up down clean config logs client record verify record-lifecycle verify-lifecycle verify-p0 parser-report

require-env:
	@test -n "$${MYSQL_CONFORMANCE_PASSWORD:-}" || (echo "Set MYSQL_CONFORMANCE_PASSWORD." >&2; exit 2)
	@test -n "$${MYSQL_CONFORMANCE_ROOT_PASSWORD:-}" || (echo "Set MYSQL_CONFORMANCE_ROOT_PASSWORD." >&2; exit 2)

require-dsn:
	@test -n "$${MYSQL_ORACLE_DSN:-}" || (echo "Set MYSQL_ORACLE_DSN to the local oracle DSN." >&2; exit 2)

require-golden:
	@test -n "$(GOLDEN)" || (echo "Set GOLDEN to an expected observation file." >&2; exit 2)

up: require-env
	$(COMPOSE) up -d --wait

down:
	@MYSQL_CONFORMANCE_PASSWORD=unused MYSQL_CONFORMANCE_ROOT_PASSWORD=unused $(COMPOSE) down --remove-orphans

clean:
	@MYSQL_CONFORMANCE_PASSWORD=unused MYSQL_CONFORMANCE_ROOT_PASSWORD=unused $(COMPOSE) down --volumes --remove-orphans

config:
	$(COMPOSE) config --no-interpolate

logs:
	@MYSQL_CONFORMANCE_PASSWORD=unused MYSQL_CONFORMANCE_ROOT_PASSWORD=unused $(COMPOSE) logs mysql

client: require-env
	$(COMPOSE) exec mysql sh -ec 'MYSQL_PWD="$$MYSQL_PASSWORD" exec mysql --protocol=tcp -h 127.0.0.1 -u "$$MYSQL_USER" "$$MYSQL_DATABASE"'

record: require-dsn up
	@cd $(ROOT_DIR) && cargo run -p turso_mysql_conformance -- record --case "$(CASE)" --output "$(OUTPUT)"

verify: require-dsn require-golden up
	@cd $(ROOT_DIR) && cargo run -p turso_mysql_conformance -- verify --case "$(CASE)" --golden "$(GOLDEN)"

record-lifecycle: require-dsn up
	@cd $(ROOT_DIR) && MYSQL_CONFORMANCE_COMPOSE_FILE="$(CURDIR)/compose.yaml" cargo run -p turso_mysql_conformance -- record-lifecycle --case "$(LIFECYCLE_CASE)" --output "$(LIFECYCLE_OUTPUT)"

verify-lifecycle: require-dsn up
	@cd $(ROOT_DIR) && MYSQL_CONFORMANCE_COMPOSE_FILE="$(CURDIR)/compose.yaml" cargo run -p turso_mysql_conformance -- verify-lifecycle --case "$(LIFECYCLE_CASE)" --golden "$(LIFECYCLE_GOLDEN)"

verify-p0: require-dsn up
	@set -e; for case_name in $(P0_CASES); do \
		cd $(ROOT_DIR) && cargo run -p turso_mysql_conformance -- verify \
			--case "$(CURDIR)/cases/p0/$$case_name.json" \
			--golden "$(PINNED_GOLDEN_DIR)/$$case_name.json"; \
	done; \
	cd $(ROOT_DIR) && MYSQL_CONFORMANCE_COMPOSE_FILE="$(CURDIR)/compose.yaml" cargo run -p turso_mysql_conformance -- verify-lifecycle --case "$(LIFECYCLE_CASE)" --golden "$(LIFECYCLE_GOLDEN)"

parser-report: require-golden
	@cd $(ROOT_DIR) && cargo run -p turso_mysql_conformance -- parser-report --case "$(CASE)" --golden "$(GOLDEN)" --output "$(PARSER_REPORT)"
