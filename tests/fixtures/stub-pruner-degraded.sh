#!/usr/bin/env bash
# A degraded-run stub: behaves like stub-pruner.sh but always exits 2 (degraded/rate-limited).
# Used to assert the composite maps exit 2 to a *successful* step (safe to re-run, not a failure)
# without depending on caller-step env propagation.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
CIP_STUB_EXIT=2 exec "${here}/stub-pruner.sh" "$@"
