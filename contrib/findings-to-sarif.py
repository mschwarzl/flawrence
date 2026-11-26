#!/usr/bin/env python3
"""Convert flawrence finding artifacts to SARIF 2.1.0 for GitHub code scanning.

Usage:
    findings-to-sarif.py <artifacts-dir> [--out findings.sarif] [--include-drafts]

Reads `<artifacts-dir>/submitted/*.json` (confirmed findings) and, with
--include-drafts, `<artifacts-dir>/draft/*finding*.json`. Each finding JSON has
at least: finding_key, title, description, reported_severity, trace (list of
"path:line" strings or {"file": ..., "line": ...} objects).

SARIF level mapping: critical/high -> error, medium -> warning, low/info -> note.
"""
import argparse
import json
import re
import sys
from pathlib import Path

LEVEL = {
    "critical": "error", "high": "error", "medium": "warning",
    "low": "note", "info": "note", "informational": "note",
}

TRACE_RE = re.compile(r"^(?P<file>[^:]+):(?P<line>\d+)(?::(?P<sym>.*))?$")


def trace_locations(trace):
    locs = []
    for entry in trace or []:
        if isinstance(entry, str):
            m = TRACE_RE.match(entry.strip())
            if m:
                locs.append((m.group("file"), int(m.group("line")), m.group("sym") or ""))
        elif isinstance(entry, dict):
            f = entry.get("file") or entry.get("path")
            line = entry.get("line")
            if f and line:
                locs.append((f, int(line), entry.get("symbol", "")))
    return locs


def finding_to_result(f: dict, tool_name: str) -> dict:
    key = f.get("finding_key", "unknown")
    severity = str(f.get("reported_severity", f.get("severity", "medium"))).lower()
    locs = trace_locations(f.get("trace"))
    result = {
        "ruleId": key,
        "level": LEVEL.get(severity, "warning"),
        "message": {"text": f.get("title", key)},
        "properties": {
            "severity": severity,
            "description": f.get("description", ""),
            "tags": ["security", "flawrence"],
        },
    }
    if locs:
        result["locations"] = [
            {
                "physicalLocation": {
                    "artifactLocation": {"uri": file},
                    "region": {"startLine": line, **({"snippet": {"text": sym}} if sym else {})},
                }
            }
            for file, line, sym in locs[:10]
        ]
    return result


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("artifacts_dir")
    ap.add_argument("--out", default="findings.sarif")
    ap.add_argument("--include-drafts", action="store_true")
    args = ap.parse_args()

    base = Path(args.artifacts_dir)
    dirs = [base / "submitted"]
    if args.include_drafts:
        dirs.append(base / "draft")

    findings = []
    for d in dirs:
        if not d.is_dir():
            continue
        for p in sorted(d.glob("*.json")):
            try:
                data = json.loads(p.read_text())
            except json.JSONDecodeError:
                continue
            if isinstance(data, dict) and "finding_key" in data:
                findings.append(data)

    if not findings:
        print(f"no findings found under {base}", file=sys.stderr)
        return 1

    rules = {}
    results = []
    for f in findings:
        key = f["finding_key"]
        rules.setdefault(key, {
            "id": key,
            "shortDescription": {"text": f.get("title", key)},
            "fullDescription": {"text": f.get("description", "")[:1000]},
            "defaultConfiguration": {"level": LEVEL.get(str(f.get("reported_severity", "medium")).lower(), "warning")},
            "properties": {"tags": ["security"]},
        })
        results.append(finding_to_result(f, "flawrence"))

    sarif = {
        "$schema": "https://raw.githubusercontent.com/oasis-tcs/sarif-spec/master/Schemata/sarif-schema-2.1.0.json",
        "version": "2.1.0",
        "runs": [{
            "tool": {"driver": {"name": "flawrence", "rules": list(rules.values())}},
            "results": results,
        }],
    }
    Path(args.out).write_text(json.dumps(sarif, indent=2))
    print(f"wrote {len(results)} result(s) to {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
