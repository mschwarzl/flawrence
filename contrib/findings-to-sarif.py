#!/usr/bin/env python3
"""Convert flawrence finding artifacts to SARIF 2.1.0 for GitHub code scanning.

Usage:
    findings-to-sarif.py <artifacts-dir> [--out findings.sarif] [--include-drafts]

Reads `<artifacts-dir>/submitted/*.json` (confirmed findings) and, with
--include-drafts, `<artifacts-dir>/draft/*finding*.json`. Each finding JSON has
at least: finding_key, title, description, reported_severity, trace (list of
"path:line" strings or {"file": ..., "line": ...} objects).

SARIF level mapping: critical/high -> error, medium -> warning, low/info -> note.

The output is validated against the vendored SARIF 2.1.0 JSON schema
(`sarif-schema-2.1.0.json`, next to this script) before it is written; the
script exits non-zero if the document does not conform.
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

SCHEMA_PATH = Path(__file__).resolve().parent / "sarif-schema-2.1.0.json"


class SchemaValidationError(Exception):
    """A schema could not be interpreted, as opposed to a failed validation."""


def resolve_ref(ref: str, root: dict):
    """Resolve a local JSON-schema pointer like '#/definitions/result'.

    Only local pointers are supported, which is all the vendored schema uses.
    """
    if not ref.startswith("#"):
        raise SchemaValidationError(f"only local $ref pointers are supported, got {ref!r}")
    node = root
    for raw in ref.lstrip("#").strip("/").split("/"):
        if not raw:
            continue
        part = raw.replace("~1", "/").replace("~0", "~")
        if isinstance(node, list):
            node = node[int(part)]
        else:
            node = node[part]
    return node


def _type_matches(value, expected) -> bool:
    if isinstance(expected, list):
        return any(_type_matches(value, t) for t in expected)
    checks = {
        "object": lambda v: isinstance(v, dict),
        "array": lambda v: isinstance(v, list),
        "string": lambda v: isinstance(v, str),
        "integer": lambda v: isinstance(v, int) and not isinstance(v, bool),
        "number": lambda v: isinstance(v, (int, float)) and not isinstance(v, bool),
        "boolean": lambda v: isinstance(v, bool),
        "null": lambda v: v is None,
    }
    if expected not in checks:
        raise SchemaValidationError(f"unsupported type keyword {expected!r}")
    return checks[expected](value)


def validate(instance, schema, root=None, path="$", errors=None) -> list:
    """Validate a JSON value against a draft-04 schema subset.

    Supports exactly the keywords the vendored SARIF 2.1.0 schema uses:
    $ref, type, enum, allOf/anyOf/oneOf, object keywords (required,
    properties, additionalProperties), array keywords (items, minItems),
    string keywords (pattern, minLength, maxLength) and number keywords
    (minimum, maximum). `format` is treated as an annotation, as draft-04
    permits. Returns the list of violation messages; an empty list means
    the instance conforms.
    """
    if errors is None:
        errors = []
    if root is None:
        root = schema
    if schema is True:
        return errors
    if schema is False:
        errors.append(f"{path}: schema is always false")
        return errors
    if not isinstance(schema, dict):
        raise SchemaValidationError(f"{path}: schema must be an object")
    if "$ref" in schema:
        validate(instance, resolve_ref(schema["$ref"], root), root, path, errors)
        return errors
    if "enum" in schema and instance not in schema["enum"]:
        errors.append(f"{path}: {instance!r} is not one of {schema['enum']!r}")
    if "type" in schema and not _type_matches(instance, schema["type"]):
        errors.append(f"{path}: expected type {schema['type']!r}, got {type(instance).__name__}")
        return errors
    for sub in schema.get("allOf", []):
        validate(instance, sub, root, path, errors)
    if "anyOf" in schema:
        attempts = [validate(instance, sub, root, path, []) for sub in schema["anyOf"]]
        if all(attempts):
            errors.append(f"{path}: does not match any of the {len(attempts)} anyOf branches")
    if "oneOf" in schema:
        attempts = [validate(instance, sub, root, path, []) for sub in schema["oneOf"]]
        matches = attempts.count([])
        if matches != 1:
            errors.append(f"{path}: matches {matches} of the {len(attempts)} oneOf branches, expected 1")
    if isinstance(instance, dict):
        for key in schema.get("required", []):
            if key not in instance:
                errors.append(f"{path}: missing required property {key!r}")
        properties = schema.get("properties", {})
        additional = schema.get("additionalProperties", True)
        for key, value in instance.items():
            if key in properties:
                validate(value, properties[key], root, f"{path}.{key}", errors)
            elif additional is False:
                errors.append(f"{path}: unexpected property {key!r}")
            elif isinstance(additional, dict):
                validate(value, additional, root, f"{path}.{key}", errors)
    if isinstance(instance, list):
        items = schema.get("items")
        if isinstance(items, list):
            for i, sub in enumerate(items[: len(instance)]):
                validate(instance[i], sub, root, f"{path}[{i}]", errors)
        elif items is not None:
            for i, value in enumerate(instance):
                validate(value, items, root, f"{path}[{i}]", errors)
        min_items = schema.get("minItems")
        if min_items is not None and len(instance) < min_items:
            errors.append(f"{path}: array has {len(instance)} item(s), minimum is {min_items}")
    if isinstance(instance, str):
        pattern = schema.get("pattern")
        if pattern is not None and re.search(pattern, instance) is None:
            errors.append(f"{path}: {instance!r} does not match pattern {pattern!r}")
        min_length = schema.get("minLength")
        if min_length is not None and len(instance) < min_length:
            errors.append(f"{path}: string is shorter than minLength {min_length}")
        max_length = schema.get("maxLength")
        if max_length is not None and len(instance) > max_length:
            errors.append(f"{path}: string is longer than maxLength {max_length}")
    if isinstance(instance, (int, float)) and not isinstance(instance, bool):
        minimum = schema.get("minimum")
        if minimum is not None and instance < minimum:
            errors.append(f"{path}: {instance} is below minimum {minimum}")
        maximum = schema.get("maximum")
        if maximum is not None and instance > maximum:
            errors.append(f"{path}: {instance} is above maximum {maximum}")
    return errors


def validate_document(document, schema_path: Path = SCHEMA_PATH) -> list:
    """Validate a SARIF document against the vendored SARIF 2.1.0 schema.

    Returns the list of violation messages, empty when the document conforms.
    """
    schema = json.loads(schema_path.read_text())
    return validate(document, schema)


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


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("artifacts_dir")
    ap.add_argument("--out", default="findings.sarif")
    ap.add_argument("--include-drafts", action="store_true")
    args = ap.parse_args(argv)

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
    violations = validate_document(sarif)
    if violations:
        print(f"generated SARIF does not conform to {SCHEMA_PATH.name}:", file=sys.stderr)
        for v in violations:
            print(f"  {v}", file=sys.stderr)
        return 2

    Path(args.out).write_text(json.dumps(sarif, indent=2))
    print(f"wrote {len(results)} result(s) to {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
