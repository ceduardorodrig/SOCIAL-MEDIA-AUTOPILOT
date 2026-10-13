//! HB-034: a slot is an **opportunity**, not an obligation.
//!
//! When a channel has already met its declared daily ceiling, a live run must
//! skip the slot *before* any network call, so an extra scheduler fire costs
//! nothing — the counterpart of the publishing-window gate.

use std::path::PathBuf;

use autopilot_core::now_unix;
use autopilot_run::{CardAttachment, RunOptions, StageStatus, run};

/// A unique runs directory per test process and run.
fn runs_dir(tag: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "autopilot-slot-{tag}-{}-{stamp}",
        std::process::id()
    ))
}

#[tokio::test]
async fn a_full_slot_is_skipped_before_any_network_call() {
    let dir = runs_dir("ceiling");
    tokio::fs::create_dir_all(&dir).await.expect("mkdir");

    // Seed today's post in the journal: the day's ceiling (`posts_per_day: 1`)
    // is already reached, so the slot has no room.
    let seeded = dir.join("slot-gate-demo-seed.jsonl");
    let line = serde_json::json!({
        "at_unix": now_unix(),
        "run_id": "seed",
        "persona": "slot-gate-demo",
        "stage": "publish",
        "status": "published",
        "platform": "bluesky",
        "variant": "A",
        "uri": "at://did:plc:abc/app.bsky.feed.post/1",
        "format": "text",
    });
    tokio::fs::write(&seeded, format!("{line}\n"))
        .await
        .expect("seed journal");

    let journal = dir.join("slot-gate-demo-live.jsonl");
    let options = RunOptions {
        persona_path: PathBuf::from("tests/fixtures/persona-slot-gate.yaml"),
        query: Some("anything".to_string()),
        limit: 5,
        // An unreachable endpoint: the gate must return before harvest dials it.
        endpoint: "http://127.0.0.1:9".to_string(),
        platform: None,
        // A path that does not exist proves the registry is never read.
        config_path: PathBuf::from("tests/fixtures/does-not-exist.yaml"),
        journal_path: Some(journal.clone()),
        memory_path: None,
        dry_run: false,
        card: CardAttachment::Off,
        media_path: PathBuf::from("config/media.yaml"),
    };

    let report = run(options).await.expect("the command itself succeeds");
    assert!(!report.failed, "a deliberate skip is not a failure");
    assert!(report.published.is_empty(), "nothing is published");
    assert_eq!(report.stages.len(), 1, "the run stops before harvest");
    assert_eq!(report.stages[0].stage, "run");
    assert_eq!(report.stages[0].status, StageStatus::Skipped);
    assert!(
        report.stages[0].detail.contains("opportunity"),
        "the journal must say the slot had no room: {}",
        report.stages[0].detail
    );
    assert!(
        report.stages.iter().all(|stage| stage.stage != "harvest"),
        "the gate must run before harvest spends the network"
    );

    tokio::fs::remove_dir_all(&dir).await.ok();
}
