//! Adapter tests against a mocked HTTP server. These pin down our assumptions about the
//! GitHub Packages REST API and the OCI Registry v2 API: pagination, owner detection,
//! delete idempotency, manifest parsing, and the rate-limit fail-soft path.

use container_image_pruner::config::OwnerType;
use container_image_pruner::github::{auth, client::GhClient, packages, registry};
use serde_json::json;
use wiremock::matchers::{method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(server: &MockServer) -> GhClient {
    GhClient::new("test-token".to_string())
        .unwrap()
        .with_bases(server.uri(), server.uri())
}

#[tokio::test]
async fn detects_organization_owner() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/users/unb-libraries"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "login": "unb-libraries",
            "type": "Organization"
        })))
        .mount(&server)
        .await;

    let ot = packages::detect_owner_type(&client(&server), "unb-libraries")
        .await
        .unwrap();
    assert_eq!(ot, OwnerType::Organization);
}

#[tokio::test]
async fn lists_versions_across_pages() {
    let server = MockServer::start().await;
    let base = server.uri();
    let vpath = "/orgs/unb-libraries/packages/container/pkg/versions";

    // Page 1: a Link header pointing at page 2.
    Mock::given(method("GET"))
        .and(path(vpath))
        .and(query_param_is_missing("page"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header(
                    "link",
                    format!("<{base}{vpath}?page=2>; rel=\"next\"").as_str(),
                )
                .set_body_json(json!([
                    {"id": 1, "name": "sha256:aaa", "created_at": "2026-01-01T00:00:00Z",
                     "metadata": {"container": {"tags": ["prod"]}}}
                ])),
        )
        .mount(&server)
        .await;

    // Page 2: no Link header (last page).
    Mock::given(method("GET"))
        .and(path(vpath))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": 2, "name": "sha256:bbb", "created_at": "2026-01-02T00:00:00Z",
             "metadata": {"container": {"tags": []}}}
        ])))
        .mount(&server)
        .await;

    let versions = packages::list_versions(
        &client(&server),
        "unb-libraries",
        OwnerType::Organization,
        "pkg",
    )
    .await
    .unwrap();
    assert_eq!(versions.len(), 2);
    assert_eq!(versions[0].id, 1);
    assert_eq!(versions[0].tags(), vec!["prod".to_string()]);
    assert_eq!(versions[1].id, 2);
    assert!(versions[1].tags().is_empty());
}

#[tokio::test]
async fn delete_404_is_treated_as_success() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/orgs/o/packages/container/pkg/versions/42"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    // Idempotent: an already-gone version must not surface as an error.
    packages::delete_version(&client(&server), "o", OwnerType::Organization, "pkg", 42)
        .await
        .unwrap();
}

#[tokio::test]
async fn delete_success() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/orgs/o/packages/container/pkg/versions/7"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    packages::delete_version(&client(&server), "o", OwnerType::Organization, "pkg", 7)
        .await
        .unwrap();
}

#[tokio::test]
async fn primary_rate_limit_is_fail_soft() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/users/o"))
        .respond_with(
            ResponseTemplate::new(403)
                .insert_header("x-ratelimit-remaining", "0")
                .insert_header("x-ratelimit-reset", "4102444800"), // 2100-01-01
        )
        .mount(&server)
        .await;

    let err = packages::detect_owner_type(&client(&server), "o")
        .await
        .unwrap_err();
    assert!(err.is_rate_limited(), "expected RateLimited, got {err:?}");
}

#[tokio::test]
async fn fetches_manifest_children_and_subject() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"token": "pull-tok"})))
        .mount(&server)
        .await;

    // An image index with two platform children plus an embedded attestation manifest.
    Mock::given(method("GET"))
        .and(path("/v2/o/pkg/manifests/sha256:index"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.index.v1+json",
            "manifests": [
                {"digest": "sha256:amd64", "mediaType": "application/vnd.oci.image.manifest.v1+json"},
                {"digest": "sha256:arm64", "mediaType": "application/vnd.oci.image.manifest.v1+json"},
                {"digest": "sha256:attest", "annotations": {"vnd.docker.reference.type": "attestation-manifest"}}
            ]
        })))
        .mount(&server)
        .await;

    let c = client(&server);
    let token = auth::pull_token(&c, "o", "pkg").await.unwrap();
    assert_eq!(token, "pull-tok");

    let info = registry::fetch_manifest(&c, "o", "pkg", &token, "sha256:index")
        .await
        .unwrap();
    assert_eq!(
        info.children,
        vec![
            "sha256:amd64".to_string(),
            "sha256:arm64".to_string(),
            "sha256:attest".to_string()
        ]
    );
    assert_eq!(info.subject, None);
}

#[tokio::test]
async fn parses_subject_linked_attestation() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/o/pkg/manifests/sha256:att"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "subject": {"digest": "sha256:image", "mediaType": "application/vnd.oci.image.index.v1+json"}
        })))
        .mount(&server)
        .await;

    let info = registry::fetch_manifest(&client(&server), "o", "pkg", "tok", "sha256:att")
        .await
        .unwrap();
    assert!(info.children.is_empty());
    assert_eq!(info.subject.as_deref(), Some("sha256:image"));
}

#[tokio::test]
async fn single_arch_manifest_has_no_children() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/o/pkg/manifests/sha256:plain"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.docker.distribution.manifest.v2+json",
            "config": {"digest": "sha256:cfg"},
            "layers": [{"digest": "sha256:layer1"}]
        })))
        .mount(&server)
        .await;

    let info = registry::fetch_manifest(&client(&server), "o", "pkg", "tok", "sha256:plain")
        .await
        .unwrap();
    assert!(info.children.is_empty());
    assert_eq!(info.subject, None);
}
