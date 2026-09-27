#!/usr/bin/env python3
"""Generate the frozen primary weights for MySQL's utf8mb4_unicode_ci.

Source data (Unicode Collation Algorithm 4.0.0):
  https://www.unicode.org/Public/UCA/4.0.0/allkeys-4.0.0.txt

Copyright 1991-2004 Unicode, Inc. Unicode data is distributed under the
Unicode License; see https://www.unicode.org/license.txt. This script does not
use MySQL's GPL-licensed generated collation tables.

MySQL gives a character outside the Basic Multilingual Plane the one weight
0xFFFD, so only BMP characters are written.

Run from the repository root:
  python3 core/translate/generate_mysql_uca400.py --allkeys /path/to/allkeys-4.0.0.txt
"""

import argparse
import hashlib
import pathlib
import re
import struct
import urllib.request

ALLKEYS_URL = "https://www.unicode.org/Public/UCA/4.0.0/allkeys-4.0.0.txt"
ALLKEYS_SHA256 = "e97345da79baf2ab6a72304fe84732b5d0c4b4c6adc888679fd17a6a546ec195"
OUTPUT = pathlib.Path(__file__).with_name("mysql_uca400_primary.bin")
CE = re.compile(r"\[([*.])([0-9A-F]{4})\.[0-9A-F]{4}\.[0-9A-F]{4}\.[0-9A-F]{4,5}\]")
# MySQL keeps at most eight collation elements for one character. A character
# with more gets the implicit weights of a character without an entry; in
# allkeys-4.0.0.txt that is only U+FDFA, which has eighteen.
MOST_ELEMENTS = 8


def source(path: pathlib.Path | None) -> str:
    data = path.read_bytes() if path else urllib.request.urlopen(ALLKEYS_URL).read()
    digest = hashlib.sha256(data).hexdigest()
    if digest != ALLKEYS_SHA256:
        raise ValueError(f"wrong SHA-256 for {path or ALLKEYS_URL}: {digest}")
    return data.decode("utf-8")


def parse_allkeys(data: str) -> dict[int, tuple[int, ...]]:
    if "@version 4.0.0" not in data:
        raise ValueError("not UCA 4.0.0 allkeys data")
    entries = {}
    for raw in data.splitlines():
        line = raw.split("#", 1)[0].strip()
        if not line or line.startswith("@"):
            continue
        characters, elements = line.split(";", 1)
        codepoints = characters.split()
        # MySQL's utf8mb4_unicode_ci has no contractions.
        if len(codepoints) != 1:
            continue
        codepoint = int(codepoints[0], 16)
        if codepoint > 0xFFFF:
            continue
        if codepoint in entries:
            raise ValueError(f"duplicate UCA entry for U+{codepoint:04X}")
        matches = list(CE.finditer(elements))
        if not matches or "".join(match.group(0) for match in matches) != elements.strip():
            raise ValueError(f"bad UCA elements for U+{codepoint:04X}")
        if len(matches) > MOST_ELEMENTS:
            if codepoint != 0xFDFA:
                raise ValueError(f"unexpected long UCA entry for U+{codepoint:04X}")
            continue
        # MySQL does not decompose Hangul syllables: allkeys-4.0.0.txt has no
        # entry for them, so they take implicit weights.
        entries[codepoint] = tuple(
            weight
            for match in matches
            if (weight := int(match.group(2), 16)) != 0
        )
    return entries


def write_table(entries: dict[int, tuple[int, ...]]) -> None:
    ordered = sorted(entries.items())
    weights = []
    records = bytearray()
    for codepoint, primary in ordered:
        records.extend(struct.pack("<HHB", codepoint, len(weights), len(primary)))
        weights.extend(primary)
    if len(weights) > 0xFFFF:
        raise ValueError("too many weights for 16-bit offsets")
    payload = bytearray(b"UCA400P1")
    payload.extend(struct.pack("<II", len(ordered), len(weights)))
    payload.extend(records)
    for weight in weights:
        payload.extend(struct.pack(">H", weight))
    OUTPUT.write_bytes(payload)
    print(f"wrote {OUTPUT}: {len(ordered)} entries, {len(weights)} weights, {len(payload)} bytes")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--allkeys", type=pathlib.Path)
    args = parser.parse_args()
    write_table(parse_allkeys(source(args.allkeys)))


if __name__ == "__main__":
    main()
