#!/usr/bin/env python3

import difflib
import json
import sys
from pathlib import Path


def main() -> int:
    if len(sys.argv) != 3:
        raise SystemExit("usage: compare-mysql-parity-probes.py ORACLE_JSON TURSO_JSON")
    oracle = json.loads(Path(sys.argv[1]).read_text())
    turso = json.loads(Path(sys.argv[2]).read_text())
    if oracle == turso:
        print(f"MySQL direct parity probe: {len(oracle)} matching SQL observations")
        return 0
    expected = json.dumps(oracle, indent=2, sort_keys=True).splitlines()
    actual = json.dumps(turso, indent=2, sort_keys=True).splitlines()
    print("\n".join(difflib.unified_diff(expected, actual, fromfile="MySQL 8.4.11", tofile="Turso")))
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
