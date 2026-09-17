//! A compact picker for mini-apps that can run in the current room or space.

use makepad_widgets::*;

use crate::{
    a2app::{
        mini_apps_screen::{mini_app_rows, MiniAppRow, MiniAppRowAction, MiniAppRowData},
        runtime::{with_a2app, A2AppOp},
    },
    home::navigation_tab_bar::NavigationBarAction,
    utils::RoomNameId,
};

script_mod! {
    use mod.prelude.widgets.*
    use mod.widgets.*

    mod.widgets.RoomAppPicker = set_type_default() do #(RoomAppPicker::register_widget(vm)) {
        ..mod.widgets.SmallModal

        width: Fill {max: 500}

        title := ModalTitle {
            text: "Run a mini-app"
            margin: Inset{bottom: 8}
        }
        context_name := Label {
            width: Fill, height: Fit
            max_lines: 1, text_overflow: Ellipsis
            align: Align{x: 0.5}
            padding: 0, margin: Inset{bottom: 16}
            draw_text +: {
                color: (MESSAGE_TEXT_COLOR)
                text_style: REGULAR_TEXT {font_size: 10.5}
            }
        }

        search_input := RobrixTextInput {
            width: Fill, height: Fit
            empty_text: "Search mini-apps..."
        }

        apps_list := PortalList {
            width: Fill, height: 300
            flow: Down
            margin: Inset{top: 10, bottom: 12}

            app_row := mod.widgets.MiniAppRow {
                margin: Inset{bottom: 4}
                row_settings_button +: {visible: false}
            }
            empty_row := View {
                width: Fill, height: 90
                align: Align{x: 0.5, y: 0.5}
                empty_label := Label {
                    width: Fill, height: Fit
                    flow: Flow.Right{wrap: true}
                    align: Align{x: 0.5}
                    padding: 8
                    draw_text +: {
                        color: (MESSAGE_TEXT_COLOR)
                        text_style: REGULAR_TEXT {font_size: 11}
                    }
                }
            }
        }

        Label {
            width: Fill, height: Fit
            flow: Flow.Right{wrap: true}
            padding: 0
            text: "See all other mini-apps and generate new ones in the Mini Apps screen."
            draw_text +: {
                color: (MESSAGE_TEXT_COLOR)
                text_style: REGULAR_TEXT {font_size: 10.5}
            }
        }
        buttons_view := ModalButtonsRow {
            padding: Inset{top: 16}
            cancel_button := RobrixNeutralIconButton {
                padding: 10
                icon_walk: Walk{width: 0, height: 0, margin: 0}
                text: "Cancel"
            }
            all_apps_button := RobrixIconButton {
                padding: 10
                icon_walk: Walk{width: 0, height: 0, margin: 0}
                text: "Mini Apps"
            }
        }
    }
}

/// App-level actions that open and close the picker modal.
#[derive(Clone, Debug, Default)]
pub enum RoomAppPickerAction {
    Show { room_name_id: RoomNameId, is_space: bool },
    Close,
    #[default]
    None,
}

#[derive(Script, ScriptHook, Widget)]
pub struct RoomAppPicker {
    #[deref] view: View,
    #[rust] context: Option<(RoomNameId, bool)>,
    #[rust] keywords: String,
    #[rust] rows: Vec<MiniAppRowData>,
}

impl Widget for RoomAppPicker {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.handle_event(cx, event, scope);
        self.widget_match_event(cx, event, scope);
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        self.load_apps(cx);
        while let Some(widget) = self.view.draw_walk(cx, scope, walk).step() {
            let portal_list = widget.as_portal_list();
            let Some(mut list) = portal_list.borrow_mut() else { continue };
            let count = self.rows.len().max(1);
            list.set_item_range(cx, 0, count);
            while let Some(index) = list.next_visible_item(cx) {
                if index >= count {
                    continue;
                }
                let item = if let Some(row_data) = self.rows.get(index) {
                    let item = list.item(cx, index, id!(app_row));
                    if let Some(mut row) = item.borrow_mut::<MiniAppRow>() {
                        row.populate(cx, row_data);
                    }
                    item
                } else {
                    let item = list.item(cx, index, id!(empty_row));
                    let message = if !self.keywords.is_empty() {
                        "No matching mini-apps for this context."
                    } else if self.context.as_ref().is_some_and(|(_, is_space)| *is_space) {
                        "No mini-apps available for this space yet."
                    } else {
                        "No mini-apps available for this room yet."
                    };
                    item.label(cx, ids!(empty_label)).set_text(cx, message);
                    item
                };
                item.draw_all(cx, scope);
            }
        }
        DrawStep::done()
    }
}

