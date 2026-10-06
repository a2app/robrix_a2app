//! The generic mini-app host: a resizable centered modal that runs one
//! mini-app at a time, keeping backgrounded apps'
//! isolates alive (iOS-style) until they're force-stopped.

use makepad_widgets::*;

use a2app_core::manifest::{MiniAppId, MiniAppManifest};
use crate::a2app::host_set::{MiniAppHostAreaWidgetExt, Templates};
use crate::a2app::instances::{self, InstanceKey, Surface};
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
                max_lines: 1
                text_overflow: Ellipsis
                draw_text +: {
                    text_style: TITLE_TEXT {font_size: 16},
                    color: #000
                }
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

        return_button := RobrixNeutralIconButton {
            visible: false
            margin: Inset{bottom: 10}
            padding: Inset{top: 5, bottom: 5, left: 10, right: 10}
            icon_walk: Walk{width: 0, height: 0, margin: 0}
            text: "Back to room"
        }

        // The active host draws in here.
        host_area := mod.widgets.MiniAppHostArea {}

        preview_controls := View {
            width: Fill, height: Fit
            flow: Right
            spacing: 8
            align: Align{y: 0.5}
            margin: Inset{top: 8}

            preview_size := Label {
                width: Fill, height: Fit
                padding: 0, margin: 0
                max_lines: 1
                text_overflow: Ellipsis
                text: "Drag corner to resize"
                draw_text +: {
                    text_style: REGULAR_TEXT {font_size: 10}
                    color: #666
                }
            }

            reset_size := RobrixNeutralIconButton {
                padding: Inset{top: 5, bottom: 5, left: 8, right: 8}
                icon_walk: Walk{width: 0, height: 0, margin: 0}
                text: "Reset size"
            }

            resize_grip := View {
                width: 28, height: 28
                cursor: MouseCursor.NwseResize
                show_bg: true
                draw_bg +: {
                    pixel: fn() {
                        let sdf = Sdf2d.viewport(self.pos * self.rect_size)
                        sdf.move_to(8.0, 22.0)
                        sdf.line_to(22.0, 8.0)
                        sdf.move_to(14.0, 22.0)
                        sdf.line_to(22.0, 14.0)
                        sdf.move_to(20.0, 22.0)
                        sdf.line_to(22.0, 20.0)
                        return sdf.stroke(#888, 1.0)
                    }
                }
            }
        }

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
    /// Only ordinary rooms have a dock to return the instance to.
    #[rust] can_return_to_room: bool,
    /// The card size chosen by dragging. The available window always wins.
    #[rust] preview_size: Option<Vec2d>,
    #[rust] available_size: Vec2d,
    #[rust] last_card_size: Vec2d,
    #[rust] drag_start_size: Option<Vec2d>,
    #[rust] drawing_walk: Option<Walk>,
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

impl MiniAppHostPane {
    /// Desktop keeps a floating card; a phone-width window gets nearly all
    /// of the screen.
    fn fit_window(&mut self, cx: &mut Cx) {
        let m = if crate::home::home_screen::effective_is_desktop(cx) { 40.0 } else { 6.0 };
        self.view.walk.margin = Inset { left: m, top: m, right: m, bottom: m };
        self.view.redraw(cx);
    }

    fn resize_preview(&mut self, cx: &mut Cx, size: Vec2d) {
        self.preview_size = Some(clamp_preview_size(size, self.available_size));
        // A pure height change must also redraw cached guest descendants.
        cx.redraw_area_and_children(self.view.area());
        self.view.redraw(cx);
    }

    fn reset_preview(&mut self, cx: &mut Cx) {
        self.preview_size = None;
        self.drag_start_size = None;
        cx.redraw_area_and_children(self.view.area());
        self.view.redraw(cx);
    }

    fn preview_walk(&mut self, available: Vec2d, mut walk: Walk) -> Walk {
        self.available_size = dvec2(
            (available.x - walk.margin.left - walk.margin.right).max(1.0).min(1000.0),
            (available.y - walk.margin.top - walk.margin.bottom).max(1.0),
        );
        if let Some(size) = self.preview_size {
            let size = clamp_preview_size(size, self.available_size);
            walk.width = Size::Fixed(size.x);
            walk.height = Size::Fixed(size.y);
        }
        walk
    }
}

fn clamp_preview_size(size: Vec2d, available: Vec2d) -> Vec2d {
    dvec2(
        size.x.max(220.0).min(available.x.max(1.0)),
        size.y.max(240.0).min(available.y.max(1.0)),
    )
}

impl Widget for MiniAppHostPane {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.handle_event(cx, event, scope);

        if let Event::Actions(actions) = event {
            if actions.iter().any(|action| matches!(action.downcast_ref::<ModalAction>(), Some(ModalAction::Dismissed))) {
                self.drag_start_size = None;
            }
            if actions.iter().any(|a| a.downcast_ref::<crate::home::home_screen::MainViewVariantChangedAction>().is_some()) {
                self.fit_window(cx);
            }
            let grip = self.view.view(cx, ids!(resize_grip));
            if let Some(down) = grip.finger_down(actions)
                && down.is_primary_hit()
            {
                self.drag_start_size = Some(self.last_card_size);
            }
            if let Some(moved) = grip.finger_move(actions)
                && let Some(start) = self.drag_start_size
            {
                // The card remains centered, so each edge moves half the
                // size change. Keep the grabbed corner under the pointer.
                self.resize_preview(cx, start + (moved.abs - moved.abs_start) * 2.0);
            }
            if grip.finger_up(actions).is_some() {
                self.drag_start_size = None;
            }
            if self.view.button(cx, ids!(reset_size)).clicked(actions) {
                self.reset_preview(cx);
            }
            if self.view.button(cx, ids!(close_button)).clicked(actions) {
                cx.action(MiniAppHostPaneAction::CloseClicked);
            }
            if self.can_return_to_room
                && self.view.button(cx, ids!(return_button)).pressed(actions)
                && let Some((app_id, Some(room_id))) = self.active.clone()
            {
                cx.action(MiniAppHostPaneAction::ReturnToRoom { app_id, room_id });
            }
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        // A stepped child can yield with the card's turtle still active.
        // Measure the parent only at the start of this draw.
        let walk = match self.drawing_walk {
            Some(walk) => walk,
            None => {
                let walk = self.preview_walk(cx.turtle().inner_size(), walk);
                self.drawing_walk = Some(walk);
                walk
            }
        };
        self.view.draw_walk(cx, scope, walk)?;
        self.drawing_walk = None;
        self.last_card_size = self.view.area().rect(cx).size;

        let size = self.view.mini_app_host_area(cx, ids!(host_area)).last_size();
        let status = format!("{} × {} · drag corner to resize", size.x.round() as u32, size.y.round() as u32);
        let size_label = self.view.label(cx, ids!(preview_size));
        if size_label.text() != status {
            size_label.set_text(cx, &status);
            // Ordinary redraw() is ignored during a draw event. This label
            // was already drawn, so explicitly queue the overlay's next frame.
            cx.redraw_area_in_draw(self.view.area());
        }
        if let Some(key) = &self.active {
            instances::note_size(key, size);
        }
        DrawStep::done()
    }
}

impl MiniAppHostPaneRef {
    /// Shows the app, adopting its running instance or starting one. `room`
    /// binds it to a room or space; ordinary rooms offer a way back to the
    /// dock. False when another surface is showing that instance.
    pub fn open_app(&self, cx: &mut Cx, manifest: &MiniAppManifest, grants: Vec<String>, room: Option<OwnedRoomId>) -> bool {
        let Some(mut inner) = self.borrow_mut() else { return false };
        let uid = inner.widget_uid();
        let room = room.or_else(|| scope_room(manifest));
        let key: InstanceKey = (manifest.id.clone(), room);
        if instances::ensure(cx, &key, manifest, &grants).is_none() {
            return false;
        }
        let Some(host) = instances::adopt(cx, &key, uid, Surface::Modal) else { return false };
        if let Some(previous) = inner.active.replace(key.clone())
            && previous != key
        {
            instances::release(cx, &previous, uid);
        }
        inner.fit_window(cx);
        let can_return_to_room = key.1.as_ref()
            .and_then(|room_id| crate::sliding_sync::get_client()?.get_room(room_id))
            .is_some_and(|room| !room.is_space());
        inner.can_return_to_room = can_return_to_room;
        inner.view.button(cx, ids!(return_button)).set_visible(cx, can_return_to_room);
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
        inner.drag_start_size = None;
        inner.can_return_to_room = false;
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
            inner.drag_start_size = None;
            inner.active = None;
            inner.can_return_to_room = false;
            inner.view.mini_app_host_area(cx, ids!(host_area)).set_host(None);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn pane_fixture() -> (Cx, MiniAppHostPaneRef) {
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let pane = cx.with_vm(|vm| {
            makepad_widgets::script_mod(vm);
            makepad_code_editor::script_mod(vm);
            crate::shared::script_mod(vm);
            super::super::host_set::script_mod(vm);
            vm.bx.captured_errors = Some(Vec::new());
            super::script_mod(vm);
            let value = script_eval!(vm, { mod.widgets.MiniAppHostPane {} });
            let pane = WidgetRef::script_from_value(vm, value).as_mini_app_host_pane();
            let errors = vm.take_errors();
            assert!(errors.is_empty(), "resizable host template failed to initialize: {errors:?}");
            pane
        });
        (cx, pane)
    }

    #[test]
    fn preview_size_is_bounded_by_the_window_and_recovers_after_window_growth() {
        let (_cx, pane) = pane_fixture();
        let mut inner = pane.borrow_mut().unwrap();
        let walk = inner.view.walk;
        inner.preview_size = Some(dvec2(600.0, 480.0));
        let desktop = inner.preview_walk(dvec2(1200.0, 900.0), walk);
        assert_eq!(desktop.width.to_fixed(), Some(600.0));
        assert_eq!(desktop.height.to_fixed(), Some(480.0));
        assert_eq!(inner.available_size, dvec2(1000.0, 820.0));

        // A small host window must remain usable even below the card's minimum.
        let narrow = inner.preview_walk(dvec2(320.0, 280.0), walk);
        assert_eq!(narrow.width.to_fixed(), Some(240.0));
        assert_eq!(narrow.height.to_fixed(), Some(200.0));
        let restored = inner.preview_walk(dvec2(1200.0, 900.0), walk);
        assert_eq!(restored.width.to_fixed(), Some(600.0));
        assert_eq!(restored.height.to_fixed(), Some(480.0));

        inner.preview_size = Some(dvec2(1.0, 1.0));
        let minimum = inner.preview_walk(dvec2(1200.0, 900.0), walk);
        assert_eq!(minimum.width.to_fixed(), Some(220.0));
        assert_eq!(minimum.height.to_fixed(), Some(240.0));
    }

    fn grip_action(cx: &mut Cx, pane: &MiniAppHostPaneRef, action: ViewAction) {
        let uid = pane.borrow().unwrap().view.view(cx, ids!(resize_grip)).widget_uid();
        let actions = cx.capture_actions(|cx| cx.widget_action(uid, action));
        pane.handle_event(cx, &Event::Actions(actions), &mut Scope::empty());
    }

    #[test]
    fn dragging_the_corner_resizes_the_preview_and_reset_restores_the_default() {
        let (mut cx, pane) = pane_fixture();
        {
            let mut inner = pane.borrow_mut().unwrap();
            inner.available_size = dvec2(1000.0, 820.0);
            inner.last_card_size = dvec2(800.0, 640.0);
        }
        let start = dvec2(800.0, 640.0);
        grip_action(&mut cx, &pane, ViewAction::FingerDown(FingerDownEvent {
            window_id: WindowId(0, 0), abs: start, digit_id: Default::default(),
            device: DigitDevice::Touch { uid: 1 }, tap_count: 1,
            modifiers: Default::default(), time: 0.0, rect: Default::default(),
        }));
        grip_action(&mut cx, &pane, ViewAction::FingerMove(FingerMoveEvent {
            window_id: WindowId(0, 0), abs: start - dvec2(220.0, 80.0), abs_start: start,
            digit_id: Default::default(), device: DigitDevice::Touch { uid: 1 },
            has_long_press_occurred: false, tap_count: 1, modifiers: Default::default(),
            time: 1.0, rect: Default::default(), is_over: true,
        }));
        assert_eq!(pane.borrow().unwrap().preview_size, Some(dvec2(360.0, 480.0)));

        // Modal dismissal cancels an in-progress drag without losing the size.
        pane.handle_event(&mut cx, &Event::Actions(vec![Box::new(ModalAction::Dismissed)]), &mut Scope::empty());
        assert!(pane.borrow().unwrap().drag_start_size.is_none());
        assert_eq!(pane.borrow().unwrap().preview_size, Some(dvec2(360.0, 480.0)));

        let reset = pane.borrow().unwrap().view.button(&mut cx, ids!(reset_size)).widget_uid();
        let actions = cx.capture_actions(|cx| cx.widget_action(reset, ButtonAction::Clicked(Default::default())));
        pane.handle_event(&mut cx, &Event::Actions(actions), &mut Scope::empty());
        let mut inner = pane.borrow_mut().unwrap();
        assert!(inner.preview_size.is_none());
        let default = inner.view.walk;
        let reset = inner.preview_walk(dvec2(1200.0, 900.0), default);
        assert_eq!(reset.width, default.width);
        assert_eq!(reset.height, default.height);
        assert!(inner.active.is_none(), "resizing a preview must also work without a running app");
    }

    #[test]
    fn return_to_room_still_emits_from_its_separate_row() {
        let (mut cx, pane) = pane_fixture();
        let app_id = "fixture-clock".to_string();
        let room_id = OwnedRoomId::try_from("!preview:example.org").unwrap();
        let uid = {
            let mut inner = pane.borrow_mut().unwrap();
            inner.active = Some((app_id.clone(), Some(room_id.clone())));
            inner.can_return_to_room = true;
            let button = inner.view.button(&mut cx, ids!(return_button));
            button.set_visible(&mut cx, true);
            button.widget_uid()
        };
        let press = cx.capture_actions(|cx| cx.widget_action(uid, ButtonAction::Pressed(Default::default())));
        let emitted = cx.capture_actions(|cx| pane.handle_event(cx, &Event::Actions(press), &mut Scope::empty()));
        assert!(emitted.iter().any(|action| matches!(action.downcast_ref::<MiniAppHostPaneAction>(),
            Some(MiniAppHostPaneAction::ReturnToRoom { app_id: returned_app, room_id: returned_room })
                if returned_app == &app_id && returned_room == &room_id)));
    }
}
