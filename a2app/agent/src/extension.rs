//! The ExtensionFoundation app-extension backend (Apple platforms).
//!
//! Apple's ExtensionFoundation runs an app extension as a separate,
//! system-managed process and connects the host to it over XPC. XPC is a
//! *message* transport with no C-callable entry point, so the Swift framework
//! cannot be driven from Rust directly. This module defines the narrow Rust
//! seam the Swift shim implements, and an [`AgentChannel`] over it, so the
//! rest of Robrix — the ACP handshake, the host broker, permissions,
//! information flow — is unchanged whether the agent runs as a pipe-backed
//! subprocess or an XPC extension.
//!
//! Mapping onto the Apple API (`robrix_xpc_bridge`, Swift):
//!
//! | Rust seam                        | Swift/ExtensionFoundation        |
//! |----------------------------------|----------------------------------|
//! | [`ExtensionBridge::discover`]    | `AppExtensionPoint.Monitor.identities` |
//! | [`ExtensionBridge::launch`]      | `AppExtensionProcess` + `makeXPCConnection()` |
//! | [`ExtensionConnection::send`]    | proxy-object method call         |
//! | [`ExtensionConnection::recv`]    | `on_receive` callback → queue    |
//! | [`ExtensionConnection::close`]   | `AppExtensionProcess.invalidate()` |
//!
//! The bridge is installed once by the platform layer via
//! [`install_extension_bridge`]. Until a bridge is installed — which is every
//! platform where Octos has not yet shipped a matching `.appex`, and every
//! non-Apple platform — the extension launcher reports itself unavailable and
//! the confined subprocess remains the backend, exactly as before. That gating
//! is what lets this land and be exercised in CI before the Octos extension
//! work does.

use std::sync::{Arc, OnceLock};

use crate::channel::{AgentChannel, ChannelError, MAX_FRAME_BYTES};
use crate::launcher::{AgentLauncher, LaunchConfig, Launched};

/// The extension point Robrix declares. Placeholder identifier; the final
/// reverse-DNS name is coordinated with Octos (see the design doc §4.4).
pub const AGENT_HOST_EXTENSION_POINT: &str = "org.robrix.agent-host";

/// One enabled, approved extension matching Robrix's extension point.
///
/// Mirrors the fields of `AppExtensionIdentity` Robrix cares about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionIdentity {
    /// The extension's bundle identifier.
    pub bundle_id: String,
    /// The extension point it implements.
    pub extension_point: String,
    /// Its localized display name.
    pub name: String,
}

/// The host-facing ExtensionFoundation operations.
///
/// `discover` is expected to be cheap after the first call (the platform
/// layer caches the monitor's identities); `available()` consults it whenever
/// a session starts.
pub trait ExtensionBridge: Send + Sync {
    /// Every enabled, approved extension matching `extension_point`.
    fn discover(&self, extension_point: &str) -> Vec<ExtensionIdentity>;
    /// Launches (or reuses) the extension process and returns a connected
    /// channel to it.
    fn launch(&self, identity: &ExtensionIdentity) -> Result<Box<dyn ExtensionConnection>, String>;
}

/// A live XPC connection to one extension process.
///
/// One whole JSON-RPC message per `send`/`recv`; there is no newline framing
/// on this path (that lives in [`crate::channel::PipeChannel`]). Implementations
/// block in `recv` until a message arrives or the connection is invalidated,
/// which is what the Swift shim's `on_receive` callback feeds.
pub trait ExtensionConnection: Send + Sync {
    /// Sends one message to the extension.
    fn send(&self, message: &[u8]) -> Result<(), String>;
    /// Blocks until the next inbound message, or returns `None` once the
    /// connection is closed/invalidated.
    fn recv(&self) -> Option<Vec<u8>>;
    /// Invalidates the connection (`AppExtensionProcess.invalidate()`).
    fn close(&self);
}

/// An [`AgentChannel`] over an [`ExtensionConnection`].
struct ExtensionChannel {
    connection: Box<dyn ExtensionConnection>,
}

impl AgentChannel for ExtensionChannel {
    fn send_frame(&self, frame: &[u8]) -> Result<(), ChannelError> {
        self.connection.send(frame).map_err(ChannelError::Transport)
    }

