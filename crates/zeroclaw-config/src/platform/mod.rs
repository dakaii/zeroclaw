pub mod docker;
pub mod native;

pub use docker::DockerRuntime;
pub use native::NativeRuntime;
pub use zeroclaw_api::runtime_traits::{RuntimeAdapter, ShellDialect, ShellProfile};

use crate::schema::{RuntimeConfig, RuntimeKind};

/// Default Unix native shell when `[runtime] shell` is unset.
///
/// Distroless release images do not ship this binary. `create_runtime`
/// treats a missing default as "no shell access" rather than failing
/// construction, so webhook/LLM-only turns can proceed.
const DEFAULT_NATIVE_SHELL: &str = "sh";

pub fn create_runtime(config: &RuntimeConfig) -> anyhow::Result<Box<dyn RuntimeAdapter>> {
    match config.kind {
        RuntimeKind::Native => {
            let shell = config
                .shell
                .clone()
                .unwrap_or_else(|| DEFAULT_NATIVE_SHELL.to_string());
            #[cfg(unix)]
            match probe_unix_shell(&shell)? {
                ShellProbe::Ready => Ok(Box::new(NativeRuntime::with_shell(shell))),
                ShellProbe::Unavailable { reason } => {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({
                                "runtime_kind": "native",
                                "configured_shell": shell,
                                "reason": reason,
                            })),
                        "native runtime has no executable shell; continuing without shell access so LLM-only turns can proceed"
                    );
                    Ok(Box::new(NativeRuntime::without_shell()))
                }
            }
            #[cfg(windows)]
            {
                validate_shell_windows(&shell)?;
                Ok(Box::new(NativeRuntime::with_shell(shell)))
            }
        }
        RuntimeKind::Docker => Ok(Box::new(DockerRuntime::new(config.docker.clone()))),
        RuntimeKind::Cloudflare => anyhow::bail!(
            "runtime.kind='cloudflare' is not implemented yet. Use runtime.kind='native' for now."
        ),
    }
}

#[cfg(unix)]
enum ShellProbe {
    Ready,
    Unavailable { reason: String },
}

/// Probe a configured Unix `runtime.shell`.
///
/// Invalid *values* (empty, relative paths) stay construction errors. A
/// well-formed name or path that cannot be resolved to an executable is
/// `Unavailable` so the native runtime can represent "no shell" instead of
/// failing agent construction (distroless release images).
#[cfg(unix)]
fn probe_unix_shell(shell: &str) -> anyhow::Result<ShellProbe> {
    use std::os::unix::fs::PermissionsExt;

    // Android pins the shell to /system/bin/sh; the configured value is never
    // used, so don't reject it.
    if zeroclaw_api::platform::is_android() {
        return Ok(ShellProbe::Ready);
    }

    if shell.trim().is_empty() {
        anyhow::bail!("runtime.shell must not be empty or whitespace");
    }

    let path = std::path::Path::new(shell);
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else if path.components().count() > 1 {
        anyhow::bail!(
            "runtime.shell {shell:?} is a relative path; use a bare name resolved on PATH (e.g. \"bash\") or an absolute path (e.g. \"/bin/bash\")"
        );
    } else {
        match std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|dir| dir.join(shell))
            .find(|candidate| candidate.is_file())
        {
            Some(found) => found,
            None => {
                return Ok(ShellProbe::Unavailable {
                    reason: format!(
                        "runtime.shell {shell:?} was not found on PATH; use an absolute path or install the shell"
                    ),
                });
            }
        }
    };

    if !resolved.exists() {
        return Ok(ShellProbe::Unavailable {
            reason: format!(
                "runtime.shell {shell:?} (resolved to {}) does not exist",
                resolved.display()
            ),
        });
    }

    let metadata = match resolved.metadata() {
        Ok(metadata) => metadata,
        Err(e) => anyhow::bail!(
            "runtime.shell {shell:?} (resolved to {}) could not be inspected: {e}",
            resolved.display()
        ),
    };
    if !metadata.is_file() {
        anyhow::bail!(
            "runtime.shell {shell:?} (resolved to {}) is not a regular file",
            resolved.display()
        );
    }

    // Coarse check: reject only when no execute bit is set at all. A precise
    // "can *we* execute it" test (uid/gid vs. the file owner) buys little —
    // the kernel's spawn is the real authority (ACLs, caps, mount flags) — and
    // this is a fail-fast sanity check, not a security gate.
    let mode = metadata.permissions().mode();
    if mode & 0o111 == 0 {
        return Ok(ShellProbe::Unavailable {
            reason: format!(
                "runtime.shell {shell:?} (resolved to {}) is not executable",
                resolved.display()
            ),
        });
    }

    Ok(ShellProbe::Ready)
}

