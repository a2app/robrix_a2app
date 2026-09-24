//! The widgets every mini-app host surface shares: the `MiniAppHost`
//! template whose `Splash` child owns an app's isolated VM, the area a
//! surface draws one host into, and the surface's captured DSL templates.

use std::collections::{HashMap, HashSet};

use makepad_widgets::*;
use crate::shared::popup_list::{enqueue_popup_notification, PopupKind};

script_mod! {
    use mod.prelude.widgets.*
    use mod.widgets.*

    // An icon-only button with no fill until hovered.
    mod.widgets.MiniAppGhostButton = RobrixIconButton {
        width: 32, height: 32
        padding: 0, margin: 0, spacing: 0
        align: Align{x: 0.5, y: 0.5}
        icon_walk: Walk{width: 16, height: 16, margin: 0}
        draw_icon.color: #666
        draw_bg +: {
            border_size: 0
            border_radius: 6.0
            color: #0000
            color_hover: #00000012
            color_down: #00000022
        }
    }

    // The slot a pane's ACTIVE host is drawn into. A custom widget so the
    // host is drawn inside this turtle's own coordinate space; abs-positioned
    // drawing from the pane level lands wrong inside a translated Modal.
    mod.widgets.MiniAppHostArea = #(MiniAppHostArea::register_widget(vm)) {
        width: Fill, height: Fill
    }

    // A mini-app's chrome-less host: the Splash child owns the isolate.
    // Splash apps style themselves for a dark backdrop, and the glass kit
    // samples the scene beneath it, so the dark fill must actually paint.
    mod.widgets.MiniAppHost = View {
        width: Fill, height: Fill
        content_bg := RoundedView {
            width: Fill, height: Fill
            flow: Down
            show_bg: true
            draw_bg +: {
                color: (COLOR_PRIMARY)
                border_color: (COLOR_SECONDARY)
                border_size: 1.0
                border_radius: 4.0
            }
            content := ScrollYView {
                width: Fill, height: Fill
                flow: Down
                padding: Inset{left: 8, right: 8, top: 8, bottom: 8}
                splash := Splash {
                    width: Fill, height: Fill
                }
            }
        }
    }
}

/// Draws one pane's active host, filling its own rect.
#[derive(Script, ScriptHook, Widget)]
pub struct MiniAppHostArea {
    #[deref] view: View,
    #[rust] host: Option<WidgetRef>,
    /// Input failures already explained for this host; cleared after successful input.
    #[rust] input_errors_shown: HashSet<String>,
    /// The content box the host was last drawn at, for `on_app_resize`.
    #[rust] last_size: Vec2d,
}