impl WidgetMatchEvent for RoomAppPicker {
    fn handle_actions(&mut self, cx: &mut Cx, actions: &Actions, _scope: &mut Scope) {
        // Closing the wrapper delivers Dismissed back to its content. Clear the
        // context without emitting Close again, including on Escape/outside taps.
        if actions.iter().any(|a| matches!(a.downcast_ref(), Some(ModalAction::Dismissed))) {
            self.clear(cx);
            return;
        }
        if self.context.is_none() {
            return;
        }
        if self.view.button(cx, ids!(cancel_button)).clicked(actions) {
            self.clear(cx);
            cx.action(RoomAppPickerAction::Close);
            return;
        }
        if self.view.button(cx, ids!(all_apps_button)).clicked(actions) {
            self.clear(cx);
            cx.action(RoomAppPickerAction::Close);
            cx.action(NavigationBarAction::GoToMiniApps);
            return;
        }

        if let Some(keywords) = self.view.text_input(cx, ids!(search_input)).changed(actions) {
            self.keywords = keywords.trim().to_lowercase();
            self.view.portal_list(cx, ids!(apps_list)).set_first_id_and_scroll(0, 0.0);
            self.view.redraw(cx);
        }

        let list = self.view.portal_list(cx, ids!(apps_list));
        if list.was_scrolling() {
            return;
        }
        for (_, row) in list.items_with_actions(actions) {
            let MiniAppRowAction::OpenApp(app_id) = actions.find_widget_action(row.widget_uid()).cast() else {
                continue;
            };
            let Some((room_name_id, is_space)) = self.context.as_ref() else { return };
            // Re-check the current manifest in case an app changed since drawing.
            let compatible = with_a2app(|state| {
                state.registry.get(&app_id).is_some_and(|m| {
                    m.can_run_in_context(room_name_id.room_id().as_str(), *is_space)
                })
            }).unwrap_or(false);
            if !compatible {
                self.view.redraw(cx);
                return;
            }
            let room_id = room_name_id.room_id().clone();
            let is_space = *is_space;
            self.clear(cx);
            cx.action(RoomAppPickerAction::Close);
            if is_space {
                cx.action(A2AppOp::OpenApp { app_id, room_id: Some(room_id), in_room_pane: false });
            } else {
                cx.action(A2AppOp::OpenInRoom { app_id, room_id });
            }
            return;
        }
    }
}

impl RoomAppPicker {
    fn show(&mut self, cx: &mut Cx, room_name_id: RoomNameId, is_space: bool) {
        self.view.label(cx, ids!(context_name)).set_text(cx, &room_name_id.display());
        self.context = Some((room_name_id, is_space));
        self.keywords.clear();
        let input = self.view.text_input(cx, ids!(search_input));
        input.set_text(cx, "");
        self.load_apps(cx);
        self.view.portal_list(cx, ids!(apps_list)).set_first_id_and_scroll(0, 0.0);
        self.view.button(cx, ids!(cancel_button)).reset_hover(cx);
        self.view.button(cx, ids!(all_apps_button)).reset_hover(cx);
        self.view.redraw(cx);
    }

    fn clear(&mut self, cx: &mut Cx) {
        self.context = None;
        self.rows.clear();
        self.keywords.clear();
        self.view.redraw(cx);
    }

    fn load_apps(&mut self, cx: &mut Cx) {
        let Some((room_name_id, is_space)) = self.context.as_ref() else {
            self.rows.clear();
            return;
        };
        let keywords = &self.keywords;
        self.rows = mini_app_rows(cx, |manifest| {
            manifest.can_run_in_context(room_name_id.room_id().as_str(), *is_space)
                && (keywords.is_empty()
                    || manifest.name.to_lowercase().contains(keywords)
                    || manifest.description.to_lowercase().contains(keywords))
        });
        for row in &mut self.rows {
            row.open_label = "Run";
        }
    }
}

impl RoomAppPickerRef {
    pub fn show(&self, cx: &mut Cx, room_name_id: RoomNameId, is_space: bool) {
        let Some(mut inner) = self.borrow_mut() else { return };
        inner.show(cx, room_name_id, is_space);
    }

    pub fn clear(&self, cx: &mut Cx) {
        let Some(mut inner) = self.borrow_mut() else { return };
        inner.clear(cx);
    }
}
