+++
title = "Flawrence +     your machine"
sort_by = "weight"
+++
This guide takes you from a fresh clone to a first review.
It covers running Flawrence on your machine, with Docker or as a native build.
To run it on a server, see [Deploy to Sliplane](@/guide/sliplane.md).

## Before you start

You need four things.

- A key for a model provider. Flawrence works with Anthropic, OpenAI, Cloudflare Workers AI and any OpenAI-compatible endpoint.
- An isolated place to run. Agents run shell commands, build the target and execute proofs of concept.
- A cost limit. A full review fans out to many agents and can use a lot of tokens. Set `MAX_COST_USD`.
- The code you want to review, on disk. Flawrence does not clone targets for you.

> [!WARNING]
> Agents execute code. Use a container or a VM, and only review code you are authorized to assess.

## Choose how to run

| | Docker | Native |
| --- | --- | --- |
| Isolation | The container. | Your own VM. |
| Toolchains | In the image: Rust, Java, Gradle, Maven, Android SDK, trailmark and CommitRaider. | You install what the target needs. |
| Start with | The browser launcher or a shell in the container. | The browser launcher or the CLI. |
| Best for | Most people. | Development and custom setups. |

Docker is the recommended way.

## Run with Docker

Install Docker and Compose, then clone Flawrence and create your settings file.

```sh
git clone https://github.com/mschwarzl/flawrence.git
cd flawrence
cp .env.example .env
```

Edit `.env`. Add your provider credentials and `MAX_COST_USD`, and choose a long `FLAWRENCE_WEB_PASSWORD`.
The launcher refuses to start without that password.

### Browser launcher

```sh
docker compose up --build flawrence-web
```

Open <http://127.0.0.1:8080/> and sign in as `flawrence`.
The form uses paths inside the container.
Code lives in `/workspace`, a persistent volume, and results go to `/artifacts`, which maps to `./artifacts` on the host.
Put the target into `/workspace` before you start a run.

> [!WARNING]
> The launcher shows the configured credentials to everyone who can open it. Only give access to people you trust.

### Shell in the container

```sh
./docker-shell.sh
```

This builds the image and opens a shell as root.
Clone a target and start the runner from there.

```sh
git clone https://github.com/example/project.git /workspace/project
flawrence-runner --agent security-review --directory /workspace/project \
  --artifacts /artifacts/project "Review this codebase for security issues"
```

`agents/` and `skills/` are mounted from the repository, so edits take effect without a rebuild.
The image builds natively on amd64 and arm64. The Android SDK tools only run on amd64.

## Run natively

Do this inside a VM that you can throw away.
You need Rust. The launcher also needs Node.js and npm.

### Command line

```sh
cargo build --release
export ANTHROPIC_API_KEY=sk-ant-...

target/release/flawrence-runner --agent security-review \
  --directory /path/to/target --artifacts out \
  "Review this codebase for security issues"
```

The runner loads `.env` from the working directory.
Use `--model <provider>/<model>` to override the model of every agent for one run.

### Browser launcher

```sh
npm --prefix frontend ci
npm --prefix frontend run build
cargo build --release --bin flawrence-runner --bin flawrence-web
./target/release/flawrence-web
```

The launcher listens on loopback port 8080.
`--host`, `--port` and `--runner` change the defaults.

## Your first run

1. Pick a provider and a model. A model name has the form `<provider>/<model>`.
2. Enter the target path and an artifacts path.
3. Keep the agent `security-review`, or choose another top-level agent.
4. Set a spending limit. For a model Flawrence cannot price, also set `TOKEN_COST_INPUT` and `TOKEN_COST_OUTPUT`, or the limit cannot be enforced.
5. Start the run. The launcher opens a live dashboard.

The final report is `<session>_summary.md` in the artifacts directory.
Submitted findings are stored under `<artifacts>/submitted/`.

Findings never leave your machine on their own.
Sending them anywhere needs an explicit request and a guard variable such as `FLAWRENCE_VULN_INGEST_LIVE=1`.

Ctrl+C works in three stages. The first press stops new work, the second cancels running agents, and the third quits at once.

## Where to go next

- [Browser launcher](@/guide/web-launcher.md) explains the form, providers, the dashboard and hosting.
- [Configuration](@/guide/configuration.md) lists every environment variable.
- [Memory watchdog](@/guide/agent-memory.md) and [Context checkpoints](@/guide/context-checkpoints.md) cover limits for long runs.
- [Deploy to Sliplane](@/guide/sliplane.md) runs the launcher as a hosted service.
- [Findings](@/findings.md) lists the bugs Flawrence has found so far.
