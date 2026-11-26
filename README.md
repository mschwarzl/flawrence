# Flawrence

Flawrence finds security vulnerabilities in source code with LLM agents.
It reads a codebase, builds a threat model, maps the attack surface and gives every entry point its own research agent.
A second agent validates each candidate finding and tries to reproduce it.
Only findings with a verified reproducer are submitted. A proof of concept is then written for each one.

Flawrence is written in Rust.
It works with Anthropic, OpenAI, Cloudflare Workers AI and any OpenAI-compatible endpoint.

The bugs Flawrence has found so far are listed in [FINDINGS.md](FINDINGS.md).

<img width="2388" height="8630" alt="image" src="https://github.com/user-attachments/assets/f2d1acea-aa25-4e3f-9e3a-6d740c6a9a28" />


> [!WARNING]
> Agents run shell commands, build the target and execute proofs of concept.
> Run Flawrence in a container or a VM. Never point it at a machine you care about.
> A full review fans out to many agents and can cost a lot of tokens. Set `MAX_COST_USD`.
> Frontier models give the best results, but their APIs are expensive and not available to everyone.
> Self-hosted open-weight models without refusals are the cheaper alternative.
> We made very good findings with Qwen, GLM, DeepSeek and Kimi models, rivalling frontier model performance wrt. discovering vulnerabilities, often at a fraction of the cost.  
> In addition, without access to Project Glasswing, Daybreak Red, or similar programmes, frontier models will often outright refuse to look for vulnerabilities, let alone provide proof-of-concept exploits, even though those are crucial when it comes to validating a claimed finding.

## Quickstart

```sh
cargo build --release
export ANTHROPIC_API_KEY=sk-ant-...

target/release/flawrence-runner --agent security-review \
  --directory /path/to/target --artifacts out \
  "Review this codebase for security issues"
```

The runner loads a `.env` file from the working directory.
Copy `.env.example` to `.env` to get started.

Every agent names a default model in its definition.
Use `--model <provider>/<model>` to override it for one run, or set `FLAWRENCE_MODEL`.

## How a review works

The `security-review` agent runs this pipeline.

1. `threat-model` describes the application, its attackers and its trust boundaries.
2. `attack-surface` lists every function an attacker can reach.
3. `test-idiom` finds a reliable way to run small reproducers against the target.
4. One `vulnerability-research` agent runs per attack surface entry. It writes a `finding_*` artifact for every candidate.
5. `vulnerability-validation` checks each finding and rejects false positives.
6. `vulnerability-poc` writes and runs a proof of concept for each confirmed vulnerability.
7. `dedup_check` drops duplicates. `submit_finding` stores the rest under `<artifacts>/submitted/`.
8. `write_advisory` writes a GitHub Security Advisory draft for each submitted finding.

The final report lists submitted findings, blocked findings, duplicates, false positives and open questions for you.

Other agents cover the steps after a review.

| Agent | Purpose |
| --- | --- |
| `vulnerability-patch` | Writes a fix for a confirmed vulnerability. Verifies it, reviews it and tries to bypass it. |
| `ghsa-pipeline` | Files a private vulnerability report on GitHub for one finding. |
| `ghsa-disclose`, `ghsa-submit` | Render and file a GitHub advisory in two separate steps. |
| `asf-disclose` | Writes a security report email for an Apache project. |
| `security-history` | Mines the git history for past security fixes. |

Agents are Markdown files in `agents/`.
Skills with background knowledge for the agents are in `skills/`.

## Running

```sh
flawrence-runner --agent <name> --directory <target> [--artifacts <dir>] "<query>"
```

