---
name: trailmark
description: "Builds and queries multi-language source code graphs for security analysis. Includes pre-analysis passes for blast radius, taint propagation, privilege boundaries, and entry point enumeration. Use when analyzing call paths, mapping attack surface, finding complexity hotspots, enumerating entry points, tracing taint propagation, measuring blast radius, or building a code graph for audit prioritization. Prefer this over grepping the whole tree for call-path and symbol-relationship queries."
---

# Trailmark

Parses source code into a directed graph of functions, classes, calls, and
semantic metadata for security analysis.

## In This Flawrence (read first)

This harness ships a built-in **`trailmark` tool** that wraps a preinstalled
Trailmark CLI. It validates and bounds CLI output, then caches graphs under
`<findings_dir>/attack-surface/trailmark-graph.json` for the current harness
process. Prefer it over ad-hoc grepping for call relationships:

1. `trailmark` with `operation=analyze` — rebuild the graph. **Run this first.**
   An artifacts/findings dir is required only to cache it for later queries.
2. `trailmark` with `operation=entrypoints` — invoke Trailmark's entrypoint
   inventory directly.
3. `trailmark` with `operation=callers` / `operation=callees` and `target=<symbol>`
   — who calls / is called by one unambiguous symbol. Use the exact graph id if
   a short name is ambiguous.

The cached graph makes subsequent queries cheap, so reach for the tool instead
of reading whole files or grepping the whole tree to reconstruct call
relationships.

For richer queries the tool does not expose (e.g. `paths_between`, taint,
`complexity_hotspots`, `preanalysis()` subgraphs, LLM annotations), use a
preinstalled Trailmark Python environment as described below. Never run
`uv run` or install packages from inside the untrusted target repository: that
can execute or import project-controlled configuration and code. If Trailmark
is unavailable, report that limitation and use targeted index/read operations.

## When to Use

- Mapping call paths from user input to sensitive functions
- Finding complexity hotspots for audit prioritization
- Identifying attack surface and entrypoints
- Understanding call relationships in unfamiliar codebases
- Security review or audit preparation across polyglot projects
- Adding LLM-inferred annotations (assumptions, preconditions) to code units
- Pre-analysis before mutation testing or diagramming

## When NOT to Use

- Single-file scripts where a call graph adds no value (read the file directly)
- Architecture diagrams not derived from code (draw by hand)
- Runtime behavior analysis (trailmark is static, not dynamic)

## Rationalizations to Reject

| Rationalization | Why It's Wrong | Required Action |
|-----------------|----------------|-----------------|
| "I'll just read the source files manually" | Manual reading misses call paths, blast radius, and taint data | Build the graph and query it |
| "Pre-analysis isn't needed for a quick query" | Blast radius, taint, and privilege data are only available after `preanalysis()` | Run `engine.preanalysis()` before deep analysis |
| "The graph is too large, I'll bypass limits" | Unbounded graph output can exhaust the harness | Keep the built-in limits; split analysis by component or language |
| "Uncertain edges don't matter" | Dynamic dispatch is where type confusion bugs hide | Account for `uncertain` edges in security claims |
| "Single-language analysis is enough" | Polyglot repos have FFI boundaries where bugs cluster | Use the correct `--language` (or `auto`) per component |
| "Complexity hotspots are the only thing worth checking" | Low-complexity functions on tainted paths are high-value targets | Combine complexity with taint and blast radius data |

---

## Advanced CLI and Python API

Use these only with a dedicated environment installed outside the target tree.
Set `TRAILMARK_BIN` explicitly and invoke it directly; do not use `uv run` in
the target repository.

```bash
# Fail if the trusted, preinstalled binary is unavailable
TRAILMARK_BIN="${TRAILMARK_BIN:-$HOME/.venv-trailmark/bin/trailmark}"
test -x "$TRAILMARK_BIN"

# Auto-detect supported languages under the tree
"$TRAILMARK_BIN" analyze -l auto {targetDir}

# Analyze one explicit language
"$TRAILMARK_BIN" analyze -l rust {targetDir}
```

