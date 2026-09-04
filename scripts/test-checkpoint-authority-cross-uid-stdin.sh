#!/usr/bin/env bash

set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
test_root="$(mktemp -d "${TMPDIR:-/tmp}/turso-mysql-cross-uid-stdin.XXXXXX")"
readonly project_root test_root
readonly fake_bin="${test_root}/bin"
readonly artifact_dir="${test_root}/artifacts"
readonly gate_script="${project_root}/scripts/test-checkpoint-authority-cross-uid.sh"

cleanup() {
  rm -rf "${test_root}"
}
trap cleanup EXIT

mkdir -p "${fake_bin}" "${artifact_dir}/deps"

cat >"${fake_bin}/docker" <<'FAKE_DOCKER'
#!/usr/bin/env bash
set -euo pipefail
if [[ "${1:-}" == info ]]; then
  printf '%s\n' linux
  exit 0
fi
if [[ "${1:-}" == run ]]; then
  exit 0
fi
exit 1
FAKE_DOCKER
cat >"${fake_bin}/file" <<'FAKE_FILE'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' 'ELF 64-bit LSB pie executable, ARM aarch64'
FAKE_FILE
chmod 0755 "${fake_bin}/docker" "${fake_bin}/file"

for artifact in \
  turso-mysql-checkpoint-authority \
  turso-mysql-offline-provision \
  turso-mysql-server \
  deps/privileged_cross_uid-test \
  deps/unix_e2e-test \
  deps/tcp_e2e-test; do
  touch "${artifact_dir}/${artifact}"
  chmod 0755 "${artifact_dir}/${artifact}"
done

set +e
output="$(
  PATH="${fake_bin}:${PATH}" \
    CROSS_UID_ARTIFACT_DIR="${artifact_dir}" \
    "${gate_script}" \
    "${artifact_dir}/deps/privileged_cross_uid-test" \
    "${artifact_dir}/deps/unix_e2e-test" \
    "${artifact_dir}/deps/tcp_e2e-test" 2>&1
)"
gate_status=$?
set -e

[[ "${gate_status}" -ne 0 ]] || {
  printf '%s\n' 'expected the gate to reject an empty Docker fixture result' >&2
  exit 1
}
[[ "${output}" == *'did not report completion'* ]] || {
  printf '%s\n' "unexpected gate output: ${output}" >&2
  exit 1
}
printf '%s\n' 'cross-UID stdin completion-marker regression: passed'
