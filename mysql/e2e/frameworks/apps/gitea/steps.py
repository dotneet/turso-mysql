"""Turns `go test -json` events into one harness step per top-level test.

A test that never reports (the binary stopped first) is a failed step, and a
suite that stopped before any test is one failed step named `suite`.
"""
import json
import sys

events, steps_path = sys.argv[1], sys.argv[2]
output, outcome, order, package = {}, {}, [], []
for line in open(events, encoding="utf-8", errors="replace"):
    try:
        event = json.loads(line)
    except json.JSONDecodeError:
        continue
    test = event.get("Test")
    if not test:
        if event.get("Action") == "output":
            package.append(event.get("Output", ""))
        continue
    if "/" in test:
        continue
    if test not in output:
        output[test] = []
        order.append(test)
    if event.get("Action") == "output":
        output[test].append(event.get("Output", ""))
    elif event.get("Action") in ("pass", "fail", "skip"):
        outcome[test] = event["Action"]
with open(steps_path, "a", encoding="utf-8") as steps:
    if not order:
        record = {"step": "suite", "ok": False, "error": "".join(package)[-4000:]}
        steps.write(json.dumps(record) + "\n")
    for test in order:
        result = outcome.get(test, "fail")
        if result == "skip":
            continue
        record = {"step": test, "ok": result == "pass"}
        if result != "pass":
            record["error"] = "".join(output[test])[-4000:]
        steps.write(json.dumps(record) + "\n")
