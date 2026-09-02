//! A mini-app broken out into its own desktop dock tab. The tab adopts the
//! (app, room) instance from the registry, so its script state carries over
//! from the room's dock; "Return to room" hands it back.

use makepad_widgets::*;
use matrix_sdk::ruma::OwnedRoomId;

use a2app_core::manifest::MiniAppId;
use crate::a2app::dock::DockCmd;
use crate::a2app::host_set::{MiniAppHostAreaWidgetExt, Templates};
use crate::a2app::instances::{self, InstanceKey, MiniAppInstanceAction};
use crate::a2app::runtime::with_a2app;

script_mod! {
    use mod.prelude.widgets.*
    use mod.widgets.*

    mod.widgets.MiniAppTabScreen = set_type_default() do #(MiniAppTabScreen::register_widget(vm)) {
        ..mod.widgets.RoundedView
        width: Fill, height: Fill
        flow: Down
        padding: Inset{top: 6, right: 8, bottom: 8, left: 8}
        show_bg: true
        draw_bg +: { color: (COLOR_PRIMARY) }

        header := View {
            width: Fill, height: Fit
            flow: Right
            spacing: 6
            align: Align{y: 0.5}
            margin: Inset{bottom: 6}

            tab_glyph := Label {
                width: Fit, height: Fit
                padding: 0, margin: 0
                draw_text +: {
                    text_style: TITLE_TEXT {font_size: 13},
                    color: #000
                }
            }
            titles := View {
                width: Fill, height: Fit
                flow: Down
                tab_title := Label {
                    width: Fill, height: Fit
                    padding: 0, margin: 0
                    draw_text +: {
                        text_style: theme.font_bold {font_size: 12},
                        color: (COLOR_TEXT)
                    }
                }
                tab_room := Label {
                    width: Fill, height: Fit
                    padding: 0, margin: 0
                    draw_text +: {
                        text_style: REGULAR_TEXT {font_size: 9},
                        color: (MESSAGE_TEXT_COLOR)
                    }
                }
            }

            return_button := RobrixNeutralIconButton {
                padding: Inset{top: 5, bottom: 5, left: 10, right: 10},
                icon_walk: Walk{width: 0, height: 0, margin: 0}
                text: "Return to room"
            }
        }

        host_area := mod.widgets.MiniAppHostArea {}

        AppHost := mod.widgets.MiniAppHost { visible: false }
    }
}

/// Asks MainDesktopUI to open (or focus) a mini-app tab.
#[derive(Clone, Debug, Default)]
pub enum A2AppTabRequest {
    Open { app_id: MiniAppId, room_id: OwnedRoomId, room_name: String },
    #[default]
    None,
}

/// Notifications from a tab screen back to MainDesktopUI.
#[derive(Clone, Debug, Default)]
pub enum MiniAppTabScreenAction {
    /// The instance left the tab (returned to its room or was stopped);
    /// the tab itself should be closed. `returning` means the user asked for
    /// the room back, so the host should show it rather than leaving them
    /// wherever the closed tab dropped them.
    Vacated {
        app_id: MiniAppId,
        room_id: OwnedRoomId,
        room_name: String,
        returning: bool,
    },
    #[default]
    None,
}

#[derive(Script, Widget)]
pub struct MiniAppTabScreen {
    #[deref] view: View,
    #[rust] templates: Templates,
    #[rust] key: Option<InstanceKey>,
    /// The room's display name, kept so returning can name the room's tab
    /// even when it is no longer open.
    #[rust] room_name: String,
}

impl ScriptHook for MiniAppTabScreen {
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

impl Drop for MiniAppTabScreen {
    fn drop(&mut self) {
        instances::release_owner_no_cx(self.widget_uid());
    }
}

impl Widget for MiniAppTabScreen {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.handle_event(cx, event, scope);

        if let Event::Actions(actions) = event {
            for action in actions {
                match action.downcast_ref::<DockCmd>() {
                    Some(DockCmd::QuitEverywhere(app_id)) => {
                        if self.key.as_ref().is_some_and(|(app, _)| app == app_id) {
                            self.vacate(cx, false);
                        }
                    }
                    Some(DockCmd::Restart(app_id)) => {
                        if let Some(key) = self.key.clone()
                            && key.0 == *app_id
                        {
                            self.view.mini_app_host_area(cx, ids!(host_area)).set_host(instances::host_of(&key));
                            self.view.redraw(cx);
                        }
                    }
                    _ => {}
                }
            }

            if self.view.button(cx, ids!(return_button)).pressed(actions) {
                self.vacate(cx, true);
            }
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        self.view.draw_walk(cx, scope, walk)?;

        if let Some(key) = &self.key {
            let size = self.view.mini_app_host_area(cx, ids!(host_area)).last_size();
            instances::note_size(key, size);
        }
        DrawStep::done()
    }
}

impl MiniAppTabScreen {
    /// Lets go of the instance: back to the room's dock (state intact)
    /// when `back_to_room`, otherwise quit for good.
    fn vacate(&mut self, cx: &mut Cx, back_to_room: bool) {
        let Some(key) = self.key.take() else { return };
        let uid = self.widget_uid();
        self.view.mini_app_host_area(cx, ids!(host_area)).set_host(None);
        let (app_id, room_id) = key.clone();
        let Some(room_id) = room_id else { return };
        if back_to_room {
            instances::release(cx, &key, uid);
            cx.action(DockCmd::Open { app_id: app_id.clone(), room_id: room_id.clone() });
        } else if instances::quit(cx, &key) {
            cx.action(MiniAppInstanceAction::AppStopped(app_id.clone()));
        }
        cx.action(MiniAppTabScreenAction::Vacated {
            app_id,
            room_id,
            room_name: std::mem::take(&mut self.room_name),
            returning: back_to_room,
        });
        self.view.redraw(cx);
    }
}

impl MiniAppTabScreenRef {
    /// Shows the app's instance for `room_id`, starting it if needed.
    pub fn open(&self, cx: &mut Cx, app_id: MiniAppId, room_id: OwnedRoomId, room_name: &str) {
        let Some(mut inner) = self.borrow_mut() else { return };
        let manifest = with_a2app(|state| state.registry.get(&app_id).cloned()).flatten();
        let Some(manifest) = manifest else { return };
        let grants = a2app_core::permissions::snapshot_grants_for(&app_id);
        let uid = inner.widget_uid();
        let key: InstanceKey = (app_id, Some(room_id));
        let seed = crate::a2app::runtime::saved_layout(&instances::tag_of(&key));
        if instances::ensure(cx, &key, &manifest, &grants, seed).is_none() {
            return;
        }
        let Some(host) = instances::adopt(cx, &key, uid) else { return };
        inner.view.mini_app_host_area(cx, ids!(host_area)).set_host(Some(host));
        inner.view.label(cx, ids!(tab_glyph)).set_text(cx, &manifest.icon);
        inner.view.label(cx, ids!(tab_title)).set_text(cx, &manifest.name);
        inner.view.label(cx, ids!(tab_room)).set_text(cx, room_name);
        inner.room_name = room_name.to_string();
        inner.key = Some(key);
        inner.view.redraw(cx);
    }

    /// Stops the tab's instance without a return; used when the tab is closed.
    pub fn quit(&self, cx: &mut Cx) {
        if let Some(mut inner) = self.borrow_mut() {
            inner.vacate(cx, false);
        }
    }

    pub fn instance(&self) -> Option<(MiniAppId, OwnedRoomId)> {
        self.borrow().and_then(|inner| {
            let (app_id, room_id) = inner.key.clone()?;
            Some((app_id, room_id?))
        })
    }
}