#[cfg(unix)]
fn validate_shell(shell: &str) -> anyhow::Result<()> {
    match probe_unix_shell(shell)? {
        ShellProbe::Ready => Ok(()),
        ShellProbe::Unavailable { reason } => anyhow::bail!("{reason}"),
    }
}

/// Validate a configured `runtime.shell` on Windows.
///
/// Unlike the Unix check this does not resolve a binary on `PATH`: on Windows
/// `runtime.shell` selects the interpreter family (`cmd.exe` vs PowerShell),
/// and the interpreter is located at spawn time. The only fail-fast condition
/// worth catching up front is an empty/whitespace value, which would otherwise
/// spawn with no program.
#[cfg(windows)]
fn validate_shell_windows(shell: &str) -> anyhow::Result<()> {
    if shell.trim().is_empty() {
        anyhow::bail!("runtime.shell must not be empty or whitespace");
    }
    Ok(())
}

/// Write an executable shell shim into `dir` that records, on stdout, that it
/// ran (`SHIM_RAN`) and each argument it received (`arg:<value>`). Used by
/// tests to prove a configured shell is the binary that actually executes a
/// command and that it receives the `-c <command>` boundary.
#[cfg(all(test, unix))]
fn write_recording_shim(dir: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let shim = dir.join("recording-shell");
    std::fs::write(
        &shim,
        "#!/bin/sh\necho SHIM_RAN\nfor a in \"$@\"; do echo \"arg:$a\"; done\n",
    )
    .unwrap();
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    shim
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{RuntimeConfig, RuntimeKind};

    #[test]
    fn factory_native() {
        let cfg = RuntimeConfig {
            kind: RuntimeKind::Native,
            ..RuntimeConfig::default()
        };
        let rt = create_runtime(&cfg).unwrap();
        assert_eq!(rt.name(), "native");
        assert!(rt.has_shell_access());
    }

    #[test]
    fn factory_docker() {
        let cfg = RuntimeConfig {
            kind: RuntimeKind::Docker,
            ..RuntimeConfig::default()
        };
        let rt = create_runtime(&cfg).unwrap();
        assert_eq!(rt.name(), "docker");
        assert!(rt.has_shell_access());
    }

    #[test]
    fn factory_cloudflare_errors() {
        let cfg = RuntimeConfig {
            kind: RuntimeKind::Cloudflare,
            ..RuntimeConfig::default()
        };
        match create_runtime(&cfg) {
            Err(err) => assert!(err.to_string().contains("not implemented")),
            Ok(_) => panic!("cloudflare runtime should error"),
        }
    }

    #[test]
    fn unknown_runtime_kind_loads_as_native() {
        let parsed: RuntimeConfig = toml::from_str("kind = \"wasm-edge-unknown\"").unwrap();
        assert_eq!(parsed.kind, RuntimeKind::Native);
        let empty: RuntimeConfig = toml::from_str("kind = \"\"").unwrap();
        assert_eq!(empty.kind, RuntimeKind::Native);
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn factory_native_default_shell_is_sh() {
        let cfg = RuntimeConfig {
            kind: RuntimeKind::Native,
            shell: None,
            ..RuntimeConfig::default()
        };
        let rt = create_runtime(&cfg).unwrap();
        let cmd = rt
            .build_shell_command("echo hi", &std::env::temp_dir())
            .unwrap();
        let debug = format!("{cmd:?}");
        assert!(
            debug.contains("\"sh\""),
            "default shell should be 'sh', got: {debug}"
        );
    }

    // ── Shell validation ─────────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn validate_shell_rejects_empty_or_whitespace() {
        for bad in ["", "   ", "\t", " \n "] {
            assert!(
                validate_shell(bad).is_err(),
                "shell {bad:?} should be rejected"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn validate_shell_rejects_nonexistent_absolute_path() {
        let err = validate_shell("/no/such/shell/binary").unwrap_err();
        assert!(
            err.to_string().contains("does not exist"),
            "error should name the missing path, got: {err}"
        );
    }

    #[cfg(all(unix, not(target_os = "android")))]
    #[test]
    fn validate_shell_rejects_directory() {
        let dir = tempfile::tempdir().unwrap();
        let err = validate_shell(dir.path().to_str().unwrap()).unwrap_err();
        assert!(
            err.to_string().contains("not a regular file"),
            "error should identify the non-file shell target, got: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn validate_shell_rejects_relative_path() {
        // Relative path-style values are rejected purely by shape (before any
        // filesystem access): they would validate from the process cwd but
        // execute from the workspace dir, so the validated and executed
        // binaries could differ. Bare names and absolute paths are unaffected.
        for rel in ["./sh", "bin/sh", "../sh", "tools/bin/sh"] {
            let err = validate_shell(rel).unwrap_err();
            assert!(
                err.to_string().contains("relative path"),
                "relative shell {rel:?} should be rejected, got: {err}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn validate_shell_rejects_bare_name_not_on_path() {
        let err = validate_shell("zc-no-such-shell-on-path").unwrap_err();
        assert!(
            err.to_string().contains("not found on PATH"),
            "error should mention PATH, got: {err}"
        );
    }

    #[cfg(all(unix, not(target_os = "android")))]
    #[test]
    fn create_runtime_treats_missing_shell_as_unavailable_capability() {
        // Regression for zeroclaw-labs/zeroclaw#9859: distroless images have
        // no `sh` on PATH. Per-agent `create_runtime()` used to fail and the
        // webhook sanitized that to `{"error":"LLM request failed"}`. The
        // factory must construct a shell-free native runtime instead so
        // LLM-only turns and gateway startup share the same capability model.
        let cfg = RuntimeConfig {
            kind: RuntimeKind::Native,
            shell: Some("zc-no-such-shell-on-path".into()),
            ..RuntimeConfig::default()
        };
        let rt = create_runtime(&cfg)
            .expect("missing executable shell must not fail native runtime construction");
        assert_eq!(rt.name(), "native");
        assert!(
            !rt.has_shell_access(),
            "missing shell must report no shell capability"
        );
        assert_eq!(rt.shell_dialect(), ShellDialect::None);
        assert!(rt.shell_profile().is_none());
        let err = rt
            .build_shell_command("echo hi", &std::env::temp_dir())
            .expect_err("shell-free runtime must fail closed on shell commands");
        assert!(
            err.to_string().contains("no executable shell"),
            "fail-closed error should name the missing capability, got: {err}"
        );
    }

    #[test]
    fn default_native_shell_name_is_sh() {
        // Distroless images fail because the unset default is `"sh"`. The
        // factory probes this name; keep it stable so the missing-shell path
        // remains the release-image contract.
        assert_eq!(DEFAULT_NATIVE_SHELL, "sh");
    }

    #[cfg(all(unix, not(target_os = "android")))]
    #[test]
    fn create_runtime_still_rejects_invalid_shell_values() {
        for (shell, needle) in [
            ("", "empty or whitespace"),
            ("   ", "empty or whitespace"),
            ("./sh", "relative path"),
            ("bin/sh", "relative path"),
        ] {
            let cfg = RuntimeConfig {
                kind: RuntimeKind::Native,
                shell: Some(shell.into()),
                ..RuntimeConfig::default()
            };
            let err = match create_runtime(&cfg) {
                Ok(_) => panic!("invalid runtime.shell {shell:?} must still fail construction"),
                Err(err) => err,
            };
            assert!(
                err.to_string().contains(needle),
                "invalid shell {shell:?} should mention {needle:?}, got: {err}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn validate_shell_rejects_nonexecutable_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not-executable");
        std::fs::write(&file, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = validate_shell(file.to_str().unwrap()).unwrap_err();
        assert!(
            err.to_string().contains("not executable"),
            "error should mention executability, got: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn validate_shell_accepts_sh() {
        validate_shell("sh").expect("'sh' must resolve on PATH");
    }

    // ── End-to-end: the configured shell actually runs the command ──

    #[cfg(unix)]
    #[tokio::test]
    async fn factory_executes_command_under_configured_shell() {
        let dir = tempfile::tempdir().unwrap();
        let shim = write_recording_shim(dir.path());

        let cfg = RuntimeConfig {
            kind: RuntimeKind::Native,
            shell: Some(shim.to_string_lossy().into_owned()),
            ..RuntimeConfig::default()
        };
        let rt = create_runtime(&cfg).unwrap();
        let output = rt
            .build_shell_command("echo factory-shim", dir.path())
            .unwrap()
            .output()
            .await
            .unwrap();

        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("SHIM_RAN"),
            "configured shim should run, got: {stdout:?}"
        );
        assert!(
            stdout.contains("arg:-c"),
            "shim should receive -c, got: {stdout:?}"
        );
        assert!(
            stdout.contains("arg:echo factory-shim"),
            "shim should receive the command, got: {stdout:?}"
        );
    }
}
