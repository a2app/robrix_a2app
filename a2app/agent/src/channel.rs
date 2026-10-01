//! The byte-transport boundary between Robrix and an agent.
//!
//! Everything above this module — the ACP handshake, JSON-RPC correlation,
//! the host broker, the information-flow and permission layers — only ever
//! sees *frames*: one UTF-8 JSON-RPC message at a time. Historically those
//! frames came from a spawned child's OS pipes (`ChildStdin`/`ChildStdout`),
//! and the pipe-specific code lived inline in `AcpClient`. This module lifts
//! that coupling out so a second transport — an ExtensionFoundation app
//! extension speaking XPC, which is message-oriented rather than a byte pipe —
//! can implement the same trait without the protocol layer noticing.
//!
//! Framing is the channel's business, not the caller's: [`PipeChannel`]
//! adds/removes the newline that makes NDJSON, while an XPC-backed channel
//! sends one whole message per call. [`AgentChannel::recv_frame`] blocks on a
//! dedicated reader thread exactly like the old `BufReader<ChildStdout>` loop
//! did, and [`AgentChannel::send_frame`] is always called from
//! `AcpClient`'s dedicated writer thread, so a wedged peer can never freeze
//! the UI.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Cap on one incoming frame. Real replies are a few KB; a frame past this is
/// not a protocol we can parse anyway, so the reader treats it as a dead peer
/// rather than buffering without bound.
pub(crate) const MAX_FRAME_BYTES: usize = octos_llm::host::MAX_FRAME_BYTES;

/// Why a channel operation failed.
///
/// Deliberately transport-neutral: the caller maps all three onto the same
/// diagnostics it would have produced for a dead child, whether the peer was
/// a subprocess or an XPC extension.
#[derive(Debug)]
pub enum ChannelError {
    /// The peer closed the connection (EOF). Not necessarily an error.
    Closed,
    /// The transport failed for a reason other than a clean close.
    Transport(String),
    /// A frame exceeded the transport's size limit and was rejected.
    Oversized,
}

/// A duplex, framed channel to an agent.
///
/// Implementations are shared across `AcpClient`'s writer and reader threads
/// and torn down from `Drop`; the methods take `&self` and keep whatever
/// interior mutability they need. `Send + Sync` lets the same channel be
/// wrapped in an `Arc` and used from every thread.
pub trait AgentChannel: Send + Sync {
    /// Writes one frame. Blocking; only ever called from the dedicated writer
    /// thread, so blocking here cannot stall the UI.
    fn send_frame(&self, frame: &[u8]) -> Result<(), ChannelError>;

    /// Reads the next frame into `buf` (cleared first). Blocking; called from
    /// the dedicated reader thread. `Ok(())` means a frame is in `buf`.
    fn recv_frame(&self, buf: &mut Vec<u8>) -> Result<(), ChannelError>;

    /// Tears down the transport, unblocking any in-flight read or write. For a
    /// subprocess channel this kills the child; for an extension channel this
    /// invalidates the host's `AppExtensionProcess` connection.
    fn close(&self);

    /// Waits up to `timeout` for captured diagnostics (stderr) to finish
    /// flushing. The default transport has none.
    fn wait_diagnostics(&self, _timeout: Duration) {}

    /// The captured diagnostic tail, once [`Self::wait_diagnostics`] returns.
    fn diagnostics(&self) -> String {
        String::new()
    }
}

/// The subprocess transport: a spawned child's stdin/stdout, with stderr
/// drained on a third thread for diagnostics.
pub struct PipeChannel {
    child: Mutex<Child>,
    stdin: Mutex<Option<ChildStdin>>,
    stdout: Mutex<Option<BufReader<ChildStdout>>>,
    diagnostics: Arc<Mutex<Vec<String>>>,
    diagnostics_done: Arc<AtomicBool>,
}

impl PipeChannel {
    /// Spawns `command` with piped stdio and returns a channel over them.
    /// `desc` is only used to name the program in the spawn error.
    pub fn spawn(mut command: Command, desc: &str) -> Result<Self, String> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("couldn't start `{desc}`: {e}"))?;

        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let stdin = child.stdin.take().expect("piped stdin");

        let diagnostics = Arc::new(Mutex::new(Vec::new()));
        let diagnostics_done = Arc::new(AtomicBool::new(false));
        // Stderr drain: keep the tail for diagnostics (a missing provider
        // config makes the agent exit immediately with the reason on stderr).
        // Read lossily — a stray invalid-UTF-8 byte must not kill the drain.
        {
            let tail = diagnostics.clone();
            let done = diagnostics_done.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stderr);
                let mut buf = Vec::new();
                while crate::mcp::read_frame(&mut reader, 4096, &mut buf) {
                    let line = String::from_utf8_lossy(&buf);
                    let line = line.trim_end_matches(['\r', '\n']);
                    let mut tail = tail.lock().unwrap();
                    tail.push(line.to_string());
                    let excess = tail.len().saturating_sub(12);
                    if excess > 0 {
                        tail.drain(..excess);
                    }
                }
                done.store(true, Ordering::Release);
            });
        }

        Ok(Self {
            child: Mutex::new(child),
            stdin: Mutex::new(Some(stdin)),
            stdout: Mutex::new(Some(BufReader::new(stdout))),
            diagnostics,
            diagnostics_done,
        })
    }
}

