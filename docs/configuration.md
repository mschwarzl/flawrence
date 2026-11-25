# Configuration

Flawrence reads its settings from environment variables.
The runner also loads `.env` from the working directory unless you pass `--no-dotenv`.
`.env.example` is a commented starting point.

## Providers

| Variable | Meaning |
| --- | --- |
| `ANTHROPIC_API_KEY` | Key for `anthropic/` models. |
| `CLOUDFLARE_API_TOKEN`, `CLOUDFLARE_ACCOUNT_ID` | Credentials for `workers-ai/` models. |
| `OPENAI_API_KEY` | Key for `openai/` models. They use the Responses API at api.openai.com. |
| `OPENCODE_ACCESS_TOKEN` | Token for an opencode gateway instead of an API key. It needs `FLAWRENCE_OPENAI_BASE_URL`. |
| `FLAWRENCE_PROVIDER_<NAME>_BASE_URL` | Base URL of a custom OpenAI-compatible provider. It ends in `/v1`. |
| `FLAWRENCE_PROVIDER_<NAME>_API_KEY` | Key for that provider, sent as a bearer token. |
| `FLAWRENCE_ANTHROPIC_BASE_URL` | Other base URL for Anthropic. |
| `FLAWRENCE_OPENAI_BASE_URL` | Other HTTPS base URL for the `openai/` Responses client. |
| `FLAWRENCE_WORKERS_AI_BASE_URL` | Other base URL for Workers AI. |
| `FLAWRENCE_MODEL` | Model for every agent. `--model` does the same for one run. |
| `FLAWRENCE_CLIENT` | `streaming` (default) or `batch`. Batch mode uses the Workers AI batch API. |
| `FLAWRENCE_STRICT_TOOLS` | Set to `0` for a provider that rejects strict tool schemas. |
| `FLAWRENCE_STREAM_IDLE_TIMEOUT_SECS` | Abort a streamed response after this many silent seconds. Default 120. |
| `FLAWRENCE_MAX_API_RETRIES` | Retries for a failing model request. Default 50. |

## Limits

| Variable | Meaning |
| --- | --- |
| `MAX_COST_USD` | Stop the run once it has cost this much. Every model needs a known price. |
| `MAX_TOTAL_TOKENS` | Stop the run after this many tokens across all agents. |
| `MAX_ITERATIONS` | Turns per agent. Default 500. An agent can set its own `max_iterations`. |
| `MAX_AGENT_DEPTH` | How deep subagents can nest. Default 5. |
| `MAX_TOKENS` | Output tokens per response. Default 65536. |
| `MAX_CONTEXT_TOKENS` | Context window of the smallest model you use. The runner checkpoints agents before they reach it. See [context-checkpoints.md](context-checkpoints.md). |
| `FLAWRENCE_MAX_PARALLEL_AGENTS` | Direct subagents that run at the same time across all runs. Default 40. Deeper agents do not count. |
| `FLAWRENCE_AGENT_MEMORY_MB` | Memory watchdog per direct subagent tree, in MiB. Off by default. See [agent-memory.md](agent-memory.md). |

The cost and token limits are checked after each response.
Requests that are already running can push a run slightly past the limit.

## Prices

Prices come from [models.dev](https://models.dev).
For a model it does not know, set the prices yourself in USD per million tokens.

| Variable | Meaning |
| --- | --- |
| `TOKEN_COST_INPUT` | Input price. Needs `TOKEN_COST_OUTPUT`. |
| `TOKEN_COST_OUTPUT` | Output price. Needs `TOKEN_COST_INPUT`. |
| `TOKEN_COST_CACHE_READ` | Cache read price. Optional. |

Some custom providers report cache reads but not cache writes.
The real cost can then be higher than the estimate.

## Builds and workspaces

| Variable | Meaning |
| --- | --- |
| `FLAWRENCE_PREWARM_BUILD` | Build the target once before agents start, so they share a warm build cache. Default `true`. |
| `FLAWRENCE_PREWARM_TIMEOUT_SECS` | Time limit for that build. Default 5400. |
| `FLAWRENCE_WORKTREE_ISOLATION` | Give every agent that writes files its own git worktree. Default `false`. The target must be a git repository with at least one commit. Changes are archived under `changes/` when an agent finishes. |
| `FLAWRENCE_MAX_VERIFIER_LANES` | Warm build directories shared by agents for heavy builds such as Gradle. Default 4. |
| `FLAWRENCE_JAVA_INSTALLATIONS` | JDK paths offered to Gradle. The Docker image sets it. |

## Paths and tools

| Variable | Meaning |
| --- | --- |
| `FLAWRENCE_AGENTS_DIR` | Directory with agent definitions. Default `agents`. |
| `FLAWRENCE_SKILLS_DIR` | Directory with skills. Default `skills`. |
| `TRAILMARK_BIN` | Path to [trailmark](https://github.com/trailofbits/trailmark). Enables the `trailmark` call graph tool. |
| `COMMITRAIDER_PATH` | Path to [CommitRaider](https://github.com/mschwarzl/commitraider). Agents fall back to plain git history without it. |
| `FLAWRENCE_RG_BIN` | Path to `rg` if it is not on `PATH`. |

To install trailmark, use a separate Python 3.13 environment.

```sh
uv venv --python 3.13 ~/.venv-trailmark
VIRTUAL_ENV=~/.venv-trailmark uv pip install 'trailmark==0.5.*'
export TRAILMARK_BIN=~/.venv-trailmark/bin/trailmark
```

Run with `--artifacts` so the call graph is cached between agents.

## Publishing findings

Nothing is sent anywhere by default.
Each live action needs an explicit request and a guard variable.

| Variable | Meaning |
| --- | --- |
| `GITHUB_TOKEN` or `GH_TOKEN` | Token for the GitHub advisory tools. |
| `FLAWRENCE_GHSA_LIVE` | Set to `1` to let agents file GitHub advisories. |
| `VULN_INGEST_ENDPOINT` | URL of your own vulnerability intake service. |
| `VULN_INGEST_TOKEN_PATH` | File with the token for that service. |
| `FLAWRENCE_VULN_INGEST_LIVE` | Set to `1` to let `submit_finding` send findings there. |

## Browser launcher

| Variable | Meaning |
| --- | --- |
| `FLAWRENCE_WEB_PASSWORD` | Password for the launcher. Required on any non-loopback address. The user name is `flawrence`. |
| `FLAWRENCE_WORKSPACE_DIR` | Default target directory in the form. |
| `FLAWRENCE_ARTIFACTS_BASE_DIR` | Default artifacts directory in the form. |
| `FLAWRENCE_WEB_MAX_EVENTS` | Events kept per run on the dashboard. Default 1000. |
| `PORT` | Port the launcher listens on. |