| Option | Meaning |
| --- | --- |
| `-d`, `--directory` | The target codebase. |
| `--artifacts` | Output directory. Without it the runner creates `artifacts-<hex>/` and prints the name. |
| `-m`, `--model` | Model for every agent in this run. |
| `-c`, `--concurrent` / `-n`, `--runs` | Run the root agent `n` times, `c` at a time. |
| `-s`, `--schedule 22:00-06:00` | Only call the model inside this daily window, in UTC. |
| `--arg key=value` | Fill a `{{key}}` placeholder in the agent prompts. |
| `--input-artifact key=path` | Load a file as a read-only artifact before the run starts. |
| `-v`, `--verbose` | Print reasoning, tool calls and results as they happen. |
| `--tui` | Full screen dashboard. Unix only. Output on disk is the same. |
| `--web-ui` | Serve a read-only status page on port 3000. |
| `--load-all` | Load every agent, not only the ones the root agent can reach. |

You can also pipe the query through stdin.

Each run writes its final report to `<session>_summary.md` in the artifacts directory.
Findings never leave your machine unless you call `submit_finding` with `live=true` and set `FLAWRENCE_VULN_INGEST_LIVE=1`.
The GitHub tools have the same kind of guard with `FLAWRENCE_GHSA_LIVE=1`.

### Stopping a run

Ctrl+C works in three stages.

1. The first press stops new work. Running agents finish on their own.
2. The second press cancels running agents. The run still deduplicates and submits what it found and writes its summary.
3. The third press quits at once.

If the summary step fails, the runner lists the finding artifacts on disk in `<session>_salvaged-summary.md`.

## Models

A model name has the form `<provider>/<model>`.

| Prefix | Provider | Credentials |
| --- | --- | --- |
| `anthropic/` | Anthropic Messages API | `ANTHROPIC_API_KEY` |
| `workers-ai/` | Cloudflare Workers AI | `CLOUDFLARE_API_TOKEN`, `CLOUDFLARE_ACCOUNT_ID` |
| `openai/` | OpenAI Responses API | `OPENAI_API_KEY` |
| `<name>/` | Any OpenAI-compatible `/v1/chat/completions` server | `FLAWRENCE_PROVIDER_<NAME>_BASE_URL`, `FLAWRENCE_PROVIDER_<NAME>_API_KEY` |

A custom provider needs two variables.
The lowercase `<NAME>` becomes the model prefix.

```sh
export FLAWRENCE_PROVIDER_VLLM_BASE_URL=http://localhost:8000/v1
export FLAWRENCE_PROVIDER_VLLM_API_KEY=unused
flawrence-runner --model vllm/Qwen/Qwen3-Coder --agent security-review --directory . "..."
```

Do not name a custom provider `anthropic`, `openai` or `workers-ai`.
The built-in prefixes win and your provider is never used.
To reach api.openai.com over chat completions instead of the Responses API, call the provider `oai` or similar.

