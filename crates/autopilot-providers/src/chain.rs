//! Fallback-chain resolution based on live probe results.

use crate::probe::ProbeOutcome;

/// Return the available providers ordered by ascending priority.
pub fn resolve_chain(outcomes: &[ProbeOutcome]) -> Vec<&ProbeOutcome> {
    let mut available: Vec<&ProbeOutcome> = outcomes.iter().filter(|o| o.available).collect();
    available.sort_by_key(|o| o.priority);
    available
}

#[cfg(test)]
mod tests {
    use super::resolve_chain;
    use crate::config::ProviderKind;
    use crate::probe::ProbeOutcome;

    fn outcome(id: &str, priority: u32, available: bool) -> ProbeOutcome {
        ProbeOutcome {
            id: id.into(),
            kind: ProviderKind::Ollama,
            model: "test".into(),
            priority,
            available,
            latency_ms: 1,
            detail: String::new(),
        }
    }

    #[test]
    fn chain_keeps_only_available_sorted_by_priority() {
        let outcomes = vec![
            outcome("ollama", 300, true),
            outcome("gemini", 200, false),
            outcome("opencode", 100, true),
        ];
        let chain = resolve_chain(&outcomes);
        let ids: Vec<&str> = chain.iter().map(|o| o.id.as_str()).collect();
        assert_eq!(ids, vec!["opencode", "ollama"]);
    }
}