    fn recv_frame(&self, buf: &mut Vec<u8>) -> Result<(), ChannelError> {
        buf.clear();
        match self.connection.recv() {
            Some(message) => {
                // XPC is message-oriented and the system bounds message size,
                // but the same per-frame cap the pipe path enforces keeps a
                // hostile extension from handing the parser an unbounded
                // frame. `>=` matches `PipeChannel`'s off-by-one discipline.
                if message.len() >= MAX_FRAME_BYTES {
                    return Err(ChannelError::Oversized);
                }
                buf.extend_from_slice(&message);
                Ok(())
            }
            None => Err(ChannelError::Closed),
        }
    }

    fn close(&self) {
        self.connection.close();
    }
}

/// The process-global bridge, installed once by the platform layer.
static BRIDGE: OnceLock<Arc<dyn ExtensionBridge>> = OnceLock::new();

/// Installs the platform bridge. Returns false if one was already installed,
/// so a double-registration is a no-op rather than a panic.
pub fn install_extension_bridge(bridge: Arc<dyn ExtensionBridge>) -> bool {
    BRIDGE.set(bridge).is_ok()
}

/// The installed bridge, if any.
pub fn extension_bridge() -> Option<&'static Arc<dyn ExtensionBridge>> {
    if let Some(bridge) = BRIDGE.get() {
        return Some(bridge);
    }
    // On macOS, try once to load the bundled ExtensionFoundation host bridge.
    // Absent dylib (older macOS, unbundled dev binary, or an install without
    // the extension) leaves this unset, so the confined child is used.
    #[cfg(target_os = "macos")]
    if let Some(bridge) = crate::apple_extension::AppleExtensionBridge::load_from_bundle() {
        makepad_widgets::log!("extension: installed Apple host bridge");
        let _ = BRIDGE.set(Arc::new(bridge));
    }
    BRIDGE.get()
}

/// The extension launcher, when a bridge has been installed. `None` on every
/// platform/config where Robrix is not hosting an ExtensionFoundation app
/// extension, which is exactly when the confined subprocess must be used.
pub(crate) fn extension_launcher() -> Option<ExtensionLauncher> {
    extension_bridge().map(|bridge| ExtensionLauncher {
        bridge: bridge.clone(),
        extension_point: AGENT_HOST_EXTENSION_POINT.to_string(),
    })
}

/// An ExtensionFoundation app-extension process model.
pub(crate) struct ExtensionLauncher {
    bridge: Arc<dyn ExtensionBridge>,
    extension_point: String,
}

impl ExtensionLauncher {
    /// Whether an enabled, approved extension matching Robrix's point is
    /// installed right now. Cheap: the bridge caches discovery.
    pub(crate) fn installed(&self) -> bool {
        !self.bridge.discover(&self.extension_point).is_empty()
    }
}

impl AgentLauncher for ExtensionLauncher {
    fn available(&self, _cfg: &LaunchConfig<'_>) -> bool {
        self.installed()
    }