Costs come from [models.dev](https://models.dev).
Set `TOKEN_COST_INPUT` and `TOKEN_COST_OUTPUT` for models it does not know, or `MAX_COST_USD` cannot be enforced.

## Configuration

Flawrence reads its settings from environment variables or `.env`.
These are the ones you will most likely need.

| Variable | Meaning |
| --- | --- |
| `MAX_COST_USD` | Stop the run at this cost. |
| `MAX_ITERATIONS` | Turns per agent. Default 500. |
| `FLAWRENCE_MAX_PARALLEL_AGENTS` | Research agents that run at the same time. Default 40. |
| `FLAWRENCE_WORKTREE_ISOLATION` | Give every agent that writes files its own git worktree. |
| `FLAWRENCE_AGENT_MEMORY_MB` | Memory watchdog per agent tree. See [docs/agent-memory.md](docs/agent-memory.md). |
| `TRAILMARK_BIN` | Path to [trailmark](https://github.com/trailofbits/trailmark) for call graphs. |
| `COMMITRAIDER_PATH` | Path to [CommitRaider](https://github.com/mschwarzl/commitraider) for git history analysis. |

[docs/configuration.md](docs/configuration.md) lists every variable.

## Docker

The image contains the runner, Rust, Java, Gradle, Maven, the Android SDK, trailmark, CommitRaider and common build tools.

```sh
./docker-shell.sh
```

This builds the image and opens a shell as root.
Clone targets into `/workspace`, which is a persistent volume.
Results go to `/artifacts`, which maps to `./artifacts` on the host.
`agents/` and `skills/` are mounted from the repository, so edits take effect without a rebuild.

```sh
git clone https://github.com/example/project.git /workspace/project
flawrence-runner --agent security-review --directory /workspace/project \
  --artifacts /artifacts/project "Review this codebase for security issues"
```

The image builds natively on amd64 and arm64.
The Android SDK tools only run on amd64.

## Browser launcher

`flawrence-web` starts runs from a browser and links to the live dashboard of the current run.
The Docker image starts it by default.
It needs `FLAWRENCE_WEB_PASSWORD` whenever it listens on a non-loopback address.
See [docs/web-launcher.md](docs/web-launcher.md).

## Writing an agent

An agent is a Markdown file.
The YAML front matter configures it and the body is its system prompt.

```yaml
---
model: anthropic/claude-sonnet-4-6
description: Reviews the session handling.
reasoning_effort: High
params:
  focus: session cookies
tools:
  - read_file
  - grep_files
  - code_index
  - artifact
  - spawn_task
tool_configs:
  artifact:
    read:
      - threat_model: input
      - finding_*
    write:
      - finding_*
subagents:
  - vulnerability-validation
---
Review {{focus}} in this application and write one finding per issue.
```

Permissions are closed by default.
An agent can only read and write the artifact keys it lists and only spawn the agents under `subagents`.
A trailing `*` matches a key prefix.

A key marked `input` must be supplied with `--input-artifact`, or the run does not start.
A glob marked `input`, such as `notes_*: input`, is optional.

`--arg focus=...` overrides a value under `params`.
The run refuses to start if a `{{placeholder}}` has no value.

| Field | Required | Meaning |
| --- | --- | --- |
| `model` | yes | Default model. |
| `description` | yes | One line about the agent. Parents see it when they choose a subagent. |
| `reasoning_effort` | yes | `Low`, `Medium` or `High`. |
| `tools` | no | Tools the agent may call. |
| `tool_configs` | no | Per-tool settings such as artifact permissions. |
| `subagents` | no | Agents this one may spawn. |
| `output_reviewer` | no | Agent that reviews the result before it is returned. |
| `params` | no | Default values for `{{placeholders}}`. |
| `output_schema` | no | JSON schema the result must follow. |
| `max_iterations` | no | Turn limit for this agent. |
| `output_grounding` | no | Result fields the runner checks against the agent's real tool calls. |
| `todos` | no | A fixed checklist the agent must work through. |

## Repository layout

| Path | Contents |
| --- | --- |
| `bin/flawrence-runner` | The `flawrence-runner` CLI and the `flawrence-web` launcher. |
| `bin/flawrence-index` | `flawrence-index-cli`, a debug tool for the code index. |
| `lib/flawrence-api` | Provider clients, retries and streaming. |
| `lib/flawrence-agent` | Agent and skill loading, permissions. |
| `lib/flawrence-task` | The agent loop, task pool and logging. |
| `lib/flawrence-tool` | All tools, the artifact store and worktrees. |
| `lib/flawrence-index` | Tree-sitter code index for Python, Rust, JS/TS, C/C++, Go, Java, Ruby and Kotlin. |
| `lib/flawrence-chat` | Shared message, usage and cost types. |
| `lib/flawrence-status` | The `--web-ui` status server. |
| `frontend` | The Vue dashboard. |

Contributors and coding agents should read [AGENTS.md](AGENTS.md) first.

```sh
cargo fmt --all && cargo clippy --workspace && cargo test --workspace
```

## Talks

- [Automated Vulnerability Hunting with LLMs at Scale](docs/talks/security-week-graz-2026.pdf). Security Week Graz, 2026-09-14 (no recording; slides only).

## License

Flawrence is licensed under the [MIT license](LICENSE).
The harp icon in `bin/flawrence-runner/src/harp.svg` is by Magnific from [Flaticon](https://www.flaticon.com/free-icon/harp_4752991) and uses the Flaticon license.
