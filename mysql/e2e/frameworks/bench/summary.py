"""Reads sysbench outputs and writes summary.md: one row per workload and
thread count, turso next to MySQL."""
import pathlib
import re
import sys

out = pathlib.Path(sys.argv[1])
rows = {}
for path in sorted(out.glob("*-*-*.txt")):
    match = re.fullmatch(r"(turso|mysql)-(.+)-(\d+)\.txt", path.name)
    if not match:
        continue
    target, workload, threads = match.groups()
    text = path.read_text(errors="replace")
    tps = re.search(r"transactions:\s+\d+\s+\(([\d.]+) per sec", text)
    qps = re.search(r"queries:\s+\d+\s+\(([\d.]+) per sec", text)
    p95 = re.search(r"95th percentile:\s+([\d.]+)", text)
    errors = re.search(r"ignored errors:\s+(\d+)", text)
    rows.setdefault((workload, int(threads)), {})[target] = (
        float(tps.group(1)) if tps else None,
        float(qps.group(1)) if qps else None,
        float(p95.group(1)) if p95 else None,
        int(errors.group(1)) if errors else None,
    )


def cell(value, digits=0):
    return "-" if value is None else f"{value:,.{digits}f}"


lines = [
    "| workload | threads | turso tps | mysql tps | turso/mysql | turso p95 ms | mysql p95 ms | turso ignored errors |",
    "|---|---|---|---|---|---|---|---|",
]
for (workload, threads), by_target in sorted(rows.items()):
    turso = by_target.get("turso", (None,) * 4)
    mysql = by_target.get("mysql", (None,) * 4)
    ratio = turso[0] / mysql[0] if turso[0] and mysql[0] else None
    lines.append(
        f"| {workload} | {threads} | {cell(turso[0])} | {cell(mysql[0])} | {cell(ratio, 2)} "
        f"| {cell(turso[2], 2)} | {cell(mysql[2], 2)} | {cell(turso[3])} |"
    )
(out / "summary.md").write_text("\n".join(lines) + "\n")
