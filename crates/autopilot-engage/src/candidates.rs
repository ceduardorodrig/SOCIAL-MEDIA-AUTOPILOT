//! Reading the social graph: mentions, comments on our own posts, and the
//! accounts of interest we might slowly follow.
//!
//! Parsing is kept pure so every payload shape is unit-tested against a fixture
//! with no network. The only inputs are the documented AppView responses:
//! `app.bsky.notification.listNotifications`, `app.bsky.feed.getPostThread`,
//! `app.bsky.feed.getAuthorFeed` and `app.bsky.actor.getProfile`.

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::records::StrongRef;

/// Why a post is a candidate for a reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateKind {
    /// Someone @-mentioned us.
    Mention,
    /// Someone replied to a post of ours (delivered as a notification).
    ReplyToUs,
    /// Someone commented on one of our posts (read from the thread).
    OwnPostReply,
}

impl CandidateKind {
    /// Deterministic relevance weight (mentions outrank passive comments).
    pub fn base_score(self) -> u32 {
        match self {
            CandidateKind::Mention => 30,
            CandidateKind::ReplyToUs => 25,
            CandidateKind::OwnPostReply => 20,
        }
    }

    /// Stable label for the console.
    pub fn as_str(self) -> &'static str {
        match self {
            CandidateKind::Mention => "mention",
            CandidateKind::ReplyToUs => "reply-to-us",
            CandidateKind::OwnPostReply => "comment",
        }
    }
}

/// A post we could answer, with the thread references a reply record needs.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub kind: CandidateKind,
    /// The post being answered (`parent` of the reply ref).
    pub parent: StrongRef,
    /// The top of the thread (`root` of the reply ref).
    pub root: StrongRef,
    pub author_did: String,
    pub author_handle: String,
    pub text: String,
    /// Deterministic relevance score.
    pub score: u32,
}

/// An account we might follow, with the reason it was selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FollowTarget {
    pub did: String,
    pub handle: String,
    pub reason: String,
}

/// A minimal actor profile (`app.bsky.actor.getProfile`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Author {
    pub did: String,
    pub handle: String,
    #[serde(default)]
    pub display_name: String,
}

/// One notification (`app.bsky.notification.listNotifications`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Notification {
    pub uri: String,
    pub cid: String,
    pub reason: String,
    pub author: Author,
    #[serde(default)]
    pub record: serde_json::Value,
    #[serde(default)]
    pub reason_subject: Option<String>,
    #[serde(default)]
    pub indexed_at: String,
}

/// The `listNotifications` response envelope.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct NotificationsResponse {
    #[serde(default)]
    pub notifications: Vec<Notification>,
}

/// A hydrated post view (the fields the stage needs).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PostView {
    pub uri: String,
    pub cid: String,
    pub author: Author,
    #[serde(default)]
    pub record: serde_json::Value,
}

/// A thread rooted at one of our posts, flattened to its replies.
#[derive(Debug, Clone)]
pub struct Thread {
    pub root: StrongRef,
    pub root_author_did: String,
    pub replies: Vec<PostView>,
}

/// One of our own posts (enough to ask for its thread).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct OwnPost {
    pub uri: String,
    pub cid: String,
}

/// A detailed actor profile (`app.bsky.actor.getProfile`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Profile {
    pub did: String,
    pub handle: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub followers_count: u64,
}

/// Parse a `listNotifications` payload.
pub fn parse_notifications(raw: &str) -> Result<Vec<Notification>> {
    let response: NotificationsResponse =
        serde_json::from_str(raw).context("invalid listNotifications JSON")?;
    Ok(response.notifications)
}

/// Parse a `getProfile` payload.
pub fn parse_profile(raw: &str) -> Result<Profile> {
    serde_json::from_str(raw).context("invalid getProfile JSON")
}

/// Parse a `getAuthorFeed` payload into our own top-level posts.
pub fn parse_author_feed(raw: &str) -> Result<Vec<OwnPost>> {
    let response: FeedResponse = serde_json::from_str(raw).context("invalid getAuthorFeed JSON")?;
    Ok(response
        .feed
        .into_iter()
        .filter(|item| item.post.record.get("reply").is_none())
        .map(|item| OwnPost {
            uri: item.post.uri,
            cid: item.post.cid,
        })
        .collect())
}

