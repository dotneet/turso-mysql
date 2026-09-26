#!/usr/bin/env python3
"""Generate the frozen primary weights for MySQL's utf8mb4_0900_ai_ci.

Source data (Unicode 9.0.0):
  https://www.unicode.org/Public/UCA/9.0.0/allkeys.txt
  https://www.unicode.org/Public/9.0.0/ucd/PropList.txt

Copyright 2016 Unicode, Inc. Unicode data is distributed under the Unicode
License; see https://www.unicode.org/license.txt. This script does not use
MySQL's GPL-licensed generated collation tables.

Run from the repository root:
  python3 core/translate/generate_mysql_uca9.py --allkeys /path/to/allkeys.txt \
      --prop-list /path/to/PropList.txt
"""

import argparse
import hashlib
import pathlib
import re
import struct
import urllib.request

ALLKEYS_URL = "https://www.unicode.org/Public/UCA/9.0.0/allkeys.txt"
ALLKEYS_SHA256 = "0633f4520c99f249b0c53aa1442cd2521702041fb00a32df944fec13c9da3ed5"
PROP_LIST_URL = "https://www.unicode.org/Public/9.0.0/ucd/PropList.txt"
PROP_LIST_SHA256 = "f413ea8dbd3858de72f3148b47dd0586019761357d1481e3b65f3a025bc27f82"
OUTPUT = pathlib.Path(__file__).with_name("mysql_uca9_primary.bin")
CE = re.compile(r"\[([*.])([0-9A-F]{4})\.[0-9A-F]{4}\.[0-9A-F]{4}\]")


def source(path: pathlib.Path | None, url: str, expected_sha256: str) -> str:
    data = path.read_bytes() if path else urllib.request.urlopen(url).read()
    digest = hashlib.sha256(data).hexdigest()
    if digest != expected_sha256:
        raise ValueError(f"wrong SHA-256 for {path or url}: {digest}")
    return data.decode("utf-8")


def parse_allkeys(data: str) -> dict[int, tuple[int, ...]]:
    if "@version 9.0.0" not in data:
        raise ValueError("not UCA 9.0.0 allkeys data")
    entries = {}
    for raw in data.splitlines():
        line = raw.split("#", 1)[0].strip()
        if not line or line.startswith("@"):
            continue
        characters, elements = line.split(";", 1)
        codepoints = characters.split()
        # MySQL's non-language-specific collation treats contractions as
        # separate characters, according to the MySQL 8.4 manual.
        if len(codepoints) != 1:
            continue
        codepoint = int(codepoints[0], 16)
        if codepoint in entries:
            raise ValueError(f"duplicate UCA entry for U+{codepoint:04X}")
        matches = list(CE.finditer(elements))
        if not matches or "".join(match.group(0) for match in matches) != elements.strip():
            raise ValueError(f"bad UCA elements for U+{codepoint:04X}")
        primary = tuple(
            weight
            for match in matches
            if (weight := int(match.group(2), 16)) != 0
        )
        # MySQL 8.4's WEIGHT_STRING() has the first eight primary weights for
        # U+FDFA, whereas Unicode's DUCET has eighteen. This is the only
        # single-codepoint difference across all 29,809 DUCET entries.
        if codepoint == 0xFDFA:
            if len(primary) != 18:
                raise ValueError("unexpected U+FDFA DUCET weights")
            primary = primary[:8]
        entries[codepoint] = primary
    return entries


def add_hangul(entries: dict[int, tuple[int, ...]]) -> None:
    # UCA decomposes each modern Hangul syllable into L, V, and optional T
    # Jamo before looking up its collation elements.
    for codepoint in range(0xAC00, 0xD7A4):
        if codepoint in entries:
            raise ValueError("Hangul syllable unexpectedly has explicit weights")
        syllable = codepoint - 0xAC00
        jamo = [0x1100 + syllable // 588, 0x1161 + syllable % 588 // 28]
        if syllable % 28:
            jamo.append(0x11A7 + syllable % 28)
        entries[codepoint] = tuple(weight for cp in jamo for weight in entries[cp])


def unified_ideograph_ranges(data: str) -> list[tuple[int, int]]:
    ranges = []
    for raw in data.splitlines():
        line = raw.split("#", 1)[0].strip()
        if not line or ";" not in line:
            continue
        codepoints, property_name = (part.strip() for part in line.split(";", 1))
        if property_name != "Unified_Ideograph":
            continue
        bounds = codepoints.split("..")
        ranges.append((int(bounds[0], 16), int(bounds[-1], 16)))
    if not ranges:
        raise ValueError("Unicode 9 Unified_Ideograph ranges are missing")
    return ranges


def write_table(entries: dict[int, tuple[int, ...]], ranges: list[tuple[int, int]]) -> None:
    ordered = sorted(entries.items())
    weights = []
    records = bytearray()
    for codepoint, primary in ordered:
        if len(primary) > 255:
            raise ValueError(f"too many primary weights for U+{codepoint:04X}")
        records.extend(struct.pack("<IIB", codepoint, len(weights), len(primary)))
        weights.extend(primary)
    payload = bytearray(b"UCA9P1\0\0")
    payload.extend(struct.pack("<III", len(ordered), len(weights), len(ranges)))
    payload.extend(records)
    for weight in weights:
        payload.extend(struct.pack(">H", weight))
    for start, end in ranges:
        payload.extend(struct.pack("<II", start, end))
    OUTPUT.write_bytes(payload)
    print(f"wrote {OUTPUT}: {len(ordered)} entries, {len(weights)} weights, {len(payload)} bytes")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--allkeys", type=pathlib.Path)
    parser.add_argument("--prop-list", type=pathlib.Path)
    args = parser.parse_args()
    entries = parse_allkeys(source(args.allkeys, ALLKEYS_URL, ALLKEYS_SHA256))
    add_hangul(entries)
    ranges = unified_ideograph_ranges(source(args.prop_list, PROP_LIST_URL, PROP_LIST_SHA256))
    write_table(entries, ranges)


if __name__ == "__main__":
    main()
