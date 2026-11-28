#!/usr/bin/env python3
"""Unit and integration tests for findings-to-sarif.py.

Run with:

    python3 contrib/test_findings_to_sarif.py

The suite is pure and offline: the filesystem is exercised through
`tempfile`, and schema validation runs against the vendored SARIF 2.1.0
schema in this directory. No network access is needed.
"""
import importlib.util
import json
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
SCHEMA_PATH = HERE / "sarif-schema-2.1.0.json"

_spec = importlib.util.spec_from_file_location("findings_to_sarif", HERE / "findings-to-sarif.py")
sarif_tool = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(sarif_tool)

SAMPLE_FINDING = {
    "finding_key": "SQLI-001",
    "title": "SQL injection in request handler",
    "description": "User-controlled input reaches a SQL query without parameterization.",
    "reported_severity": "high",
    "trace": ["src/main.rs:42", {"file": "src/db.rs", "line": 7, "symbol": "build_query"}],
}


def write_artifacts(base: Path, findings: list, subdir: str = "submitted") -> None:
    d = base / subdir
    d.mkdir(parents=True, exist_ok=True)
    for i, finding in enumerate(findings):
        (d / f"finding-{i}.json").write_text(json.dumps(finding))


class TraceLocationTests(unittest.TestCase):
    def test_trace_locations_parses_path_line_strings(self):
        locs = sarif_tool.trace_locations(["src/main.rs:42", "lib/util.py:3:helper"])
        self.assertEqual(locs, [("src/main.rs", 42, ""), ("lib/util.py", 3, "helper")])

    def test_trace_locations_accepts_dict_entries(self):
        locs = sarif_tool.trace_locations(
            [{"file": "src/db.rs", "line": 7, "symbol": "q"}, {"path": "a/b.c", "line": 1}]
        )
        self.assertEqual(locs, [("src/db.rs", 7, "q"), ("a/b.c", 1, "")])

    def test_trace_locations_skips_malformed_entries(self):
        locs = sarif_tool.trace_locations(["no-line-here", {"file": "x.rs"}, None, 42])
        self.assertEqual(locs, [])

    def test_trace_locations_handles_missing_trace(self):
        self.assertEqual(sarif_tool.trace_locations(None), [])


class FindingToResultTests(unittest.TestCase):
    def test_severity_maps_to_sarif_levels(self):
        cases = [
            ("critical", "error"), ("high", "error"), ("medium", "warning"),
            ("low", "note"), ("info", "note"), ("informational", "note"),
        ]
        for severity, level in cases:
            result = sarif_tool.finding_to_result({"finding_key": "K", "reported_severity": severity}, "t")
            self.assertEqual(result["level"], level)

    def test_unknown_severity_defaults_to_warning(self):
        result = sarif_tool.finding_to_result({"finding_key": "K", "reported_severity": "bizarre"}, "t")
        self.assertEqual(result["level"], "warning")

    def test_result_carries_rule_id_and_message(self):
        result = sarif_tool.finding_to_result(SAMPLE_FINDING, "t")
        self.assertEqual(result["ruleId"], "SQLI-001")
        self.assertEqual(result["message"]["text"], "SQL injection in request handler")
        self.assertEqual(result["properties"]["severity"], "high")

    def test_result_includes_trace_locations(self):
        result = sarif_tool.finding_to_result(SAMPLE_FINDING, "t")
        uris = [loc["physicalLocation"]["artifactLocation"]["uri"] for loc in result["locations"]]
        self.assertEqual(uris, ["src/main.rs", "src/db.rs"])
        region = result["locations"][1]["physicalLocation"]["region"]
        self.assertEqual(region["startLine"], 7)
        self.assertEqual(region["snippet"]["text"], "build_query")

    def test_result_without_trace_has_no_locations(self):
        result = sarif_tool.finding_to_result({"finding_key": "K", "title": "t"}, "t")
        self.assertNotIn("locations", result)


