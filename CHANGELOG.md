# Changelog

This file lists notable changes to Flawrence.
The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Versions follow [Semantic Versioning](https://semver.org/).

## [0.1.0](https://github.com/mschwarzl/flawrence/releases/tag/v0.1.0) - 2026-11-27

The first public release.

### Added

- The `security-review` pipeline. It builds a threat model and an attack surface, and runs one research agent per entry point. A second agent validates each finding and a third writes a proof of concept.
- Agents for the steps after a review. They patch a finding, file a GitHub advisory, write an Apache security report and mine the git history for past fixes.
- Agents and skills as Markdown files with YAML front matter. Permissions are closed by default.
- Providers for Anthropic, OpenAI, Cloudflare Workers AI and any OpenAI-compatible endpoint.
- Cost tracking with prices from models.dev, and a hard limit through `MAX_COST_USD`.
- A tree-sitter code index for the agents.
- Build detection and reproducer setup for Cargo, Gradle, Maven, Go, Python, CMake and Make.
- A separate git worktree per writing agent, and verifier lanes for slow builds.
- A terminal dashboard, a live web status page and the `flawrence-web` browser launcher.
- A Docker image for amd64 and arm64.
- [FINDINGS.md](FINDINGS.md) with the vulnerabilities found so far, and reports past the 90 day deadline under [disclosures](disclosures).
- The slides of the Security Week Graz 2026 talk.

