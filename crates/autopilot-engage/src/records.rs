//! AT Protocol record builders used by the engagement stage.
//!
//! These only shape the `app.bsky.*` records (a reply, a like, a follow). The
//! XRPC transport that writes them is shared with the publication adapter, so
//! no two adapters carry their own HTTP write path.

use serde_json::{Value, json};

/// `app.bsky.feed.post` — a reply is still a post record.
pub const POST_COLLECTION: &str = "app.bsky.feed.post";

/// `app.bsky.feed.like`.
pub const LIKE_COLLECTION: &str = "app.bsky.feed.like";

/// `app.bsky.graph.follow`.
pub const FOLLOW_COLLECTION: &str = "app.bsky.graph.follow";

/// A `com.atproto.repo.strongRef`: a URI with its content hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StrongRef {
    pub uri: String,
    pub cid: String,
}

impl StrongRef {
    /// Build a strong reference from a URI and its CID.
    pub fn new(uri: impl Into<String>, cid: impl Into<String>) -> Self {
        Self {
            uri: uri.into(),
            cid: cid.into(),
        }
    }

    /// The JSON shape a record references.
    pub fn to_json(&self) -> Value {
        json!({ "uri": self.uri, "cid": self.cid })
    }
}

/// Build the `app.bsky.feed.post` record for a reply.
///
/// `root` is the top of the thread and `parent` is the post being answered;
/// both are `com.atproto.repo.strongRef`s (Lexicon `app.bsky.feed.post#replyRef`).
pub fn build_reply_record(
    text: &str,
    created_at: &str,
    root: &StrongRef,
    parent: &StrongRef,
) -> Value {
    json!({
        "$type": POST_COLLECTION,
        "text": text,
        "createdAt": created_at,
        "reply": { "root": root.to_json(), "parent": parent.to_json() },
    })
}

/// Build the `app.bsky.feed.like` record for a subject post.
pub fn build_like_record(subject: &StrongRef, created_at: &str) -> Value {
    json!({
        "$type": LIKE_COLLECTION,
        "subject": subject.to_json(),
        "createdAt": created_at,
    })
}

/// Build the `app.bsky.graph.follow` record for an account DID.
///
/// There is deliberately **no** `build_unfollow_*`: this crate never unfollows,
/// so a follow/unfollow loop cannot exist.
pub fn build_follow_record(subject_did: &str, created_at: &str) -> Value {
    json!({
        "$type": FOLLOW_COLLECTION,
        "subject": subject_did,
        "createdAt": created_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_record_carries_root_and_parent() {
        let root = StrongRef::new("at://root", "cid-root");
        let parent = StrongRef::new("at://parent", "cid-parent");
        let record = build_reply_record("boa pergunta", "2026-10-12T20:00:00.000Z", &root, &parent);

        assert_eq!(record["$type"], "app.bsky.feed.post");
        assert_eq!(record["text"], "boa pergunta");
        assert_eq!(record["reply"]["root"]["uri"], "at://root");
        assert_eq!(record["reply"]["root"]["cid"], "cid-root");
        assert_eq!(record["reply"]["parent"]["uri"], "at://parent");
        assert_eq!(record["reply"]["parent"]["cid"], "cid-parent");
    }

    #[test]
    fn like_record_references_the_subject() {
        let record =
            build_like_record(&StrongRef::new("at://p", "cid"), "2026-10-12T20:00:00.000Z");
        assert_eq!(record["$type"], "app.bsky.feed.like");
        assert_eq!(record["subject"]["uri"], "at://p");
        assert_eq!(record["subject"]["cid"], "cid");
    }

    #[test]
    fn follow_record_references_the_did() {
        let record = build_follow_record("did:plc:abc", "2026-10-12T20:00:00.000Z");
        assert_eq!(record["$type"], "app.bsky.graph.follow");
        assert_eq!(record["subject"], "did:plc:abc");
    }
}
