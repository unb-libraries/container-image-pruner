# container-image-pruner

A GitHub Action to **safely prune old per-build container images** from the GitHub Container Registry (GHCR). It is purpose built for use at [UNB Libraries](https://lib.unb.ca). Unless your builds produce hashes similarly, this tool likely isn't of use to you.

This replaces a more [complex retention tool](https://github.com/snok/container-retention-policy) that we were using. It is designed to do one thing well: delete old hash-time build tags (and their now-orphaned child manifests) while **never** touching human/text tags (`dev`, `prod`, `feature-*`, cosign `.sig`) or any manifest still referenced by something it keeps.

## What it deletes

For a container package it:

1. **Keeps** any version carrying a non-build tag: a text tag, a branch name, a cosign `.sig`, etc. A version tagged both `afef854e-20260704…` **and** `prod` is kept.
2. **Keeps** the `keep-at-least` most-recent **build** (hash-time) versions, regardless of age. Text-tagged versions are never counted toward this minimum.
3. **Deletes** the remaining build versions **older than** `older-than-days`.
4. **Deletes** untagged manifests (multi-arch children, attestations) left orphaned by the above: but **only** those old enough *and* not referenced by any retained image.

Age is taken from the API `created_at`, never from the (non-clock) Moment.js `YYYYMMDDHHMMSS` build-tag timestamp; build tags are classified by **shape**, so an upstream format change won't break it.

## Usage

```yaml
- uses: unb-libraries/container-image-pruner@v1
  with:
    image-name: ghcr.io/unb-libraries/my-app   # or: owner + package
    token: ${{ secrets.GH_CONTAINER_REGISTRY_TOKEN }}   # needs delete:packages
    # defaults: older-than-days 60, keep-at-least 5, max-delete 500, upload-audit-log true
```

Dry-run first to review the plan (nothing is deleted):

```yaml
- uses: unb-libraries/container-image-pruner@v1
  with:
    image-name: ghcr.io/unb-libraries/my-app
    token: ${{ secrets.GH_CONTAINER_REGISTRY_TOKEN }}
    dry-run: true
```

### Inputs

| Input | Default | Description |
| --- | --- | --- |
| `image-name` | `''` | Full `ghcr.io/<owner>/<package>` (alternative to `owner`/`package`). |
| `owner` | `''` | Owner login (used when `image-name` is not given). |
| `package` | `''` | Package name. Empty processes **every** package under the owner. |
| `token` | *(required)* | Token with `read:packages` (+ `delete:packages` unless `dry-run`). |
| `older-than-days` | `60` | Delete build images strictly older than this. |
| `keep-at-least` | `5` | Always keep the N most-recent build images. |
| `dry-run` | `false` | Report the plan only; delete nothing. |
| `max-delete` | `500` | Abort (delete nothing) if the plan exceeds this. |
| `protect-subject-attestations` | `false` | Also protect subject-linked attestations (extra fetches). |
| `read-concurrency` | `8` | Concurrency for read/manifest requests. |
| `delete-concurrency` | `3` | Concurrency for DELETE requests. |
| `owner-type` | `''` | `user` or `org` (default: auto-detect). |
| `github-api-url` | `''` | Override for GitHub Enterprise Server. |
| `upload-audit-log` | `true` | Upload the plan (+ audit JSONL when executing) as an artifact. |
| `artifact-name` | `container-image-pruner-audit` | Name of the uploaded artifact. |
| `artifact-retention-days` | `14` | Retention for the uploaded artifact. |
| `version` | `''` | Release tag of the binary (default: derived from the crate version). |

### Outputs

| Output | Description |
| --- | --- |
| `deleted` | Number of versions actually deleted. |
| `planned-delete` | Number of versions the plan would delete. |
| `status` | `clean` \| `degraded` \| `errored` \| `aborted`. |
| `summary-json` | Path to the JSON run summary (the plan/results). |
| `audit-log` | Path to the JSONL audit log (present only when executing). |

The Action also writes a compact table to the **job summary**, emits `::warning`/`::error` **annotations** for degraded/errored packages, and (when `upload-audit-log` is true) uploads the plan `cip-summary.json` plus, in execute mode, `cip-audit.jsonl`: even if the run failed, so the diagnostic is always available. If you invoke the action more than once in a single workflow run, give each call a distinct `artifact-name` — `actions/upload-artifact` rejects duplicate artifact names within a run.

### Token & permissions

Authenticates with the **token you pass** (not the ambient `GITHUB_TOKEN`), so it works with the same PAT used to push to GHCR. It needs `read:packages`, plus **`delete:packages`** to actually delete (a push PAT often has only `write:packages`: delete is a separate scope), and `repo` for private packages. The token is read from the environment, never placed on the command line. Linux/x64 runners only.

## Operational runbook

- **Release ordering.** The Action resolves its version from `Cargo.toml`. Publish a release by bumping `Cargo.toml`, pushing the matching `vX.Y.Z` tag (the release workflow builds + uploads the binary and its `.sha256`), and only **then** moving the floating `v1` tag. Never point the Action at a branch. A missing asset produces an explicit "pin a published release tag" error.
- **Over-cap packages.** A long-lived package may plan more than `max-delete` deletions on first prune; the run **aborts** (deletes nothing), annotates a warning, and notes it in the job summary. Do a one-time run with a raised `max-delete` for those packages, after which steady-state stays under the cap.

## Local testing (CLI)

The engine also runs as a non-interactive CLI, useful for a local dry-run preview:

```sh
cargo build --release
GITHUB_TOKEN=$(gh auth token) ./target/release/container-image-pruner \
  --image ghcr.io/unb-libraries/my-app --older-than-days 60 --keep-at-least 5
# add --execute to delete (guarded by --max-delete); --format is gone: output is always JSON.
```

Exit codes: `0` clean; `1` usage/auth error (nothing deleted); `2` degraded/errored/rate-limited or aborted (safe to re-run).

## Development

```sh
cargo test                    # unit + wiremock adapter + orchestrator + delete-correctness e2e
cargo clippy --all-targets -- -D warnings
cargo fmt --check
shellcheck scripts/*.sh
```

The correctness core (`src/domain/`) is pure and IO-free. Adapters (`src/github/`) are tested against a mock HTTP server. `tests/action_e2e.rs` drives the compiled binary against a mock GHCR in `--execute` mode and asserts the **exact** set of DELETE requests (no secrets, runs on every PR). A live, read-only dry-run (`tests/live_smoke.rs`, `#[ignore]`) runs nightly to catch API drift.
