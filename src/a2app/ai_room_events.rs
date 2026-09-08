//! The Matrix event types and content shapes shared by AI Rooms, plus the
//! timeline widget that renders an `ai_reply`.
//!
//! An AI room is an ordinary room carrying a `rs.robius.robrix.ai_room`
//! marker state event; the agent's output is written back as
//! `rs.robius.robrix.ai_reply` state events (never `m.room.message`, so it
//! can't loop back as input). This module holds just the wire types and the
//! rendering side, so it can be used from `room_screen.rs` on every
//! platform; the session/forwarding machinery that actually *acts* on these
//! events (unix-only, see `ai::rooms`) builds on top of it.

use makepad_widgets::*;
use serde::{Deserialize, Serialize};

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

        reply_text := Label {
            width: Fill, height: Fit
            padding: 0, margin: 0
            flow: Flow.Right{wrap: true},
            draw_text +: {
                text_style: MESSAGE_TEXT_STYLE {},
                color: (MESSAGE_TEXT_COLOR)
            }
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

impl AiReplyTimelineCardRef {
    /// Populates the card from an `ai_reply` event's content.
    /// Re-set on every draw, since timeline items get recycled.
    pub fn populate(&self, cx: &mut Cx, content: Option<&AiReplyContent>) {
        let Some(inner) = self.borrow_mut() else { return };
        let text = content.map(|c| c.text.as_str()).unwrap_or("(unreadable reply)");
        inner.view.label(cx, ids!(reply_text)).set_text(cx, text);
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
        let reply_tools = inner.view.label(cx, ids!(reply_tools));
        reply_tools.set_visible(cx, !tools_line.is_empty());
        reply_tools.set_text(cx, &tools_line);
    }
}
