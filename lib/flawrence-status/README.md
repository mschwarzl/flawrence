# flawrence-status

Web status UI for the Flawrence agent runner.

## Overview

This crate serves the read-only status dashboard of `flawrence-runner --web-ui`.
It exposes the in-memory [`JobRegistry`](../flawrence-task/src/ui.rs) as a REST and SSE API and serves the Vue app from `frontend/dist/`.

The server keeps the newest `FLAWRENCE_WEB_MAX_EVENTS` events per run, 1000 by default.
Events larger than 64 KiB are shown as a short preview.
Artifacts and `--transcripts` exports are not affected by either limit.

## Endpoints

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/api/jobs` | List all jobs |
| GET | `/api/jobs/:id` | Job details |
| GET | `/api/jobs/:id/events` | Retained events as JSON. The `X-Flawrence-Max-Events` header gives the limit. |
| GET | `/api/jobs/:id/events/stream` | SSE stream. Resume with `?since=<seq>` or `Last-Event-ID` |
| GET | `/api/jobs/:id/artifacts` | Artifact list |
| GET | `/api/jobs/:id/artifacts/:key` | Artifact content |
| GET | `/api/agents` | Available agents |
| GET | `/health` | Health check |

## Development

Start the frontend dev server. It proxies `/api` to the Rust server.

```bash
cd frontend
npm install
npm run dev
```

Then start the runner with `--web-ui`.

```bash
cargo run --bin flawrence-runner -- --web-ui --agent security-review "query"
```

## Release build

```bash
cd frontend && npm run build
cargo build --release --bin flawrence-runner
```
