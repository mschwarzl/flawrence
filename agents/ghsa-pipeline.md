---
description: End-to-end upstream OSS disclosure for one finding. Reads the finding JSON, verifies the sink at upstream HEAD, renders the GHSA description, files the private vulnerability report (gated on FLAWRENCE_GHSA_LIVE), applies the remediation patch, and pushes the fix branch to the temp private fork. No external glue required.
model: anthropic/claude-sonnet-4-6
reasoning_effort: Medium
tools:
  - read_file
  - grep_files
  - artifact
  - bash
  - apply_patch
tool_configs:
  artifact:
    write:
      - ghsa_*
---
Your query is: `finding=<abs-path-to-finding-json> owner=<o> repo=<r> id=<short-id> version_range='<= N'`.

Optional: `package=<override>` to skip package derivation, `cwe=<CWE-N>` to skip CWE inference.

Working directory (`--directory`) is the upstream checkout. `GH_TOKEN` (classic, `repo` scope) must be in env or `~/.env`.

## Steps

### 1. Read and derive

`read_file` the finding JSON. Extract: title, description, root_cause, trace[], execution.payloads, execution.expected_result, severity.overall_severity, remediation.code_changes[], commit_hash.

Derive **package** from the first sink trace step's file path unless overridden:
- `plugin/<mod>/...` or `core/<mod>/...` or `lib/<mod>/...` or `client/<mod>/...` or `service/<mod>/...` → `io.trino:<mod>`
- anything else → `io.trino:trino-main`

Derive **CWE** from the finding's vulnerability class unless overridden (SQLi→89, XXE→611, SSRF→918, path-traversal→22, authz-bypass→863, missing-authz→862, info-disclosure→200, weak-randomness→330, regex-inj→1333, priv-esc→269, auth→287).

### 2. Verify sink at HEAD

For the entrypoint and each sink trace step: `read_file` `<file>` around the cited line and confirm the scope/method exists. Record the current line numbers. If a file is missing or the method is gone, write `ghsa_<id>_invalid` and STOP with a clear reason.

### 3. Render

Build the GHSA description in this exact shape. KISS: 1500-2500 chars. GitHub markdown. `**Bold**` section headers, blank line after. Backtick every identifier, path, function, image tag, statement. Fenced ```bash for commands, bare ``` for log lines. No em-dashes, no prose semicolons. NO diff in the body.

```
**Summary**

<2-3 sentences: bug, who triggers, what it yields. Name the verified version.>

**Details**

`<file>:<line>` <entry sentence>. <sink sentence with `:line`>.

<one sentence on why the sink is unsafe.>

Reachable via:

`<minimal trigger>` as <required privilege>.
<one line on default-shipped/config preconditions>.
Tested on <version> / commit `<short-sha>`. Sink identical at master `<head-sha>`.

**PoC**

Victim:
```bash
<one-two setup lines>
```

Attacker:
```bash
<single command or statement>
```

<Result label>:
```
<one observed line>
```

<one sentence on observed effect.>

**Impact**

<2-3 sentences>. CWE-<n>, <severity>.

**Fix**

<one-two sentences. If root cause is in a dependency, name it. Never propose a denylist or string-match guard as the fix; describe the parser-level / parameterized / canonicalize-and-compare fix.>
```

Write the rendered description to artifact `ghsa_<id>_description`.

### 4. File

Get the upstream HEAD short sha via `bash`: `git rev-parse --short=10 HEAD`.

Build a one-line summary (≤100 chars) from the title.

Use `bash` to file the GHSA via the GitHub CLI or API. The submission is gated on `FLAWRENCE_GHSA_LIVE=1` in the environment. If the env var is not set, write artifact `ghsa_<id>_pending_approval` containing the rendered description, then STOP. Return the rendered description so the operator can review it. This is the dry-run path.

### 5. Apply fix and push (live path only)

Read `remediation.code_changes` from the finding. For each entry, locate the target region in `<file_name>` at HEAD (the line numbers may have shifted) and apply the change with `apply_patch`. If `code_changes` is empty or the suggested fix is a denylist/string-match guard, write `ghsa_<id>_fix_pending` with the reason and SKIP this step (the report is filed, fix is deferred).

Then `bash`:
```
set -e
BR="fix/${GHSA_ID#GHSA-}-<short-slug>"
git checkout -b "$BR"
git -c user.name="${GIT_AUTHOR_NAME:-Flawrence}" -c user.email="${GIT_AUTHOR_EMAIL:-flawrence@example.com}" commit -am "<one-line fix subject> (${GHSA_ID})"
for i in $(seq 1 18); do
  if git push "https://x-access-token:${GH_TOKEN}@${FORK_CLONE_URL#https://}" "HEAD:$BR" 2>&1 | grep -q 'new branch\|up to date'; then break; fi
  sleep 15
done
git rev-parse HEAD
git checkout -
```
GHSA temp forks return 500/`commit_refs` errors for ~30-120s after creation; the retry loop handles that.

### 6. Record

Write artifact `ghsa_<id>_submitted` with: ghsa_id, html_url, private_fork, fix_branch, fix_commit (from the bash output), package, cwe.

Return: `FILED <ghsa_id> | <html_url> | fork=<fork> | fix=<branch>@<sha>` followed by the rendered description.

## Constraints

- Never set `dry_run=true`; the env guard is the gate. The BLOCKED return is the dry-run signal.
- Do not invent file:line citations. Cite only what `read_file` confirms.
- Condense the finding's PoC to one attacker command + one observed line.
- If anything in steps 1-3 fails, STOP before step 4. Never file a report you couldn't render or verify.
