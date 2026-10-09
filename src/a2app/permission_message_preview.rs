//! A local, bounded preview of the message awaiting permission.

use makepad_widgets::*;
use crate::shared::html_or_plaintext::{HtmlOrPlaintextWidgetExt, MatrixHtmlSpan};
use super::permission_prompt::PermissionMessagePreview;

script_mod! {
    use mod.prelude.widgets.*
    use mod.widgets.*

    mod.widgets.PermissionMessageLink = #(PermissionMessageLink::register_widget(vm)) {
        ..mod.widgets.HtmlLink
        grab_key_focus: false
    }
    mod.widgets.PermissionMessageSpan = #(PermissionMessageSpan::register_widget(vm)) {
        ..mod.widgets.MatrixHtmlSpan
        grab_key_focus: false
    }

    mod.widgets.PermissionMessageContent = #(PermissionMessageContent::register_widget(vm)) {
        width: Fill, height: Fit
        draw_ellipsis: mod.draw.DrawText {
            color: (MESSAGE_TEXT_COLOR)
            ink_centered: false
            text_style: MESSAGE_TEXT_STYLE { font_size: (MESSAGE_FONT_SIZE) }
        }
        inset := RoundedView {
            width: Fill, height: Fit, padding: 12
            draw_bg +: { color: (COLOR_BG_PREVIEW), border_radius: 4.0 }
            content := HtmlOrPlaintext {
                html_view +: {
                    html +: {
                        font_size: (MESSAGE_FONT_SIZE)
                        max_lines: 3
                        text_overflow: Clip
                        paragraph_margin: Inset{top: 0.33, bottom: 0.33}
                        quote_layout +: { padding: Inset{left: 15, top: 0, bottom: 0} }
                        quote_walk +: { margin: 0 }
                        code_layout +: { padding: Inset{left: 15, right: 5, top: 0, bottom: 0} }
                        code_walk +: { margin: 0 }
                        list_item_layout +: { padding: Inset{left: 5, top: 0, bottom: 0} }
                        list_item_walk +: { margin: 0 }
                        table_cell_layout +: { padding: Inset{left: 6, right: 6, top: 0, bottom: 0} }
                        // Matrix pills can fetch room/profile/avatar data while drawing.
                        // Permission previews use local text links instead.
                        a := mod.widgets.PermissionMessageLink {}
                        font := mod.widgets.PermissionMessageSpan {}
                        span := mod.widgets.PermissionMessageSpan {}
                    }
                }
                plaintext_view +: {
                    pt_label +: {
                        max_lines: 3
                        text_overflow: Ellipsis
                        draw_text +: {
                            text_style: MESSAGE_TEXT_STYLE { font_size: (MESSAGE_FONT_SIZE) }
                        }
                    }
                }
            }
        }
    }
}

/// Text-backed custom HTML widgets must keep wrapping: the pinned Html
/// renderer otherwise treats them as atomic on the last allowed row and
/// draws its own ellipsis even with Clip, duplicating our local marker.
fn draw_wrapping_inline(widget: &mut impl Widget, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
    let parent_flow = cx.turtle().flow();
    if matches!(parent_flow, Flow::Right { .. }) {
        cx.turtle_mut().set_flow_wrap(true);
    }
    // Finish every draw step before restoring the parent's flow. Both these
    // text widgets draw directly into TextFlow, which still owns max_lines.
    widget.draw_walk_all(cx, scope, walk);
    if let Flow::Right { wrap, .. } = parent_flow {
        cx.turtle_mut().set_flow_wrap(wrap);
    }
    DrawStep::done()
}

#[derive(Script, ScriptHook, Widget)]
struct PermissionMessageLink {
    #[deref] link: HtmlLink,
}

impl Widget for PermissionMessageLink {
    fn handle_event(&mut self, _cx: &mut Cx, _event: &Event, _scope: &mut Scope) {}

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        draw_wrapping_inline(&mut self.link, cx, scope, walk)
    }

    fn text(&self) -> String { self.link.text() }

    fn set_text(&mut self, cx: &mut Cx, text: &str) { self.link.set_text(cx, text); }
}

#[derive(Script, ScriptHook, Widget)]
struct PermissionMessageSpan {
    #[deref] span: MatrixHtmlSpan,
}

impl Widget for PermissionMessageSpan {
    fn handle_event(&mut self, _cx: &mut Cx, _event: &Event, _scope: &mut Scope) {}

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        draw_wrapping_inline(&mut self.span, cx, scope, walk)
    }

    fn text(&self) -> String { self.span.text() }

    fn set_text(&mut self, cx: &mut Cx, text: &str) { self.span.set_text(cx, text); }
}

#[derive(Script, ScriptHook, Widget)]
pub struct PermissionMessageContent {
    #[source] source: ScriptObjectRef,
    #[deref] view: View,
    #[live] draw_ellipsis: DrawText,
    #[rust] rich: bool,
}

impl Widget for PermissionMessageContent {
    fn handle_event(&mut self, _cx: &mut Cx, _event: &Event, _scope: &mut Scope) {
        // Reviewing a message must not activate its links or other HTML controls.
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        let step = self.view.draw_walk(cx, scope, walk);
        if step.is_done() && self.view.visible && self.rich {
            let html_ref = self.view.html(cx, ids!(inset.content.html_view.html));
            let Some(html) = html_ref.borrow() else { return step };
            if html.is_content_truncated() {
                let rect = html.area().rect(cx);
                if rect.size.x > 0.0 && rect.size.y > 0.0 {
                    // The pinned Html renderer drops a new block after max_lines
                    // without drawing an ellipsis. Clip all rich text and draw
                    // one marker in the inset's right gutter on the last row.
                    // Keeping it outside the HTML avoids covering text or its
                    // quote/code backgrounds with the inset's background color.
                    let marker = self.draw_ellipsis.layout(cx, 0.0, 0.0, None, false, Align::default(), "…");
                    let marker_width = marker.size_in_lpxs.width as f64;
                    let marker_height = marker.size_in_lpxs.height as f64;
                    let inset = self.view.view(cx, ids!(inset)).area().rect(cx);
                    let right = rect.pos.x + rect.size.x;
                    let gutter = (inset.pos.x + inset.size.x - right).max(0.0);
                    let pos = dvec2(right + ((gutter - marker_width) * 0.5).max(0.0),
                        rect.pos.y + rect.size.y - marker_height);
                    cx.push_clip_rect(inset);
                    self.draw_ellipsis.draw_abs(cx, pos, "…");
                    cx.pop_clip_rect();
                }
            }
        }
        step
    }
}

impl PermissionMessageContentRef {
    /// Shows the captured native message format, or hides an absent preview.
    pub fn set_message(&self, cx: &mut Cx, message: Option<&PermissionMessagePreview>) {
        let Some(mut inner) = self.borrow_mut() else { return };
        inner.view.set_visible(cx, message.is_some());
        let content = inner.view.html_or_plaintext(cx, ids!(inset.content));
        inner.rich = message.is_some_and(|message| message.formatted_html.is_some());
        if let Some(html) = message.and_then(|message| message.formatted_html.as_deref()) {
            content.show_html(cx, html);
        } else {
            content.show_plaintext(cx, message.map_or("", |message| message.body.as_str()));
        }
        inner.redraw(cx);
    }
}
