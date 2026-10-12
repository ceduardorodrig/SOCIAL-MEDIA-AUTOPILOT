//! The pacing brain: turn candidates and state into a bounded, jittered plan.
//!
//! Everything here is pure and deterministic for a given seed, so the caps, the
//! per-run budget, the contextual-like rule and the sporadic skip are all
//! unit-testable without touching the network or the clock.

use crate::candidates::{Candidate, FollowTarget};
use crate::config::EngageConfig;
use crate::jitter::Rng;
use crate::records::StrongRef;
use crate::state::{ActionKind, ActionLog};

/// One planned write.
#[derive(Debug, Clone)]
pub enum Action {
    /// Answer a post.
    Reply(Candidate),
    /// Like the parent of a reply we are about to make — never a timeline sweep.
    Like(StrongRef),
    /// Follow an account of interest — slowly, never in a burst.
    Follow(FollowTarget),
}

impl Action {
    /// The action type, used for caps and state.
    pub fn kind(&self) -> ActionKind {
        match self {
            Action::Reply(_) => ActionKind::Reply,
            Action::Like(_) => ActionKind::Like,
            Action::Follow(_) => ActionKind::Follow,
        }
    }

    /// The state key this action deduplicates on.
    pub fn target(&self) -> &str {
        match self {
            Action::Reply(candidate) => &candidate.parent.uri,
            Action::Like(subject) => &subject.uri,
            Action::Follow(target) => &target.did,
        }
    }

    /// Human-readable description for the console and the dry run.
    pub fn describe(&self) -> String {
        match self {
            Action::Reply(candidate) => format!(
                "reply to {} ({}): {}",
                candidate.author_handle,
                candidate.kind.as_str(),
                truncate(&candidate.text, 80)
            ),
            Action::Like(subject) => format!("like {}", subject.uri),
            Action::Follow(target) => format!("follow {} ({})", target.handle, target.reason),
        }
    }
}

/// Everything the planner reads.
pub struct PlanInput<'a> {
    pub candidates: &'a [Candidate],
    pub follows: &'a [FollowTarget],
    pub state: &'a ActionLog,
    pub config: &'a EngageConfig,
    pub now_unix: u64,
}

/// The actions chosen for one run.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub actions: Vec<Action>,
    /// A sporadic run that intentionally does nothing.
    pub skipped_run: bool,
    /// Why the plan looks the way it does.
    pub reason: String,
}

/// Choose the actions for one run under the caps, budget and jitter.
pub fn plan(input: &PlanInput, rng: &mut Rng) -> Plan {
    if rng.fraction() < input.config.pacing.run_skip_probability {
        return Plan {
            actions: Vec::new(),
            skipped_run: true,
            reason: "sporadic skip — this run intentionally does nothing".to_string(),
        };
    }

    let budget = input.config.caps.max_actions_per_run as usize;
    let mut actions: Vec<Action> = Vec::new();

    let reply_room = remaining(
        input.config.caps.replies_per_day,
        input.state.count_today(ActionKind::Reply, input.now_unix),
    );
    let like_room = remaining(
        input.config.caps.likes_per_day,
        input.state.count_today(ActionKind::Like, input.now_unix),
    );
    let follow_day_room = remaining(
        input.config.caps.follows_per_day,
        input.state.count_today(ActionKind::Follow, input.now_unix),
    );
    let follow_week_room = remaining(
        input.config.caps.follows_per_week,
        input
            .state
            .count_this_week(ActionKind::Follow, input.now_unix),
    );

    // Highest relevance first, with a stable tie-break on the URI.
    let mut ordered: Vec<&Candidate> = input.candidates.iter().collect();
    ordered.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.parent.uri.cmp(&b.parent.uri))
    });

    let mut replies = 0usize;
    let mut likes = 0usize;
    for candidate in ordered {
        if replies >= reply_room || actions.len() >= budget {
            break;
        }
        if input.state.has(ActionKind::Reply, &candidate.parent.uri) {
            continue;
        }
        actions.push(Action::Reply(candidate.clone()));
        replies += 1;

        // Contextual like: only ever the post we are answering.
        if likes < like_room
            && actions.len() < budget
            && !input.state.has(ActionKind::Like, &candidate.parent.uri)
        {
            actions.push(Action::Like(candidate.parent.clone()));
            likes += 1;
        }
    }

    let mut follows = 0usize;
    for target in input.follows {
        if actions.len() >= budget {
            break;
        }
        if follows >= follow_day_room || follows >= follow_week_room {
            break;
        }
        if input.state.has(ActionKind::Follow, &target.did) {
            continue;
        }
        if actions
            .iter()
            .any(|action| action.target() == target.did.as_str())
        {
            continue;
        }
        actions.push(Action::Follow(target.clone()));
        follows += 1;
    }

    let reason = if actions.is_empty() {
        "nothing to do — no candidate cleared the caps and the state".to_string()
    } else {
        format!(
            "{} action(s) selected under the caps and the budget",
            actions.len()
        )
    };

    Plan {
        actions,
        skipped_run: false,
        reason,
    }
}

