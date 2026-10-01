//! Process-model selection: how a session's agent gets started.
//!
//! Each [`AgentLauncher`] is one way to run the agent — a confined
//! `octos acp --host-managed` subprocess, an arbitrary external ACP command,
//! or (Apple platforms) an ExtensionFoundation app extension. A launcher's
//! only job is to decide whether it is usable right now and, if so, produce a
//! connected [`AgentChannel`]. Everything downstream — the ACP handshake,
//! [`HostBroker`], permissions, information flow — is identical regardless of
//! which launcher won, because none of it can see past the channel.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use crate::channel::{AgentChannel, PipeChannel};
use crate::host_broker::HostBroker;
use crate::mcp::McpServerConfig;

/// The per-launch, transport-agnostic inputs every launcher shares. The
/// launcher-specific bits (command line, environment, executable path) live on
/// the launcher itself.
pub(crate) struct LaunchConfig<'a> {
    /// Absolute working directory advertised as the ACP session's `cwd`.
    pub workspace: &'a Path,
    /// stdio MCP servers advertised in `session/new` (`mcpServers`).
    pub mcp_servers: &'a [McpServerConfig],
    /// The host broker, when this is a protected host-managed session. Its
    /// presence is what tells `AcpClient` to negotiate `_meta` capabilities.
    pub broker: Option<Arc<HostBroker>>,
}

/// A connected channel plus the launch-time metadata `AcpClient` needs.
pub(crate) struct Launched {
    pub channel: Box<dyn AgentChannel>,
    /// Human-readable description, for error messages and the Providers page.
    pub desc: String,
    /// The broker to bind to the connection, if this is a protected session.
    pub broker: Option<Arc<HostBroker>>,
}

