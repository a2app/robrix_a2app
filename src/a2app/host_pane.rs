//! The generic mini-app host: a large centered modal (event-source-modal
//! style) that runs one mini-app at a time, keeping backgrounded apps'
//! isolates alive (iOS-style) until they're force-stopped.

use makepad_widgets::*;

use a2app_core::manifest::{MiniAppId, MiniAppManifest};
use crate::a2app::host_set::{MiniAppHostAreaWidgetExt, Templates};
use crate::a2app::instances::{self, InstanceKey};
use matrix_sdk::ruma::OwnedRoomId;

script_mod! {
    use mod.prelude.widgets.*
    use mod.widgets.*

    mod.widgets.MiniAppHostPane = set_type_default() do #(MiniAppHostPane::register_widget(vm)) {
        ..mod.widgets.RoundedView

        width: Fill { max: 1000 }
        height: Fill
        margin: 40,
        flow: Down
        padding: Inset{top: 15, right: 20, bottom: 20, left: 20}

        show_bg: true
        draw_bg +: {
            color: (COLOR_PRIMARY)
            border_radius: 6.0
            border_size: 0.0
        }

        header := View {
            width: Fill, height: Fit
            flow: Right
            spacing: 10
            align: Align{y: 0.5}
            margin: Inset{bottom: 10}

            app_glyph := Label {
                width: Fit, height: Fit
                padding: 0, margin: 0
                draw_text +: {
                    text_style: TITLE_TEXT {font_size: 18},
                    color: #000
                }
            }
            app_title := Label {
                width: Fill, height: Fit
                padding: 0, margin: 0
                draw_text +: {
                    text_style: TITLE_TEXT {font_size: 16},
                    color: #000
                }
            }

            return_button := RobrixNeutralIconButton {
                visible: false
                padding: Inset{top: 5, bottom: 5, left: 10, right: 10},
                icon_walk: Walk{width: 0, height: 0, margin: 0}
                text: "Return to room"
            }

            close_button := RobrixIconButton {
                width: Fit, height: Fit,
                padding: 12,
                spacing: 0
                align: Align{x: 0.5, y: 0.5}
                icon_walk: Walk{width: 18, height: 18, margin: 0}
                draw_icon.svg: (ICON_CLOSE)
                draw_icon.color: #666
                draw_bg +: {
                    border_size: 0
                    color: #0000
                    color_hover: #00000015
                    color_down: #00000025
                }
            }
        }

        // The active host draws in here.
        host_area := mod.widgets.MiniAppHostArea {}

        // A TEMPLATE, not real content: the pane's View would happily draw it,
        // so it stays invisible; instantiated copies are un-hidden.
        AppHost := mod.widgets.MiniAppHost { visible: false }
    }
}

/// Actions this pane emits for the runtime to apply.
#[derive(Clone, Debug, Default)]
pub enum MiniAppHostPaneAction {
    /// The user closed the pane; the app keeps running in the background.
    CloseClicked,
    /// A room-bound app wants to go back into that room's dock.
    ReturnToRoom { app_id: MiniAppId, room_id: OwnedRoomId },
    #[default]
    None,
}

#[derive(Script, Widget)]
pub struct MiniAppHostPane {
    #[deref] view: View,
    #[rust] templates: Templates,
    /// The instance currently shown in the pane.
    #[rust] active: Option<InstanceKey>,
}

impl ScriptHook for MiniAppHostPane {
    fn on_before_apply(
        &mut self,
        _vm: &mut ScriptVm,
        apply: &Apply,
        _scope: &mut Scope,
        _value: ScriptValue,
    ) {
        if apply.is_reload() {
            self.templates.clear();
        }
    }

    fn on_after_apply(
        &mut self,
        vm: &mut ScriptVm,
        apply: &Apply,
        _scope: &mut Scope,
        value: ScriptValue,
    ) {
        self.templates.capture(vm, apply, value);
        if let Some(template) = self.templates.get(live_id!(AppHost)) {
            instances::set_host_template(template);
        }
        vm.cx_mut().widget_tree_mark_dirty(self.widget_uid());
    }
}

impl Drop for MiniAppHostPane {
    fn drop(&mut self) {
        instances::release_owner_no_cx(self.widget_uid());
    }
}

