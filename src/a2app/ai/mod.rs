//! AI agent sessions for mini-app rooms, and the MCP tool bridge they reach
//! host tools through.
//!
//! An agent session (one per "AI room") is a long-lived ACP session whose model
//! can call Robrix-provided tools. The plumbing, in three pieces:
//!
//! 1. [`tools`] — the host tools (`launch_splash_app`, `send_message`, …), as
//!    thin adapters from the model's argument shape to one [`AiHost`] call.
//!    Adding a tool is: define its `Tool` impl (name/description/schema/call),
//!    register it in `register_session_tools`. Nothing else changes.
//! 2. [`server`] — the session-scoped Unix-socket endpoint where the *real*
//!    Robrix process answers MCP. One server per agent session; the socket path
//!    is the session's capability (only the process told the path can connect),
//!    and the socket is torn down with the session.
//! 3. [`bridge`] — `robrix --mcp-bridge --socket <path>`: the child process an
//!    agent spawns as the MCP server. It is a dumb relay between its stdio and
//!    the socket in (2), so the agent's MCP client and Robrix's [`server`]
//!    negotiate directly with no second copy of any logic.
//! 4. [`session`] — the Robrix-side owner of one AI room's agent session: the
//!    tool server above, the real [`AiHost`] that executes its calls on the
//!    UI thread, and the long-lived ACP agent spawned pointed at that server.
//!
//! The wire protocol itself (framing, `initialize`/`tools/list`/`tools/call`
//! dispatch) lives in `a2app_agent::mcp` and is agent-agnostic.

pub mod tools;

#[cfg(unix)]
pub mod bridge;
#[cfg(unix)]
pub mod server;
#[cfg(unix)]
pub mod session;
/// AI Rooms: Matrix operations (create/mark/attach, forwarding cursor,
/// `ai_reply` writes) run on the async worker. Unix-only, like [`session`],
/// since a session is what a room attaches to.
#[cfg(unix)]
pub mod rooms;
