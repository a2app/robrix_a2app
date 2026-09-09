//! The session-scoped MCP tool server: the Unix-socket endpoint inside the
//! *real* Robrix process that the `--mcp-bridge` relay child connects to.
//!
//! One [`ToolServer`] exists per agent session, bound to a fresh socket path
//! under the a2app data root. The path is the capability: only the process
//! Robrix told the path to (the agent, via its MCP-server config) can connect,
//! and the socket dies with the session ([`Drop`] removes it). Each incoming
//! connection gets its own [`McpServer`] session over the shared tool set, on
//! its own thread; tool execution happens on that thread, which is why tools
//! that need UI-thread state (the app registry, a `Cx`) go through a host that
//! marshals to the UI thread and blocks on the reply.

use std::io::Write;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use a2app_agent::mcp::{MAX_FRAME_BYTES, McpServer, read_frame};

/// Handles one accepted connection until the peer closes it (or sends a frame
/// too large to be a protocol we can answer). Reports WHY the connection
/// ended on stderr, so a relay that dies mid-session ("socket EOF right after
/// initialize") can be traced to the exact serve-thread exit.
/// Handles one accepted connection until the peer closes it (or sends a frame
/// too large to be a protocol we can answer).
fn serve_connection(mut stream: UnixStream, server: McpServer) {
    let read_half = match stream.try_clone() {
        Ok(read_half) => read_half,
        Err(_) => return,
    };
    let mut reader = std::io::BufReader::new(read_half);
    let mut buf = Vec::new();
    while read_frame(&mut reader, MAX_FRAME_BYTES, &mut buf) {
        if buf.len() >= MAX_FRAME_BYTES {
            // Oversized frame: not a protocol we can answer; treat the peer as
            // broken and drop the connection.
            return;
        }
        let Ok(line) = std::str::from_utf8(&buf) else {
            continue;
        };
        for reply in server.handle_frame(line) {
            if stream
                .write_all(reply.as_bytes())
                .and_then(|_| stream.write_all(b"\n"))
                .and_then(|_| stream.flush())
                .is_err()
            {
                return;
            }
        }
    }
}

/// Accepts connections until told to stop (the session ended). Each connection
/// is served on its own thread with its own copy of the tool set — the tools
/// are shared (`Arc`), so this is cheap and a busy agent never blocks another.
/// Every accepted connection is registered in `conns` under a fresh id and
/// removed when its serve thread finishes, so [`ToolServer`]'s teardown can
/// close exactly the connections still alive.
fn accept_loop(
    listener: UnixListener,
    template: McpServer,
    stop: Arc<AtomicBool>,
    conns: Arc<Mutex<Vec<(u64, UnixStream)>>>,
) {
    let mut next_id = 0u64;
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                let id = next_id;
                next_id += 1;
                // macOS/BSD `accept()` inherits the listener's O_NONBLOCK onto
                // the accepted socket. Each connection is served on its own
                // thread with BLOCKING reads, so clear it: without this, a
                // quiet moment between requests (e.g. right after the
                // initialize reply, before the client's next frame lands)
                // makes the serve thread's read return EAGAIN, which the
                // frame reader treats as a dead peer and drops the
                // connection. Linux does not inherit the flag, which is why
                // this only ever broke on macOS.
                if let Err(e) = stream.set_nonblocking(false) {
                    // A connection we can't serve on its own thread is not a
                    // connection at all; drop it and keep accepting.
                    eprintln!("robrix tool server: could not clear nonblocking on an accepted connection: {e}");
                    continue;
                }
                // Register a write handle so session teardown (Drop) can
                // interrupt this connection even while its serve thread is
                // blocked reading from it.
                if let Ok(handle) = stream.try_clone() {
                    if let Ok(mut conns) = conns.lock() {
                        conns.push((id, handle));
                    }
                }
                let template = template.clone();
                let conns = conns.clone();
                std::thread::spawn(move || {
                    serve_connection(stream, template);
                    if let Ok(mut conns) = conns.lock() {
                        conns.retain(|(conn_id, _)| *conn_id != id);
                    }
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => return,
        }
    }
}

/// The next unique session id for socket paths. Process-wide: one Robrix
/// process owns all of its sessions.
static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

