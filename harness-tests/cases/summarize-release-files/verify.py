import json
import pathlib
import sys


workspace = pathlib.Path(sys.argv[1])
summary = json.loads((workspace / "release-summary.json").read_text(encoding="utf-8"))

assert summary["release"] == "Aurora 2.4"
assert summary["ready"] is False
assert summary["owners"] == {
    "api": "Mina",
    "cli": "Jules",
    "ui": "Priya",
    "ops": "Chen",
}

blockers = "\n".join(summary["blockers"]).lower()
for phrase in ["rate-limit headers", "windows paths", "screen readers"]:
    assert phrase in blockers, f"missing blocker {phrase!r}"
assert "none" not in blockers
assert len(summary["blockers"]) == 3

completed = "\n".join(summary["completed"]).lower()
for phrase in [
    "cursor pagination",
    "json output",
    "non-interactive authentication",
    "keyboard navigation",
    "rollback drill",
]:
    assert phrase in completed, f"missing completed item {phrase!r}"
assert len(summary["completed"]) == 5

print("PASS summarize-release-files")
