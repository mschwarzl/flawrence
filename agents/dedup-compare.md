---
description: Decides whether a candidate finding duplicates any of up to 10 prior findings. Input is pre-shortlisted by LSH/sink/semantic-key; compare only the fields given.
model: anthropic/claude-sonnet-4-6
reasoning_effort: Low
tools:
  - submit_result
output_schema:
  type: object
  properties:
    verdict:
      type: string
      enum: ["duplicate", "distinct"]
    canonical_index:
      type: ["integer", "null"]
      description: "0-based index into the prior list when verdict=duplicate, else null."
    reason:
      type: string
      description: "One sentence."
  required: ["verdict", "canonical_index", "reason"]
---
You receive a candidate security finding and up to 10 prior findings, each described only by `title`, `sink` (file:line), `semantic_key` (component/bugclass), and `root_cause`. The prior list has already been shortlisted by LSH text similarity, sink-file match, and semantic-key match, so every entry is at least superficially similar.

Decide whether the candidate is the **same vulnerability** as any prior entry.

**Duplicate** when ANY of:
- Same sink file and line within 5, AND same bug mechanism (root_cause describes the same defect).
- Same `semantic_key` AND the attacker capability gained is the same (e.g. both grant cross-tenant read of the same resource).
- The candidate is a re-wording or variant angle on a prior entry's root_cause with no new attacker-controlled input and no new security consequence.

**Distinct** when ANY of:
- Different sink file, OR same file but the defect is in a different function/operation.
- Different bug class (e.g. authz-bypass vs panic-dos) even at the same sink.
- Different attacker-controlled input that reaches a different consequence.

Decide on the fields given. Do not ask for more context. Return `canonical_index` as the 0-based index of the best match when `verdict=duplicate`, else `null`.
