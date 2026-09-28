#!/bin/sh
# Runs Gitea's integration suite against $E2E_HOST:$E2E_PORT (the harness
# proxy) over TLS, and turns each top-level test into one step.
#
# E2E_GITEA_RUN picks the tests (a `go test -run` pattern); the default is a
# spread across the API, the web UI, pulls, issues, orgs and admin.
set -u
cd /gitea
# The suite's MySQL settings, pointed at the harness: TLS is required there
# (the proxy serves the test certificate), and the issue indexer is the
# built-in one, the template's Elasticsearch not being part of the harness.
sed -e 's/^SSL_MODE = disable/SSL_MODE = skip-verify/' \
  -e 's/^ISSUE_INDEXER_TYPE = elasticsearch/ISSUE_INDEXER_TYPE = bleve/' \
  -e '/^ISSUE_INDEXER_CONN_STR/d' \
  tests/mysql.ini.tmpl >tests/mysql.ini.tmpl.e2e && mv tests/mysql.ini.tmpl.e2e tests/mysql.ini.tmpl
export GITEA_TEST_DATABASE=mysql
export TEST_MYSQL_HOST="${E2E_HOST}:${E2E_PORT}"
export TEST_MYSQL_DBNAME=giteatest
export TEST_MYSQL_USERNAME="${E2E_USER}"
export TEST_MYSQL_PASSWORD="${E2E_PASSWORD}"
# Tests that register a mock Actions runner fail here against MySQL too (the
# runner gets no token in this setup) and one of them panics, which ends the
# whole binary; they say nothing about the database and are left out.
skip="^($(paste -sd'|' /gitea/runner-tests.txt))\$"
# E2E_GITEA_RUN narrows the tests (a `go test -run` pattern); by default every
# test runs, in E2E_GITEA_SHARDS shards so that a test that panics ends its
# own shard and no other.
pattern="${E2E_GITEA_RUN:-^Test}"
shards="${E2E_GITEA_SHARDS:-8}"
[ -n "${shards}" ] || shards=8
names=$(./integration.test -test.list "${pattern}" | grep '^Test' | grep -Ev "${skip}" | LC_ALL=C sort -u)
: >/tmp/test.jsonl
: >/tmp/test.stderr
status=0
shard=0
while [ "${shard}" -lt "${shards}" ]; do
  selected=$(echo "${names}" | awk -v r="${shard}" -v t="${shards}" '(NR - 1) % t == r' | paste -sd'|' -)
  shard=$((shard + 1))
  [ -n "${selected}" ] || continue
  echo "shard ${shard}/${shards}" >>/tmp/test.stderr
  # The container runs as root, which Gitea refuses; the suite runs as the
  # image's own user.
  setpriv --reuid=gitea --regid=gitea --init-groups env HOME=/home/gitea \
    go tool test2json -p integration ./integration.test -test.v=test2json \
    -test.run "^(${selected})\$" -test.timeout 60m >>/tmp/test.jsonl 2>>/tmp/test.stderr || status=1
done
cp /tmp/test.jsonl /tmp/test.stderr "${E2E_OUT}/"
python3 /app/steps.py /tmp/test.jsonl "${E2E_OUT}/steps.jsonl"
exit ${status}
