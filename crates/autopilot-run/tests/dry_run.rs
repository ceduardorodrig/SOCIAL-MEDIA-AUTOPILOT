//! `autopilot run --dry-run` must be safe anywhere: no LLM, no GPU, no network
//! and no publish — yet it still walks every stage and writes the journal.

use std::path::PathBuf;

use autopilot_run::{CardAttachment, RunOptions, StageStatus, run};

fn journal_path() -> PathBuf {
    std::env::temp_dir().join(format!("autopilot-run-dry-{}.jsonl", std::process::id()))
}

#[tokio::test]
async fn dry_run_walks_every_stage_without_side_effects() {
    let journal = journal_path();
    let _ = tokio::fs::remove_file(&journal).await;

    let options = RunOptions {
        persona_path: PathBuf::from("tests/fixtures/persona-dry-run.yaml"),
        query: None,
        limit: 5,
        // An unreachable endpoint proves the dry run never dials the network.
        endpoint: "http://127.0.0.1:9".to_string(),
        platform: None,
        // A path that does not exist proves the dry run never reads the registry.
        config_path: PathBuf::from("tests/fixtures/does-not-exist.yaml"),
        journal_path: Some(journal.clone()),
        memory_path: None,
        dry_run: true,
        card: CardAttachment::Off,
    };

    let report = run(options).await.expect("dry run succeeds");

    assert!(report.dry_run);
    assert!(!report.failed, "a dry run never fails");
    assert!(report.published.is_empty(), "a dry run never publishes");
    assert_eq!(report.persona, "dry-run-demo");

    let stages: Vec<&str> = report
        .stages
        .iter()
        .map(|stage| stage.stage.as_str())
        .collect();
    for expected in ["harvest", "triage", "create", "guardrail", "publish"] {
        assert!(stages.contains(&expected), "missing stage {expected}");
    }

    // HB-033 fallback: a pack with no `harvest.queries` still searches the
    // humanised niche (`demo-niche` -> `demo niche`).
    let harvest = report
        .stages
        .iter()
        .find(|stage| stage.stage == "harvest")
        .expect("harvest stage recorded");
    assert!(
        harvest.detail.contains("demo niche"),
        "harvest should fall back to the humanised niche, got: {}",
        harvest.detail
    );
    assert!(
        report
            .stages
            .iter()
            .all(|stage| stage.status == StageStatus::DryRun),
        "every dry-run stage is labelled dry-run"
    );

    let raw = tokio::fs::read_to_string(&journal)
        .await
        .expect("journal written");
    let lines: Vec<&str> = raw.lines().collect();
    assert_eq!(lines.len(), report.stages.len());

    let first: serde_json::Value =
        serde_json::from_str(lines.first().copied().unwrap_or("{}")).expect("valid journal JSON");
    assert_eq!(first["persona"], "dry-run-demo");
    assert_eq!(first["stage"], "harvest");
    assert_eq!(first["status"], "dry-run");
    assert_eq!(first["run_id"], report.run_id);

    tokio::fs::remove_file(&journal).await.ok();
}

#[tokio::test]
async fn a_draft_persona_is_never_executed_for_real() {
    let journal = journal_path().with_extension("draft.jsonl");
    let _ = tokio::fs::remove_file(&journal).await;

    let options = RunOptions {
        persona_path: PathBuf::from("tests/fixtures/persona-dry-run.yaml"),
        query: None,
        limit: 5,
        endpoint: "http://127.0.0.1:9".to_string(),
        platform: None,
        config_path: PathBuf::from("tests/fixtures/does-not-exist.yaml"),
        journal_path: Some(journal.clone()),
        memory_path: None,
        dry_run: false,
        card: CardAttachment::Off,
    };

    let report = run(options).await.expect("the command itself succeeds");
    assert!(!report.failed, "skipping a draft is not a failure");
    assert!(report.published.is_empty());
    assert_eq!(report.stages.len(), 1);
    assert_eq!(report.stages[0].stage, "run");
    assert_eq!(report.stages[0].status, StageStatus::Skipped);

    tokio::fs::remove_file(&journal).await.ok();
}

#[tokio::test]
async fn the_harvest_query_comes_from_the_persona_pack() {
    // HB-033: two personas in the same broad subject must harvest different
    // sources because the pack — not the niche alone — names the query.
    let journal = journal_path().with_extension("harvest.jsonl");
    let _ = tokio::fs::remove_file(&journal).await;

    let options = RunOptions {
        persona_path: PathBuf::from("tests/fixtures/persona-harvest-queries.yaml"),
        query: None,
        limit: 5,
        endpoint: "http://127.0.0.1:9".to_string(),
        platform: None,
        config_path: PathBuf::from("tests/fixtures/does-not-exist.yaml"),
        journal_path: Some(journal.clone()),
        memory_path: None,
        dry_run: true,
        card: CardAttachment::Off,
    };

    let report = run(options).await.expect("dry run succeeds");
    let harvest = report
        .stages
        .iter()
        .find(|stage| stage.stage == "harvest")
        .expect("harvest stage recorded");

    assert!(
        harvest.detail.contains("space mission anomalies")
            || harvest.detail.contains("strange telescope observations"),
        "harvest should search one of the pack's queries, got: {}",
        harvest.detail
    );
    assert!(
        !harvest.detail.contains("astronomy space curiosities"),
        "the niche fallback must not be used when the pack declares queries: {}",
        harvest.detail
    );

    tokio::fs::remove_file(&journal).await.ok();
}
