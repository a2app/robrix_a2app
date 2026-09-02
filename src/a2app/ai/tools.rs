//! The host tools Robrix exposes to one agent session.
//!
//! Each session builds these with an [`AiHost`] — the Robrix-side object that
//! actually does the work (run the app-generation pipeline, post to the
//! session's room). The [`Tool`] impls below are deliberately thin: they exist
//! to give the model the right *shape* to call (name, description, argument
//! schema) and to hand the parsed arguments to the host. All the state lives
//! in the host, which is what keeps this list trivially extensible.

use std::sync::Arc;

use a2app_agent::mcp::Tool;
use serde_json::{Map, Value, json};

/// What Robrix can do for a session's tools.
///
/// Implemented by the session host on the UI thread — the only place with the
/// state a tool needs (the app registry to install into, the Matrix room to
/// post to, the running `Generation`). Each method returns the text payload the
/// model sees: `Ok` text for a success, `Err` text for a failure the model
/// should be told about. `launch_splash_app`'s success payload is a small JSON
/// summary the model can quote; everything else is prose.
pub trait AiHost: Send + Sync {
    /// Generates and installs a room-scoped mini-app from a natural-language
    /// description, then runs it. Returns a structured summary on success:
    /// `{"app_id":…,"name":…,"status":"installed_and_running"}`.
    fn launch_splash_app(&self, description: &str) -> Result<String, String>;

    /// Posts `text` into the room associated with this session.
    fn send_room_message(&self, text: &str) -> Result<String, String>;
}

/// `launch_splash_app` — generate a sandboxed mini-app and run it in the
/// session's room.
///
/// The description is everything: the pipeline's own prompt (dialect guide,
/// validation, up to two repair turns) runs against it untouched, exactly as
/// the Mini Apps screen's direct generation does today — this tool is that
/// pipeline, called by the model instead of by a button.
pub struct LaunchSplashAppTool {
    host: Arc<dyn AiHost>,
}

impl LaunchSplashAppTool {
    pub fn new(host: Arc<dyn AiHost>) -> Self {
        Self { host }
    }
}

impl Tool for LaunchSplashAppTool {
    fn name(&self) -> &str {
        "launch_splash_app"
    }

    fn description(&self) -> &str {
        "Builds a sandboxed mini-app from a natural-language description and \
         installs it into this room, then runs it. Call this when the user asks \
         to create, build, make, or modify an app. The result is a JSON summary \
         of the installed app."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "description": {
                    "type": "string",
                    "description": "What the app should do, in the user's own words.",
                },
            },
            "required": ["description"],
            "additionalProperties": false,
        })
    }

    fn call(&self, arguments: &Map<String, Value>) -> Result<String, String> {
        let description = arguments
            .get("description")
            .and_then(Value::as_str)
            .ok_or_else(|| "`launch_splash_app` needs a string `description`".to_string())?
            .trim();
        if description.is_empty() {
            return Err("`description` must not be empty".to_string());
        }
        self.host.launch_splash_app(description)
    }
}

/// `send_message` — post an agent-authored message to the session's room.
///
/// This is how the agent talks to the user. It exists so a session can reply
/// in prose at all; without it the model's only output channels are tool calls
/// and the fenced-code reply contract of the app generator.
pub struct SendMessageTool {
    host: Arc<dyn AiHost>,
}

impl SendMessageTool {
    pub fn new(host: Arc<dyn AiHost>) -> Self {
        Self { host }
    }
}

impl Tool for SendMessageTool {
    fn name(&self) -> &str {
        "send_message"
    }

    fn description(&self) -> &str {
        "Posts a plain-text message to this room's timeline. Use it to answer \
         the user's questions and report progress or results in words. For \
         building an app, use launch_splash_app instead."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "text": {
                    "type": "string",
                    "description": "The message text to post.",
                },
            },
            "required": ["text"],
            "additionalProperties": false,
        })
    }

    fn call(&self, arguments: &Map<String, Value>) -> Result<String, String> {
        let text = arguments
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| "`send_message` needs a string `text`".to_string())?;
        self.host.send_room_message(text)
    }
}

/// Builds a session's full tool set and registers it on `server`.
///
/// This is the "add a tool" seam in one place: a new tool is a `Tool` impl
/// plus one line here, and every agent (octos and Claude Code alike, via the
/// bridge) sees it on the next `tools/list`.
pub fn register_session_tools(server: &mut a2app_agent::mcp::McpServer, host: Arc<dyn AiHost>) {
    server.add_tool(LaunchSplashAppTool::new(host.clone()));
    server.add_tool(SendMessageTool::new(host));
}
