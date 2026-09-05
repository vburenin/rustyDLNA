#!/usr/bin/env bash
# agent-verify.sh -- fail-fast verify loop for coding agents
# Installing agent: enable only checks supported by the target project.
# Leave unknown steps commented; an unconfigured script exits with status 2.

set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$project_root"

step() { echo "==> agent-verify: $*"; }

checks_run=0
run_check() {
  if (( $# < 2 )); then
    echo "==> agent-verify: UNCONFIGURED: run_check needs a label and a command" >&2
    exit 2
  fi
  local label="$1"
  shift
  step "$label"
  "$@"
  checks_run=$((checks_run + 1))
}

# The canonical quality gate already covers Python/shell checks, web unit tests,
# Rust formatting, Clippy/type checking, workspace tests, docs, and isolated E2E.
# Keep its order and failure propagation; do not duplicate those checks here.
# Run on the project's supported Linux host with prerequisites already installed
# (see docs/INDEX.md). Dependency installation belongs outside the verify loop.
run_check quality ./scripts/check.sh

# No separate JavaScript/Python lint or typecheck task is declared.
# Optional release builds and browser/privileged suites follow AGENTS.md;
# the existing CI workflows configure and run their additional checks.

if (( checks_run == 0 )); then
  echo "==> agent-verify: UNCONFIGURED: enable at least one real check in scripts/agent-verify.sh" >&2
  exit 2
fi

step "ok ($checks_run checks)"
