"""Joins every app's step results and wire logs into summary.md/summary.json.

A step or statement counts as a turso difference only when it fails against
turso and the same app got past it against MySQL 8.4; statements that MySQL
refuses too are the app's own expected errors and are left out.
"""

import json
import re
import sys
from collections import defaultdict
from pathlib import Path

TARGETS = ("turso", "mysql")


def main() -> None:
    root = Path(sys.argv[1])
    apps = sorted(p.name for p in root.iterdir() if p.is_dir())
    summary = {"apps": {}, "refused": []}
    refused = {}
    for app in apps:
        runs = {t: load_run(root / app / t) for t in TARGETS}
        summary["apps"][app] = app_summary(runs)
        mysql_failed = {
            normalize(e["sql"]) for e in runs["mysql"]["wire"] if not e["ok"]
        }
        for e in runs["turso"]["wire"]:
            if e["ok"] or normalize(e.get("sql", "")) in mysql_failed:
                continue
            key = normalize(e.get("sql", "")) or f"<{e['command']}>"
            item = refused.setdefault(
                key,
                {
                    "sql": key,
                    "command": e["command"],
                    "code": e.get("code"),
                    "sqlstate": e.get("sqlstate"),
                    "message": e.get("message"),
                    "apps": [],
                    "count": 0,
                },
            )
            item["count"] += 1
            if app not in item["apps"]:
                item["apps"].append(app)
    summary["refused"] = sorted(
        refused.values(), key=lambda r: (-len(r["apps"]), r["code"] or 0, r["sql"])
    )
    (root / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    (root / "summary.md").write_text(render(summary))
    print(render_overview(summary))


def load_run(path: Path) -> dict:
    run = {"steps": [], "wire": [], "exit": None}
    if (path / "steps.jsonl").exists():
        run["steps"] = read_jsonl(path / "steps.jsonl")
    if (path / "wire.jsonl").exists():
        run["wire"] = read_jsonl(path / "wire.jsonl")
    if (path / "exit.json").exists():
        run["exit"] = json.loads((path / "exit.json").read_text())
    return run


def read_jsonl(path: Path) -> list:
    rows = []
    for line in path.read_text(errors="replace").splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            rows.append(json.loads(line))
        except json.JSONDecodeError:
            rows.append({"step": "<unparsable line>", "ok": False, "error": line[:500]})
    return rows


def app_summary(runs: dict) -> dict:
    names = []
    for t in TARGETS:
        for s in runs[t]["steps"]:
            if s["step"] not in names:
                names.append(s["step"])
    steps = []
    for name in names:
        row = {"step": name}
        for t in TARGETS:
            found = [s for s in runs[t]["steps"] if s["step"] == name]
            row[t] = None if not found else all(s["ok"] for s in found)
            if found and not row[t]:
                row[f"{t}_error"] = next(s.get("error", "") for s in found if not s["ok"])
        steps.append(row)
    return {
        "steps": steps,
        "exit": {t: runs[t]["exit"] for t in TARGETS},
        "statements": {t: len(runs[t]["wire"]) for t in TARGETS},
        "refused": {t: sum(1 for e in runs[t]["wire"] if not e["ok"]) for t in TARGETS},
    }


def normalize(sql: str) -> str:
    return re.sub(r"\s+", " ", sql or "").strip()


def mark(value) -> str:
    return {True: "pass", False: "FAIL", None: "-"}[value]


def render_overview(summary: dict) -> str:
    lines = []
    for app, s in summary["apps"].items():
        diff = [r["step"] for r in s["steps"] if r["turso"] is False and r["mysql"]]
        both = [r["step"] for r in s["steps"] if r["turso"] is False and r["mysql"] is False]
        lines.append(
            f"{app}: {sum(1 for r in s['steps'] if r['turso'])}/{len(s['steps'])} steps pass on turso;"
            f" turso-only failures: {', '.join(diff) or 'none'}"
            + (f"; failing on both: {', '.join(both)}" if both else "")
        )
    lines.append(f"distinct statements refused only by turso: {len(summary['refused'])}")
    return "\n".join(lines)


def render(summary: dict) -> str:
    out = ["# Framework E2E results", "", "```", render_overview(summary), "```", ""]
    for app, s in summary["apps"].items():
        out += [f"## {app}", ""]
        exits = ", ".join(
            f"{t}: exit {e['exit']} in {e['seconds']}s" if e else f"{t}: not run"
            for t, e in s["exit"].items()
        )
        out += [
            f"{exits}; statements turso {s['statements']['turso']} "
            f"(refused {s['refused']['turso']}), mysql {s['statements']['mysql']} "
            f"(refused {s['refused']['mysql']})",
            "",
            "| step | turso | mysql 8.4 |",
            "|---|---|---|",
        ]
        for r in s["steps"]:
            out.append(f"| {r['step']} | {mark(r['turso'])} | {mark(r['mysql'])} |")
        out.append("")
        for r in s["steps"]:
            if r["turso"] is False:
                err = (r.get("turso_error") or "").strip()[-800:]
                out += [f"- `{r['step']}` on turso:", "", "```", err, "```", ""]
    out += ["## Statements refused only by turso", ""]
    groups = defaultdict(list)
    for r in summary["refused"]:
        groups[(r["code"], r["message"])].append(r)
    for (code, message), rows in sorted(groups.items(), key=lambda kv: (-len(kv[1]), kv[0][0] or 0)):
        out += [f"### {code} {message}", ""]
        for r in rows:
            out.append(f"- apps: {', '.join(r['apps'])} ({r['count']}x)")
            out += ["", "```sql", r["sql"][:4000], "```", ""]
    return "\n".join(out) + "\n"


if __name__ == "__main__":
    main()