impl AgentChannel for PipeChannel {
    fn send_frame(&self, frame: &[u8]) -> Result<(), ChannelError> {
        let mut guard = self.stdin.lock().unwrap();
        let Some(stdin) = guard.as_mut() else {
            return Err(ChannelError::Closed);
        };
        stdin
            .write_all(frame)
            .and_then(|_| stdin.write_all(b"\n"))
            .and_then(|_| stdin.flush())
            .map_err(|e| ChannelError::Transport(e.to_string()))
    }

    fn recv_frame(&self, buf: &mut Vec<u8>) -> Result<(), ChannelError> {
        let mut guard = self.stdout.lock().unwrap();
        let Some(reader) = guard.as_mut() else {
            return Err(ChannelError::Closed);
        };
        buf.clear();
        if !read_capped_line(reader, buf) {
            return Err(ChannelError::Closed);
        }
        if buf.len() >= MAX_FRAME_BYTES {
            return Err(ChannelError::Oversized);
        }
        Ok(())
    }

    fn close(&self) {
        // Kill FIRST: it takes no locks and closes the pipes, so any thread
        // blocked on the child (reader mid-read, writer mid-write) unwedges.
        // Then drop the write end so the writer channel-drain can exit.
        {
            let mut child = self.child.lock().unwrap();
            let _ = child.kill();
            let _ = child.wait();
        }
        self.stdin.lock().unwrap().take();
    }

    fn wait_diagnostics(&self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while !self.diagnostics_done.load(Ordering::Acquire) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn diagnostics(&self) -> String {
        self.diagnostics.lock().unwrap().join("\n")
    }
}

/// Reads one `\n`-terminated line into `buf` (cleared first), lossily and
/// capped at [`MAX_FRAME_BYTES`]. Returns false on EOF/error with nothing
/// read. A line hitting the cap is returned as-is (caller checks `buf.len()`).
pub(crate) fn read_capped_line(reader: &mut impl BufRead, buf: &mut Vec<u8>) -> bool {
    buf.clear();
    let mut limited = reader.take(MAX_FRAME_BYTES as u64);
    match limited.read_until(b'\n', buf) {
        Ok(0) => false,
        Ok(_) => {
            // Preserve the cap marker even when its last byte is whitespace.
            // Otherwise a frame ending this chunk with CR could be split and
            // accepted as several independently bounded protocol frames.
            if buf.len() >= MAX_FRAME_BYTES {
                return true;
            }
            while buf.last() == Some(&b'\n') || buf.last() == Some(&b'\r') {
                buf.pop();
            }
            true
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn capped_frame_cannot_hide_its_limit_with_trailing_carriage_return() {
        let mut input = vec![b'x'; MAX_FRAME_BYTES];
        input[MAX_FRAME_BYTES - 1] = b'\r';
        let mut buffer = Vec::new();
        assert!(read_capped_line(&mut std::io::Cursor::new(input), &mut buffer));
        assert_eq!(buffer.len(), MAX_FRAME_BYTES);
    }

    /// A pipe channel is a faithful duplex transport: what one end writes the
    /// other reads, one frame per line, and closing unblocks a pending read.
    #[cfg(unix)]
    #[test]
    fn pipe_channel_round_trips_newline_framed_bytes() {
        let dir = std::env::temp_dir().join("a2app_pipe_channel_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("echo.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\nwhile IFS= read -r line; do printf 'reply:%s\\n' \"$line\"; done\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let channel = PipeChannel::spawn(Command::new(&script), "echo test").unwrap();
        channel.send_frame(b"hello").unwrap();
        let mut buf = Vec::new();
        channel.recv_frame(&mut buf).unwrap();
        assert_eq!(buf, b"reply:hello");
        channel.close();
        // After close, a read observes the dead transport rather than blocking.
        let mut buf = Vec::new();
        assert!(channel.recv_frame(&mut buf).is_err());
    }
}
