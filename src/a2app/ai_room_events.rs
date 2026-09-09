//! The Matrix event types and content shapes shared by AI Rooms, plus the
//! timeline widgets that render them.
//!
//! An AI room is an ordinary room carrying a `rs.robius.robrix.ai_room`
//! marker state event; the agent's output is written back as
//! `rs.robius.robrix.ai_reply` state events (never `m.room.message`, so it
//! can't loop back as input). Everything else the agent does while a turn is
//! live is also reflected as state events so the chat doubles as its
//! activity log: `rs.robius.robrix.ai_activity` rows mark "thinking…" /
//! errors / the session stopping, and each tool call is its own
//! `rs.robius.robrix.ai_tool_call` row (written `Started` when the agent
//! picks the tool and rewritten `Done` with its outcome when Robrix
//! finishes it). This module holds just the wire types and the rendering
//! side, so it can be used from `room_screen.rs` on every platform; the
//! session/forwarding machinery that actually *acts* on these events
//! (unix-only, see `ai::rooms`) builds on top of it.

use makepad_widgets::*;
use serde::{Deserialize, Serialize};

// For the `reply_body` HtmlOrPlaintext child accessor in the card's
// populate; `HtmlOrPlaintext` and its widget are shared from `crate::shared`.
use crate::shared::html_or_plaintext::HtmlOrPlaintextWidgetExt as _;

/// The state event that marks a room as an AI room. `state_key` is always
/// `""` (one marker per room). Only the room creator's own client ever
/// writes this (state-event power levels keep other members from forging
/// it), which is what lets the forwarder trust every marked room.
pub const AI_ROOM_EVENT_TYPE: &str = "rs.robius.robrix.ai_room";
/// One state event per completed agent turn: the agent's output, stored as
/// state (not a message) so it never loops back into the forwarder as input.
pub const AI_REPLY_EVENT_TYPE: &str = "rs.robius.robrix.ai_reply";
/// Room account data: the last-forwarded event id, so a restart doesn't
/// re-forward the whole transcript as new prompts.
pub const AI_SESSION_DATA_EVENT_TYPE: &str = "rs.robius.robrix.ai_session_data";
/// One append-only state event per notable *live* agent activity that is not
/// itself a reply: the model starting to think (shown as "thinking…"), a
/// turn that errored, or the session stopping. Each row gets its own fresh
/// state key, so the chat's timeline doubles as the agent's activity log
/// (same shape as the completed `ai_reply` rows, minus the prose).
pub const AI_ACTIVITY_EVENT_TYPE: &str = "rs.robius.robrix.ai_activity";
/// One state event per agent tool call. A fresh key is minted when the call
/// starts (`Started`); when Robrix finishes executing the call the same key
/// is rewritten with the outcome (`Done`, `ok`, `summary`) — a tool turn's
/// rows therefore show its full lifecycle in the timeline. This is what
/// makes every tool call a first-class, capability-visible event in the
/// room rather than a footnote on the final reply.
pub const AI_TOOL_CALL_EVENT_TYPE: &str = "rs.robius.robrix.ai_tool_call";

/// The content of the `rs.robius.robrix.ai_room` marker event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiRoomMarkerContent {
    pub v: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// One tool call summary attached to an [`AiReplyContent`], shown as a chip.
/// A granted call that ran is `ok: true`; a call the permission model refused
/// (or one whose fetch failed) is `ok: false` with the reason in `summary`,
/// so the room's transcript doubles as a permission receipt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiReplyToolCall {
    pub name: String,
    #[serde(default)]
    pub ok: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub summary: String,
}

/// The content of one `rs.robius.robrix.ai_reply` state event: one agent
/// turn's output (a completed reply, or a `send_message` tool call).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiReplyContent {
    pub v: u32,
    pub text: String,
    /// The reply rendered as HTML (Markdown → HTML, mentions/links as
    /// `matrix.to` anchors), when the text has any formatting worth it. Rendered
    /// like an ordinary rich message, so a user handle or a permalink the agent
    /// included becomes a clickable pill. `None` when the reply is plain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub formatted: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<AiReplyToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(rename = "createdAt")]
    pub created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<String>,
}