/// Parse a `getPostThread` payload.
pub fn parse_thread(raw: &str) -> Result<Thread> {
    let value: serde_json::Value =
        serde_json::from_str(raw).context("invalid getPostThread JSON")?;
    parse_thread_value(&value)
}

/// Parse an already-decoded `getPostThread` payload.
pub fn parse_thread_value(value: &serde_json::Value) -> Result<Thread> {
    let thread = value
        .get("thread")
        .context("getPostThread response has no `thread` field")?;
    let post_value = thread
        .get("post")
        .context("thread root is not a readable post (notFound/blocked)")?;
    let root_post: PostView =
        serde_json::from_value(post_value.clone()).context("invalid thread root postView")?;

    let replies = collect_replies(thread.get("replies"));

    Ok(Thread {
        root: StrongRef::new(root_post.uri, root_post.cid),
        root_author_did: root_post.author.did,
        replies,
    })
}

/// Flatten the nested reply tree into a single list.
fn collect_replies(node: Option<&serde_json::Value>) -> Vec<PostView> {
    let mut out = Vec::new();
    let Some(serde_json::Value::Array(items)) = node else {
        return out;
    };
    for item in items {
        if let Some(post_value) = item.get("post")
            && let Ok(view) = serde_json::from_value::<PostView>(post_value.clone())
        {
            out.push(view);
        }
        out.extend(collect_replies(item.get("replies")));
    }
    out
}

/// Turn notifications into reply candidates (mentions and replies to us).
pub fn mention_candidates(notifications: &[Notification], our_did: &str) -> Vec<Candidate> {
    let mut candidates = Vec::new();
    for notification in notifications {
        if notification.author.did == our_did {
            continue;
        }
        let kind = match notification.reason.as_str() {
            "mention" => CandidateKind::Mention,
            "reply" => CandidateKind::ReplyToUs,
            _ => continue,
        };
        let parent = StrongRef::new(notification.uri.clone(), notification.cid.clone());
        let root = reply_root(&notification.record).unwrap_or_else(|| parent.clone());
        candidates.push(Candidate {
            kind,
            parent,
            root,
            author_did: notification.author.did.clone(),
            author_handle: notification.author.handle.clone(),
            text: record_text(&notification.record),
            score: kind.base_score(),
        });
    }
    dedupe(candidates)
}

/// Turn our own post threads into reply candidates (comments by other people).
pub fn own_post_reply_candidates(threads: &[Thread], our_did: &str) -> Vec<Candidate> {
    let mut candidates = Vec::new();
    for thread in threads {
        for reply in &thread.replies {
            if reply.author.did == our_did {
                continue;
            }
            candidates.push(Candidate {
                kind: CandidateKind::OwnPostReply,
                parent: StrongRef::new(reply.uri.clone(), reply.cid.clone()),
                root: thread.root.clone(),
                author_did: reply.author.did.clone(),
                author_handle: reply.author.handle.clone(),
                text: record_text(&reply.record),
                score: CandidateKind::OwnPostReply.base_score(),
            });
        }
    }
    dedupe(candidates)
}

/// Combine the accounts that interacted with us and the curated accounts of
/// interest into a single, deduplicated follow list (interactions first).
pub fn follow_candidates(
    interactions: &[Candidate],
    interests: &[Profile],
    our_did: &str,
) -> Vec<FollowTarget> {
    let mut targets: Vec<FollowTarget> = Vec::new();
    let mut seen: Vec<String> = Vec::new();

    for candidate in interactions {
        if candidate.author_did == our_did || seen.contains(&candidate.author_did) {
            continue;
        }
        seen.push(candidate.author_did.clone());
        targets.push(FollowTarget {
            did: candidate.author_did.clone(),
            handle: candidate.author_handle.clone(),
            reason: "interacted with us".to_string(),
        });
    }

    for profile in interests {
        if profile.did == our_did || seen.contains(&profile.did) {
            continue;
        }
        seen.push(profile.did.clone());
        targets.push(FollowTarget {
            did: profile.did.clone(),
            handle: profile.handle.clone(),
            reason: "account of interest".to_string(),
        });
    }

    targets
}

