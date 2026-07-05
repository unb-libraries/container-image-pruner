#!/usr/bin/env bash
#
# Test stub standing in for the real engine binary (used via the action's `binary-path` input to
# test the composite glue offline). It records the exact argv it was invoked with, emits a canned
# JSON summary on stdout, writes canned Action outputs + step summary, and exits with a chosen
# code. Controlled by:
#   CIP_STUB_ARGV_FILE  - where to write one argv token per line (optional)
#   CIP_STUB_EXIT       - exit code to return (default 0)
set -euo pipefail

if [[ -n "${CIP_STUB_ARGV_FILE:-}" ]]; then
  : > "${CIP_STUB_ARGV_FILE}"
  for a in "$@"; do printf '%s\n' "$a" >> "${CIP_STUB_ARGV_FILE}"; done
fi

# Canned machine record on stdout.
printf '{"dry_run":true,"rate_limited":false,"packages":[]}\n'

# Canned Action surfaces, mirroring what the real binary writes.
if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
  {
    echo "deleted=0"
    echo "planned-delete=0"
    echo "status=clean"
  } >> "${GITHUB_OUTPUT}"
fi
if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
  echo "### container-image-pruner (stub)" >> "${GITHUB_STEP_SUMMARY}"
fi

exit "${CIP_STUB_EXIT:-0}"
