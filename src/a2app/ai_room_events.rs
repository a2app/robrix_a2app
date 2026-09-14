//! The Matrix event types and content shapes shared by AI Rooms, plus the
//! timeline widgets that render them.
//!
//! An AI room is an ordinary room carrying a `rs.robius.robrix.ai_room`
//! marker state event; the agent's output into its own room is written back
//! as `rs.robius.robrix.ai_reply` state events (never `m.room.message`, so it
//! can't loop back as input). Cross-room posts (`post_room_message`) are
//! ordinary `m.notice` messages instead. Everything else the agent does while
//! a turn is live is also reflected as state events so the chat doubles as its
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
///
/// New turns no longer write these rows: a turn's calls are aggregated into
/// its single [`AI_TURN_EVENT_TYPE`] card, so the chat shows one collapsible
/// widget per assistant turn instead of a scatter of one-liners. The
/// per-call event type is kept only so rooms written by older builds still
/// render (each old row as its own small card).
pub const AI_TOOL_CALL_EVENT_TYPE: &str = "rs.robius.robrix.ai_tool_call";
/// One state event per assistant turn that called at least one tool: the
/// ordered tool calls the turn made, plus whether the turn is still running.
/// A fresh key is minted on the turn's first tool call and the SAME key is
/// rewritten as calls start, gain their target detail, and finish; the turn
/// is finally rewritten `Done` when its reply (or error) arrives. State events
/// are not replaced in a Matrix timeline, so each rewrite is its own event;
/// the renderer shows only the latest `turn` id and hides the earlier
/// snapshots, yielding exactly one collapsible card per turn.
pub const AI_TURN_EVENT_TYPE: &str = "rs.robius.robrix.ai_turn";

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
    /// Human-readable target/detail for the call, when there is one (the room
    /// name for a cross-room read or post, the space name, the generated
    /// app's description). Rendered after the humanized action
    /// ([`ai_tool_display_name`]): `Read messages in “General”`. `None` when
    /// the call has no target worth naming.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
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
///
/// Legacy: new turns aggregate their calls into [`AiTurnContent`] instead.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiToolCallContent {
    pub v: u32,
    /// The tool's name as the agent called it (e.g. `read_room_messages`,
    /// `launch_splash_app`, `send_message`). The chat never shows this raw;
    /// [`ai_tool_display_name`] turns it into a phrase.
    pub name: String,
    /// Human-readable target/detail, as on [`AiReplyToolCall::detail`]: the
    /// room name for a cross-room read or post, the space name, the generated
    /// app's description. `None` when the call has no particular target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
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

/// Whether the whole turn is still in flight. Drives the turn card's colour
/// and spinner: `Running` is the warm "working" state, `Done` the settled
/// one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiTurnStatus {
    Running,
    Done,
}

/// The lifecycle of one tool call inside an [`AiTurnContent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiTurnToolStatus {
    Started,
    Done,
}

/// One tool call a turn made, as shown inside the turn's collapsible card.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiTurnToolCall {
    /// The tool's raw MCP name (see [`ai_tool_display_name`]).
    pub name: String,
    /// Human-readable target/detail, as on [`AiReplyToolCall::detail`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub status: AiTurnToolStatus,
    /// Whether the call succeeded; meaningful once `status` is `Done`.
    #[serde(default)]
    pub ok: bool,
    /// Why it failed, or its success summary. Empty when nothing to say.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub summary: String,
}