/// Room account data recording forwarding progress (see [`AI_SESSION_DATA_EVENT_TYPE`]).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AiSessionCursorContent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// Reserved for the id of the last `ai_reply` written; not yet tracked.
    #[serde(rename = "lastTurn", default, skip_serializing_if = "Option::is_none")]
    pub last_turn: Option<String>,
}

/// What one [`AiActivityContent`] row is about. The rows are append-only
/// markers of *what the agent is doing right now* that are not tool calls or
/// prose, so the set is deliberately small; anything with more structure gets
/// its own event type (a tool call is [`AiToolCallContent`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiActivityKind {
    /// The model is reasoning before it writes (a `Thought` stream started).
    Thinking,
    /// The turn failed with an error the room should see.
    Error,
    /// The agent process stopped (died, was powered off, or never started).
    Stopped,
}

/// The content of one `rs.robius.robrix.ai_activity` state event: a live
/// marker of something the agent is doing that the chat should show (e.g.
/// "thinking…"). Fresh state key per row — the timeline keeps the history,
/// exactly like it keeps every `ai_reply`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiActivityContent {
    pub v: u32,
    pub kind: AiActivityKind,
    /// Extra detail for `Error`/`Stopped` rows (the message); `None` for a
    /// plain `Thinking` marker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub created_at: u64,
}

/// The lifecycle state of one [`AiToolCallContent`] row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiToolCallStatus {
    /// The agent asked for the tool; Robrix is running (or gating) it.
    Started,
    /// Robrix finished executing the call (or refused it).
    Done,
}

/// The content of one `rs.robius.robrix.ai_tool_call` state event. The row is
/// written once as [`AiToolCallStatus::Started`] with a fresh state key when
/// the agent calls the tool, then the SAME key is rewritten as
/// [`AiToolCallStatus::Done`] with the outcome once Robrix executes (or
/// refuses) it — so the room's state always holds the latest status and the
/// timeline holds the full lifecycle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiToolCallContent {
    pub v: u32,
    /// The tool's name as the agent called it (e.g. `read_room_messages`,
    /// `launch_splash_app`, `send_message`).
    pub name: String,
    pub status: AiToolCallStatus,
    /// Whether the call succeeded; meaningful once `status` is `Done`.
    #[serde(default)]
    pub ok: bool,
    /// Why it failed (refusal/permission reason, error), or the success
    /// summary when the tool returns one. Empty when there is nothing to say.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub summary: String,
    pub created_at: u64,
}