impl Widget for MiniAppHostArea {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.handle_event(cx, event, scope);
        // Input goes to the host drawn here; network responses reach every
        // host through the set, so backgrounded requests still complete.
        if let Some(host) = self.host.clone()
            && !matches!(event, Event::NetworkResponses(_))
        {
            // Native input can carry pasted or dragged account data. Label
            // before the isolate sees it, including keyboard-derived values.
            if matches!(event, Event::TextInput(_) | Event::TextRangeReplace(_)
                | Event::KeyDown(_) | Event::KeyUp(_) | Event::Drag(_) | Event::Drop(_))
            {
                let context = super::instances::context_of_host(&host);
                let is_public = matches!(&context, Some(a2app_core::information_flow::ContextId::PublicApp { .. }));
                let recorded = context
                    .ok_or_else(|| "Mini-app input context is unavailable.".to_string())
                    .and_then(|context| {
                        super::information_flow::current_context(&context)?;
                        a2app_core::information_flow::add_sources(&context,
                            [super::information_flow::account_source(&context)])?;
                        if matches!(event, Event::TextInput(input) if input.was_paste)
                            || matches!(event, Event::TextRangeReplace(_) | Event::Drop(_))
                        {
                            a2app_core::information_flow::add_influences(&context,
                                [a2app_core::information_flow::Influence::Unknown])?;
                        }
                        Ok(())
                    });
                if let Err(error) = recorded {
                    // These events reach every host. Explain a failure only when
                    // keyboard input belongs to this host, not the room composer.
                    // A drag/drop target cannot be inferred from keyboard focus.
                    let focus = cx.key_focus();
                    if !self.input_errors_shown.contains(&error)
                        && matches!(event, Event::TextInput(_) | Event::TextRangeReplace(_)
                            | Event::KeyDown(_) | Event::KeyUp(_))
                        && focus.is_valid(cx)
                        && active_host_contains_focus(&host, focus)
                    {
                        self.input_errors_shown.insert(error.clone());
                        let guidance = if is_public {
                            "A public instance cannot accept typed, pasted, or dropped private data. Close this public instance, then open the mini-app normally from Mini Apps to enter text."
                        } else {
                            "Close this mini-app and reopen it from its room or Mini Apps. If input is still blocked, restart Robrix."
                        };
                        enqueue_popup_notification(
                            format!("Mini-app input was blocked.\n\n{error}\n\n{guidance}"),
                            PopupKind::Warning,
                            Some(12.0),
                        );
                    }
                    return;
                }
                self.input_errors_shown.clear();
            }
            // Robrix opens the URLs of any link actions it sees, so a guest's must not get out.
            let actions = cx.capture_actions(|cx| host.handle_event(cx, event, scope));
            let mut allowed = ActionsBuf::new();
            for action in actions {
                if !is_url_action(&action) {
                    allowed.push(action);
                }
            }
            cx.extend_actions(allowed);
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, _scope: &mut Scope, walk: Walk) -> DrawStep {
        cx.begin_turtle(walk, Layout::flow_overlay());
        let rect = cx.turtle().rect();
        self.last_size = rect.size;
        if let Some(host) = self.host.clone()
            && rect.size.x > 1.0 && rect.size.y > 1.0
        {
            let host_walk = Walk {
                abs_pos: Some(rect.pos),
                width: Size::Fixed(rect.size.x),
                height: Size::Fixed(rect.size.y),
                ..Walk::default()
            };
            host.draw_walk_all(cx, &mut Scope::empty(), host_walk);
        }
        cx.end_turtle();
        DrawStep::done()
    }
}

/// Whether the focused area belongs to an active widget inside this host.
fn active_host_contains_focus(host: &WidgetRef, focus: Area) -> bool {
    if focus.is_empty() { return false; }
    let mut found = host.area() == focus;
    let active = host.visit_cancel(&mut |_, child| {
        if !found {
            found = active_host_contains_focus(&child, focus);
        }
    });
    active && found
}

/// Whether the given action carries a URL, e.g., from a tap on a link.
fn is_url_action(action: &Action) -> bool {
    let widget_action = action.as_widget_action();
    matches!(widget_action.cast(), HtmlLinkAction::Clicked { .. } | HtmlLinkAction::SecondaryClicked { .. })
        || matches!(widget_action.cast(), MarkdownAction::LinkNavigated(_))
}

impl MiniAppHostAreaRef {
    /// Sets (or clears) the host this area draws.
    pub fn set_host(&self, host: Option<WidgetRef>) {
        if let Some(mut inner) = self.borrow_mut() {
            if inner.host.as_ref().map(WidgetRef::widget_uid) != host.as_ref().map(WidgetRef::widget_uid) {
                inner.input_errors_shown.clear();
            }
            inner.host = host;
        }
    }

    pub fn last_size(&self) -> Vec2d {
        self.borrow().map(|inner| inner.last_size).unwrap_or_default()
    }
}

/// The DSL templates a host surface instantiates at runtime (e.g., `AppHost`).
#[derive(Default)]
pub struct Templates {
    templates: HashMap<LiveId, ScriptObjectRef>,
}

impl Templates {
    /// Captures the owning widget's DSL templates; call from `on_after_apply`.
    pub fn capture(&mut self, vm: &mut ScriptVm, apply: &Apply, value: ScriptValue) {
        if !apply.is_eval()
            && let Some(obj) = value.as_object()
        {
            vm.vec_with(obj, |vm, vec| {
                for kv in vec {
                    if let Some(id) = kv.key.as_id()
                        && let Some(template_obj) = kv.value.as_object()
                    {
                        self.templates.insert(id, vm.bx.heap.new_object_ref(template_obj));
                    }
                }
            });
        }
    }