/// The content of one `rs.robius.robrix.ai_turn` state event: every tool call
/// an assistant turn made, in the order they started, plus whether the turn is
/// still running. Written on the first tool call and rewritten in place until
/// the turn's reply (or error) marks it `Done`, so the timeline shows one
/// collapsible widget per turn that changes colour and shows a spinner while
/// the agent works.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiTurnContent {
    pub v: u32,
    /// The turn's stable id (the state key every rewrite of this turn reuses).
    /// The timeline keeps every rewrite as its own event, so the renderer
    /// shows only the latest snapshot of a turn and hides the earlier ones —
    /// one collapsible card per turn, never a stack of them.
    #[serde(default)]
    pub turn: String,
    /// Whether this is the FIRST snapshot of its turn (the state-key row the
    /// turn was created with). The renderer anchors the card at this row and
    /// reads the latest snapshot's content for it, so the card never jumps
    /// position as the turn progresses. `None` on rows written before this
    /// field existed, which fall back to the latest-scan behaviour.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first: Option<bool>,
    pub status: AiTurnStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<AiTurnToolCall>,
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

    // A small spinning arc, animated by a looping animator (the built-in
    // LoadingSpinner cannot be used here: it is driven by `draw_pass.time`,
    // which only advances on frames something else requests). Shown only
    // while a turn is running.
    mod.widgets.AiTurnSpinner = #(AiTurnSpinner::register_widget(vm)) {
        width: 13, height: 13,
        show_bg: true,
        draw_bg +: {
            color: uniform(#x0f88fe)
            rotation: uniform(0.0)
            pixel: fn() {
                let sdf = Sdf2d.viewport(self.pos * self.rect_size)
                let radius = min(self.rect_size.x, self.rect_size.y) * 0.5 - 1.5
                let center = self.rect_size * 0.5
                let start = self.rotation * 2.0 * PI
                sdf.arc_round_caps(center.x, center.y, radius, start, start + 2.0 * PI * 0.72, 2.0)
                return sdf.fill(self.color)
            }
        }
        animator: Animator {
            spin: {
                default: @off
                off: AnimatorState {
                    redraw: true,
                    from: {all: Forward {duration: 0.0}}
                    apply: { draw_bg: {rotation: 0.0} }
                }
                on: AnimatorState {
                    redraw: true,
                    from: {all: Loop {duration: 1.0, end: 1.0}}
                    apply: { draw_bg: {rotation: 1.0} }
                }
            }
        }
    }

    // One assistant turn's tool calls, grouped into a single collapsible
    // card. While the turn runs the card is warm-tinted and shows a spinner;
    // once the turn's reply (or error) lands it settles to the neutral tint
    // and the spinner disappears. Clicking the header toggles the tool list.
    mod.widgets.AiTurnTimelineCard = set_type_default() do #(AiTurnTimelineCard::register_widget(vm)) {
        ..mod.widgets.RoundedView

        width: Fill,
        height: Fit,
        flow: Down
        spacing: 5
        padding: Inset{top: 6, bottom: 6, left: 10, right: 10}
        margin: Inset{top: 2, bottom: 2, left: 10, right: 60}

        show_bg: true
        draw_bg +: {
            color: #xEEF5FF
            border_color: (COLOR_DIVIDER_DARK)
            border_size: 1.0
            border_radius: 4.0
        }

        header := View {
            width: Fill, height: Fit
            flow: Right, align: Align{y: 0.5}, spacing: 6
            cursor: MouseCursor.Hand

            turn_arrow := Label {
                width: Fit, height: Fit
                padding: 0, margin: 0
                draw_text +: {
                    text_style: SMALL_STATE_TEXT_STYLE {},
                    color: (SMALL_STATE_TEXT_COLOR)
                }
                text: "▸"
            }
            turn_spinner := mod.widgets.AiTurnSpinner {}
            turn_title := Label {
                width: Fill, height: Fit
                padding: 0, margin: 0
                flow: Flow.Right{wrap: true},
                draw_text +: {
                    text_style: SMALL_STATE_TEXT_STYLE {},
                    color: (SMALL_STATE_TEXT_COLOR)
                }
            }
        }

        turn_body := Label {
            width: Fill, height: Fit
            visible: false
            padding: 0, margin: Inset{left: 19}
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
                    let mut label = format!("{}{}", ai_tool_display_name(&t.name), detail_suffix(t.detail.as_deref()));
                    if !t.ok {
                        label.push_str(" ✗");
                        if !t.summary.is_empty() {
                            label.push_str(&format!(" ({})", t.summary));
                        }
                    }
                    label
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

/// The little looping spinner shown on a running turn card.
#[derive(Script, ScriptHook, Widget, Animator)]
pub struct AiTurnSpinner {
    #[source] source: ScriptObjectRef,
    #[deref] view: View,
    #[apply_default] animator: Animator,
}

impl Widget for AiTurnSpinner {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        if self.animator_handle_event(cx, event).must_redraw() {
            self.redraw(cx);
        }
        self.view.handle_event(cx, event, scope);
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        self.view.draw_walk(cx, scope, walk)
    }
}

