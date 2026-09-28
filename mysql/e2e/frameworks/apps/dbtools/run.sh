#!/bin/sh
set -u
keytool -importcert -noprompt -alias e2e -file "${E2E_CA}" \
  -keystore /tmp/ca.p12 -storetype PKCS12 -storepass changeit >/dev/null
exec java -cp '/app/lib/*' e2e.Probe
