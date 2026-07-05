#!/usr/bin/env bash
#
# Flag contract: every engine flag the composite (prune.sh) passes must exist in the binary's
# --help. Catches drift between action.yml/prune.sh and the Rust CLI (cli.rs) — the seam that
# the unit and composite tests otherwise only cover separately.
#
# Usage: check-flag-contract.sh <path-to-binary>
set -euo pipefail

bin="${1:?usage: check-flag-contract.sh <path-to-binary>}"
here="$(cd "$(dirname "$0")" && pwd)"

help_text="$("${bin}" --help)"

# All long flags referenced in the composite script.
mapfile -t flags < <(grep -oE -- '--[a-z][a-z-]+' "${here}/prune.sh" | sort -u)

missing=0
for flag in "${flags[@]}"; do
  if ! grep -qF -- "${flag}" <<<"${help_text}"; then
    echo "MISSING: composite passes ${flag}, but the engine --help does not list it"
    missing=1
  fi
done

if [[ "${missing}" -ne 0 ]]; then
  echo "flag contract FAILED"
  exit 1
fi
echo "flag contract OK (${#flags[@]} flags checked)"