impl AiTurnSpinnerRef {
    /// Starts (`spinning`) or stops the spinner's looping animation. Called by
    /// the turn card as the turn's status changes.
    pub fn set_spinning(&self, cx: &mut Cx, spinning: bool) {
        if let Some(mut inner) = self.borrow_mut() {
            inner.animator_play(cx, if spinning { ids!(spin.on) } else { ids!(spin.off) });
        }
    }
}

/// The one collapsible card an assistant turn's tool calls are grouped into.
///
/// The header carries an expand/collapse arrow, the running spinner, and a
/// title (`Working…` while the turn is live, `Used N tools` once it settles);
/// the body lists each call, one per line. The card's tint and the spinner's
/// visibility are driven by [`AiTurnContent::status`], so a glance at the
/// timeline tells a running turn from a finished one.
#[derive(Script, ScriptHook, Widget)]
pub struct AiTurnTimelineCard {
    #[deref] view: View,
    /// Whether the tool list is expanded. Preserved across repopulates so a
    /// turn card the user opened does not snap shut on the next live update.
    #[rust(true)] is_expanded: bool,
    /// Whether this turn has any tool calls to show. Kept so the expand state
    /// can be applied without re-reading the body label (whose `text()`
    /// accessor is not available on `LabelRef`).
    #[rust] has_tools: bool,
}

impl Widget for AiTurnTimelineCard {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        if let Hit::FingerUp(fe) = event.hits(cx, self.view.area()) {
            if fe.is_over && fe.is_primary_hit() && fe.was_tap() {
                self.is_expanded = !self.is_expanded;
                self.apply_expanded(cx);
                self.redraw(cx);
            }
        }
        self.view.handle_event(cx, event, scope);
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        self.view.draw_walk(cx, scope, walk)
    }
}

impl AiTurnTimelineCard {
    /// Applies the current expand state to the arrow and body.
    fn apply_expanded(&mut self, cx: &mut Cx) {
        self.view.label(cx, ids!(turn_arrow)).set_text(cx, if self.is_expanded { "▾" } else { "▸" });
        self.view.label(cx, ids!(turn_body)).set_visible(cx, self.is_expanded && self.has_tools);
    }
}

/// The phrase a turn card's header shows.
fn ai_turn_title(content: &AiTurnContent) -> String {
    let n = content.tool_calls.len();
    match content.status {
        AiTurnStatus::Running => {
            if n <= 1 {
                String::from("Working…")
            } else {
                format!("Working… ({n} tools)")
            }
        }
        AiTurnStatus::Done => match n {
            0 => String::from("Used a tool"),
            1 => String::from("Used 1 tool"),
            _ => format!("Used {n} tools"),
        },
    }
}

/// The one-line text one tool call inside a turn card renders as. Mirrors
/// [`ai_tool_call_label`] but without the per-call status glyph, since a
/// leading `✓`/`✗` reads better in a list than the `⚙`/`✓` pair.
pub fn ai_turn_tool_label(call: &AiTurnToolCall) -> String {
    let action = match call.status {
        AiTurnToolStatus::Started => ai_tool_display_name_running(&call.name),
        AiTurnToolStatus::Done => ai_tool_display_name(&call.name),
    };
    let detail = detail_suffix(call.detail.as_deref());
    match call.status {
        AiTurnToolStatus::Started => format!("· {action}{detail}…"),
        AiTurnToolStatus::Done => {
            if call.ok {
                if call.summary.is_empty() {
                    format!("✓ {action}{detail}")
                } else {
                    format!("✓ {action}{detail}: {}", call.summary)
                }
            } else if call.summary.is_empty() {
                format!("✗ {action}{detail} refused")
            } else {
                format!("✗ {action}{detail}: {}", call.summary)
            }
        }
    }
}