fn remaining(cap: u32, used: u32) -> usize {
    cap.saturating_sub(used) as usize
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candidates::CandidateKind;
    use crate::state::StateEntry;

    const NOW: u64 = 1_700_000_000;

    fn config(yaml: &str) -> EngageConfig {
        EngageConfig::parse_yaml(yaml).expect("config parses")
    }

    fn candidate(uri: &str, score: u32) -> Candidate {
        Candidate {
            kind: CandidateKind::Mention,
            parent: StrongRef::new(uri, "cid"),
            root: StrongRef::new(uri, "cid"),
            author_did: format!("did:plc:{uri}"),
            author_handle: "them.bsky.social".to_string(),
            text: "what do you think?".to_string(),
            score,
        }
    }

    fn target(did: &str) -> FollowTarget {
        FollowTarget {
            did: did.to_string(),
            handle: "someone.bsky.social".to_string(),
            reason: "account of interest".to_string(),
        }
    }

    fn entry(action: ActionKind, target: &str, at_unix: u64) -> StateEntry {
        StateEntry {
            at_unix,
            action,
            target: target.to_string(),
            persona: "tech-curiosities".to_string(),
            detail: String::new(),
        }
    }

    #[test]
    fn a_forced_skip_runs_nothing() {
        let cfg = config("pacing: {run_skip_probability: 1.0}");
        let candidates = [candidate("at://1", 30)];
        let input = PlanInput {
            candidates: &candidates,
            follows: &[],
            state: &ActionLog::default(),
            config: &cfg,
            now_unix: NOW,
        };
        let plan = plan(&input, &mut Rng::from_seed(1));
        assert!(plan.skipped_run);
        assert!(plan.actions.is_empty());
    }

    #[test]
    fn a_never_skip_always_plans() {
        let cfg = config("pacing: {run_skip_probability: 0.0}");
        let candidates = [candidate("at://1", 30)];
        let input = PlanInput {
            candidates: &candidates,
            follows: &[],
            state: &ActionLog::default(),
            config: &cfg,
            now_unix: NOW,
        };
        let plan = plan(&input, &mut Rng::from_seed(1));
        assert!(!plan.skipped_run);
        assert_eq!(plan.actions.len(), 2); // reply + contextual like
    }

    #[test]
    fn replies_are_capped_and_a_like_only_follows_a_reply() {
        let cfg = config("caps: {replies_per_day: 1, likes_per_day: 5, max_actions_per_run: 10}");
        let candidates = [candidate("at://1", 30), candidate("at://2", 20)];
        let input = PlanInput {
            candidates: &candidates,
            follows: &[],
            state: &ActionLog::default(),
            config: &cfg,
            now_unix: NOW,
        };
        let plan = plan(&input, &mut Rng::from_seed(2));
        let replies: Vec<_> = plan
            .actions
            .iter()
            .filter(|a| a.kind() == ActionKind::Reply)
            .collect();
        let likes: Vec<_> = plan
            .actions
            .iter()
            .filter(|a| a.kind() == ActionKind::Like)
            .collect();
        assert_eq!(replies.len(), 1);
        assert_eq!(likes.len(), 1);
        // The like targets exactly the post we replied to.
        assert_eq!(likes[0].target(), replies[0].target());
    }

    #[test]
    fn the_per_run_budget_bounds_every_write() {
        let cfg = config("caps: {replies_per_day: 10, likes_per_day: 10, max_actions_per_run: 3}");
        let candidates = [
            candidate("at://1", 30),
            candidate("at://2", 20),
            candidate("at://3", 10),
        ];
        let input = PlanInput {
            candidates: &candidates,
            follows: &[],
            state: &ActionLog::default(),
            config: &cfg,
            now_unix: NOW,
        };
        let plan = plan(&input, &mut Rng::from_seed(3));
        assert_eq!(plan.actions.len(), 3);
    }

    #[test]
    fn state_dedup_skips_what_was_already_answered() {
        let cfg = config("pacing: {run_skip_probability: 0.0}");
        let candidates = [candidate("at://1", 30), candidate("at://2", 20)];
        let state = ActionLog::from_entries(vec![entry(ActionKind::Reply, "at://1", NOW)]);
        let input = PlanInput {
            candidates: &candidates,
            follows: &[],
            state: &state,
            config: &cfg,
            now_unix: NOW,
        };
        let plan = plan(&input, &mut Rng::from_seed(4));
        let replies: Vec<_> = plan
            .actions
            .iter()
            .filter(|a| a.kind() == ActionKind::Reply)
            .collect();
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].target(), "at://2");
    }

    #[test]
    fn higher_scoring_candidates_are_chosen_first() {
        let cfg = config("pacing: {run_skip_probability: 0.0}\ncaps: {replies_per_day: 1}");
        let candidates = [candidate("at://low", 10), candidate("at://high", 30)];
        let input = PlanInput {
            candidates: &candidates,
            follows: &[],
            state: &ActionLog::default(),
            config: &cfg,
            now_unix: NOW,
        };
        let plan = plan(&input, &mut Rng::from_seed(5));
        assert_eq!(plan.actions[0].target(), "at://high");
    }

    #[test]
    fn follows_respect_the_daily_weekly_and_budget_caps() {
        let cfg = config(
            "pacing: {run_skip_probability: 0.0}\n\
             caps: {replies_per_day: 0, likes_per_day: 0, follows_per_day: 1, follows_per_week: 2, max_actions_per_run: 10}",
        );
        let follows = [
            target("did:plc:a"),
            target("did:plc:b"),
            target("did:plc:c"),
        ];
        let input = PlanInput {
            candidates: &[],
            follows: &follows,
            state: &ActionLog::default(),
            config: &cfg,
            now_unix: NOW,
        };
        let allowed = plan(&input, &mut Rng::from_seed(6));
        assert_eq!(allowed.actions.len(), 1);
        assert_eq!(allowed.actions[0].kind(), ActionKind::Follow);

        // With two already followed this week, the weekly cap blocks more.
        let state = ActionLog::from_entries(vec![
            entry(ActionKind::Follow, "did:plc:x", NOW - 86_400),
            entry(ActionKind::Follow, "did:plc:y", NOW - 86_400 * 2),
        ]);
        let input = PlanInput {
            candidates: &[],
            follows: &follows,
            state: &state,
            config: &cfg,
            now_unix: NOW,
        };
        let blocked = plan(&input, &mut Rng::from_seed(6));
        assert!(blocked.actions.is_empty());
    }

    #[test]
    fn the_same_seed_gives_the_same_plan() {
        let cfg = config("{}");
        let candidates = [candidate("at://1", 30), candidate("at://2", 20)];
        let build = |seed| {
            let input = PlanInput {
                candidates: &candidates,
                follows: &[],
                state: &ActionLog::default(),
                config: &cfg,
                now_unix: NOW,
            };
            plan(&input, &mut Rng::from_seed(seed)).actions.len()
        };
        assert_eq!(build(123), build(123));
    }
}