    /// Drops captured templates; call from `on_before_apply` on a reload.
    pub fn clear(&mut self) {
        self.templates.clear();
    }

    pub fn get(&self, id: LiveId) -> Option<ScriptObjectRef> {
        self.templates.get(&id).cloned()
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    struct FocusBranch {
        uid: WidgetUid,
        area: Area,
        visible: bool,
        children: Vec<WidgetRef>,
    }

    impl ScriptApply for FocusBranch {
        fn script_apply(&mut self, _vm: &mut ScriptVm, _apply: &Apply, _scope: &mut Scope, _value: ScriptValue) {}
    }

    impl WidgetNode for FocusBranch {
        fn widget_uid(&self) -> WidgetUid { self.uid }
        fn walk(&mut self, _cx: &mut Cx) -> Walk { Walk::default() }
        fn area(&self) -> Area { self.area }
        fn visible(&self) -> bool { self.visible }
        fn redraw(&mut self, _cx: &mut Cx) {}
        fn children(&self, visit: &mut dyn FnMut(LiveId, WidgetRef)) {
            for child in &self.children { visit(id!(child), child.clone()); }
        }
    }

    impl Widget for FocusBranch {
        fn draw_walk(&mut self, _cx: &mut Cx2d, _scope: &mut Scope, _walk: Walk) -> DrawStep {
            DrawStep::done()
        }
    }

    fn focus_branch(area: Area, children: Vec<WidgetRef>) -> WidgetRef {
        WidgetRef::new_with_inner(Box::new(FocusBranch {
            uid: WidgetUid::new(), area, children, visible: true,
        }))
    }

    #[test]
    fn input_feedback_only_matches_focus_in_the_hosts_active_subtree() {
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let list = DrawList::new(&mut cx);
        let guest_focus = Area::Rect(RectArea { draw_list_id: list.id(), rect_id: 0, redraw_id: 1 });
        let composer_focus = Area::Rect(RectArea { draw_list_id: list.id(), rect_id: 1, redraw_id: 1 });
        let guest = focus_branch(guest_focus, Vec::new());
        let guest_view = focus_branch(Area::Empty, vec![guest]);
        let host = focus_branch(Area::Empty, vec![guest_view.clone()]);
        assert!(!active_host_contains_focus(&host, Area::Empty));
        assert!(!active_host_contains_focus(&host, composer_focus), "typing in another part of Robrix must not warn for this host");
        assert!(active_host_contains_focus(&host, guest_focus));
        guest_view.borrow_mut::<FocusBranch>().unwrap().visible = false;
        assert!(!active_host_contains_focus(&host, guest_focus), "hidden cached content cannot own input feedback");
    }

    #[test]
    fn redrawing_the_same_host_keeps_warning_deduplication_but_replacement_resets_it() {
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let (area, first, second) = cx.with_vm(|vm| {
            makepad_widgets::script_mod(vm);
            crate::shared::script_mod(vm);
            super::script_mod(vm);
            let area = script_eval!(vm, { mod.widgets.MiniAppHostArea {} });
            let area = WidgetRef::script_from_value(vm, area).as_mini_app_host_area();
            let first = script_eval!(vm, { mod.widgets.View {} });
            let first = WidgetRef::script_from_value(vm, first);
            let second = script_eval!(vm, { mod.widgets.View {} });
            let second = WidgetRef::script_from_value(vm, second);
            (area, first, second)
        });
        area.set_host(Some(first.clone()));
        area.borrow_mut().unwrap().input_errors_shown.insert("blocked input".into());
        // A room pane reattaches the same host on draw; this must not rearm its popup.
        area.set_host(Some(first));
        assert_eq!(area.borrow().unwrap().input_errors_shown.len(), 1);
        area.set_host(Some(second));
        assert!(area.borrow().unwrap().input_errors_shown.is_empty());
        area.borrow_mut().unwrap().input_errors_shown.insert("blocked input".into());
        area.set_host(None);
        assert!(area.borrow().unwrap().input_errors_shown.is_empty());
    }
}
