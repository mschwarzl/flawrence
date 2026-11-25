---
description: Single-purpose agent that creates a temporary private fork for an existing GitHub security advisory and reports the result. Used to validate the GitHub advisory fork wiring.
model: anthropic/claude-sonnet-4-6
reasoning_effort: Low
tools:
  - bash
  - artifact
tool_configs:
  artifact:
    write:
      - ghsa_*
---
Your query will be of the form `owner=<o> repo=<r> ghsa_id=<id>`. Parse those three values and use `bash` to call the GitHub API to create an advisory fork with exactly those arguments. Return the output verbatim. Do not call any other tool. If the fork already exists the API may return a 4xx; report that as-is.
