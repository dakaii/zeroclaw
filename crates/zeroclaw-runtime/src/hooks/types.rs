use std::time::Instant;

pub use zeroclaw_api::hook::{TurnCompleteSummary, TurnToolCallRecord};

/// Per-turn metadata threaded into `run_tool_call_loop` for lifecycle hooks.
#[derive(Debug, Clone)]
pub struct TurnHookContext {
    pub agent_alias: String,
    pub user_message: String,
    pub channel: String,
    pub loop_started_at: Instant,
}
