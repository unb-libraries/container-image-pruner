//! End-to-end delete-correctness gate for the **assembled binary** — the Action's engine.
//!
//! Drives the actual compiled binary (via `CARGO_BIN_EXE_*`) against an in-process wiremock GHCR,
//! pointed there with `--github-api-url`/`--registry-url`, in `--execute` mode, and asserts the
//! *exact* set of DELETE requests the server received (plus the Action side effects: JSON stdout,
//! `$GITHUB_OUTPUT`, `$GITHUB_STEP_SUMMARY`, the audit JSONL, and the exit code).
//!
//! This is the primary "deletes exactly the right images" gate. It needs no secrets and no
//! network, so it runs on every PR.

use std::collections::BTreeSet;

use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A crafted package with the classic multi-arch hazard:
///   * id 1 `sha256:new-index` — recent build (kept), an index referencing the shared child.
///   * id 2 `sha256:old-index` — old build beyond keep-at-least -> DELETE.
///   * id 3 `sha256:shared`    — old untagged child referenced by the retained index -> KEPT.
///   * id 4 `sha256:orphan`    — old untagged, referenced by nobody retained -> DELETE.
async fn mount_multi_arch_fixture(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/users/o"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"type": "Organization"})))
        .mount(server)
        .await;

    let versions = json!([
        {"id": 1, "name": "sha256:new-index", "created_at": "2026-07-01T00:00:00Z",
         "metadata": {"container": {"tags": ["a1b2c3d-20260701120000"]}}},
        {"id": 2, "name": "sha256:old-index", "created_at": "2026-01-01T00:00:00Z",
         "metadata": {"container": {"tags": ["a1b2c3d-20260101120000"]}}},
        {"id": 3, "name": "sha256:shared", "created_at": "2026-01-01T00:00:00Z",
         "metadata": {"container": {"tags": []}}},
        {"id": 4, "name": "sha256:orphan", "created_at": "2026-01-01T00:00:00Z",
         "metadata": {"container": {"tags": []}}}
    ]);
    Mock::given(method("GET"))
        .and(path("/orgs/o/packages/container/pkg/versions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(versions))
        .mount(server)
        .await;

    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"token": "tok"})))
        .mount(server)
        .await;

    Mock::given(method("GET"))
        .and(path("/v2/o/pkg/manifests/sha256:new-index"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "mediaType": "application/vnd.oci.image.index.v1+json",
            "manifests": [{"digest": "sha256:shared"}]
        })))
        .mount(server)
        .await;

    // TOCTOU re-check reads each planned version by id, tags unchanged since planning.
    Mock::given(method("GET"))
        .and(path("/orgs/o/packages/container/pkg/versions/2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!(
            {"id": 2, "name": "sha256:old-index", "created_at": "2026-01-01T00:00:00Z",
             "metadata": {"container": {"tags": ["a1b2c3d-20260101120000"]}}}
        )))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/orgs/o/packages/container/pkg/versions/4"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!(
            {"id": 4, "name": "sha256:orphan", "created_at": "2026-01-01T00:00:00Z",
             "metadata": {"container": {"tags": []}}}
        )))
        .mount(server)
        .await;
}

/// Paths of the DELETE requests the mock server received, as a sorted set.
async fn received_deletes(server: &MockServer) -> BTreeSet<String> {
    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.method == wiremock::http::Method::DELETE)
        .map(|r| r.url.path().to_string())
        .collect()
}

struct RunResult {
    code: i32,
    stdout: String,
    outputs: String,
    step_summary: String,
    audit: String,
}

