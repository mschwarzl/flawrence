#!/usr/bin/env bash
set -euo pipefail

mkdir -p "${FLAWRENCE_ARTIFACTS_DIR:-artifacts}"
docker compose build
exec docker compose run --rm flawrence
