mod build;
pub mod builtin;
mod runner;
mod traits;
pub mod types;
#[cfg(feature = "plugins-wasm")]
mod wasm_hook;

pub use build::build_hook_runner;
pub use runner::HookRunner;
pub use types::{TurnCompleteSummary, TurnHookContext, TurnToolCallRecord};

/// Run `before_prompt_build` hooks when a runner is configured.
pub async fn apply_before_prompt_build(
    hooks: Option<&HookRunner>,
    prompt: String,
) -> Result<String, String> {
    let Some(hooks) = hooks else {
        return Ok(prompt);
    };
    match hooks.run_before_prompt_build(prompt).await {
        HookResult::Continue(p) => Ok(p),
        HookResult::Cancel(reason) => Err(reason),
    }
}

tokio::task_local! {
    /// Optional per-turn context for lifecycle hooks inside `run_tool_call_loop`.
    pub static TURN_HOOK_CONTEXT: Option<TurnHookContext>;
}

/// Fire `on_turn_complete` when a turn context is scoped and hooks are available.
pub async fn fire_turn_complete(
    hooks: Option<&HookRunner>,
    tool_calls: &[TurnToolCallRecord],
    final_response: &str,
    success: bool,
) {
    let Some(hooks) = hooks else {
        return;
    };
    let Ok(Some(ctx)) = TURN_HOOK_CONTEXT.try_with(|c| c.clone()) else {
        return;
    };

    let summary = TurnCompleteSummary {
        session_id: None,
        channel: Some(ctx.channel),
        agent_alias: ctx.agent_alias,
        user_message: ctx.user_message,
        final_response: final_response.to_string(),
        tool_calls: tool_calls.to_vec(),
        turn_duration_ms: u64::try_from(ctx.loop_started_at.elapsed().as_millis())
            .unwrap_or(u64::MAX),
        success,
    };
    hooks.fire_on_turn_complete(&summary).await;
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use crate::hooks::traits::HookHandler;
    use async_trait::async_trait;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct TurnCompleteCounter {
        count: Arc<AtomicU32>,
    }

    #[async_trait]
    impl HookHandler for TurnCompleteCounter {
        fn name(&self) -> &str {
            "turn-counter"
        }

        async fn on_turn_complete(&self, summary: &TurnCompleteSummary) {
            if summary.success {
                self.count.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    #[tokio::test]
    async fn fire_turn_complete_noops_without_scoped_context() {
        let mut runner = HookRunner::new();
        let count = Arc::new(AtomicU32::new(0));
        runner.register(Box::new(TurnCompleteCounter {
            count: count.clone(),
        }));
        fire_turn_complete(Some(&runner), &[], "done", true).await;
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn fire_turn_complete_fires_when_context_scoped() {
        let mut runner = HookRunner::new();
        let count = Arc::new(AtomicU32::new(0));
        runner.register(Box::new(TurnCompleteCounter {
            count: count.clone(),
        }));
        let ctx = Some(TurnHookContext {
            agent_alias: "default".to_string(),
            user_message: "hello".to_string(),
            channel: "cli".to_string(),
            loop_started_at: std::time::Instant::now(),
        });
        TURN_HOOK_CONTEXT
            .scope(ctx, fire_turn_complete(Some(&runner), &[], "done", true))
            .await;
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}

// HookHandler and HookResult are part of the crate's public hook API surface.
// They may appear unused internally but are intentionally re-exported for
// external integrations and future plugin authors.
#[allow(unused_imports)]
pub use traits::{HookHandler, HookResult};
