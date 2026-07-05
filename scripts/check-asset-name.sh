#!/usr/bin/env bash
#
# Assert the release asset name is identical in prune.sh (what the Action downloads) and
# release.yml (what the release uploads). A mismatch would 404 silently at runtime.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"

# The canonical name is the `readonly ASSET=` line in prune.sh.
asset="$(sed -n 's/^readonly ASSET="\(.*\)"/\1/p' "${root}/scripts/prune.sh")"
if [[ -z "${asset}" ]]; then
  echo "could not find ASSET in scripts/prune.sh"
  exit 1
fi

if ! grep -qF "${asset}" "${root}/.github/workflows/release.yml"; then
  echo "MISMATCH: asset '${asset}' (prune.sh) not referenced in release.yml"
  exit 1
fi
echo "asset-name consistency OK (${asset})"
