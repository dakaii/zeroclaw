//! Minimal ZeroClaw WASM hook plugin for integration tests.
//!
//! **Exports:**
//! - `on_hook(json) -> json` — handles lifecycle events and returns
//!   `{"action":"continue","payload":...}` envelopes.

use extism_pdk::*;
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Deserialize)]
struct HookInvokeRequest {
    event: String,
    payload: serde_json::Value,
}

#[derive(Serialize)]
struct HookInvokeResponse {
    action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    payload: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

static mut TURN_COUNT: u32 = 0;

#[plugin_fn]
pub fn on_hook(input: String) -> FnResult<String> {
    let req: HookInvokeRequest = serde_json::from_str(&input)
        .map_err(|e| Error::msg(format!("invalid hook request JSON: {e}")))?;

    let response = match req.event.as_str() {
        "on_turn_complete" => {
            // SAFETY: Extism serializes plugin calls; tests run single-threaded per instance.
            let count = unsafe {
                TURN_COUNT = TURN_COUNT.saturating_add(1);
                TURN_COUNT
            };
            HookInvokeResponse {
                action: "continue".into(),
                payload: Some(json!({ "turn_count": count })),
                reason: None,
            }
        }
        "before_prompt_build" => {
            let prompt = req
                .payload
                .get("prompt")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            HookInvokeResponse {
                action: "continue".into(),
                payload: Some(json!({
                    "prompt": format!("{prompt}\n[hook-test]")
                })),
                reason: None,
            }
        }
        _ => HookInvokeResponse {
            action: "continue".into(),
            payload: None,
            reason: None,
        },
    };

    Ok(serde_json::to_string(&response)?)
}
