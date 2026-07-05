#!/usr/bin/env bash
#
# Composite-action entry point for container-image-pruner.
#
# Reads its configuration from CIP_* environment variables (set by action.yml from the Action
# inputs), obtains the engine binary (a pinned release download, sha256-verified — or a local
# binary via CIP_BINARY_PATH for testing), runs it, and maps its exit code:
#   0 -> success; 2 -> success + warning (degraded/aborted/rate-limited, safe to re-run);
#   1 -> fail the step.
# The binary itself writes $GITHUB_STEP_SUMMARY and $GITHUB_OUTPUT (deleted/planned-delete/status).
# We additionally publish the summary/audit paths as step outputs for the caller.
set -euo pipefail

readonly REPO="unb-libraries/container-image-pruner"
readonly ASSET="container-image-pruner-x86_64-unknown-linux-gnu"

action_path="${GITHUB_ACTION_PATH:?GITHUB_ACTION_PATH not set}"
runner_temp="${RUNNER_TEMP:?RUNNER_TEMP not set}"
summary_json="${runner_temp}/cip-summary.json"
audit_log="${runner_temp}/cip-audit.jsonl"

# --- runner guard: the released binary is linux/x64 only --------------------------------------
if [[ "${RUNNER_OS:-Linux}" != "Linux" || "${RUNNER_ARCH:-X64}" != "X64" ]]; then
  echo "::error title=container-image-pruner::unsupported runner ${RUNNER_OS:-?}/${RUNNER_ARCH:-?}; requires Linux/X64" >&2
  exit 1
fi

# --- obtain the binary ------------------------------------------------------------------------
if [[ -n "${CIP_BINARY_PATH:-}" ]]; then
  bin="${CIP_BINARY_PATH}"
  if [[ ! -x "${bin}" ]]; then
    echo "::error title=container-image-pruner::binary-path '${bin}' is not executable" >&2
    exit 1
  fi
else
  # Resolve the release tag: explicit input, else derived from the committed crate version.
  if [[ -n "${CIP_VERSION:-}" ]]; then
    version="${CIP_VERSION}"
  else
    crate_version="$(sed -n 's/^version = "\(.*\)"/\1/p' "${action_path}/Cargo.toml" | head -n1)"
    if [[ -z "${crate_version}" ]]; then
      echo "::error title=container-image-pruner::could not read version from Cargo.toml" >&2
      exit 1
    fi
    version="v${crate_version}"
  fi

  bin="${runner_temp}/${ASSET}"
  base_url="https://github.com/${REPO}/releases/download/${version}/${ASSET}"
  if ! curl -fsSL -o "${bin}" "${base_url}"; then
    echo "::error title=container-image-pruner::release asset for ${version} not found at ${base_url} — pin a published release tag (a moved major tag must point at a published release)" >&2
    exit 1
  fi
  if ! curl -fsSL -o "${bin}.sha256" "${base_url}.sha256"; then
    echo "::error title=container-image-pruner::checksum for ${version} not found at ${base_url}.sha256" >&2
    exit 1
  fi
  # Verify integrity before making the binary executable.
  if ! ( cd "${runner_temp}" && sha256sum -c "${ASSET}.sha256" ); then
    echo "::error title=container-image-pruner::sha256 verification failed for ${ASSET}" >&2
    exit 1
  fi
  chmod +x "${bin}"
fi

# --- build the argument list ------------------------------------------------------------------
args=()
if [[ -n "${CIP_IMAGE:-}" ]]; then
  args+=(--image "${CIP_IMAGE}")
else
  [[ -n "${CIP_OWNER:-}" ]] && args+=("${CIP_OWNER}")
  [[ -n "${CIP_PACKAGE:-}" ]] && args+=("${CIP_PACKAGE}")
fi
args+=(--older-than-days "${CIP_OLDER_THAN_DAYS}")
args+=(--keep-at-least "${CIP_KEEP_AT_LEAST}")
args+=(--read-concurrency "${CIP_READ_CONCURRENCY}")
args+=(--delete-concurrency "${CIP_DELETE_CONCURRENCY}")
args+=(--audit-log "${audit_log}")
[[ -n "${CIP_MAX_DELETE:-}" ]] && args+=(--max-delete "${CIP_MAX_DELETE}")
[[ -n "${CIP_OWNER_TYPE:-}" ]] && args+=(--owner-type "${CIP_OWNER_TYPE}")
[[ -n "${CIP_GITHUB_API_URL:-}" ]] && args+=(--github-api-url "${CIP_GITHUB_API_URL}")
[[ "${CIP_PROTECT_ATTEST:-false}" == "true" ]] && args+=(--protect-subject-attestations)
[[ "${CIP_DRY_RUN:-false}" != "true" ]] && args+=(--execute)

# --- run (stdout JSON -> summary file + log; the binary writes its own outputs/summary) --------
set +e
"${bin}" "${args[@]}" | tee "${summary_json}"
rc="${PIPESTATUS[0]}"
set -e

# Publish paths as step outputs (the binary already wrote deleted/planned-delete/status).
{
  echo "summary-json=${summary_json}"
  [[ -s "${audit_log}" ]] && echo "audit-log=${audit_log}"
} >> "${GITHUB_OUTPUT}"

case "${rc}" in
  0) ;;
  2) echo "::notice title=container-image-pruner::completed with warnings (degraded/aborted/rate-limited) — safe to re-run" >&2 ;;
  *) echo "::error title=container-image-pruner::engine exited with code ${rc}" >&2; exit "${rc}" ;;
esac
