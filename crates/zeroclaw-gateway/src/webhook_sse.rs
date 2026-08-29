//! SSE transport for `POST /webhook` chat turns.
//!
//! Requires both `stream: true` on the JSON body and `Accept: text/event-stream`.
//! Tokens are [`TurnEvent::Chunk`] values from [`Agent::turn_streamed`].
//! Either signal missing keeps the JSON `{ "response" }` path.

use super::{
    AppState, WebhookBody, is_needs_quickstart_err, needs_quickstart_for,
    resolve_gateway_chat_agent_alias,
};
use axum::{
    http::{HeaderMap, StatusCode, header},
    response::{
        IntoResponse, Json, Response,
        sse::{Event, KeepAlive, Sse},
    },
};
use futures_util::Stream;
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;
use zeroclaw_runtime::agent::TurnEvent;
use zeroclaw_runtime::rpc::turn::{TurnAttribution, TurnError, TurnOutcome, execute_turn};

const WEBHOOK_CHANNEL_KEY: &str = "webhook";

/// Dual opt-in: `stream: true` and `Accept` includes `text/event-stream`.
pub(crate) fn request_wants_sse(headers: &HeaderMap, body: &WebhookBody) -> bool {
    body.stream && accept_includes_event_stream(headers)
}

fn accept_includes_event_stream(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value.split(',').any(|part| {
                part.split(';')
                    .next()
                    .map(str::trim)
                    .is_some_and(|mime| mime.eq_ignore_ascii_case("text/event-stream"))
            })
        })
}

pub(crate) async fn stream_webhook_turn(
    state: AppState,
    message: String,
    session_id: Option<String>,
    agent_override: Option<String>,
) -> Response {
    if needs_quickstart_for(&state.model).is_some() {
        return needs_quickstart_response();
    }

    let mut config = state.config.read().clone();
    // Gateway HTTP owns webhook autosave; disable Agent auto_save so the
    // constructor does not also write `user_msg`.
    config.memory.auto_save = false;

    let agent_alias = match resolve_gateway_chat_agent_alias(&config, agent_override.as_deref()) {
        Some(alias) => alias,
        None => {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure),
                "webhook chat rejected: no configured [agents.<alias>] entry"
            );
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({"error": "LLM request failed"}),
            );
        }
    };

    let mut agent =
        match zeroclaw_runtime::agent::Agent::from_config_with_session_cwd_and_mcp_backchannel(
            &config,
            &agent_alias,
            None,
            true,
            false,
            false,
            state.sop_engine.clone(),
            state.sop_audit.clone(),
            Some(state.canvas_store.clone()),
        )
        .await
        {
            Ok(agent) => agent,
            Err(error) => {
                let sanitized = zeroclaw_providers::sanitize_api_error(&error.to_string());
                ::zeroclaw_log::record!(
                    ERROR,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"error": sanitized})),
                    "webhook SSE agent initialization failed"
                );
                if is_needs_quickstart_err(&error) {
                    return needs_quickstart_response();
                }
                return json_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    serde_json::json!({"error": "LLM request failed"}),
                );
            }
        };

    agent.set_channel_name(WEBHOOK_CHANNEL_KEY.to_string());
    agent.set_memory_session_id(session_id.clone());

    let (turn_alias, turn_provider, turn_model) = agent.attribution_fields();
    let cost_tracking_context = state.cost_tracker.as_ref().map(|tracker| {
        let pricing = zeroclaw_runtime::agent::cost::build_model_provider_pricing(&config);
        zeroclaw_runtime::agent::cost::ToolLoopCostTrackingContext::new(
            tracker.clone(),
            Arc::new(pricing),
        )
        .with_agent_alias(&turn_alias)
    });
    let turn_usage = state.cost_tracker.as_ref().map(|_| {
        Arc::new(parking_lot::Mutex::new(
            zeroclaw_runtime::agent::cost::TurnUsage::default(),
        ))
    });

    let cancel = CancellationToken::new();
    let (sse_tx, sse_rx) = mpsc::channel::<Event>(64);
    let agent = Arc::new(Mutex::new(agent));
    let cancel_for_turn = cancel.clone();
    let sse_tx_for_turn = sse_tx.clone();
    let prompt = message;
    let session_key = session_id.clone();

    zeroclaw_spawn::spawn!(async move {
        let accumulated = Arc::new(Mutex::new(String::new()));
        let sse_tx_for_events = sse_tx_for_turn.clone();
        let cancel_for_events = cancel_for_turn.clone();
        let result = zeroclaw_runtime::agent::cost::TOOL_LOOP_TURN_USAGE
            .scope(
                turn_usage,
                execute_turn(
                    agent,
                    prompt,
                    cancel_for_turn.clone(),
                    TurnAttribution {
                        session_key,
                        agent_alias: turn_alias,
                        model_provider: turn_provider,
                        model: turn_model,
                        channel: WEBHOOK_CHANNEL_KEY,
                    },
                    cost_tracking_context,
                    move |event| {
                        let accumulated = Arc::clone(&accumulated);
                        let sse_tx = sse_tx_for_events.clone();
                        let cancel_for_events = cancel_for_events.clone();
                        async move {
                            if let TurnEvent::Chunk { delta } = event
                                && !delta.is_empty()
                            {
                                let mut text = accumulated.lock().await;
                                text.push_str(&delta);
                                if sse_tx.send(token_event(&text)).await.is_err() {
                                    cancel_for_events.cancel();
                                }
                            }
                        }
                    },
                ),
            )
            .await;

        match result {
            Ok(TurnOutcome::Completed { .. }) => {
                let _ = sse_tx_for_turn.send(done_event()).await;
            }
            Ok(TurnOutcome::Cancelled { .. }) => {}
            Err(error) => {
                let _ = sse_tx_for_turn
                    .send(error_event(&turn_error_message(&error)))
                    .await;
            }
        }
    });

    // Keep-alives probe disconnect so a hung model does not hold the turn
    // until the next token.
    Sse::new(WebhookSseStream {
        rx: sse_rx,
        _cancel_on_drop: CancelOnDrop(cancel),
    })
    .keep_alive(KeepAlive::new().interval(Duration::from_millis(500)))
    .into_response()
}