impl Widget for MiniAppHostPane {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.handle_event(cx, event, scope);

        if let Event::Actions(actions) = event {
            if self.view.button(cx, ids!(close_button)).clicked(actions) {
                cx.action(MiniAppHostPaneAction::CloseClicked);
            }
            if self.view.button(cx, ids!(return_button)).pressed(actions)
                && let Some((app_id, Some(room_id))) = self.active.clone()
            {
                cx.action(MiniAppHostPaneAction::ReturnToRoom { app_id, room_id });
            }
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        self.view.draw_walk(cx, scope, walk)?;

        if let Some(key) = &self.active {
            let size = self.view.mini_app_host_area(cx, ids!(host_area)).last_size();
            instances::note_size(key, size);
        }
        DrawStep::done()
    }
}

impl MiniAppHostPaneRef {
    /// Shows the app, adopting its running instance or starting one. `room`
    /// binds it like a docked instance and offers a way back. False when
    /// another surface is showing that instance.
    pub fn open_app(&self, cx: &mut Cx, manifest: &MiniAppManifest, grants: Vec<String>, room: Option<OwnedRoomId>) -> bool {
        let Some(mut inner) = self.borrow_mut() else { return false };
        let uid = inner.widget_uid();
        let room = room.or_else(|| scope_room(manifest));
        let key: InstanceKey = (manifest.id.clone(), room);
        let seed = crate::a2app::runtime::saved_layout(&instances::tag_of(&key));
        if instances::ensure(cx, &key, manifest, &grants, seed).is_none() {
            return false;
        }
        let Some(host) = instances::adopt(cx, &key, uid) else { return false };
        if let Some(previous) = inner.active.replace(key.clone())
            && previous != key
        {
            instances::release(cx, &previous, uid);
        }
        inner.view.button(cx, ids!(return_button)).set_visible(cx, key.1.is_some());
        inner.view.mini_app_host_area(cx, ids!(host_area)).set_host(Some(host));
        inner.view.label(cx, ids!(app_glyph)).set_text(cx, &manifest.icon);
        inner.view.label(cx, ids!(app_title)).set_text(cx, &manifest.name);
        inner.view.redraw(cx);
        true
    }

    /// Stops showing the active instance. It is quit unless `keep`, which
    /// parks it for a room dock to adopt.
    pub fn close_active(&self, cx: &mut Cx, keep: bool) -> Option<InstanceKey> {
        let mut inner = self.borrow_mut()?;
        let key = inner.active.take()?;
        let uid = inner.widget_uid();
        inner.view.mini_app_host_area(cx, ids!(host_area)).set_host(None);
        if keep {
            instances::release(cx, &key, uid);
        } else {
            instances::quit(cx, &key);
        }
        inner.view.redraw(cx);
        Some(key)
    }

    /// Lets go of the app's instance if it is the one shown; the registry
    /// owns the teardown.
    pub fn drop_app(&self, cx: &mut Cx, app_id: &str) {
        let Some(mut inner) = self.borrow_mut() else { return };
        if inner.active.as_ref().is_some_and(|(app, _)| app == app_id) {
            inner.active = None;
            inner.view.mini_app_host_area(cx, ids!(host_area)).set_host(None);
            inner.view.redraw(cx);
        }
    }

    /// Re-points the area at the (possibly restarted) active host.
    pub fn refresh_host(&self, cx: &mut Cx) {
        let Some(mut inner) = self.borrow_mut() else { return };
        if let Some(key) = &inner.active {
            inner.view.mini_app_host_area(cx, ids!(host_area)).set_host(instances::host_of(key));
            inner.view.redraw(cx);
        }
    }

    pub fn active(&self) -> Option<InstanceKey> {
        self.borrow().and_then(|inner| inner.active.clone())
    }
}

/// A room-scoped app keeps its room binding in the modal too, so its
/// `matrix.*` calls resolve that room.
fn scope_room(manifest: &MiniAppManifest) -> Option<OwnedRoomId> {
    match &manifest.scope {
        a2app_core::manifest::A2AppScope::Room { room_id } => OwnedRoomId::try_from(room_id.as_str()).ok(),
        _ => None,
    }
}