script_mod! {
    use mod.prelude.widgets.*
    use mod.widgets.*

    // The timeline card for one `ai_reply` turn. Height is `Fit` (unlike the
    // fixed-height mini-app share card) since a reply's length varies.
    mod.widgets.AiReplyTimelineCard = set_type_default() do #(AiReplyTimelineCard::register_widget(vm)) {
        ..mod.widgets.RoundedView

        width: Fill,
        height: Fit,
        flow: Down
        spacing: 4
        padding: Inset{top: 8, bottom: 8, left: 12, right: 12}
        margin: Inset{top: 4, bottom: 4, left: 10, right: 60}

        show_bg: true
        draw_bg +: {
            color: #xEEF5FF
            border_color: (COLOR_DIVIDER_DARK)
            border_size: 1.0
            border_radius: 4.0
        }

        reply_label := Label {
            width: Fit, height: Fit
            padding: 0, margin: 0
            draw_text +: {
                text_style: theme.font_bold {font_size: 10},
                color: (COLOR_ACTIVE_PRIMARY)
            }
            text: "AI"
        }

        reply_body := HtmlOrPlaintext {
            width: Fill, height: Fit
            padding: 0, margin: 0
        }

        reply_tools := Label {
            width: Fill, height: Fit
            visible: false
            padding: 0, margin: 0
            flow: Flow.Right{wrap: true},
            draw_text +: {
                text_style: SMALL_STATE_TEXT_STYLE {},
                color: (SMALL_STATE_TEXT_COLOR)
            }
        }
    }

    // The card for one live `ai_activity` or `ai_tool_call` row: a single
    // muted line ("💭 thinking…", "⚙ read_room_messages ✓") that shows in
    // the chat whatever the agent is doing while it does it.
    mod.widgets.AiEventTimelineCard = set_type_default() do #(AiEventTimelineCard::register_widget(vm)) {
        ..mod.widgets.RoundedView

        width: Fill,
        height: Fit,
        flow: Down
        spacing: 4
        padding: Inset{top: 4, bottom: 4, left: 12, right: 12}
        margin: Inset{top: 2, bottom: 2, left: 10, right: 60}

        show_bg: true
        draw_bg +: {
            color: #xEEF5FF
            border_color: (COLOR_DIVIDER_DARK)
            border_size: 1.0
            border_radius: 4.0
        }

        event_label := Label {
            width: Fill, height: Fit
            visible: false
            padding: 0, margin: 0
            flow: Flow.Right{wrap: true},
            draw_text +: {
                text_style: SMALL_STATE_TEXT_STYLE {},
                color: (SMALL_STATE_TEXT_COLOR)
            }
        }
    }
}

#[derive(Script, ScriptHook, Widget)]
pub struct AiReplyTimelineCard {
    #[deref] view: View,
}

impl Widget for AiReplyTimelineCard {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.handle_event(cx, event, scope);
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        self.view.draw_walk(cx, scope, walk)
    }
}

/// Converts an agent's reply text into display HTML when it carries any
/// formatting: Markdown→HTML exactly like Robrix's own composer, so user
/// mentions written as `[Name](https://matrix.to/#/@user:server)` and message
/// links written as `[..](https://matrix.to/#/!room:server/$event)` come out
/// as clickable `matrix.to` anchors — which the timeline renders as pills.
/// Returns `None` when the text is plain (no headings, lists, emphasis, links
/// …), in which case the card renders the plain text path instead.
pub fn agent_reply_formatted_html(text: &str) -> Option<String> {
    use matrix_sdk::ruma::events::room::message::{MessageType, RoomMessageEventContent};
    let content = RoomMessageEventContent::text_markdown(text);
    match content.msgtype {
        MessageType::Text(m) => m.formatted.map(|f| f.body),
        MessageType::Notice(m) => m.formatted.map(|f| f.body),
        _ => None,
    }
}

impl AiReplyTimelineCardRef {
    /// Populates the card from an `ai_reply` event's content.
    /// Re-set on every draw, since timeline items get recycled.
    pub fn populate(&self, cx: &mut Cx, content: Option<&AiReplyContent>) {
        let Some(inner) = self.borrow_mut() else { return };
        let view = &inner.view;
        // The reply renders like an ordinary message: rich HTML when the text
        // had any Markdown (so a user handle or a permalink the agent included
        // becomes a clickable pill), plain text otherwise (bare URLs still get
        // linkified). See `agent_reply_formatted_html` for the rich side.
        let reply_body = view.html_or_plaintext(cx, ids!(reply_body));
        match content.and_then(|c| c.formatted.as_ref()) {
            Some(html) => {
                let html = crate::utils::linkify_get_urls(html, true, None);
                reply_body.show_html(cx, html);
            }
            None => {
                let text = content.map(|c| c.text.as_str()).unwrap_or("(unreadable reply)");
                match crate::utils::linkify_get_urls(text, false, None) {
                    std::borrow::Cow::Owned(linkified) => reply_body.show_html(cx, linkified),
                    std::borrow::Cow::Borrowed(_) => reply_body.show_plaintext(cx, text),
                }
            }
        }
        let tools_line = content
            .filter(|c| !c.tool_calls.is_empty())
            .map(|c| {
                let parts: Vec<String> = c.tool_calls.iter().map(|t| {
                    let mut name = t.name.clone();
                    if !t.ok {
                        name.push_str(" ✗");
                        if !t.summary.is_empty() {
                            name.push_str(&format!(" ({})", t.summary));
                        }
                    }
                    name
                }).collect();
                format!("Used: {}", parts.join(", "))
            })
            .unwrap_or_default();
        let reply_tools = view.label(cx, ids!(reply_tools));
        reply_tools.set_visible(cx, !tools_line.is_empty());
        reply_tools.set_text(cx, &tools_line);
    }
}