/// A bound, listening tool server for one agent session.
pub struct ToolServer {
    listener: UnixListener,
    /// The tool set this session offers, cloned per connection.
    template: McpServer,
    /// The session directory (removed on drop along with the socket).
    dir: PathBuf,
    /// The socket path the agent's `--mcp-bridge` process is told to connect to.
    path: PathBuf,
    /// Set on drop to end the accept loop.
    stop: Arc<AtomicBool>,
    /// Every live connection's write handle, shut down on drop so the session's
    /// relay children see EOF the moment the session ends.
    conns: Arc<Mutex<Vec<(u64, UnixStream)>>>,
}

impl ToolServer {
    /// Binds a fresh per-session socket under the a2app data root.
    ///
    /// `template` is the tool set this session offers (build it with
    /// [`McpServer::new`] + `add_tool`, or see
    /// [`crate::a2app::ai::tools::register_session_tools`]); it is cloned —
    /// cheaply, the tools are `Arc` — per incoming connection.
    ///
    /// The socket is created with owner-only permissions: it carries the whole
    /// session's authority (which room it may post to, what it may install), so
    /// nothing else on the machine should be able to reach it.
    pub fn bind(template: McpServer) -> Result<Self, String> {
        let root = a2app_core::data_root().join("sessions");
        std::fs::create_dir_all(&root)
            .map_err(|e| format!("couldn't create {}: {e}", root.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700));
        }

        // A fresh directory per session keeps sockets from colliding and makes
        // teardown a single directory removal. A Robrix process that was killed
        // (not dropped) leaves its session-<n> directories behind, and the
        // process-wide counter restarts at 1, so a taken id is reclaimed when
        // stale — its socket is gone or accepts no connection — and skipped
        // when a live listener still owns it (a concurrent second process must
        // never have its live session torn down).
        let dir = loop {
            let id = NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed);
            let dir = root.join(format!("session-{id}"));
            match std::fs::create_dir(&dir) {
                Ok(()) => break dir,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let socket = dir.join("tools.sock");
                    if UnixStream::connect(&socket).is_ok() {
                        // A live session (e.g. another process) owns this id;
                        // leave it alone and take the next one.
                        continue;
                    }
                    // A killed process's leftovers: clear the socket and dir.
                    let _ = std::fs::remove_file(&socket);
                    std::fs::remove_dir(&dir).map_err(|e| {
                        format!("couldn't clear stale session dir {}: {e}", dir.display())
                    })?;
                    std::fs::create_dir(&dir)
                        .map_err(|e| format!("couldn't create {}: {e}", dir.display()))?;
                    break dir;
                }
                Err(e) => return Err(format!("couldn't create {}: {e}", dir.display())),
            }
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        }

        let path = dir.join("tools.sock");
        let listener = UnixListener::bind(&path)
            .map_err(|e| format!("couldn't bind {}: {e}", path.display()))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| format!("couldn't set nonblocking on {}: {e}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }

        Ok(Self {
            listener,
            template,
            dir,
            path,
            stop: Arc::new(AtomicBool::new(false)),
            conns: Arc::new(Mutex::new(Vec::new())),
        })
    }

    /// The socket path the session's `--mcp-bridge` child must be told to
    /// connect to.
    pub fn socket_path(&self) -> &Path {
        &self.path
    }

    /// Spawns the accept loop. Connections are served on their own threads
    /// from here on; the loop ends when this server is dropped.
    pub fn start(&self) -> Result<(), String> {
        let listener = self
            .listener
            .try_clone()
            .map_err(|e| format!("couldn't clone the session listener: {e}"))?;
        let template = self.template.clone();
        let stop = self.stop.clone();
        let conns = self.conns.clone();
        std::thread::spawn(move || accept_loop(listener, template, stop, conns));
        Ok(())
    }
}

