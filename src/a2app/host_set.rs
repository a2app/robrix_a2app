//! The widgets every mini-app host surface shares: the `MiniAppHost`
//! template whose `Splash` child owns an app's isolated VM, the area a
//! surface draws one host into, and the surface's captured DSL templates.

use std::collections::HashMap;

use makepad_widgets::*;

script_mod! {
    use mod.prelude.widgets.*
    use mod.widgets.*

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
                    width: Fill, height: Fit
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
            host.handle_event(cx, event, scope);
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
                margin: Default::default(),
                width: Size::Fixed(rect.size.x),
                height: Size::Fixed(rect.size.y),
                metrics: Default::default(),
            };
            host.draw_walk_all(cx, &mut Scope::empty(), host_walk);
        }
        cx.end_turtle();
        DrawStep::done()
    }
}

impl MiniAppHostAreaRef {
    /// Sets (or clears) the host this area draws.
    pub fn set_host(&self, host: Option<WidgetRef>) {
        if let Some(mut inner) = self.borrow_mut() {
            inner.host = host;
        }
    }

    pub fn last_size(&self) -> Vec2d {
        self.borrow().map(|inner| inner.last_size).unwrap_or_default()
    }
}

/// The DSL templates a host surface instantiates at runtime (`AppHost`,
/// and the dock's `PaneFrame` / `Chip`).
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
