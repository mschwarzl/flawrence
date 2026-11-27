---
name: supply-chain-ci
description: "Supply chain and CI/CD security: GitHub Actions/GitLab CI workflow abuse, dependency confusion, artifact and secret exposure in pipelines. Load when the target repo contains workflow files, package manifests with install scripts, or deploy pipelines."
---

# Supply chain and CI/CD security

When the repo contains CI workflows, package manifests or deploy pipelines, check:

## Workflow injection
- `pull_request_target` combined with `actions/checkout` of the PR head and a following `run:` step (arbitrary code execution with access to the base repo's secrets)
- Untrusted input (`github.event.*`) interpolated directly into `run:` blocks (script injection via `${{ }}`)
- `secrets.*` passed to untrusted actions, or secrets echoed into logs
- Workflows triggered by fork PRs that can access `secrets` (they should not)

## Dependency and package managers
- Install scripts (`postinstall`, `preinstall`) in dependencies that touch the network or filesystem
- Dependency confusion: public package name shadowing a private one, unpinned internal registry URLs
- Lockfile bypasses: CI installing without `--frozen-lockfile` / `npm ci`
- Typosquatting candidates: new deps whose name is a small edit of a popular package

## Artifacts, images, secrets
- Docker images built `FROM` mutable tags (`latest`) in production paths
- Secrets baked into image layers (check `Dockerfile` ARG/ENV, build args in history)
- Artifact upload/download without integrity pinning (digest vs tag)
- Caches used across trust boundaries (cache poisoning between PRs and main)
- Self-hosted runners: runner groups writable by fork PRs, `queue` label abuse

## When reviewing
- Map which workflows can touch which secrets; the exploit is usually the intersection.
- A PoC can be a harmless workflow run (e.g. `echo` of a secret name, not its value) on a fork PR.