class ValidatorTests(unittest.TestCase):
    def test_valid_document_produces_no_violations(self):
        document = {
            "$schema": "https://raw.githubusercontent.com/oasis-tcs/sarif-spec/master/Schemata/sarif-schema-2.1.0.json",
            "version": "2.1.0",
            "runs": [{"tool": {"driver": {"name": "flawrence"}}, "results": []}],
        }
        self.assertEqual(sarif_tool.validate_document(document), [])

    def test_missing_version_is_rejected(self):
        document = {"runs": []}
        violations = sarif_tool.validate_document(document)
        self.assertTrue(any("version" in v for v in violations))

    def test_bad_level_enum_is_rejected(self):
        document = {
            "$schema": "https://example.com/sarif-schema-2.1.0.json",
            "version": "2.1.0",
            "runs": [{
                "tool": {"driver": {"name": "flawrence"}},
                "results": [{"ruleId": "R", "level": "fatal", "message": {"text": "m"}}],
            }],
        }
        violations = sarif_tool.validate_document(document)
        self.assertTrue(any("level" in v for v in violations))

    def test_wrong_type_is_rejected(self):
        document = {
            "$schema": "https://example.com/sarif-schema-2.1.0.json",
            "version": "2.1.0",
            "runs": "not-a-list",
        }
        violations = sarif_tool.validate_document(document)
        self.assertTrue(any("expected type" in v for v in violations))

    def test_schema_file_exists_and_parses(self):
        schema = json.loads(SCHEMA_PATH.read_text())
        self.assertEqual(
            schema.get("title"), "Static Analysis Results Format (SARIF) Version 2.1.0 JSON Schema"
        )
        self.assertIn("result", schema.get("definitions", {}))


class MainIntegrationTests(unittest.TestCase):
    def run_main(self, argv):
        return sarif_tool.main(argv)
    def test_main_writes_schema_conforming_sarif(self):
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp) / "artifacts"
            out = Path(tmp) / "out.sarif"
            write_artifacts(base, [SAMPLE_FINDING])
            rc = self.run_main([str(base), "--out", str(out)])
            self.assertEqual(rc, 0)
            document = json.loads(out.read_text())
            self.assertEqual(sarif_tool.validate_document(document), [])
            self.assertEqual(document["version"], "2.1.0")
            results = document["runs"][0]["results"]
            self.assertEqual(results[0]["ruleId"], "SQLI-001")
            rules = document["runs"][0]["tool"]["driver"]["rules"]
            self.assertEqual([r["id"] for r in rules], ["SQLI-001"])

    def test_main_include_drafts_picks_up_draft_findings(self):
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp) / "artifacts"
            out = Path(tmp) / "out.sarif"
            write_artifacts(base, [SAMPLE_FINDING])
            write_artifacts(base, [{**SAMPLE_FINDING, "finding_key": "XSS-002"}], subdir="draft")
            rc = self.run_main([str(base), "--out", str(out), "--include-drafts"])
            self.assertEqual(rc, 0)
            rule_ids = [r["id"] for r in json.loads(out.read_text())["runs"][0]["tool"]["driver"]["rules"]]
            self.assertEqual(sorted(rule_ids), ["SQLI-001", "XSS-002"])

    def test_main_fails_when_no_findings(self):
        with tempfile.TemporaryDirectory() as tmp:
            rc = self.run_main([str(Path(tmp) / "empty"), "--out", str(Path(tmp) / "out.sarif")])
            self.assertEqual(rc, 1)

    def test_main_skips_non_finding_json_files(self):
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp) / "artifacts"
            write_artifacts(base, [SAMPLE_FINDING])
            (base / "submitted" / "notes.json").write_text(json.dumps({"note": "not a finding"}))
            out = Path(tmp) / "out.sarif"
            rc = self.run_main([str(base), "--out", str(out)])
            self.assertEqual(rc, 0)
            results = json.loads(out.read_text())["runs"][0]["results"]
            self.assertEqual(len(results), 1)


if __name__ == "__main__":
    unittest.main()