fn token_event(cumulative_text: &str) -> Event {
    Event::default()
        .event("token")
        .data(serde_json::json!({ "text": cumulative_text }).to_string())
}

fn done_event() -> Event {
    Event::default().event("done").data("{}")
}

fn error_event(message: &str) -> Event {
    Event::default()
        .event("error")
        .data(serde_json::json!({ "message": message }).to_string())
}

fn turn_error_message(error: &TurnError) -> String {
    if let Some(user_message) = error.user_message() {
        return user_message.to_string();
    }
    let sanitized = zeroclaw_providers::sanitize_api_error(&error.to_string());
    if sanitized.trim().is_empty() {
        "LLM request failed".to_string()
    } else {
        sanitized
    }
}

fn json_error(status: StatusCode, body: serde_json::Value) -> Response {
    (status, Json(body)).into_response()
}

fn needs_quickstart_response() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({
            "error": "needs_quickstart",
            "url": "/quickstart"
        })),
    )
        .into_response()
}

struct CancelOnDrop(CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

struct WebhookSseStream {
    rx: mpsc::Receiver<Event>,
    _cancel_on_drop: CancelOnDrop,
}

impl Stream for WebhookSseStream {
    type Item = Result<Event, Infallible>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        this.rx.poll_recv(cx).map(|item| item.map(Ok))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{WebhookQuery, handle_webhook};
    use axum::extract::{ConnectInfo, Query, State};
    use http_body_util::BodyExt;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;
    use zeroclaw_config::schema::Config;

    fn test_connect_info() -> ConnectInfo<SocketAddr> {
        ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 30_301)))
    }

    fn sse_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, "text/event-stream".parse().unwrap());
        headers
    }

    fn json_accept_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, "application/json".parse().unwrap());
        headers
    }

    async fn collect_body(response: Response) -> (StatusCode, HeaderMap, String) {
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            headers,
            String::from_utf8_lossy(&bytes).into_owned(),
        )
    }

    fn anthropic_sse_body(first: &str, second: &str) -> String {
        format!(
            "event: message_start\n\
data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_test\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-test\",\"usage\":{{\"input_tokens\":1}}}}}}\n\n\
event: content_block_start\n\
data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n\
event: content_block_delta\n\
data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{first}\"}}}}\n\n\
event: content_block_delta\n\
data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{second}\"}}}}\n\n\
event: content_block_stop\n\
data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n\
event: message_delta\n\
data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"output_tokens\":2}}}}\n\n\
event: message_stop\n\
data: {{\"type\":\"message_stop\"}}\n\n"
        )
    }

    fn streaming_agent_config(tmp: &tempfile::TempDir, mock_addr: SocketAddr) -> Config {
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("gateway workspace");
        let mut config = Config {
            data_dir: workspace.clone(),
            config_path: tmp.path().join("config.toml"),
            ..Default::default()
        };
        config.memory.backend = "none".to_string();
        config.memory.auto_save = true;
        config.reliability.provider_retries = 0;
        config.providers.models.anthropic.insert(
            "fixture".to_string(),
            zeroclaw_config::schema::AnthropicModelProviderConfig {
                base: zeroclaw_config::schema::ModelProviderConfig {
                    api_key: Some("test-key".to_string()),
                    uri: Some(format!("http://{mock_addr}")),
                    model: Some("claude-test".to_string()),
                    ..Default::default()
                },
            },
        );
        config.risk_profiles.insert(
            "fixture".to_string(),
            zeroclaw_config::schema::RiskProfileConfig::default(),
        );
        config.runtime_profiles.insert(
            "fixture".to_string(),
            zeroclaw_config::schema::RuntimeProfileConfig::default(),
        );
        config.agents.insert(
            "web".to_string(),
            zeroclaw_config::schema::AliasedAgentConfig {
                model_provider: "anthropic.fixture".into(),
                risk_profile: "fixture".into(),
                runtime_profile: "fixture".into(),
                workspace: zeroclaw_config::multi_agent::AgentWorkspaceConfig {
                    path: Some(workspace),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        config
    }

    #[test]
    fn request_wants_sse_requires_both_stream_flag_and_accept() {
        let mut sse = HeaderMap::new();
        sse.insert(header::ACCEPT, "text/event-stream".parse().unwrap());
        let mut json = HeaderMap::new();
        json.insert(header::ACCEPT, "application/json".parse().unwrap());
        let mixed = {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::ACCEPT,
                "application/json, text/event-stream".parse().unwrap(),
            );
            headers
        };

        assert!(request_wants_sse(
            &sse,
            &WebhookBody {
                message: "hi".into(),
                stream: true,
            }
        ));
        assert!(request_wants_sse(
            &mixed,
            &WebhookBody {
                message: "hi".into(),
                stream: true,
            }
        ));
        assert!(!request_wants_sse(
            &sse,
            &WebhookBody {
                message: "hi".into(),
                stream: false,
            }
        ));
        assert!(!request_wants_sse(
            &json,
            &WebhookBody {
                message: "hi".into(),
                stream: true,
            }
        ));
        assert!(!request_wants_sse(
            &HeaderMap::new(),
            &WebhookBody {
                message: "hi".into(),
                stream: true,
            }
        ));
    }

    #[tokio::test]
    async fn stream_false_with_sse_accept_stays_json() {
        let state = crate::api::tests::test_state(Config::default());
        let response = handle_webhook(
            State(state),
            test_connect_info(),
            Query(WebhookQuery::default()),
            sse_headers(),
            Ok(Json(WebhookBody {
                message: "hello".into(),
                stream: false,
            })),
        )
        .await;
        let (status, headers, body) = collect_body(response).await;
        assert_eq!(status, StatusCode::OK);
        let content_type = headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        assert!(
            content_type.contains("application/json"),
            "non-stream path must stay JSON, got {content_type}"
        );
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["response"], "ok");
        assert!(!body.contains("event: token"));
    }

    #[tokio::test]
    async fn stream_true_without_sse_accept_stays_json() {
        let state = crate::api::tests::test_state(Config::default());
        let response = handle_webhook(
            State(state),
            test_connect_info(),
            Query(WebhookQuery::default()),
            json_accept_headers(),
            Ok(Json(WebhookBody {
                message: "hello".into(),
                stream: true,
            })),
        )
        .await;
        let (status, headers, body) = collect_body(response).await;
        assert_eq!(status, StatusCode::OK);
        let content_type = headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        assert!(
            content_type.contains("application/json"),
            "missing SSE Accept must stay JSON, got {content_type}"
        );
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["response"], "ok");
    }

    #[tokio::test]
    async fn webhook_sse_streams_cumulative_tokens_then_done() {
        use axum::{Router, routing::post};
        use tokio::sync::oneshot;

        let captured_tools = Arc::new(std::sync::Mutex::new(None::<serde_json::Value>));
        let captured_tools_for_mock = Arc::clone(&captured_tools);
        let (ready_tx, ready_rx) = oneshot::channel::<()>();
        let mock_app = Router::new().route(
            "/v1/messages",
            post(move |Json(request): Json<serde_json::Value>| {
                let captured_tools = Arc::clone(&captured_tools_for_mock);
                async move {
                    *captured_tools.lock().expect("tools capture") = request.get("tools").cloned();
                    (
                        [(header::CONTENT_TYPE, "text/event-stream")],
                        anthropic_sse_body("Hel", "lo"),
                    )
                        .into_response()
                }
            }),
        );
        let mock_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock provider");
        let mock_addr = mock_listener.local_addr().expect("mock address");
        let mock_server = zeroclaw_spawn::spawn!(async move {
            let _ = ready_tx.send(());
            axum::serve(mock_listener, mock_app)
                .await
                .expect("mock provider serves");
        });
        let _ = ready_rx.await;

        let tmp = tempfile::tempdir().expect("temporary workspace");
        let mut state = crate::api::tests::test_state(streaming_agent_config(&tmp, mock_addr));
        state.auto_save = true;
        state.model = "claude-test".into();

        let mut headers = sse_headers();
        headers.insert("X-Session-Id", "sess-stream-1".parse().unwrap());
        let response = handle_webhook(
            State(state),
            test_connect_info(),
            Query(WebhookQuery {
                agent: Some("web".into()),
            }),
            headers,
            Ok(Json(WebhookBody {
                message: "hello".into(),
                stream: true,
            })),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        assert!(
            content_type.contains("text/event-stream"),
            "streaming Accept must return SSE, got {content_type}"
        );

        let body = collect_body(response).await.2;
        assert!(
            body.contains("event: token"),
            "expected token frames, got {body}"
        );
        assert!(
            body.contains(r#"{"text":"Hel"}"#) || body.contains(r#"{"text":"Hello"}"#),
            "expected cumulative token payload, got {body}"
        );
        assert!(
            body.contains(r#"{"text":"Hello"}"#),
            "second frame must be cumulative, got {body}"
        );
        assert!(
            body.contains("event: done"),
            "stream must finish with done, got {body}"
        );
        assert!(
            captured_tools
                .lock()
                .expect("tools capture")
                .as_ref()
                .is_some_and(|tools| tools.as_array().is_some_and(|items| !items.is_empty())),
            "streaming path must send tool specs to the provider like the JSON webhook turn"
        );

        drop(mock_server);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn webhook_sse_disconnect_cancels_provider_stream() {
        use axum::body::{Body, Bytes};
        use axum::{Router, routing::post};
        use futures_util::stream::unfold;
        use tokio::sync::oneshot;

        let cancelled = Arc::new(AtomicBool::new(false));
        let cancelled_for_mock = Arc::clone(&cancelled);
        let chunks_sent = Arc::new(AtomicUsize::new(0));
        let chunks_sent_for_mock = Arc::clone(&chunks_sent);
        let (ready_tx, ready_rx) = oneshot::channel::<()>();

        let mock_app = Router::new().route(
            "/v1/messages",
            post(move || {
                let cancelled = Arc::clone(&cancelled_for_mock);
                let chunks_sent = Arc::clone(&chunks_sent_for_mock);
                async move {
                    struct DropFlag(Arc<AtomicBool>);
                    impl Drop for DropFlag {
                        fn drop(&mut self) {
                            self.0.store(true, Ordering::SeqCst);
                        }
                    }
                    let first = anthropic_partial_sse();
                    // Idle `pending()` bodies do not drop on client abort;
                    // keep writing so Hyper notices the disconnect.
                    let stream = unfold(
                        (Some(first), DropFlag(cancelled), chunks_sent),
                        |(first, flag, chunks_sent)| async move {
                            if let Some(payload) = first {
                                chunks_sent.fetch_add(1, Ordering::SeqCst);
                                Some((
                                    Ok::<Bytes, std::io::Error>(Bytes::from(payload)),
                                    (None, flag, chunks_sent),
                                ))
                            } else {
                                tokio::time::sleep(Duration::from_millis(50)).await;
                                Some((
                                    Ok(Bytes::from_static(b": keepalive\n\n")),
                                    (None, flag, chunks_sent),
                                ))
                            }
                        },
                    );
                    (
                        [(header::CONTENT_TYPE, "text/event-stream")],
                        Body::from_stream(stream),
                    )
                        .into_response()
                }
            }),
        );
        let mock_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock provider");
        let mock_addr = mock_listener.local_addr().expect("mock address");
        let mock_server = zeroclaw_spawn::spawn!(async move {
            let _ = ready_tx.send(());
            axum::serve(mock_listener, mock_app)
                .await
                .expect("mock provider serves");
        });
        let _ = ready_rx.await;

        let tmp = tempfile::tempdir().expect("temporary workspace");
        let mut state = crate::api::tests::test_state(streaming_agent_config(&tmp, mock_addr));
        state.model = "claude-test".into();

        let response = handle_webhook(
            State(state),
            test_connect_info(),
            Query(WebhookQuery {
                agent: Some("web".into()),
            }),
            sse_headers(),
            Ok(Json(WebhookBody {
                message: "hello".into(),
                stream: true,
            })),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        let mut body = response.into_body();
        let mut acc = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "timed out waiting for SSE token; got {}",
                String::from_utf8_lossy(&acc)
            );
            match tokio::time::timeout(remaining, BodyExt::frame(&mut body)).await {
                Ok(Some(Ok(frame))) => {
                    if let Some(data) = frame.data_ref() {
                        acc.extend_from_slice(data);
                    }
                    if String::from_utf8_lossy(&acc).contains("event: token") {
                        break;
                    }
                }
                Ok(Some(Err(error))) => panic!("webhook stream read failed: {error}"),
                Ok(None) => panic!(
                    "SSE ended before a token frame; got {}",
                    String::from_utf8_lossy(&acc)
                ),
                Err(_) => panic!(
                    "timed out waiting for SSE token; got {}",
                    String::from_utf8_lossy(&acc)
                ),
            }
        }
        let streamed = String::from_utf8_lossy(&acc);
        assert!(
            streamed.contains("event: token"),
            "expected a live token before abort, got {streamed}"
        );

        drop(body);

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while !cancelled.load(Ordering::SeqCst) && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            cancelled.load(Ordering::SeqCst),
            "dropping the SSE body must cancel the in-flight provider stream; chunks_sent={}",
            chunks_sent.load(Ordering::SeqCst)
        );

        drop(mock_server);
    }

    fn anthropic_partial_sse() -> String {
        "event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_test\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-test\",\"usage\":{\"input_tokens\":1}}}\n\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hel\"}}\n\n"
            .to_string()
    }
}
