use crate::llm::{CostRates, Usage};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TurnDiagnostics {
    pub elapsed_ms: u64,
    pub usage: Usage,
    pub usage_estimated: bool,
    pub tool_calls: u64,
    pub successful_tool_calls: u64,
    pub failed_tool_calls: u64,
    pub tool_duration_ms: u64,
    pub estimated_cost_usd: Option<f64>,
}

impl TurnDiagnostics {
    pub fn add_usage(&mut self, usage: Usage, estimated: bool, rates: CostRates) {
        self.usage.add_assign(usage.normalized());
        self.usage_estimated |= estimated;
        self.estimated_cost_usd = rates.estimate(self.usage);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentDiagnostics {
    pub turns: u64,
    pub successful_turns: u64,
    pub failed_turns: u64,
    pub tool_calls: u64,
    pub successful_tool_calls: u64,
    pub failed_tool_calls: u64,
    pub total_duration_ms: u64,
    pub tool_duration_ms: u64,
    pub usage: Usage,
    pub usage_estimated: bool,
    pub estimated_cost_usd: Option<f64>,
    pub last_error: Option<String>,
}

impl AgentDiagnostics {
    pub fn record_turn(&mut self, turn: &TurnDiagnostics) {
        self.turns = self.turns.saturating_add(1);
        self.successful_turns = self.successful_turns.saturating_add(1);
        self.tool_calls = self.tool_calls.saturating_add(turn.tool_calls);
        self.successful_tool_calls = self
            .successful_tool_calls
            .saturating_add(turn.successful_tool_calls);
        self.failed_tool_calls = self.failed_tool_calls.saturating_add(turn.failed_tool_calls);
        self.total_duration_ms = self.total_duration_ms.saturating_add(turn.elapsed_ms);
        self.tool_duration_ms = self.tool_duration_ms.saturating_add(turn.tool_duration_ms);
        self.usage.add_assign(turn.usage);
        self.usage_estimated |= turn.usage_estimated;
        self.estimated_cost_usd = match (self.estimated_cost_usd, turn.estimated_cost_usd) {
            (Some(total), Some(current)) => Some(total + current),
            (Some(total), None) => Some(total),
            (None, Some(current)) => Some(current),
            (None, None) => None,
        };
        self.last_error = None;
    }

    pub fn record_failure(&mut self, elapsed_ms: u64, error: impl Into<String>) {
        self.turns = self.turns.saturating_add(1);
        self.failed_turns = self.failed_turns.saturating_add(1);
        self.total_duration_ms = self.total_duration_ms.saturating_add(elapsed_ms);
        self.last_error = Some(error.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulates_turn_usage_and_tool_metrics() {
        let mut total = AgentDiagnostics::default();
        let mut turn = TurnDiagnostics {
            elapsed_ms: 120,
            tool_calls: 2,
            successful_tool_calls: 1,
            failed_tool_calls: 1,
            tool_duration_ms: 40,
            ..TurnDiagnostics::default()
        };
        turn.add_usage(
            Usage {
                input_tokens: 1_000,
                output_tokens: 500,
                total_tokens: 1_500,
                ..Usage::default()
            },
            false,
            CostRates {
                input_per_million_usd: Some(1.0),
                output_per_million_usd: Some(2.0),
            },
        );

        total.record_turn(&turn);
        assert_eq!(total.turns, 1);
        assert_eq!(total.tool_calls, 2);
        assert_eq!(total.usage.total_tokens, 1_500);
        assert_eq!(total.estimated_cost_usd, Some(0.002));
    }
}
