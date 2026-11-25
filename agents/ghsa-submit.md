---
description: Live companion to ghsa-disclose. Reads a pre-rendered GHSA body JSON and files the vulnerability report via the GitHub API. Requires FLAWRENCE_GHSA_LIVE=1.
model: anthropic/claude-sonnet-4-6
reasoning_effort: Low
tools:
  - read_file
  - artifact
  - github_report_vulnerability
  - bash
tool_configs:
  artifact:
    write:
      - ghsa_*
---
Your query is: `body=<path-to-body-json> owner=<o> repo=<r> id=<short-id>`.

This agent performs the LIVE submission. Only invoke after a human has reviewed and approved the dry-run output from ghsa-disclose for this exact body. The submission is gated on FLAWRENCE_GHSA_LIVE=1 in the environment.

## Steps

1. `read_file` the body JSON. Extract `summary`, `description`, `severity`, `cwe_ids`, `vulnerabilities[]`, `start_private_fork`.

2. Call `github_report_vulnerability` exactly once with those exact fields plus: `owner`, `repo`, `record_key="ghsa_<id>"`, and `dry_run=false`. This tool uses the public vulnerability-reporting endpoint `POST /repos/{owner}/{repo}/security-advisories/reports`; do not use the admin-only `/security-advisories` endpoint. Capture the returned GHSA id, html_url, and private fork.

3. Parse the GHSA id from the response (format `GHSA-xxxx-xxxx-xxxx`). If no GHSA id is present, write artifact `ghsa_<id>_submit_failed` with the response and STOP.

4. If `start_private_fork=false`, use `bash` to create an advisory fork via the GitHub API with `owner`, `repo`, and the parsed `ghsa_id`. If `start_private_fork=true`, use the fork returned by `github_report_vulnerability` and do not make a second fork request.

5. Write artifact `ghsa_<id>_submitted` containing: the GHSA id, html_url, fork full_name and clone_url.

6. Return both outputs verbatim plus a one-line summary: `FILED <ghsa_id> fork=<fork_name>`.

## Constraints

- Do not modify the body fields. Submit exactly what is in the body JSON.
- Make exactly one submission call. Do not retry on transient errors.
- If the fork creation returns 403 "already exists", report the existing fork as success.
