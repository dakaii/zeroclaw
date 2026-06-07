use std::time::Instant;

use serde::{Deserialize, Serialize};

/// Summary of a completed agent turn, passed to `on_turn_complete` hooks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TurnCompleteSummary {
    pub session_id: Option<String>,
    pub channel: Option<String>,
    pub agent_alias: String,
    pub user_message: String,
    pub final_response: String,
    pub tool_calls: Vec<TurnToolCallRecord>,
    pub turn_duration_ms: u64,
    pub success: bool,
}

/// Minimal tool-call record for hook payloads (no raw arguments — may contain secrets).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TurnToolCallRecord {
    pub name: String,
    pub success: bool,
    pub duration_ms: u64,
}

/// Per-turn metadata threaded into `run_tool_call_loop` for lifecycle hooks.
#[derive(Debug, Clone)]
pub struct TurnHookContext {
    pub agent_alias: String,
    pub user_message: String,
    pub channel: String,
    pub loop_started_at: Instant,
}
