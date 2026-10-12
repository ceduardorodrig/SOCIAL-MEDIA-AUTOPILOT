//! Engagement stage — reply to mentions, like contextually and follow a handful
//! of accounts of interest, at a human pace.
//!
//! This stage behaves like a person, not a bot. It runs on its **own schedule**
//! (separate windows from publishing), acts **sporadically over time**, and is
//! bounded by hard per-day caps plus a per-run action budget. Every action that
//! happens is remembered in a JSONL state file under `var/` (gitignored), so
//! nothing is answered, liked or followed twice.
//!
//! Hard constraints, enforced here and never to be relaxed:
//! - **No mass follow / unfollow.** Follows are capped, jittered and sporadic;
//!   there is no unfollow path at all, so a follow/unfollow loop cannot exist.
//! - **No mass likes.** A like is only ever cast on a post we are replying to
//!   (contextual); there is no timeline sweep anywhere in this crate.
//! - **No auto-DM.** No direct-message code exists here.
//! - Every reply is generated through the shared LLM chain and **must** pass the
//!   deterministic guardrail before it is written: a reply is content and never
//!   bypasses the blocklist or the rubric.
//! - Interactions need **no GPU**. This stage adds no GPU dependency; it only
//!   uses the existing cloud-first provider chain.

pub mod bluesky;
pub mod candidates;
pub mod config;
pub mod jitter;
pub mod planner;
pub mod prompt;
pub mod records;
pub mod runner;
pub mod state;

pub use candidates::{
    Candidate, CandidateKind, FollowTarget, Notification, NotificationsResponse, OwnPost, PostView,
    Profile, Thread,
};
pub use config::EngageConfig;
pub use planner::{Action, Plan, PlanInput, plan};
pub use prompt::{build_reply_prompt, parse_reply, screen_reply};
pub use records::StrongRef;
pub use runner::{EngageOptions, EngageReport, ExecutedAction, run};
pub use state::{ActionKind, ActionLog, StateEntry};
