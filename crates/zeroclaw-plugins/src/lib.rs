//! WASM plugin system for ZeroClaw.
//!
//! Plugins are WebAssembly modules loaded via Extism that can extend
//! ZeroClaw with custom tools and channels. Enable with `--features plugins-wasm`.

pub mod error;
pub mod host;
pub mod runtime;
pub mod signature;
pub mod wasm_channel;
pub mod wasm_tool;

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

fn default_hook_priority() -> i32 {
    -100
}

/// A plugin's declared manifest (loaded from manifest.toml alongside the .wasm).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    /// Plugin name (unique identifier)
    pub name: String,
    /// Plugin version
    pub version: String,
    /// Human-readable description
    pub description: Option<String>,
    /// Author name or organization
    pub author: Option<String>,
    /// Path to the .wasm file (relative to manifest).
    /// Required for tool/channel/memory/observer plugins; optional (and ignored)
    /// for skill-only plugins, which carry no WASM payload.
    #[serde(default)]
    pub wasm_path: Option<String>,
    /// Capabilities this plugin provides
    pub capabilities: Vec<PluginCapability>,
    /// Permissions this plugin requests
    #[serde(default)]
    pub permissions: Vec<PluginPermission>,
    /// Lifecycle hook events this plugin subscribes to (requires `Hook` capability).
    #[serde(default)]
    pub hooks: Vec<String>,
    /// Priority for modifying hooks (lower runs later). Default: -100.
    #[serde(default = "default_hook_priority")]
    pub hook_priority: i32,
    /// Ed25519 signature over the canonical manifest (base64url-encoded).
    /// Set by the plugin publisher when signing the manifest.
    #[serde(default)]
    pub signature: Option<String>,
    /// Hex-encoded Ed25519 public key of the publisher who signed this manifest.
    #[serde(default)]
    pub publisher_key: Option<String>,
}

/// What a plugin can do.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PluginCapability {
    /// Provides one or more tools
    Tool,
    /// Provides a channel implementation
    Channel,
    /// Provides a memory backend
    Memory,
    /// Provides an observer/metrics backend
    Observer,
    /// Provides one or more agentskills.io-format skills under `skills/`
    Skill,
    /// Subscribes to agent lifecycle hooks via the `on_hook` WASM export
    Hook,
}

/// Permissions a plugin may request.
#[derive(Debug, Clone, Hash, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PluginPermission {
    /// Can make HTTP requests
    HttpClient,
    /// Can read from the filesystem (within sandbox)
    FileRead,
    /// Can write to the filesystem (within sandbox)
    FileWrite,
    /// Can access environment variables
    EnvRead,
    /// Can read agent memory
    MemoryRead,
    /// Can write agent memory
    MemoryWrite,
    /// Can modify prompts/messages via modifying lifecycle hooks
    PromptModify,
}

/// Lifecycle hook event names a plugin may subscribe to in `manifest.toml`.
pub const HOOK_EVENT_ON_TURN_COMPLETE: &str = "on_turn_complete";
pub const HOOK_EVENT_BEFORE_PROMPT_BUILD: &str = "before_prompt_build";
pub const HOOK_EVENT_ON_AFTER_TOOL_CALL: &str = "on_after_tool_call";

/// All hook event names accepted in plugin manifests.
pub const ALLOWED_HOOK_EVENTS: &[&str] = &[
    HOOK_EVENT_ON_TURN_COMPLETE,
    HOOK_EVENT_BEFORE_PROMPT_BUILD,
    HOOK_EVENT_ON_AFTER_TOOL_CALL,
];

/// Returns an error message when `event` is not a supported hook subscription.
pub fn validate_hook_event(event: &str) -> Result<(), String> {
    if ALLOWED_HOOK_EVENTS.contains(&event) {
        Ok(())
    } else {
        Err(format!(
            "unknown hook event '{event}'; allowed: {}",
            ALLOWED_HOOK_EVENTS.join(", ")
        ))
    }
}

/// Information about a loaded plugin.
#[derive(Debug, Clone, Serialize)]
pub struct PluginInfo {
    pub name: String,
    pub version: String,
    pub description: Option<String>,
    pub capabilities: Vec<PluginCapability>,
    pub permissions: Vec<PluginPermission>,
    /// Resolved path to the WASM file. `None` for skill-only plugins.
    pub wasm_path: Option<PathBuf>,
    pub loaded: bool,
}
