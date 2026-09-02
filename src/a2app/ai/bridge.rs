//! `robrix --mcp-bridge --socket <path>`: the process an agent spawns as its
//! MCP server.
//!
//! Both supported agents register Robrix's own binary as a *stdio* MCP server
//! — octos through the `mcp_servers` in its config, Claude Code through
//! `mcpServers` at `session/new` — with `command` = this binary and
//! `args = ["--mcp-bridge", "--socket", <path>]`. This mode is that child: a
//! dumb relay between its own stdio (which the agent's MCP client talks to)
//! and the session-scoped socket in the *real* Robrix process (see
//! [`super::server`]). It parses nothing — the two real ends negotiate — so
//! there is exactly one copy of any protocol logic, and it lives in the
//! process that owns the state.
//!
//! Lifecycle: exits when either direction closes. The agent ending the session
//! closes the socket; Robrix ending the session closes the socket too; either
//! way the relay process goes away and the agent sees its MCP server die.

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc;

use a2app_agent::mcp::copy_frames;

/// What the process should do given its command line.
#[derive(Debug, PartialEq)]
pub enum BridgeLaunch {
    /// No `--mcp-bridge` flag: run the normal app.
    No,
    /// Bridge mode: relay stdio to this socket.
    Run(PathBuf),
    /// `--mcp-bridge` was given but the arguments are wrong.
    Usage(String),
}

/// Reads the launch decision from the process's own arguments.
pub fn from_args() -> BridgeLaunch {
    from_argv(std::env::args())
}

/// The decision for an explicit argv (first entry is the binary, skipped).
/// Split out from [`from_args`] so the parser is testable without env surgery.
pub fn from_argv(args: impl IntoIterator<Item = String>) -> BridgeLaunch {
    let args: Vec<String> = args.into_iter().collect();
    let mut socket: Option<PathBuf> = None;
    let mut bridge = false;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--mcp-bridge" => bridge = true,
            "--socket" => {
                i += 1;
                let Some(value) = args.get(i) else {
                    return BridgeLaunch::Usage("--socket needs a path".to_string());
                };
                socket = Some(PathBuf::from(value));
            }
            // The config file / env that agents pass carries only the flags we
            // set; anything else is tolerated so a future flag doesn't break
            // an old binary.
            _ => {}
        }
        i += 1;
    }
    if !bridge {
        return BridgeLaunch::No;
    }
    match socket {
        Some(path) => BridgeLaunch::Run(path),
        None => BridgeLaunch::Usage("--mcp-bridge requires --socket <path>".to_string()),
    }
}

/// Runs bridge mode if the command line asks for it. Returns the process exit
/// code when it did (`0` after a clean relay, `1` on failure), `None` when the
/// normal app should start instead.
pub fn maybe_run() -> Option<i32> {
    match from_args() {
        BridgeLaunch::No => None,
        BridgeLaunch::Run(path) => {
            let code = match run(&path) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("robrix --mcp-bridge: {e}");
                    1
                }
            };
            Some(code)
        }
        BridgeLaunch::Usage(msg) => {
            eprintln!("robrix --mcp-bridge: {msg}");
            Some(1)
        }
    }
}

/// Relays frames between this process's stdio and the session socket until
/// either direction closes.
fn run(socket: &std::path::Path) -> Result<(), String> {
    let stream = UnixStream::connect(socket)
        .map_err(|e| format!("couldn't connect to session socket {}: {e}", socket.display()))?;

    // Two pumps, one per direction. Each owns its own handles, so neither can
    // block the other's write; whoever reaches EOF first signals done and the
    // process exits (the other pump is killed with it, which is exactly right
    // for a per-session relay).
    let (done_tx, done_rx) = mpsc::channel::<()>();

    // Agent's stdin -> Robrix's socket.
    {
        let mut writer = stream
            .try_clone()
            .map_err(|e| format!("couldn't clone the session socket: {e}"))?;
        let done_tx = done_tx.clone();
        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            let mut reader = stdin.lock();
            let _ = copy_frames(&mut reader, &mut writer);
            let _ = done_tx.send(());
        });
    }

    // Robrix's socket -> agent's stdout.
    {
        let done_tx = done_tx.clone();
        std::thread::spawn(move || {
            let mut reader = std::io::BufReader::new(stream);
            let stdout = std::io::stdout();
            let mut writer = stdout.lock();
            let _ = copy_frames(&mut reader, &mut writer);
            let _ = done_tx.send(());
        });
    }

    // Wait for either direction to finish.
    drop(done_tx);
    let _ = done_rx.recv();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_bridge_flags() {
        // A full agent-style spawn: command, flags, socket path.
        let argv = [
            "robrix",
            "--mcp-bridge",
            "--socket",
            "/tmp/robrix-session/tools.sock",
        ];
        assert_eq!(
            from_argv(argv.iter().map(|s| s.to_string())),
            BridgeLaunch::Run(PathBuf::from("/tmp/robrix-session/tools.sock"))
        );
        // Bridge mode without a socket is a usage error.
        assert!(matches!(
            from_argv(["robrix", "--mcp-bridge"].iter().map(|s| s.to_string())),
            BridgeLaunch::Usage(_)
        ));
        // No bridge flag: normal app. Unknown flags are tolerated.
        assert!(matches!(
            from_argv(["robrix", "--some-future-flag"].iter().map(|s| s.to_string())),
            BridgeLaunch::No
        ));
    }
}
