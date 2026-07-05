//! Thin binary entry point: parse args, init tracing, run, emit the machine record (JSON) plus
//! the GitHub Action surfaces (annotations, step summary, outputs), map to an exit code. No
//! business logic lives here.

use std::process::ExitCode;

use clap::Parser;

use container_image_pruner::cli::Args;
use container_image_pruner::report;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    // Parse manually so usage errors exit 1 (our "hard failure"), not clap's default 2 — in this
    // tool exit 2 means "degraded, safe to re-run", which the Action maps to a *successful* step.
    let args = match Args::try_parse() {
        Ok(a) => a,
        Err(e) => {
            let help_or_version = matches!(
                e.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            );
            let _ = e.print();
            return ExitCode::from(if help_or_version { 0 } else { 1 });
        }
    };
    let config = match args.resolve() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("::error title=container-image-pruner::{e}");
            return ExitCode::from(1);
        }
    };

    let summary = match container_image_pruner::run(config).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("::error title=container-image-pruner::{e}");
            return ExitCode::from(1);
        }
    };

    // Machine record on stdout — always pretty JSON (the Action tees this to a file).
    match serde_json::to_string_pretty(&summary) {
        Ok(s) => println!("{s}"),
        Err(e) => {
            eprintln!("::error title=container-image-pruner::failed to serialize summary: {e}");
            return ExitCode::from(1);
        }
    }

    // Human-facing surfaces (annotations to stderr; step summary + outputs to their env files),
    // written BEFORE returning a non-zero (degraded/aborted) code so a partial run still reports.
    report::emit_annotations(&summary);
    write_env_file(
        "GITHUB_STEP_SUMMARY",
        &report::step_summary_markdown(&summary),
    );
    write_env_file("GITHUB_OUTPUT", &report::github_outputs(&summary));

    ExitCode::from(summary.exit_code() as u8)
}

/// Append `content` to the file named by env var `var`, if that var is set. Best-effort: a
/// failure here is logged but must not change the run's outcome.
fn write_env_file(var: &str, content: &str) {
    let Ok(path) = std::env::var(var) else {
        return;
    };
    use std::io::Write as _;
    let result = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| f.write_all(content.as_bytes()));
    if let Err(e) = result {
        eprintln!("::warning::could not write {var} ({path}): {e}");
    }
}