/// One process model that can start an agent.
///
/// [`Self::available`] is consulted before use so a caller can fall through to
/// the next launcher exactly as the old backend `if/else` chain did.
pub(crate) trait AgentLauncher: Send + Sync {
    /// Whether this launcher is usable on this machine right now (binary
    /// present, extension installed and enabled, platform supported).
    fn available(&self, cfg: &LaunchConfig<'_>) -> bool;
    /// Starts the backend and returns its connected channel.
    fn launch(&self, cfg: &LaunchConfig<'_>) -> Result<Launched, String>;
}

/// Picks the first available launcher in `launchers` and starts it.
pub(crate) fn launch_first(
    launchers: &[&dyn AgentLauncher],
    cfg: &LaunchConfig<'_>,
) -> Result<Launched, String> {
    for launcher in launchers {
        if launcher.available(cfg) {
            return launcher.launch(cfg);
        }
    }
    Err("No agent backend is available.".into())
}

/// An arbitrary external ACP command: `ROBRIX_AGENT_CMD`, the
/// `claude-code-acp` bridge, or a plain `octos acp`. Unconfined — the command
/// is the user's (or a well-known adapter's) own process, with no sandbox.
pub(crate) struct ExternalCommandLauncher {
    cmd_line: String,
    env: Vec<(String, String)>,
    extra_args: Vec<String>,
}

impl ExternalCommandLauncher {
    pub(crate) fn new(
        cmd_line: impl Into<String>,
        env: Vec<(String, String)>,
        extra_args: Vec<String>,
    ) -> Self {
        Self { cmd_line: cmd_line.into(), env, extra_args }
    }
}

impl AgentLauncher for ExternalCommandLauncher {
    fn available(&self, _cfg: &LaunchConfig<'_>) -> bool {
        true
    }

    fn launch(&self, cfg: &LaunchConfig<'_>) -> Result<Launched, String> {
        let mut parts = self.cmd_line.split_whitespace();
        let bin = parts.next().ok_or("agent command is empty")?;
        let mut args: Vec<String> = parts.map(str::to_string).collect();
        args.extend(self.extra_args.iter().cloned());

        std::fs::create_dir_all(cfg.workspace).ok();
        let mut command = Command::new(bin);
        command
            .args(&args)
            .envs(self.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            // Robrix may itself be running from inside a Claude Code session
            // (dev workflows); the claude-code-acp adapter refuses to start
            // when it sees CLAUDECODE, thinking it's being nested. Robrix
            // isn't Claude Code — drop the marker for the child (the
            // adapter's own documented bypass).
            .env_remove("CLAUDECODE")
            .current_dir(cfg.workspace);
        let channel = PipeChannel::spawn(command, &self.cmd_line)?;
        Ok(Launched { channel: Box::new(channel), desc: self.cmd_line.clone(), broker: None })
    }
}

/// The confined `octos acp --host-managed` subprocess: mode A's process model.
/// The OS-level sandbox comes from [`octos_sandbox::host_managed_command`].
pub(crate) struct ConfinedOctosLauncher {
    executable: PathBuf,
}

impl ConfinedOctosLauncher {
    pub(crate) fn new(executable: PathBuf) -> Self {
        Self { executable }
    }
}

impl AgentLauncher for ConfinedOctosLauncher {
    fn available(&self, _cfg: &LaunchConfig<'_>) -> bool {
        self.executable.is_file()
    }

    fn launch(&self, cfg: &LaunchConfig<'_>) -> Result<Launched, String> {
        let mut command = octos_sandbox::host_managed_command(&self.executable)
            .map_err(|error| format!("Could not confine Octos: {error}"))?;
        command.args(["acp", "--host-managed"]);
        let channel = PipeChannel::spawn(command, "confined Octos host broker")?;
        Ok(Launched {
            channel: Box::new(channel),
            desc: "confined Octos host broker".into(),
            broker: cfg.broker.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn cfg<'a>(workspace: &'a Path, broker: Option<Arc<HostBroker>>) -> LaunchConfig<'a> {
        LaunchConfig { workspace, mcp_servers: &[], broker }
    }

    /// The fallback shape the backend chain depends on: an unavailable
    /// launcher is skipped, and the first available one wins.
    #[test]
    fn launch_first_skips_unavailable_launchers() {
        struct Never;
        impl AgentLauncher for Never {
            fn available(&self, _: &LaunchConfig<'_>) -> bool {
                false
            }
            fn launch(&self, _: &LaunchConfig<'_>) -> Result<Launched, String> {
                Err("must not be called".into())
            }
        }
        struct Always;
        impl AgentLauncher for Always {
            fn available(&self, _: &LaunchConfig<'_>) -> bool {
                true
            }
            fn launch(&self, _: &LaunchConfig<'_>) -> Result<Launched, String> {
                Err("reached the available launcher".into())
            }
        }
        let dir = std::env::temp_dir();
        let never = Never;
        let always = Always;
        let err = launch_first(&[&never, &always], &cfg(&dir, None)).err().unwrap();
        assert_eq!(err, "reached the available launcher");
        assert_eq!(launch_first(&[&never], &cfg(&dir, None)).err().unwrap(), "No agent backend is available.");
    }

    /// An external command launcher produces a real, usable channel: the child
    /// sees the configured environment and working directory.
    #[cfg(unix)]
    #[test]
    fn external_command_launcher_runs_in_the_workspace_with_env() {
        let dir = std::env::temp_dir().join("a2app_external_launcher_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("report.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\nwhile IFS= read -r line; do printf '%s:%s:%s\\n' \"$PWD\" \"$A2APP_TEST_MARKER\" \"$line\"; done\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let launcher = ExternalCommandLauncher::new(
            script.to_string_lossy().into_owned(),
            vec![("A2APP_TEST_MARKER".into(), "seen".into())],
            vec![],
        );
        let launched = launcher.launch(&cfg(&dir, None)).unwrap();
        launched.channel.send_frame(b"ping").unwrap();
        let mut buf = Vec::new();
        launched.channel.recv_frame(&mut buf).unwrap();
        let text = String::from_utf8_lossy(&buf);
        // Compare against the resolved path: a shell's `$PWD` reports the
        // canonical directory (macOS resolves /var through /private/var).
        let canonical = dir.canonicalize().unwrap();
        assert!(text.starts_with(&format!("{}:seen:ping", canonical.display())), "{text}");
        launched.channel.close();
    }
}
