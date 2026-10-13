//! HB-034: the warming ramp is **autonomous**.
//!
//! The engine detects the account's birth from its own history — the first
//! publication recorded in the run journals — instead of depending on an
//! operator filling in `started_on`. This drives the detection through the real
//! journal reader: an empty base (no ramp), a single fresh post (week 1) and an
//! old post (steady state).

use std::path::{Path, PathBuf};

use autopilot_core::{Format, now_unix, utc_day};
use autopilot_run::{
    detected_start_day, read_published, warming_ceiling, warming_releases, warming_week,
};

/// A unique runs directory per test process and run.
fn runs_dir(tag: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "autopilot-ramp-{tag}-{}-{stamp}",
        std::process::id()
    ))
}

/// Write one published post at `at_unix` into `dir` as a journal line.
async fn seed(dir: &Path, at_unix: u64) {
    tokio::fs::create_dir_all(dir).await.expect("mkdir");
    let line = serde_json::json!({
        "at_unix": at_unix,
        "run_id": "seed",
        "persona": "ramp-demo",
        "stage": "publish",
        "status": "published",
        "platform": "bluesky",
        "variant": "A",
        "uri": "at://did:plc:abc/app.bsky.feed.post/1",
        "format": "text",
    });
    tokio::fs::write(dir.join("ramp-demo-seed.jsonl"), format!("{line}\n"))
        .await
        .expect("seed journal");
}

#[tokio::test]
async fn the_birth_is_detected_from_the_journal_not_declared_by_hand() {
    let day = utc_day(now_unix());
    let seconds_per_day = 86_400;

    // An empty base has no birth: the ramp stays inert (mature), never an
    // eternal week 1.
    let empty = runs_dir("empty");
    tokio::fs::create_dir_all(&empty).await.expect("mkdir");
    let published = read_published(&empty).await.expect("reads");
    assert!(published.is_empty());
    assert_eq!(detected_start_day(&published), None);

    // A single post published today: the account is born today, week 1 —
    // text-only, one post a day.
    let fresh = runs_dir("fresh");
    seed(&fresh, now_unix()).await;
    let published = read_published(&fresh).await.expect("reads");
    let start = detected_start_day(&published).expect("the first post is the birth");
    let age = day - start;
    assert_eq!(age, 0);
    assert_eq!(warming_week(age), 1);
    assert_eq!(warming_ceiling(age), 1);
    assert_eq!(warming_releases(age), [Format::Text]);

    // A post from thirty days ago: the same engine reads the account as mature
    // and releases every format — no `started_on` was ever written.
    let old = runs_dir("old");
    seed(&old, now_unix() - 30 * seconds_per_day).await;
    let published = read_published(&old).await.expect("reads");
    let start = detected_start_day(&published).expect("the first post is the birth");
    let age = day - start;
    assert_eq!(age, 30);
    assert_eq!(warming_week(age), 3);
    assert!(warming_releases(age).contains(&Format::Video));

    tokio::fs::remove_dir_all(&empty).await.ok();
    tokio::fs::remove_dir_all(&fresh).await.ok();
    tokio::fs::remove_dir_all(&old).await.ok();
}