    fn launch(&self, cfg: &LaunchConfig<'_>) -> Result<Launched, String> {
        let identity = self
            .bridge
            .discover(&self.extension_point)
            .into_iter()
            .next()
            .ok_or("No agent extension is installed.")?;
        let connection = self.bridge.launch(&identity)?;
        Ok(Launched {
            channel: Box::new(ExtensionChannel { connection }),
            desc: format!("agent extension {} ({})", identity.name, identity.bundle_id),
            broker: cfg.broker.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Mutex, mpsc};
    use std::time::{Duration, Instant};

    use crate::acp_client::AcpEvent;

    /// A throwaway "extension": echoes each message back with a prefix. This
    /// is the test extension the migration plan calls for — it validates
    /// discover/launch/send/receive/close without real Octos.
    struct FakeConnection {
        incoming: Mutex<mpsc::Receiver<Vec<u8>>>,
        outgoing: mpsc::Sender<Vec<u8>>,
        open: AtomicBool,
    }

    impl FakeConnection {
        fn new() -> Self {
            let (outgoing, incoming) = mpsc::channel();
            Self { incoming: Mutex::new(incoming), outgoing, open: AtomicBool::new(true) }
        }
    }

    impl ExtensionConnection for FakeConnection {
        fn send(&self, message: &[u8]) -> Result<(), String> {
            if !self.open.load(Ordering::Acquire) {
                return Err("connection closed".into());
            }
            let mut reply = b"reply:".to_vec();
            reply.extend_from_slice(message);
            self.outgoing.send(reply).map_err(|e| e.to_string())
        }

        fn recv(&self) -> Option<Vec<u8>> {
            if !self.open.load(Ordering::Acquire) {
                return None;
            }
            self.incoming.lock().unwrap().recv().ok()
        }

        fn close(&self) {
            self.open.store(false, Ordering::Release);
        }
    }

    struct FakeBridge {
        identities: Vec<ExtensionIdentity>,
        launched: AtomicBool,
    }

    impl ExtensionBridge for FakeBridge {
        fn discover(&self, extension_point: &str) -> Vec<ExtensionIdentity> {
            self.identities
                .iter()
                .filter(|identity| identity.extension_point == extension_point)
                .cloned()
                .collect()
        }

        fn launch(&self, _identity: &ExtensionIdentity) -> Result<Box<dyn ExtensionConnection>, String> {
            self.launched.store(true, Ordering::Release);
            Ok(Box::new(FakeConnection::new()))
        }
    }

    /// An in-process stand-in for an Octos/ACP app extension: enough ACP to
    /// take `AcpClient` through initialize → session/new → session/prompt and
    /// stream one chunk back. This is what proves the whole protocol layer,
    /// not merely `ExtensionChannel`, is transport-agnostic — the same
    /// `AcpClient` that drives a child's pipes runs unchanged over a
    /// message-oriented channel.
    struct ScriptedAcpExtension {
        requests: mpsc::Sender<Vec<u8>>,
        replies: Mutex<mpsc::Receiver<Vec<u8>>>,
        open: AtomicBool,
    }

    impl ScriptedAcpExtension {
        fn new() -> Self {
            let (requests, request_rx) = mpsc::channel::<Vec<u8>>();
            let (reply_tx, reply_rx) = mpsc::channel::<Vec<u8>>();
            std::thread::spawn(move || {
                while let Ok(frame) = request_rx.recv() {
                    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&frame) else {
                        continue;
                    };
                    let id = value.get("id").cloned().unwrap_or(serde_json::Value::Null);
                    let reply = |payload: serde_json::Value| {
                        let _ = reply_tx.send(payload.to_string().into_bytes());
                    };
                    match value.get("method").and_then(serde_json::Value::as_str) {
                        Some("initialize") => reply(serde_json::json!({
                            "jsonrpc": "2.0", "id": id,
                            "result": {"protocolVersion": 1, "agentCapabilities": {}}
                        })),
                        Some("session/new") => reply(serde_json::json!({
                            "jsonrpc": "2.0", "id": id,
                            "result": {"sessionId": "ext-session"}
                        })),
                        Some("session/prompt") => {
                            reply(serde_json::json!({
                                "jsonrpc": "2.0", "method": "session/update",
                                "params": {"sessionId": "ext-session", "update": {
                                    "sessionUpdate": "agent_message_chunk",
                                    "content": {"type": "text", "text": "hello from extension"},
                                }}
                            }));
                            reply(serde_json::json!({
                                "jsonrpc": "2.0", "id": id, "result": {"stopReason": "end_turn"}
                            }));
                        }
                        _ => {}
                    }
                }
            });
            Self {
                requests,
                replies: Mutex::new(reply_rx),
                open: AtomicBool::new(true),
            }
        }
    }

    impl ExtensionConnection for ScriptedAcpExtension {
        fn send(&self, message: &[u8]) -> Result<(), String> {
            if !self.open.load(Ordering::Acquire) {
                return Err("connection closed".into());
            }
            self.requests.send(message.to_vec()).map_err(|e| e.to_string())
        }

        fn recv(&self) -> Option<Vec<u8>> {
            loop {
                if !self.open.load(Ordering::Acquire) {
                    return None;
                }
                match self.replies.lock().unwrap().recv_timeout(Duration::from_millis(20)) {
                    Ok(message) => return Some(message),
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => return None,
                }
            }
        }

        fn close(&self) {
            self.open.store(false, Ordering::Release);
        }
    }

    fn identity() -> ExtensionIdentity {
        ExtensionIdentity {
            bundle_id: "org.robrix.test-agent.appex".into(),
            extension_point: AGENT_HOST_EXTENSION_POINT.into(),
            name: "Test Agent".into(),
        }
    }

    #[test]
    fn extension_channel_carries_whole_messages_and_closes_without_newline_framing() {
        let bridge = Arc::new(FakeBridge { identities: vec![identity()], launched: AtomicBool::new(false) });
        let launcher = ExtensionLauncher {
            bridge: bridge.clone(),
            extension_point: AGENT_HOST_EXTENSION_POINT.into(),
        };
        let work = std::env::temp_dir();
        let cfg = LaunchConfig { workspace: Path::new(&work), mcp_servers: &[], broker: None };

        assert!(launcher.available(&cfg));
        let launched = launcher.launch(&cfg).unwrap();
        assert!(bridge.launched.load(Ordering::Acquire), "launch reached the bridge");

        launched.channel.send_frame(br#"{"jsonrpc":"2.0","id":1}"#).unwrap();
        let mut buf = Vec::new();
        launched.channel.recv_frame(&mut buf).unwrap();
        assert_eq!(buf, br#"reply:{"jsonrpc":"2.0","id":1}"#);

        launched.channel.close();
        assert!(launched.channel.recv_frame(&mut buf).is_err(), "a closed extension reports Closed");
    }

    #[test]
    fn extension_launcher_is_unavailable_without_a_matching_extension() {
        let bridge = Arc::new(FakeBridge { identities: vec![], launched: AtomicBool::new(false) });
        let launcher = ExtensionLauncher {
            bridge,
            extension_point: AGENT_HOST_EXTENSION_POINT.into(),
        };
        let work = std::env::temp_dir();
        let cfg = LaunchConfig { workspace: Path::new(&work), mcp_servers: &[], broker: None };
        assert!(!launcher.available(&cfg), "no identity means the confined subprocess must remain the backend");
    }

    /// An extension that answers with an oversized message is rejected exactly
    /// like an oversized pipe frame, not handed to the JSON parser.
    #[test]
    fn extension_channel_rejects_an_oversized_message() {
        struct Oversized;
        impl ExtensionConnection for Oversized {
            fn send(&self, _: &[u8]) -> Result<(), String> {
                Ok(())
            }
            fn recv(&self) -> Option<Vec<u8>> {
                Some(vec![b'x'; MAX_FRAME_BYTES])
            }
            fn close(&self) {}
        }
        let channel = ExtensionChannel { connection: Box::new(Oversized) };
        let mut buf = Vec::new();
        assert!(matches!(channel.recv_frame(&mut buf), Err(ChannelError::Oversized)));
    }

    /// End-to-end: the transport-agnostic `AcpClient` completes its whole
    /// handshake and a prompt turn over a message-oriented extension channel.
    /// This is the macOS/Linux-runnable proof that the XPC path needs no
    /// protocol changes.
    #[test]
    fn acp_handshake_runs_over_an_extension_channel() {
        let channel = ExtensionChannel { connection: Box::new(ScriptedAcpExtension::new()) };
        let work = std::env::temp_dir();
        let mut client = crate::acp_client::AcpClient::new(
            Box::new(channel),
            "scripted extension",
            None,
            Path::new(&work),
            &[],
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut ready = false;
        let mut reply = String::new();
        while Instant::now() < deadline && reply.is_empty() {
            for event in client.drain_events() {
                match event {
                    AcpEvent::SessionReady if !ready => {
                        ready = true;
                        client.send_prompt("hi");
                    }
                    AcpEvent::TurnDone { text, .. } => reply = text,
                    AcpEvent::Error(error) | AcpEvent::ProcessGone(error) => panic!("{error}"),
                    _ => {}
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(ready, "the extension handshake reached session/new");
        assert_eq!(reply, "hello from extension");
    }
}
