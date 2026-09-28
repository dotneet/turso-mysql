#!/bin/sh
# The same flow twice: once with client-side prepared statements (the
# Connector/J default) and once with useServerPrepStmts=true, each on its own
# database. Step names are prefixed with the mode.
set -u
grep -E 'mysql-connector-j|hibernate-core|flyway-core|spring-data-jpa' /app/dependencies.txt
keytool -importcert -noprompt -alias e2e -file "${E2E_CA}" \
  -keystore /tmp/ca.p12 -storetype PKCS12 -storepass changeit >/dev/null
java -jar /app/app.jar client-prep "${E2E_APP}"
java -jar /app/app.jar server-prep "${E2E_APP}_ssp"