### Programmatic API

Run trusted scripts with the dedicated environment's interpreter, for example
`$HOME/.venv-trailmark/bin/python /trusted/path/query.py`. Keep the script
outside the target repository and pass `{targetDir}` as data.

```python
from trailmark.parse import detect_languages, supported_languages
from trailmark.query.api import QueryEngine

# Ask the installed Trailmark build what it supports
supported_languages()
detect_languages("{targetDir}")

# Prefer auto for unknown or polyglot trees; use explicit lists when needed
engine = QueryEngine.from_directory("{targetDir}", language="auto")

engine.callers_of("function_name")
engine.callees_of("function_name")
engine.paths_between("entry_func", "db_query")
engine.complexity_hotspots(threshold=10)
engine.attack_surface()
engine.summary()
engine.to_json()

# Run pre-analysis (blast radius, entrypoints, privilege boundaries, taint)
result = engine.preanalysis()

# Query subgraphs created by pre-analysis
engine.subgraph_names()
engine.subgraph("tainted")
engine.subgraph("high_blast_radius")
engine.subgraph("privilege_boundary")
engine.subgraph("entrypoint_reachable")

# Add LLM-inferred annotations
from trailmark.models import AnnotationKind

engine.annotate("function_name", AnnotationKind.ASSUMPTION,
                "input is URL-encoded", source="llm")

engine.annotations_of("function_name")
engine.annotations_of("function_name", kind=AnnotationKind.BLAST_RADIUS)
engine.annotations_of("function_name", kind=AnnotationKind.TAINT_PROPAGATION)
```

## Pre-Analysis Passes

Run `engine.preanalysis()` before deep analysis. It enriches the graph with four
passes:

1. **Blast radius estimation** — counts downstream and upstream nodes per
   function, identifies critical high-complexity descendants
2. **Entry point enumeration** — maps entrypoints by trust level, computes
   reachable node sets
3. **Privilege boundary detection** — finds call edges where trust levels
   change (untrusted -> trusted)
4. **Taint propagation** — marks all nodes reachable from untrusted entrypoints

Results are stored as annotations and named subgraphs on the graph. For detailed
documentation, see [references/preanalysis-passes.md](references/preanalysis-passes.md).

## Graph Model

**Node kinds:** `function`, `method`, `class`, `module`, `struct`,
`interface`, `trait`, `enum`, `namespace`, `contract`, `library`, `template`

**Edge kinds:** `calls`, `inherits`, `implements`, `contains`, `imports`

**Edge confidence:** `certain` (direct call, `self.method()`), `inferred`
(attribute access on non-self object), `uncertain` (dynamic dispatch)

### Per Code Unit
- Parameters with types, return types, exception types
- Cyclomatic complexity and branch metadata
- Docstrings
- Annotations: `assumption`, `precondition`, `postcondition`, `invariant`,
  `blast_radius`, `privilege_boundary`, `taint_propagation`, `finding`,
  `audit_note`

### Per Edge
- Source/target node IDs, edge kind, confidence level

### Project Level
- Dependencies (imported packages)
- Entrypoints with trust levels and asset values
- Named subgraphs (populated by pre-analysis)

## Key Concepts

**Declared contract vs. effective input domain:** Trailmark separates what a
function *declares* it accepts from what can *actually reach* it via call paths.
Mismatches are where vulnerabilities hide:
- **Widening**: Unconstrained data reaches a function that assumes validation
- **Safe by coincidence**: No validation, but only safe callers exist today

**Edge confidence:** Dynamic dispatch produces `uncertain` edges. Account for
confidence when making security claims.

**Subgraphs:** Named collections of node IDs produced by pre-analysis. Query
with `engine.subgraph("name")`. Available after `engine.preanalysis()`.

## Query Patterns

See [references/query-patterns.md](references/query-patterns.md) for common
security analysis patterns and [references/preanalysis-passes.md](references/preanalysis-passes.md)
for pre-analysis pass documentation.
