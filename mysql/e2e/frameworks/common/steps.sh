# Sourced by shell apps. `step NAME COMMAND...` runs one step, prints its
# output, and appends {"step","ok","error"} to $E2E_OUT/steps.jsonl.

json_escape() {
  awk 'BEGIN { ORS = "" }
    { gsub(/\\/, "\\\\"); gsub(/"/, "\\\""); gsub(/\t/, "\\t"); gsub(/\r/, "");
      if (NR > 1) print "\\n"; print }'
}

step() {
  name="$1"
  shift
  echo "=== step ${name}"
  if output="$("$@" 2>&1)"; then
    printf '%s\n' "${output}"
    printf '{"step":"%s","ok":true}\n' "${name}" >>"${E2E_OUT}/steps.jsonl"
  else
    printf '%s\n' "${output}"
    error="$(printf '%s' "${output}" | tail -c 2000 | json_escape)"
    printf '{"step":"%s","ok":false,"error":"%s"}\n' "${name}" "${error}" >>"${E2E_OUT}/steps.jsonl"
  fi
}

# mysql client over TLS through the proxy. Extra arguments are passed on.
sql() {
  MYSQL_PWD="${E2E_PASSWORD}" mysql --no-defaults --protocol=tcp \
    --host="${E2E_HOST}" --port="${E2E_PORT}" --user="${E2E_USER}" \
    --ssl-mode=VERIFY_IDENTITY --ssl-ca="${E2E_CA}" \
    --default-character-set=utf8mb4 "$@"
}