/// Merge two candidate lists, keeping one entry per parent (highest score wins).
pub fn combine(first: Vec<Candidate>, second: Vec<Candidate>) -> Vec<Candidate> {
    let mut merged = first;
    merged.extend(second);
    merged.sort_by_key(|candidate| std::cmp::Reverse(candidate.score));
    dedupe(merged)
}

/// Keep only the first candidate per parent post (mentions win over comments).
fn dedupe(candidates: Vec<Candidate>) -> Vec<Candidate> {
    let mut seen: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for candidate in candidates {
        if seen.contains(&candidate.parent.uri) {
            continue;
        }
        seen.push(candidate.parent.uri.clone());
        out.push(candidate);
    }
    out
}

/// The text of a post record, when present.
fn record_text(record: &serde_json::Value) -> String {
    record
        .get("text")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// The thread root referenced by a reply record, when the post is a reply.
fn reply_root(record: &serde_json::Value) -> Option<StrongRef> {
    let root = record.get("reply")?.get("root")?;
    Some(StrongRef::new(
        root.get("uri")?.as_str()?,
        root.get("cid")?.as_str()?,
    ))
}

#[derive(Debug, Deserialize)]
struct FeedResponse {
    #[serde(default)]
    feed: Vec<FeedItem>,
}

#[derive(Debug, Deserialize)]
struct FeedItem {
    post: PostView,
}

#[cfg(test)]
mod tests {
    use super::*;

    const OUR_DID: &str = "did:plc:our";

    fn notifications_fixture() -> &'static str {
        r#"{
          "notifications": [
            {
              "uri": "at://did:plc:them/app.bsky.feed.post/1",
              "cid": "cid-1",
              "reason": "mention",
              "author": {"did": "did:plc:them", "handle": "them.bsky.social"},
              "record": {"text": "@us o que achou do kernel novo?"},
              "indexedAt": "2026-10-12T09:00:00.000Z"
            },
            {
              "uri": "at://did:plc:other/app.bsky.feed.post/2",
              "cid": "cid-2",
              "reason": "reply",
              "author": {"did": "did:plc:other", "handle": "other.bsky.social"},
              "record": {"text": "concordo",
                         "reply": {"root": {"uri": "at://us/root", "cid": "root-cid"},
                                   "parent": {"uri": "at://us/root", "cid": "root-cid"}}},
              "reasonSubject": "at://us/root",
              "indexedAt": "2026-10-12T08:00:00.000Z"
            },
            {
              "uri": "at://did:plc:fan/app.bsky.feed.post/3",
              "cid": "cid-3",
              "reason": "follow",
              "author": {"did": "did:plc:fan", "handle": "fan.bsky.social"},
              "record": {}
            },
            {
              "uri": "at://did:plc:our/app.bsky.feed.post/4",
              "cid": "cid-4",
              "reason": "mention",
              "author": {"did": "did:plc:our", "handle": "us.bsky.social"},
              "record": {"text": "nota para mim mesmo"}
            }
          ]
        }"#
    }

    #[test]
    fn mentions_and_replies_become_candidates_and_others_are_ignored() {
        let notifications = parse_notifications(notifications_fixture()).expect("fixture parses");
        let candidates = mention_candidates(&notifications, OUR_DID);

        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].kind, CandidateKind::Mention);
        assert_eq!(candidates[0].author_handle, "them.bsky.social");
        assert_eq!(
            candidates[0].root.uri,
            "at://did:plc:them/app.bsky.feed.post/1"
        );

        // The reply to us carries the thread root from its record.
        assert_eq!(candidates[1].kind, CandidateKind::ReplyToUs);
        assert_eq!(candidates[1].root.uri, "at://us/root");
        assert_eq!(
            candidates[1].parent.uri,
            "at://did:plc:other/app.bsky.feed.post/2"
        );

        // The follow notification and our own mention are not candidates.
        assert!(!candidates.iter().any(|c| c.author_did == OUR_DID));
    }

    #[test]
    fn comments_on_our_post_become_candidates() {
        let raw = r#"{
          "thread": {
            "post": {"uri": "at://us/root", "cid": "root-cid",
                     "author": {"did": "did:plc:our", "handle": "us.bsky.social"},
                     "record": {"text": "post nosso"}},
            "replies": [
              {"post": {"uri": "at://them/1", "cid": "c1",
                        "author": {"did": "did:plc:them", "handle": "them.bsky.social"},
                        "record": {"text": "boa!"}},
               "replies": [
                 {"post": {"uri": "at://deep/2", "cid": "c2",
                           "author": {"did": "did:plc:deep", "handle": "deep.bsky.social"},
                           "record": {"text": "concordo com eles"}}}
               ]},
              {"post": {"uri": "at://us/self", "cid": "c3",
                        "author": {"did": "did:plc:our", "handle": "us.bsky.social"},
                        "record": {"text": "obrigado"}}}
            ]
          }
        }"#;
        let thread = parse_thread(raw).expect("fixture parses");
        assert_eq!(thread.root.uri, "at://us/root");
        assert_eq!(thread.replies.len(), 3);

        let candidates = own_post_reply_candidates(&[thread], OUR_DID);
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].kind, CandidateKind::OwnPostReply);
        assert_eq!(candidates[0].root.uri, "at://us/root");
        assert_eq!(candidates[1].author_handle, "deep.bsky.social");
    }

    #[test]
    fn a_duplicate_parent_is_kept_once() {
        let notifications = parse_notifications(notifications_fixture()).expect("fixture parses");
        let mentions = mention_candidates(&notifications, OUR_DID);
        let duplicated = mention_candidates(&notifications, OUR_DID);
        let mut all = mentions;
        all.extend(duplicated);
        let deduped = dedupe(all);
        assert_eq!(deduped.len(), 2);
    }

    #[test]
    fn author_feed_keeps_only_top_level_posts() {
        let raw = r#"{
          "feed": [
            {"post": {"uri": "at://us/1", "cid": "c1",
                      "author": {"did": "did:plc:our", "handle": "us.bsky.social"},
                      "record": {"text": "post"}}},
            {"post": {"uri": "at://us/2", "cid": "c2",
                      "author": {"did": "did:plc:our", "handle": "us.bsky.social"},
                      "record": {"text": "resposta",
                                 "reply": {"root": {"uri": "at://x", "cid": "y"},
                                           "parent": {"uri": "at://x", "cid": "y"}}}}}
          ]
        }"#;
        let posts = parse_author_feed(raw).expect("fixture parses");
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0].uri, "at://us/1");
    }

    #[test]
    fn follow_candidates_prefer_interactions_and_skip_ourselves() {
        let interactions = vec![
            Candidate {
                kind: CandidateKind::Mention,
                parent: StrongRef::new("at://1", "c1"),
                root: StrongRef::new("at://1", "c1"),
                author_did: "did:plc:them".to_string(),
                author_handle: "them.bsky.social".to_string(),
                text: "oi".to_string(),
                score: 30,
            },
            Candidate {
                kind: CandidateKind::Mention,
                parent: StrongRef::new("at://2", "c2"),
                root: StrongRef::new("at://2", "c2"),
                author_did: OUR_DID.to_string(),
                author_handle: "us.bsky.social".to_string(),
                text: "eu".to_string(),
                score: 30,
            },
        ];
        let interests = vec![
            Profile {
                did: "did:plc:them".to_string(),
                handle: "them.bsky.social".to_string(),
                description: String::new(),
                followers_count: 10,
            },
            Profile {
                did: "did:plc:gnome".to_string(),
                handle: "gnome.org".to_string(),
                description: "GNOME".to_string(),
                followers_count: 1000,
            },
        ];

        let targets = follow_candidates(&interactions, &interests, OUR_DID);
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].did, "did:plc:them");
        assert_eq!(targets[0].reason, "interacted with us");
        assert_eq!(targets[1].did, "did:plc:gnome");
        assert_eq!(targets[1].reason, "account of interest");
    }

    #[test]
    fn profile_and_invalid_json() {
        let profile =
            parse_profile(r#"{"did":"did:plc:gnome","handle":"gnome.org","followersCount":42}"#)
                .expect("profile parses");
        assert_eq!(profile.handle, "gnome.org");
        assert_eq!(profile.followers_count, 42);

        assert!(parse_notifications("not json").is_err());
        assert!(parse_thread("{}").is_err());
    }
}
