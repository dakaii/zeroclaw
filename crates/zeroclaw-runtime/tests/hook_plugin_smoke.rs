//! End-to-end smoke test: WASM hook plugin mutates the system prompt before LLM call.
//!
//! Requires `plugins/hook-test/hook_test.wasm` (build with
//! `cd plugins/hook-test && cargo build --target wasm32-wasip1 --release &&
//! cp target/wasm32-wasip1/release/hook_test.wasm .`).

#![cfg(feature = "plugins-wasm")]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use axum::{Router, extract::State, routing::post};
use tempfile::TempDir;
use tokio::sync::Mutex as AsyncMutex;
use zeroclaw_config::schema::{AliasedAgentConfig, Config, HooksConfig, PluginsConfig, RiskProfileConfig};

const HOOK_MARKER: &str = "[hook-test]";
const FAKE_OPENAI_RESPONSE: &str = r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"pong"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#;

type CapturedBodies = Arc<AsyncMutex<Vec<String>>>;

async fn handle_chat(State(captured): State<CapturedBodies>, body: String) -> &'static str {
    captured.lock().await.push(body);
    FAKE_OPENAI_RESPONSE
}

async fn spawn_mock_provider() -> (SocketAddr, CapturedBodies) {
    let captured: CapturedBodies = Arc::new(AsyncMutex::new(Vec::new()));
    let app = Router::new()
        .route("/chat/completions", post(handle_chat))
        .with_state(captured.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    zeroclaw_spawn::spawn!(async move {
        let _ = axum::serve(listener, app.into_make_service()).await;
    });
    (addr, captured)
}

fn install_hook_test_plugin(workspace_dir: &Path) -> bool {
    let fixture_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/hook-test");
    let wasm_src = fixture_dir.join("hook_test.wasm");
    if !wasm_src.exists() {
        eprintln!(
            "SKIP: {} not found — build plugins/hook-test first",
            wasm_src.display()
        );
        return false;
    }

    let plugin_dir = workspace_dir.join("plugins").join("hook-test");
    std::fs::create_dir_all(&plugin_dir).expect("plugin dir");
    std::fs::copy(&wasm_src, plugin_dir.join("hook_test.wasm")).expect("copy wasm");
    std::fs::copy(
        fixture_dir.join("manifest.toml"),
        plugin_dir.join("manifest.toml"),
    )
    .expect("copy manifest");
    true
}

#[tokio::test]
async fn wasm_hook_plugin_mutates_system_prompt_in_agent_run() {
    let fixture_wasm = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../plugins/hook-test/hook_test.wasm");
    if !fixture_wasm.exists() {
        eprintln!("SKIP: hook_test.wasm not built");
        return;
    }

    let tmp = TempDir::new().unwrap();
    let workspace_dir = tmp.path().join("workspace");
    tokio::fs::create_dir_all(&workspace_dir).await.unwrap();
    assert!(install_hook_test_plugin(&workspace_dir));

    let (addr, captured) = spawn_mock_provider().await;
    let provider_uri = format!("http://{addr}");
    let provider_type = "custom";

    let mut providers = zeroclaw_config::providers::Providers::default();
    {
        let base = providers
            .models
            .ensure(provider_type, "default")
            .expect("custom provider slot");
        base.api_key = Some("test-key".to_string());
        base.model = Some("test-model".to_string());
        base.uri = Some(provider_uri);
    }

    let mut agents = HashMap::new();
    agents.insert(
        "default".to_string(),
        AliasedAgentConfig {
            enabled: true,
            model_provider: format!("{provider_type}.default").into(),
            risk_profile: "default".to_string(),
            ..Default::default()
        },
    );

    let mut risk_profiles = HashMap::new();
    risk_profiles.insert("default".to_string(), RiskProfileConfig::default());

    let plugins_dir = workspace_dir.join("plugins");
    let mut config = Config {
        data_dir: workspace_dir.clone(),
        config_path: tmp.path().join("config.toml"),
        providers,
        agents,
        risk_profiles,
        hooks: HooksConfig {
            enabled: true,
            ..HooksConfig::default()
        },
        plugins: PluginsConfig {
            enabled: true,
            plugins_dir: plugins_dir.to_string_lossy().into_owned(),
            auto_discover: true,
            ..PluginsConfig::default()
        },
        ..Config::default()
    };
    config.reliability.scheduler_retries = 0;
    config.reliability.provider_retries = 0;

    let run_result = zeroclaw_runtime::agent::run(
        config,
        "default",
        Some("ping".to_string()),
        None,
        None,
        Some(0.0),
        vec![],
        false,
        None,
        None,
        zeroclaw_runtime::agent::loop_::AgentRunOverrides::default(),
    )
    .await;

    let output = run_result.expect("agent run should succeed with mock provider");
    assert!(
        output.contains("pong"),
        "expected mock assistant reply in output, got: {output}"
    );

    let bodies = captured.lock().await;
    assert!(
        !bodies.is_empty(),
        "mock provider received zero requests — hook wiring never reached LLM"
    );

    let joined = bodies.join("\n---\n");
    assert!(
        joined.contains(HOOK_MARKER),
        "WASM before_prompt_build hook should append {HOOK_MARKER:?} to system prompt; bodies:\n{joined}"
    );
}
