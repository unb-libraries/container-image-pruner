//! Env-gated live smoke test against a real GHCR package. `#[ignore]` by default; run with:
//!
//!   GHCR_TEST_OWNER=unb-libraries GHCR_TEST_PACKAGE=loyalist.lib.unb.ca \
//!   GITHUB_TOKEN=... cargo test --test live_smoke -- --ignored --nocapture
//!
//! It only ever performs a **dry run** (read-only) and asserts the core safety invariant:
//! nothing that is text-tagged is ever selected for deletion. This exercises the real API
//! shapes that wiremock cannot verify (that `name` is the digest, media types, `subject`).

use container_image_pruner::config::Config;
use container_image_pruner::policy::Policy;

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.is_empty())
}

#[tokio::test]
#[ignore = "requires GHCR_TEST_OWNER/GHCR_TEST_PACKAGE/GITHUB_TOKEN and network access"]
async fn dry_run_never_plans_to_delete_text_tagged() {
    let (owner, package, token) = match (
        env("GHCR_TEST_OWNER"),
        env("GHCR_TEST_PACKAGE"),
        env("GITHUB_TOKEN"),
    ) {
        (Some(o), Some(p), Some(t)) => (o, p, t),
        _ => {
            eprintln!("skipping: set GHCR_TEST_OWNER, GHCR_TEST_PACKAGE, GITHUB_TOKEN");
            return;
        }
    };

    let config = Config {
        owner,
        package: Some(package),
        owner_type: None,
        policy: Policy {
            older_than_days: 30,
            keep_at_least: 5,
        },
        token,
        dry_run: true,
        max_delete: None,
        read_concurrency: 8,
        delete_concurrency: 3,
        protect_subject_attestations: false,
        audit_log: None,
        api_base: None,
        registry_base: None,
    };

    let summary = container_image_pruner::run(config)
        .await
        .expect("run failed");
    println!(
        "{}",
        serde_json::to_string_pretty(&summary).expect("serialize summary")
    );

    for pkg in &summary.packages {
        for d in &pkg.deletes {
            // A planned delete must never carry a non-build tag.
            for tag in &d.tags {
                assert!(
                    container_image_pruner::domain::tag::is_build_tag(tag),
                    "planned to delete a non-build tag {tag:?} on {}",
                    d.digest
                );
            }
        }
    }
}
