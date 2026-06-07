use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use zeroclaw_api::tool::ToolResult;

use super::traits::{HookHandler, HookResult};
use super::types::TurnCompleteSummary;
use zeroclaw_plugins::{
    HOOK_EVENT_BEFORE_PROMPT_BUILD, HOOK_EVENT_ON_AFTER_TOOL_CALL, HOOK_EVENT_ON_TURN_COMPLETE,
    PluginPermission,
};

/// Bridges a WASM plugin's `on_hook` export into the in-process [`HookHandler`] trait.
pub struct WasmHookHandler {
    plugin_name: String,
    wasm_path: PathBuf,
    permissions: Vec<PluginPermission>,
    subscribed: HashSet<String>,
    priority: i32,
}

impl WasmHookHandler {
    pub fn new(
        plugin_name: String,
        wasm_path: PathBuf,
        permissions: Vec<PluginPermission>,
        subscribed: Vec<String>,
        priority: i32,
    ) -> Self {
        Self {
            plugin_name,
            wasm_path,
            permissions,
            subscribed: subscribed.into_iter().collect(),
            priority,
        }
    }

    fn is_subscribed(&self, event: &str) -> bool {
        self.subscribed.contains(event)
    }

    async fn invoke_void(&self, event: &str, payload: Value) {
        if !self.is_subscribed(event) {
            return;
        }
        let wasm_path = self.wasm_path.clone();
        let permissions = self.permissions.clone();
        let plugin_name = self.plugin_name.clone();
        let event = event.to_string();
        let event_for_log = event.clone();

        let result = tokio::task::spawn_blocking(move || {
            zeroclaw_plugins::runtime::call_on_hook_pooled(
                &wasm_path,
                &permissions,
                &event,
                payload,
            )
        })
        .await;

        match result {
            Ok(Ok(Some(response))) if response.action == "cancel" => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_attrs(::serde_json::json!({
                            "plugin": plugin_name,
                            "event": event_for_log,
                            "reason": response.reason,
                        })),
                    "wasm hook returned cancel on void event; ignored"
                );
            }
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({
                            "plugin": plugin_name,
                            "event": event_for_log,
                            "error": format!("{e}"),
                        })),
                    "wasm hook invocation failed"
                );
            }
            Err(e) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({
                            "plugin": plugin_name,
                            "event": event_for_log,
                            "error": format!("{e}"),
                        })),
                    "wasm hook spawn_blocking panicked"
                );
            }
        }
    }

    async fn invoke_modify_string(&self, event: &str, payload: Value, current: String) -> String {
        if !self.is_subscribed(event) {
            return current;
        }
        if event == HOOK_EVENT_BEFORE_PROMPT_BUILD
            && !self.permissions.contains(&PluginPermission::PromptModify)
        {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_attrs(::serde_json::json!({
                        "plugin": self.plugin_name,
                        "event": event,
                    })),
                "wasm hook lacks prompt_modify permission; skipping"
            );
            return current;
        }

        let wasm_path = self.wasm_path.clone();
        let permissions = self.permissions.clone();
        let plugin_name = self.plugin_name.clone();
        let event_owned = event.to_string();
        let event_for_log = event_owned.clone();

        let result = tokio::task::spawn_blocking(move || {
            zeroclaw_plugins::runtime::call_on_hook_pooled(
                &wasm_path,
                &permissions,
                &event_owned,
                payload,
            )
        })
        .await;

        match result {
            Ok(Ok(Some(response))) => match response.action.as_str() {
                "cancel" => current,
                "continue" => {
                    let payload = response.payload;
                    payload
                        .as_ref()
                        .and_then(|p| p.get("prompt").and_then(|v| v.as_str()).map(str::to_string))
                        .or_else(|| {
                            payload
                                .as_ref()
                                .and_then(|p| p.as_str().map(str::to_string))
                        })
                        .unwrap_or(current)
                }
                _ => current,
            },
            Ok(Ok(None)) | Ok(Err(_)) | Err(_) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({
                            "plugin": plugin_name,
                            "event": event_for_log,
                        })),
                    "wasm modifying hook failed; keeping previous value"
                );
                current
            }
        }
    }
}

#[async_trait]
impl HookHandler for WasmHookHandler {
    fn name(&self) -> &str {
        &self.plugin_name
    }

    fn priority(&self) -> i32 {
        self.priority
    }

    async fn on_turn_complete(&self, summary: &TurnCompleteSummary) {
        let payload = match serde_json::to_value(summary) {
            Ok(v) => v,
            Err(e) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_attrs(::serde_json::json!({"error": format!("{e}")})),
                    "failed to serialize turn summary for wasm hook"
                );
                return;
            }
        };
        self.invoke_void(HOOK_EVENT_ON_TURN_COMPLETE, payload).await;
    }

    async fn before_prompt_build(&self, prompt: String) -> HookResult<String> {
        let modified = self
            .invoke_modify_string(
                HOOK_EVENT_BEFORE_PROMPT_BUILD,
                serde_json::json!({ "prompt": prompt }),
                prompt,
            )
            .await;
        HookResult::Continue(modified)
    }

    async fn on_after_tool_call(&self, tool: &str, result: &ToolResult, duration: Duration) {
        let payload = serde_json::json!({
            "tool": tool,
            "success": result.success,
            "duration_ms": duration.as_millis(),
        });
        self.invoke_void(HOOK_EVENT_ON_AFTER_TOOL_CALL, payload)
            .await;
    }
}

#[cfg(all(test, feature = "plugins-wasm"))]
mod integration_tests {
    use super::*;
    use crate::hooks::traits::HookHandler;
    use std::path::PathBuf;

    fn hook_test_wasm_path() -> Option<PathBuf> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../plugins/hook-test/hook_test.wasm");
        if path.exists() { Some(path) } else { None }
    }

    #[tokio::test]
    async fn wasm_hook_handler_invokes_on_turn_complete() {
        let Some(wasm_path) = hook_test_wasm_path() else {
            eprintln!("SKIP: hook_test.wasm not found (build plugins/hook-test first)");
            return;
        };

        let handler = WasmHookHandler::new(
            "hook-test".into(),
            wasm_path,
            vec![],
            vec![HOOK_EVENT_ON_TURN_COMPLETE.to_string()],
            0,
        );

        let summary = TurnCompleteSummary {
            session_id: None,
            channel: Some("cli".into()),
            agent_alias: "default".into(),
            user_message: "hi".into(),
            final_response: "hello".into(),
            tool_calls: vec![],
            turn_duration_ms: 1,
            success: true,
        };

        handler.on_turn_complete(&summary).await;
        handler.on_turn_complete(&summary).await;
    }

    #[tokio::test]
    async fn wasm_hook_handler_modifies_prompt_with_permission() {
        let Some(wasm_path) = hook_test_wasm_path() else {
            return;
        };

        let handler = WasmHookHandler::new(
            "hook-test".into(),
            wasm_path,
            vec![PluginPermission::PromptModify],
            vec![HOOK_EVENT_BEFORE_PROMPT_BUILD.to_string()],
            0,
        );

        match handler.before_prompt_build("base".into()).await {
            HookResult::Continue(prompt) => {
                assert!(prompt.contains("base"));
                assert!(prompt.contains("[hook-test]"));
            }
            HookResult::Cancel(_) => panic!("hook should not cancel"),
        }
    }
}
