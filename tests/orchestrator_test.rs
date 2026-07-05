//! End-to-end orchestrator test: drives `container_image_pruner::run()` (dry-run) against a fully
//! mocked GitHub + registry, exercising the wiring the pure-planner and adapter tests don't
//! cover — owner detection, version listing, retained-manifest fetch, and the final plan,
//! including the multi-arch safety invariant that a child of a retained index is protected.

use container_image_pruner::config::Config;
use container_image_pruner::policy::Policy;
use container_image_pruner::report::PackageStatus;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn config_for(server: &MockServer) -> Config {
    Config {
        owner: "o".to_string(),
        package: Some("pkg".to_string()),
        owner_type: None,
        policy: Policy {
            older_than_days: 30,
            keep_at_least: 1,
        },
        token: "t".to_string(),
        dry_run: true,
        max_delete: None,
        read_concurrency: 8,
        delete_concurrency: 3,
        protect_subject_attestations: false,
        audit_log: None,
        api_base: Some(server.uri()),
        registry_base: Some(server.uri()),
    }
}

/// Scenario:
///   * `sha256:new-index`  — recent build (kept by keep-at-least), a multi-arch index whose
///     child `sha256:shared` is old and shared with an old build.
///   * `sha256:old-index`  — old build beyond keep-at-least -> deleted.
///   * `sha256:shared`     — old untagged child, referenced by the retained new index -> KEPT.
///   * `sha256:orphan`     — old untagged, referenced by nobody retained -> DELETED.
#[tokio::test]
async fn dry_run_plans_safely_for_multi_arch() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/users/o"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"type": "Organization"})))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/orgs/o/packages/container/pkg/versions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": 1, "name": "sha256:new-index", "created_at": "2026-07-01T00:00:00Z",
             "metadata": {"container": {"tags": ["a1b2c3d-20260701120000"]}}},
            {"id": 2, "name": "sha256:old-index", "created_at": "2026-01-01T00:00:00Z",
             "metadata": {"container": {"tags": ["a1b2c3d-20260101120000"]}}},
            {"id": 3, "name": "sha256:shared", "created_at": "2026-01-01T00:00:00Z",
             "metadata": {"container": {"tags": []}}},
            {"id": 4, "name": "sha256:orphan", "created_at": "2026-01-01T00:00:00Z",
             "metadata": {"container": {"tags": []}}}
        ])))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"token": "tok"})))
        .mount(&server)
        .await;

    // The one retained index references the shared child.
    Mock::given(method("GET"))
        .and(path("/v2/o/pkg/manifests/sha256:new-index"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "mediaType": "application/vnd.oci.image.index.v1+json",
            "manifests": [{"digest": "sha256:shared"}]
        })))
        .mount(&server)
        .await;

    // now() is "today" (well after 2026-07-01), so the recent build is ~ current; with a
    // 30-day cutoff and keep_at_least=1 it is retained, the Jan build is deleted.
    let summary = container_image_pruner::run(config_for(&server))
        .await
        .unwrap();

    let pkg = &summary.packages[0];
    assert_eq!(pkg.status, PackageStatus::Clean);

    let deleted: std::collections::HashSet<&str> =
        pkg.deletes.iter().map(|d| d.digest.as_str()).collect();

    assert!(
        deleted.contains("sha256:old-index"),
        "old build must be deleted"
    );
    assert!(
        deleted.contains("sha256:orphan"),
        "unreferenced orphan must be deleted"
    );
    assert!(
        !deleted.contains("sha256:shared"),
        "child referenced by a retained index must be protected"
    );
    assert!(
        !deleted.contains("sha256:new-index"),
        "recent build must be kept"
    );
    assert_eq!(deleted.len(), 2);
}

/// If the retained-manifest fetch fails, the untagged pass must be skipped (degraded), and no
/// untagged version may be deleted — only the plainly build-tagged one.
///
/// `start_paused` makes the retry backoff sleeps resolve instantly.
#[tokio::test(start_paused = true)]
async fn manifest_fetch_failure_degrades_and_spares_untagged() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/users/o"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"type": "Organization"})))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/orgs/o/packages/container/pkg/versions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": 1, "name": "sha256:keep-index", "created_at": "2026-07-01T00:00:00Z",
             "metadata": {"container": {"tags": ["a1b2c3d-20260701120000"]}}},
            {"id": 2, "name": "sha256:old-index", "created_at": "2026-01-01T00:00:00Z",
             "metadata": {"container": {"tags": ["a1b2c3d-20260101120000"]}}},
            {"id": 3, "name": "sha256:orphan", "created_at": "2026-01-01T00:00:00Z",
             "metadata": {"container": {"tags": []}}}
        ])))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"token": "tok"})))
        .mount(&server)
        .await;

    // Retained-manifest fetch fails (500) -> incomplete protected set -> hard-stop untagged.
    Mock::given(method("GET"))
        .and(path("/v2/o/pkg/manifests/sha256:keep-index"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let summary = container_image_pruner::run(config_for(&server))
        .await
        .unwrap();
    let pkg = &summary.packages[0];

    assert_eq!(pkg.status, PackageStatus::Degraded);
    let deleted: std::collections::HashSet<&str> =
        pkg.deletes.iter().map(|d| d.digest.as_str()).collect();
    assert!(deleted.contains("sha256:old-index"));
    assert!(
        !deleted.contains("sha256:orphan"),
        "untagged must be spared when the protected set is incomplete"
    );
}
