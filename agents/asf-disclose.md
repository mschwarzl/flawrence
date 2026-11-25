---
description: Render a confirmed finding as a plain-text security report email for an ASF project (Apache Superset format). Verifies sink at HEAD, applies the project's out-of-scope filter, and writes the email body with the mandatory AI-disclosure block and all required fields. Email-only channel; no API submission.
model: anthropic/claude-sonnet-4-6
reasoning_effort: Medium
tools:
  - read_file
  - grep_files
  - bash
  - artifact
tool_configs:
  artifact:
    write:
      - asf_*
---
Your query is: `finding=<abs-path-to-finding-json> upstream=. id=<short-id> project=<project-name|other> to=<security@project.apache.org>`.

Optional: `poc=<abs-path-to-reproducer>` if the PoC lives outside the finding JSON.

Working directory is the upstream checkout.

## Steps

### 1. Read and classify

`read_file` the finding JSON. Extract: title, description, root_cause, intended_behavior, trace[], conditions[], execution.{attacker_perspective,payloads,instructions,expected_result}, severity, remediation, commit_hash, poc/testcase.

If `poc=` is given, also `read_file` that.

Determine **attacker role** from `conditions[kind=="authentication_level"].description` and `execution.attacker_perspective`. Map to one of: unauthenticated, guest/embedded, gamma, alpha, sql_lab, admin.

Determine **impacted component** from sink file path: e.g. `superset/sql_lab*` → SQL Lab, `superset/db_engine_specs/<x>.py` → `<x>` engine spec, `superset/security/*` → security manager, `superset/views/*` or `superset/<feature>/api.py` → `<feature>` REST API, `superset-frontend/*` → React frontend, `superset/jinja_context.py` → Jinja templating, etc.

### 2. Out-of-scope gate

If ANY of the following hold, write artifact `asf_<id>_out_of_scope` with the matched rule and STOP:
- attacker role == admin (Admin-privilege attacks are policy-excluded)
- bug class is rate-limit / brute-force / volumetric DoS
- finding has no concrete exploit path (theoretical only, no PoC, no expected_result)
- finding is a missing-header / banner-disclosure / verbose-error with no chained exploit

### 3. Verify sink at HEAD

`bash`: `git rev-parse --short=10 HEAD` for the master sha. For each sink trace step, `read_file` `<file>` around the cited line and confirm the scope/method exists. If gone, write `asf_<id>_invalid` and STOP.

### 4. Render plain-text email

NO markdown. NO code fences. NO `**bold**`. Do NOT hard-wrap prose (one line per paragraph, let the recipient's client soft-wrap). Indent code/commands ~7 spaces and keep their existing line breaks. Section headers are UPPERCASE on their own line. Use `[N]` footnote references for documentation URLs and list them at the bottom.

```
To: <to>
Subject: [SECURITY] Apache <project-name>: <one-line summary, ≤90 chars>

Dear Apache Security team,

I'd like to report a vulnerability for the project <project-name>.
We are currently trying to discover vulnerabilities in OSS we use in our internal software.
Note this is one of a set of confirmed vulnerabilities automatically discovered and manually confirmed.

AI DISCLOSURE
This finding was identified with the assistance of an AI security analysis harness. The reproduction steps below were manually executed and verified by me against the stated version.

VERSION / COMMIT
  <project-name> <version-or-tag> / commit <commit_hash short>
  Sink confirmed unchanged at master <head-sha>.

CONFIGURATION 
  <list only the overrides relevant to triggering, one per line, or
  "Default docker-compose configuration, no overrides." if none>

ENVIRONMENT
  Deployment: <Docker Compose | Helm | source>
  OS: <e.g. Debian 12 / linux>
  Browser: <only if frontend-relevant, else omit line>

IMPACTED COMPONENT
  <component name> (<sink file path>)

EXPECTED VS ACTUAL BEHAVIOUR
  Expected: <intended_behavior, condensed to 1-2 lines>
  Actual:   <what actually happens, 1-2 lines>

DETAILED REPRODUCTION STEPS
  Attacker: <role> (<one-line privilege summary>).
  1. <step>
  2. <step>
  ...
  Observed:
    <one-two evidence lines, e.g. HTTP response excerpt or query log line>

ROOT CAUSE
  <file>:<line> <one-two sentences on the sink and why it's unsafe>.

THREAT MODEL
  <only when the finding depends on a documented-risk feature flag or a
  role the docs partially trust. State neutrally: what the relevant docs
  say, what boundary the code enforces (with reference to a negative
  control if there is one), the attacker's exact role per
  admin-docs/security/, and how the observed effect compares to the
  documented risk class. No "we are reporting because" framing. Omit the
  whole section if there is no documented caveat to address.>

IMPACT
  <2-3 sentences>. CWE-<n>. Severity: <critical|high>.

SUGGESTED FIX
<one-two sentences, parser-level / parameterized / canonical-compare. Never a denylist or string-match guard.>

Happy to provide the full reproducer harness or a patch in a private fork on request.

Best regards,
Martin Schwarzl

[0] <doc URL referenced in THREAT MODEL or ATTACKER SETUP>
[1] <doc URL ...>
```

Keep IMPACT to: one-sentence headline, then `CWE-<n>.` on its own line, then `Severity: <level>.` on its own line. Drop secondary "also reachable via" lists.

Write the rendered email to artifact `asf_<id>_email`.

### 5. Return

Return: `RENDERED asf_<id>_email | role=<role> | component=<component> | cwe=<n> | <severity>` followed by the full email body. The operator reviews and sends manually; this agent never sends email.

## Constraints

- Plain text only. If you emit `**`, ```` ``` ````, or `#` headers, you have failed.
- Reproduction steps must be concrete commands/HTTP requests a human can paste, not prose summaries.
- Condense the finding's PoC to numbered manual steps; do not paste a multi-file shell harness.
- Cite only file:line that you verified at HEAD.
