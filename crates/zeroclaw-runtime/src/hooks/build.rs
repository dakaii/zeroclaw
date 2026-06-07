use std::path::{Path, PathBuf};
use std::sync::Arc;

use directories::UserDirs;
use zeroclaw_config::schema::{HooksConfig, PluginsConfig};

use super::builtin::{CommandLoggerHook, WebhookAuditHook};
use super::runner::HookRunner;

/// Build the process-wide hook runner: built-in handlers plus optional WASM hook plugins.
pub fn build_hook_runner(
    hooks: &HooksConfig,
    plugins: &PluginsConfig,
    data_dir: &Path,
) -> Option<Arc<HookRunner>> {
    if !hooks.enabled {
        return None;
    }

    let mut runner = HookRunner::new();

    if hooks.builtin.command_logger {
        runner.register(Box::new(CommandLoggerHook::new()));
    }
    if hooks.builtin.webhook_audit.enabled {
        runner.register(Box::new(WebhookAuditHook::new(
            hooks.builtin.webhook_audit.clone(),
        )));
    }

    #[cfg(feature = "plugins-wasm")]
    if plugins.enabled {
        register_wasm_hook_plugins(&mut runner, plugins, data_dir);
    }

    Some(Arc::new(runner))
}

#[cfg(feature = "plugins-wasm")]
fn register_wasm_hook_plugins(runner: &mut HookRunner, plugins: &PluginsConfig, data_dir: &Path) {
    use super::wasm_hook::WasmHookHandler;

    let parent = resolve_plugins_parent(&plugins.plugins_dir, data_dir);
    let signature_mode =
        zeroclaw_plugins::host::PluginHost::parse_signature_mode(&plugins.security.signature_mode);

    let host = match zeroclaw_plugins::host::PluginHost::with_security(
        &parent,
        signature_mode,
        plugins.security.trusted_publisher_keys.clone(),
    ) {
        Ok(host) => host,
        Err(e) => {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({"error": format!("{e}")})),
                "failed to discover wasm hook plugins"
            );
            return;
        }
    };

    for (manifest, wasm_path, events) in host.hook_plugin_details() {
        runner.register(Box::new(WasmHookHandler::new(
            manifest.name.clone(),
            wasm_path.to_path_buf(),
            manifest.permissions.clone(),
            events,
            manifest.hook_priority,
        )));
        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(
                ::serde_json::json!({
                    "plugin": manifest.name,
                    "hooks": manifest.hooks,
                })
            ),
            "registered wasm lifecycle hook plugin"
        );
    }
}

fn resolve_plugins_parent(plugins_dir: &str, data_dir: &Path) -> PathBuf {
    if let Some(rest) = plugins_dir.strip_prefix("~/")
        && let Some(dirs) = UserDirs::new()
    {
        return dirs
            .home_dir()
            .join(rest)
            .parent()
            .unwrap_or(data_dir)
            .to_path_buf();
    }

    let path = PathBuf::from(plugins_dir);
    path.parent()
        .map_or_else(|| data_dir.to_path_buf(), Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_hook_runner_disabled_returns_none() {
        let mut hooks = HooksConfig {
            enabled: false,
            builtin: Default::default(),
        };
        hooks.builtin.command_logger = true;
        let plugins = PluginsConfig::default();
        let dir = tempfile::tempdir().unwrap();
        assert!(build_hook_runner(&hooks, &plugins, dir.path()).is_none());
    }

    #[test]
    fn build_hook_runner_enabled_returns_some() {
        let hooks = HooksConfig {
            enabled: true,
            builtin: Default::default(),
        };
        let plugins = PluginsConfig::default();
        let dir = tempfile::tempdir().unwrap();
        assert!(build_hook_runner(&hooks, &plugins, dir.path()).is_some());
    }
}