/// The card that renders one live agent-activity row: an `ai_activity`
/// marker (thinking started / error / stopped) or an `ai_tool_call` row
/// (started, or done with its outcome). Both are small muted one-liners so a
/// busy turn reads as a short log in the chat instead of a wall of cards.
#[derive(Script, ScriptHook, Widget)]
pub struct AiEventTimelineCard {
    #[deref] view: View,
}

impl Widget for AiEventTimelineCard {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.handle_event(cx, event, scope);
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        self.view.draw_walk(cx, scope, walk)
    }
}

/// The one-line text an [`AiActivityContent`] row renders as.
pub fn ai_activity_label(content: &AiActivityContent) -> String {
    match content.kind {
        AiActivityKind::Thinking => String::from("💭 thinking…"),
        AiActivityKind::Error => {
            format!("⚠ AI session error{}", {
                let msg = content.label.as_deref().unwrap_or_default();
                if msg.is_empty() {
                    String::new()
                } else {
                    format!(": {msg}")
                }
            })
        }
        AiActivityKind::Stopped => {
            format!("⏹ The AI in this room stopped{}", {
                let msg = content.label.as_deref().unwrap_or_default();
                if msg.is_empty() {
                    String::new()
                } else {
                    format!(": {msg}")
                }
            })
        }
    }
}

/// The one-line text an [`AiToolCallContent`] row renders as.
pub fn ai_tool_call_label(content: &AiToolCallContent) -> String {
    let name = content.name.trim();
    match content.status {
        AiToolCallStatus::Started => {
            let name = if name.is_empty() { "a tool" } else { name };
            format!("⚙ using {name}…")
        }
        AiToolCallStatus::Done => {
            let name = if name.is_empty() { "a tool" } else { name };
            if content.ok {
                if content.summary.is_empty() {
                    format!("✓ used {name}")
                } else {
                    format!("✓ used {name}: {}", content.summary)
                }
            } else if content.summary.is_empty() {
                format!("✗ {name} refused")
            } else {
                format!("✗ {name}: {}", content.summary)
            }
        }
    }
}

impl AiEventTimelineCardRef {
    /// Populates the card from an `ai_activity` event's content (hiding it
    /// when there is nothing to show). Re-set on every draw, since timeline
    /// items get recycled.
    pub fn populate_activity(&self, cx: &mut Cx, content: Option<&AiActivityContent>) {
        let text = content.map(ai_activity_label).unwrap_or_default();
        self.populate_text(cx, &text);
    }

    /// Populates the card from an `ai_tool_call` event's content (hiding it
    /// when there is nothing to show). Re-set on every draw, since timeline
    /// items get recycled.
    pub fn populate_tool_call(&self, cx: &mut Cx, content: Option<&AiToolCallContent>) {
        let text = content.map(ai_tool_call_label).unwrap_or_default();
        self.populate_text(cx, &text);
    }

    fn populate_text(&self, cx: &mut Cx, text: &str) {
        let Some(inner) = self.borrow_mut() else { return };
        let view = &inner.view;
        let label = view.label(cx, ids!(event_label));
        label.set_visible(cx, !text.is_empty());
        label.set_text(cx, text);
    }
}