impl Drop for ToolServer {
    fn drop(&mut self) {
        // End the accept loop and remove the session's socket + directory.
        // Socket paths are unique per session, so a stale file from a crashed
        // host can never block a future session's bind.
        self.stop.store(true, Ordering::Relaxed);
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_dir(&self.dir);
        // Close every live connection: the session is over, so its relay
        // children must see EOF and exit rather than hang waiting on us. The
        // blocked reads in their serve threads unblock and those threads exit.
        if let Ok(mut conns) = self.conns.lock() {
            for (_, conn) in conns.iter() {
                let _ = conn.shutdown(std::net::Shutdown::Both);
            }
            conns.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};

    use serde_json::{Map, Value, json};

    /// A canned tool so socket tests can assert a full call round trip.
    struct EchoTool;

    impl a2app_agent::mcp::Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "Echoes its `text` argument, for socket tests."
        }
        fn input_schema(&self) -> Value {
            json!({"type": "object"})
        }
        fn call(&self, arguments: &Map<String, Value>) -> Result<String, String> {
            Ok(arguments
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string())
        }
    }

    fn echo_server() -> ToolServer {
        let mut template = McpServer::new();
        template.add_tool(EchoTool);
        ToolServer::bind(template).expect("bind a session socket")
    }

    /// Writes one JSON-RPC frame and reads exactly one reply line back.
    fn exchange(stream: &mut UnixStream, frame: &str) -> Value {
        stream
            .write_all(frame.as_bytes())
            .and_then(|_| stream.write_all(b"\n"))
            .and_then(|_| stream.flush())
            .expect("write the request");
        let mut line = String::new();
        BufReader::new(stream)
            .read_line(&mut line)
            .expect("read the reply");
        serde_json::from_str(&line).expect("the reply is JSON")
    }

    /// A real socket round trip through the accept loop: connect, handshake,
    /// list tools, call one. Proves the wire half end to end without a child
    /// process (the bridge relay is covered by the crate's integration test).
    #[test]
    fn a_connected_client_is_served_over_the_socket() {
        let server = echo_server();
        server.start().expect("start the accept loop");

        let mut client = UnixStream::connect(server.socket_path()).expect("connect");
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set a read timeout");

        let reply = exchange(
            &mut client,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"socket-test"}}}"#,
        );
        assert_eq!(reply["id"], json!(1));
        assert_eq!(reply["result"]["protocolVersion"], "2025-11-25");
        assert_eq!(reply["result"]["serverInfo"]["name"], a2app_agent::mcp::SERVER_NAME);

        // An initialized notification gets no reply: nothing may arrive on the
        // wire. A short read whose timeout expires IS the assertion.
        let frame = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        client.write_all(frame.as_bytes()).unwrap();
        client.write_all(b"\n").unwrap();
        client.flush().unwrap();
        client
            .set_read_timeout(Some(Duration::from_millis(300)))
            .expect("set a short read timeout");
        let mut line = String::new();
        let silence = matches!(
            BufReader::new(&mut client).read_line(&mut line),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                )
        );
        assert!(silence, "a notification must be answered with silence, got: {line}");
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("restore the read timeout");

        // tools/list advertises the registered tool; tools/call round-trips.
        let reply = exchange(
            &mut client,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
        );
        let names: Vec<&str> = reply["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["echo"]);

        let reply = exchange(
            &mut client,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"echo","arguments":{"text":"over the socket"}}}"#,
        );
        assert_eq!(reply["result"]["isError"], false);
        assert_eq!(reply["result"]["content"][0]["text"], "over the socket");
    }

    /// Dropping the server must close its live connections: a relay child
    /// blocked reading the socket sees EOF the moment the session ends. The
    /// ping before the drop proves the connection was accepted AND registered
    /// (registration happens before the serve thread starts), so the drop is
    /// exercising the registry rather than racing it.
    #[test]
    fn dropping_the_server_closes_live_connections() {
        let server = echo_server();
        server.start().expect("start the accept loop");

        let mut client = UnixStream::connect(server.socket_path()).expect("connect");
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set a read timeout");

        // Round-trip a ping so accept() has run and the connection is in the
        // registry.
        let reply = exchange(
            &mut client,
            r#"{"jsonrpc":"2.0","id":1,"method":"ping","params":{}}"#,
        );
        assert!(reply.get("result").is_some());

        drop(server);

        // The session is over; the peer must now see EOF, not a hang.
        let mut line = String::new();
        let n = BufReader::new(&mut client).read_line(&mut line).expect("read");
        assert_eq!(n, 0, "the server must close live connections on drop, got: {line}");
    }
}