pub(super) const KEEP_RECENT_TURNS: usize = 4;

pub(super) fn threshold_reached(estimated_tokens: usize, context_window: usize) -> bool {
    estimated_tokens.saturating_mul(4) >= context_window.saturating_mul(3)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_compaction_starts_at_seventy_five_percent() {
        assert!(!threshold_reached(74_999, 100_000));
        assert!(threshold_reached(75_000, 100_000));
    }
}