/// Like [`ai_turn_tool_label`], but for a turn that has SETTLED: a call that
/// never reported an outcome (the turn was cancelled, aborted, or the app
/// restarted mid-call) is shown as interrupted instead of still running — so a
/// settled card never contains a dangling "Reading a web page…" line.
pub fn ai_turn_tool_label_settled(call: &AiTurnToolCall) -> String {
    if call.status == AiTurnToolStatus::Started {
        let action = ai_tool_display_name(&call.name);
        let detail = detail_suffix(call.detail.as_deref());
        format!("⊘ {action}{detail} (interrupted)")
    } else {
        ai_turn_tool_label(call)
    }
}

impl AiTurnTimelineCardRef {
    /// Populates the turn card from an `ai_turn` event's content. Re-set on
    /// every draw, since timeline items get recycled.
    pub fn populate(&self, cx: &mut Cx, content: Option<&AiTurnContent>) {
        let Some(content) = content else {
            if let Some(mut inner) = self.borrow_mut() {
                inner.view.set_visible(cx, false);
            }
            return;
        };
        let running = content.status == AiTurnStatus::Running;
        // Warm while working, cool once settled. The border follows the same
        // family so the card reads as one object in either state.
        let (bg, border) = if running {
            (vec4(1.0, 0.972, 0.902, 1.0), vec4(0.98, 0.82, 0.45, 1.0))
        } else {
            (vec4(0.933, 0.961, 1.0, 1.0), vec4(0.78, 0.78, 0.8, 1.0))
        };
        // Applied on the card's own widget ref (not the inner `view`), since
        // `draw_bg` lives on the widget's script object. Must run before the
        // `borrow_mut` below, or the script call re-borrows the same RefCell.
        let mut card = self.clone();
        script_apply_eval!(cx, card, {
            draw_bg +: {
                color: #(bg)
                border_color: #(border)
            }
        });
        // Start/stop the spinner's loop with the turn, so a settled card stops
        // scheduling animation frames. The built-in LoadingSpinner is avoided
        // here because it is driven by `draw_pass.time` and would sit frozen
        // between unrelated redraws; this one animates itself while running.
        self.widget(cx, ids!(turn_spinner))
            .as_ai_turn_spinner()
            .set_spinning(cx, running);
        let Some(mut inner) = self.borrow_mut() else { return };
        inner.view.set_visible(cx, true);
        inner.view.label(cx, ids!(turn_title)).set_text(cx, &ai_turn_title(content));
        inner.view.widget(cx, ids!(turn_spinner)).set_visible(cx, running);
        let body = content
            .tool_calls
            .iter()
            .map(|call| {
                if running {
                    ai_turn_tool_label(call)
                } else {
                    ai_turn_tool_label_settled(call)
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        inner.has_tools = !content.tool_calls.is_empty();
        inner.view.label(cx, ids!(turn_body)).set_text(cx, &body);
        inner.apply_expanded(cx);
        inner.view.redraw(cx);
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

/// The human phrase the chat shows for a tool's raw MCP name, so a row reads
/// as what the AI *did* rather than as an identifier: `read_room_memory` →
/// "Read room memory", `list_space_rooms` → "Listed a space's rooms".
///
/// Known tools get a curated phrase; an unknown name (a future tool, or one
/// this build doesn't know) falls back to a generic `snake_case`-to-words
/// humanization, so it still reads as a phrase rather than leaking the raw
/// identifier.
pub fn ai_tool_display_name(name: &str) -> String {
    match name.trim() {
        "read_room_messages" => String::from("Read recent messages"),
        "read_older_messages" => String::from("Read older messages"),
        "room_info" => String::from("Read room details"),
        "read_other_room_messages" => String::from("Read messages"),
        "list_rooms" => String::from("Listed your rooms"),
        "list_spaces" => String::from("Listed your spaces"),
        "space_info" => String::from("Read space details"),
        "list_space_rooms" => String::from("Listed a space's rooms"),
        "read_room_memory" => String::from("Read room memory"),
        "launch_splash_app" => String::from("Built and ran a mini-app"),
        "list_apps" => String::from("Listed your mini-apps"),
        "launch_app" => String::from("Launched a mini-app"),
        "send_message" => String::from("Replied"),
        "post_room_message" => String::from("Posted a message"),
        // octos's own tools, kept on the session's profile (see
        // `a2app_agent::robrix_session_profile`). Robrix doesn't execute
        // these; the agent closes their card itself.
        "web_search" => String::from("Searched the web"),
        "web_fetch" => String::from("Read a web page"),
        "browser" => String::from("Browsed the web"),
        "" => String::from("Used a tool"),
        other => humanize_identifier(other),
    }
}

/// The present-progressive phrase for a tool that is STILL RUNNING, so a live
/// row reads `Replying in this room…` rather than `Replied in this room…`.
/// Only the verb differs from [`ai_tool_display_name`]; unknown tools keep the
/// humanized fallback (there is no safe way to derive an `-ing` form).
pub fn ai_tool_display_name_running(name: &str) -> String {
    match name.trim() {
        "read_room_messages" => String::from("Reading recent messages"),
        "read_older_messages" => String::from("Reading older messages"),
        "room_info" => String::from("Reading room details"),
        "read_other_room_messages" => String::from("Reading messages"),
        "list_rooms" => String::from("Listing your rooms"),
        "list_spaces" => String::from("Listing your spaces"),
        "space_info" => String::from("Reading space details"),
        "list_space_rooms" => String::from("Listing a space's rooms"),
        "read_room_memory" => String::from("Reading room memory"),
        "launch_splash_app" => String::from("Building and running a mini-app"),
        "list_apps" => String::from("Listing your mini-apps"),
        "launch_app" => String::from("Launching a mini-app"),
        "send_message" => String::from("Replying"),
        "post_room_message" => String::from("Posting a message"),
        "web_search" => String::from("Searching the web"),
        "web_fetch" => String::from("Reading a web page"),
        "browser" => String::from("Browsing the web"),
        "" => String::from("Using a tool"),
        other => humanize_identifier(other),
    }
}

/// Falls back to words for an identifier this build doesn't have a curated
/// phrase for: underscores become spaces and the first letter is capitalized
/// (`some_new_tool` → "Some new tool").
fn humanize_identifier(name: &str) -> String {
    let words = name.replace(['_', '-'], " ");
    let trimmed = words.trim();
    let mut chars = trimmed.chars();
    match chars.next() {
        None => String::from("Used a tool"),
        Some(first) => {
            let mut out: String = first.to_ascii_uppercase().to_string();
            out.push_str(chars.as_str());
            out
        }
    }
}

/// The `detail` rendered after an action phrase, with a leading space, or the
/// empty string when there is none.
fn detail_suffix(detail: Option<&str>) -> String {
    match detail.map(str::trim).filter(|d| !d.is_empty()) {
        Some(detail) => format!(" {detail}"),
        None => String::new(),
    }
}

/// The one-line text an [`AiToolCallContent`] row renders as: the humanized
/// action plus whatever target detail the call carried (`Read messages in
/// “General”`, `Built and ran a mini-app “a pomodoro timer”`).
pub fn ai_tool_call_label(content: &AiToolCallContent) -> String {
    let action = match content.status {
        AiToolCallStatus::Started => ai_tool_display_name_running(&content.name),
        AiToolCallStatus::Done => ai_tool_display_name(&content.name),
    };
    let detail = detail_suffix(content.detail.as_deref());
    match content.status {
        AiToolCallStatus::Started => format!("⚙ {action}{detail}…"),
        AiToolCallStatus::Done => {
            if content.ok {
                if content.summary.is_empty() {
                    format!("✓ {action}{detail}")
                } else {
                    format!("✓ {action}{detail}: {}", content.summary)
                }
            } else if content.summary.is_empty() {
                format!("✗ {action}{detail} refused")
            } else {
                format!("✗ {action}{detail}: {}", content.summary)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The chat never shows a raw MCP identifier: every known tool gets a
    /// phrase, and an unknown one humanizes instead of leaking its name.
    #[test]
    fn tool_names_render_as_phrases() {
        assert_eq!(ai_tool_display_name("read_room_memory"), "Read room memory");
        assert_eq!(ai_tool_display_name("read_other_room_messages"), "Read messages");
        assert_eq!(ai_tool_display_name("list_space_rooms"), "Listed a space's rooms");
        assert_eq!(ai_tool_display_name("send_message"), "Replied");
        assert_eq!(ai_tool_display_name("web_search"), "Searched the web");
        assert_eq!(ai_tool_display_name("web_fetch"), "Read a web page");
        assert_eq!(ai_tool_display_name("some_new_tool"), "Some new tool");
        assert_eq!(ai_tool_display_name("   "), "Used a tool");
    }

    /// A call row carries its target detail after the action phrase, so a
    /// cross-room read names the room it looked in.
    #[test]
    fn labels_carry_the_target_detail() {
        let content = AiToolCallContent {
            v: 1,
            name: "read_other_room_messages".to_string(),
            detail: Some("in “General”".to_string()),
            status: AiToolCallStatus::Done,
            ok: true,
            summary: String::new(),
            created_at: 0,
        };
        assert_eq!(ai_tool_call_label(&content), "✓ Read messages in “General”");
    }

    /// A turn card's title and per-call lines reflect the running/done state,
    /// so a glance tells a live turn from a finished one.
    #[test]
    fn turn_card_labels_track_the_turn_state() {
        let running = AiTurnContent {
            v: 1,
            turn: "turn-1".to_string(),
            first: Some(true),
            status: AiTurnStatus::Running,
            tool_calls: vec![
                AiTurnToolCall {
                    name: "web_search".to_string(),
                    detail: None,
                    status: AiTurnToolStatus::Started,
                    ok: false,
                    summary: String::new(),
                },
                AiTurnToolCall {
                    name: "read_room_messages".to_string(),
                    detail: None,
                    status: AiTurnToolStatus::Done,
                    ok: true,
                    summary: String::new(),
                },
                AiTurnToolCall {
                    name: "send_message".to_string(),
                    detail: Some("in this room".to_string()),
                    status: AiTurnToolStatus::Started,
                    ok: false,
                    summary: String::new(),
                },
            ],
            created_at: 0,
        };
        assert_eq!(ai_turn_title(&running), "Working… (3 tools)");
        assert_eq!(ai_turn_tool_label(&running.tool_calls[0]), "· Searching the web…");
        assert_eq!(ai_turn_tool_label(&running.tool_calls[1]), "✓ Read recent messages");
        // A running call uses the present-progressive phrase, not the past one.
        assert_eq!(ai_turn_tool_label(&running.tool_calls[2]), "· Replying in this room…");
        // A call still `Started` when the turn settles is shown as interrupted,
        // not as still working.
        assert_eq!(
            ai_turn_tool_label_settled(&running.tool_calls[0]),
            "⊘ Searched the web (interrupted)"
        );

        let mut done = AiTurnContent { status: AiTurnStatus::Done, ..running };
        assert_eq!(ai_turn_title(&done), "Used 3 tools");
        done.tool_calls[2].status = AiTurnToolStatus::Done;
        done.tool_calls[2].ok = true;
        assert_eq!(ai_turn_tool_label(&done.tool_calls[2]), "✓ Replied in this room");
        let one = AiTurnContent { tool_calls: done.tool_calls[..1].to_vec(), ..done };
        assert_eq!(ai_turn_title(&one), "Used 1 tool");
    }
}
