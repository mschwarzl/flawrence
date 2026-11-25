---
name: finding-dedup
description: Two-stage duplicate detection for security-audit-report/v1 findings. Stage 1 is built-in feature shortlisting (text similarity, sink-file proximity, semantic component/bugclass key). Stage 2 is an LLM comparison restricted to the top-10 shortlist on four fields only. Used by the dedup_check and submit_finding tools.
---

# Finding dedup

## Stage 1: built-in feature shortlist

Three independent signals over `<findings_dir>/{submitted,draft,validated}/*.json`:

| signal | match condition |
|---|---|
| sink-proximity | same `trace[sink].file` and `abs(line_a - line_b) <= 5` |
| text similarity | Jaccard(title+root_cause words) >= 0.30 |
| semantic-key | same `(component, bugclass)` derived from title prefix or sink path + 31-pattern bugclass taxonomy |

Flawrence scans the local `submitted/` and `validated/` JSON corpus and retains only entries matching at least one signal. No Python installation or home-directory state is involved.

Verdict from stage 1 alone:
- `NEW` (exit 0): no signal matched
- `DUP-WEAK` (exit 10): one signal matched
- `DUP-STRONG` (exit 11): two+ signals, or sink-exact + same bugclass, or sink-exact + jaccard>=0.12, or filename contains `amendment`

The script also emits `shortlist`: top-N candidates by `(strong desc, jaccard desc)` with `{file, strong, jaccard, signals, sink, semantic_key, title, root_cause[:300]}`.

## Stage 2: LLM compare (`dedup-compare` agent)

The shortlist (max 10 entries) plus the candidate's `{title, root_cause, sink, semantic_key}` go to a fresh-context Sonnet 4.6 task. **Nothing else** is sent: not the full description, not the trace, not the conditions. The agent returns `{verdict: duplicate|distinct, canonical_index, reason}`.

## Combining

| stage 1 | stage 2 | final |
|---|---|---|
| DUP-STRONG | any | DUP-STRONG |
| any | duplicate | DUP-STRONG |
| DUP-WEAK | distinct | NEW (LLM overrules single weak signal) |
| NEW | distinct | NEW |

`submit_finding` blocks on final `DUP-STRONG` unless `force=true`.

## In-flight dedup

Separately, `report_finding` appends every claimed sink to the `shared_claimed_sinks` artifact under the task-tree write lock and short-circuits any second claim on the same sink file within 5 lines. This catches parallel research agents before validation runs; the two-stage check above catches cross-run duplicates against the persisted corpus.