/// Run the compiled binary against `server`, capturing stdout + the Action env files.
async fn run_bin(server: &MockServer, extra_args: &[&str]) -> RunResult {
    let dir = tempdir();
    let out_path = dir.join("out");
    let summary_path = dir.join("summary.md");
    let audit_path = dir.join("audit.jsonl");

    let mut args: Vec<String> = vec![
        "o".into(),
        "pkg".into(),
        "--older-than-days".into(),
        "30".into(),
        "--keep-at-least".into(),
        "1".into(),
        "--execute".into(),
        "--github-api-url".into(),
        server.uri(),
        "--registry-url".into(),
        server.uri(),
        "--audit-log".into(),
        audit_path.to_string_lossy().into(),
    ];
    args.extend(extra_args.iter().map(|s| s.to_string()));

    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_container-image-pruner"))
        .args(&args)
        .env("GITHUB_TOKEN", "t")
        .env("GITHUB_OUTPUT", &out_path)
        .env("GITHUB_STEP_SUMMARY", &summary_path)
        .env_remove("RUST_LOG")
        .output()
        .await
        .expect("spawn binary");

    RunResult {
        code: output.status.code().expect("exit code"),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        outputs: std::fs::read_to_string(&out_path).unwrap_or_default(),
        step_summary: std::fs::read_to_string(&summary_path).unwrap_or_default(),
        audit: std::fs::read_to_string(&audit_path).unwrap_or_default(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn execute_deletes_exactly_the_right_versions() {
    let server = MockServer::start().await;
    mount_multi_arch_fixture(&server).await;
    Mock::given(method("DELETE"))
        .and(path("/orgs/o/packages/container/pkg/versions/2"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/orgs/o/packages/container/pkg/versions/4"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    let r = run_bin(&server, &[]).await;

    // Exactly the old build and the unreferenced orphan were deleted — nothing else.
    let deletes = received_deletes(&server).await;
    let expected: BTreeSet<String> = [
        "/orgs/o/packages/container/pkg/versions/2".to_string(),
        "/orgs/o/packages/container/pkg/versions/4".to_string(),
    ]
    .into_iter()
    .collect();
    assert_eq!(deletes, expected, "wrong DELETE set");

    assert_eq!(
        r.code, 0,
        "clean run exits 0; stderr/summary: {}",
        r.step_summary
    );

    // stdout is valid JSON reporting 2 deletions.
    let summary: Value = serde_json::from_str(&r.stdout).expect("stdout is JSON");
    let deleted: u64 = summary["packages"][0]["deleted"].as_u64().unwrap();
    assert_eq!(deleted, 2);

    // Action outputs + audit + step summary all reflect the run.
    assert!(r.outputs.contains("deleted=2"), "outputs: {}", r.outputs);
    assert!(
        r.outputs.contains("planned-delete=2"),
        "outputs: {}",
        r.outputs
    );
    assert!(r.outputs.contains("status=clean"), "outputs: {}", r.outputs);
    assert_eq!(r.audit.lines().count(), 2, "audit JSONL: {}", r.audit);
    assert!(r.step_summary.contains("container-image-pruner"));
    assert!(r.step_summary.contains("| pkg |"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn max_delete_aborts_without_deleting_anything() {
    let server = MockServer::start().await;
    mount_multi_arch_fixture(&server).await;
    // No DELETE mocks: if the binary tried to delete, the request would 404 and we'd see it.

    let r = run_bin(&server, &["--max-delete", "1"]).await;

    let deletes = received_deletes(&server).await;
    assert!(
        deletes.is_empty(),
        "aborted run must issue no DELETEs, saw {deletes:?}"
    );
    assert_eq!(r.code, 2, "abort exits 2 (safe to re-run)");
    assert!(
        r.outputs.contains("status=aborted"),
        "outputs: {}",
        r.outputs
    );
    assert!(r.outputs.contains("deleted=0"), "outputs: {}", r.outputs);
    assert!(
        r.step_summary.contains("Aborted"),
        "summary: {}",
        r.step_summary
    );
    assert_eq!(
        r.audit.trim(),
        "",
        "nothing should be written to the audit log"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn insufficient_scope_403_on_delete_degrades_without_crashing() {
    let server = MockServer::start().await;
    mount_multi_arch_fixture(&server).await;
    // A token without delete:packages -> 403 on DELETE (not a rate limit).
    Mock::given(method("DELETE"))
        .and(path("/orgs/o/packages/container/pkg/versions/2"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/orgs/o/packages/container/pkg/versions/4"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&server)
        .await;

    let r = run_bin(&server, &[]).await;

    assert_eq!(
        r.code, 2,
        "a failed delete degrades (exit 2), never a crash"
    );
    assert!(
        r.outputs.contains("status=degraded"),
        "outputs: {}",
        r.outputs
    );
    assert!(r.outputs.contains("deleted=0"), "outputs: {}", r.outputs);
}

// --- tiny tempdir helper (avoids an extra dependency) ---------------------------------------

fn tempdir() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let dir = std::env::temp_dir().join(format!("cip-e2e-{pid}-{n}"));
    std::fs::create_dir_all(&dir).expect("create tempdir");
    dir
}
