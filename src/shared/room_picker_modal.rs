//! A modal that lets the user pick one of their joined rooms.
//!
//! Open it by emitting `RoomPickerModalAction::Show` with a `RoomPickerContent`;
//! the `on_picked` callback receives the chosen room.

use std::{borrow::Cow, cell::RefCell};
use makepad_widgets::*;
use crate::{
    home::rooms_list::{RoomPickerEntry, RoomsListRef},
    room::FetchedRoomAvatar,
    shared::{avatar::AvatarWidgetRefExt, room_filter_input_bar::RoomFilterInputBarWidgetExt},
    utils::{self, RoomNameId},
};

script_mod! {
    use mod.prelude.widgets.*
    use mod.widgets.*

    // One joined room in the picker's list.
    mod.widgets.RoomPickerRow = RoundedView {
        width: Fill, height: 52, flow: Right, spacing: 9, align: Align{y: 0.5}
        padding: Inset{left: 10, right: 10}
        show_bg: true, cursor: MouseCursor.Hand
        draw_bg +: { color: #00000000, border_radius: 5.0 }
        avatar := Avatar { width: 30, height: 30 }
        info := View {
            width: Fill, height: Fit, flow: Down, spacing: 1
            title := Label {
                width: Fill, height: Fit, max_lines: 1, text_overflow: Ellipsis, padding: 0
                draw_text +: { color: (COLOR_TEXT), text_style: theme.font_bold {font_size: 11, line_spacing: 1.0} }
            }
            subtitle := Label {
                width: Fill, height: Fit, max_lines: 1, text_overflow: Ellipsis, padding: 0
                draw_text +: { color: #555, text_style: theme.font_regular {font_size: 9.5, line_spacing: 1.0} }
            }
        }
    }

    mod.widgets.RoomPickerModal = set_type_default() do #(RoomPickerModal::register_widget(vm)) {
        ..mod.widgets.SmallModal

        color_hover: #xEAEFF5

        title := ModalTitle {}

        filter_bar := RoomFilterInputBar {
            input +: { empty_text: "Filter rooms..." }
        }

        list := PortalList {
            width: Fill, height: 320
            flow: Down
            margin: Inset{top: 10}
            row := mod.widgets.RoomPickerRow {}
            empty_row := View {
                width: Fill, height: 52, align: Align{x: 0.5, y: 0.5}
                Label {
                    height: Fit
                    draw_text +: { color: #555, text_style: theme.font_regular {font_size: 10.5} }
                    text: "No rooms match"
                }
            }
        }

        buttons_view := ModalButtonsRow {
            cancel_button := RobrixNeutralIconButton {
                width: Fit{min: FitBound.Abs(120.0)},
                align: Align{x: 0.5, y: 0.5}
                padding: 12,
                draw_icon.svg: (ICON_FORBIDDEN)
                icon_walk: Walk{width: 16, height: 16, margin: Inset{left: -2, right: -1} }
                text: "Cancel"
            }
        }
    }
}

pub type OnRoomPicked = Box<dyn FnOnce(&mut Cx, RoomNameId)>;

/// What the picker shows and what to do with the room the user picks.
#[derive(Default)]
pub struct RoomPickerContent {
    pub title: Cow<'static, str>,
    pub on_picked: Option<OnRoomPicked>,
}
impl std::fmt::Debug for RoomPickerContent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoomPickerContent")
            .field("title", &self.title)
            .field("on_picked", &self.on_picked.is_some())
            .finish()
    }
}

/// Opens or closes the app-level room picker modal. This is NOT a widget action.
#[derive(Debug, Default)]
pub enum RoomPickerModalAction {
    /// The `RefCell` lets the one handler take ownership of the content instead of cloning it.
    Show(RefCell<Option<RoomPickerContent>>),
    Close,
    #[default]
    None,
}

#[derive(Script, ScriptHook, Widget)]
pub struct RoomPickerModal {
    #[deref] view: View,
    #[live] color_hover: Vec4f,
    #[rust] content: RoomPickerContent,
    #[rust] rooms: Vec<RoomPickerEntry>,
    #[rust] hovered_index: Option<usize>,
}

impl Widget for RoomPickerModal {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.handle_event(cx, event, scope);
        self.widget_match_event(cx, event, scope);
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        while let Some(widget) = self.view.draw_walk(cx, scope, walk).step() {
            let portal_list = widget.as_portal_list();
            let Some(mut list) = portal_list.borrow_mut() else { continue };
            // With no matches we draw the single "no rooms" row.
            let count = self.rooms.len().max(1);
            list.set_item_range(cx, 0, count);
            while let Some(index) = list.next_visible_item(cx) {
                if index >= count {
                    continue;
                }
                let row = match self.rooms.get(index) {
                    Some(entry) => {
                        let mut row = list.item(cx, index, id!(row));
                        row.label(cx, ids!(info.title)).set_text(cx, &entry.room_name_id.display());
                        let subtitle = if entry.is_direct {
                            "Direct message"
                        } else {
                            ""
                        };
                        row.label(cx, ids!(info.subtitle)).set_text(cx, subtitle);
                        let avatar = row.avatar(cx, ids!(avatar));
                        match &entry.room_avatar {
                            FetchedRoomAvatar::Text(text) => avatar.show_text(cx, None, None, text),
                            FetchedRoomAvatar::Image(image) => {
                                let _ = avatar.show_image(cx, None, |cx, img| utils::load_avatar_image(&img, cx, image));
                            }
                        }
                        let color = if self.hovered_index == Some(index) { self.color_hover } else { Vec4f::default() };
                        script_apply_eval!(cx, row, { draw_bg.color: #(color) });
                        row
                    }
                    None => list.item(cx, index, id!(empty_row)),
                };
                row.draw_all(cx, scope);
            }
        }
        DrawStep::done()
    }
}

impl WidgetMatchEvent for RoomPickerModal {
    fn handle_actions(&mut self, cx: &mut Cx, actions: &Actions, _scope: &mut Scope) {
        let cancel_clicked = self.view.button(cx, ids!(cancel_button)).clicked(actions);
        if cancel_clicked ||
            actions.iter().any(|a| matches!(a.downcast_ref(), Some(ModalAction::Dismissed)))
        {
            // Closing the modal delivers `Dismissed` back to us, so only the cancel click
            // may emit `Close`; re-emitting it on `Dismissed` would loop forever.
            if cancel_clicked {
                cx.action(RoomPickerModalAction::Close);
            } else {
                self.content = RoomPickerContent::default();
                self.rooms.clear();
                self.hovered_index = None;
            }
            return;
        }

        if let Some(keywords) = self.view.room_filter_input_bar(cx, ids!(filter_bar)).changed(actions) {
            self.load_rooms(cx, &keywords);
            self.view.portal_list(cx, ids!(list)).set_first_id_and_scroll(0, 0.0);
            self.view.redraw(cx);
        }

        let list = self.view.portal_list(cx, ids!(list));
        let mut picked_index = None;
        let mut should_redraw = false;
        for (index, widget) in list.items_with_actions(actions) {
            let row = widget.as_view();
            // A touch that drags is a scroll, not a tap on the row.
            if !list.was_scrolling()
                && let Some(fe) = row.finger_up(actions)
                && fe.is_over && fe.is_primary_hit() && fe.was_tap()
            {
                picked_index = Some(index);
            }
            if row.finger_hover_in(actions).is_some() {
                self.hovered_index = Some(index);
                should_redraw = true;
            }
            if row.finger_hover_out(actions).is_some() && self.hovered_index == Some(index) {
                self.hovered_index = None;
                should_redraw = true;
            }
        }
        if let Some(entry) = picked_index.and_then(|i| self.rooms.get(i)) {
            let room_name_id = entry.room_name_id.clone();
            if let Some(on_picked) = self.content.on_picked.take() {
                on_picked(cx, room_name_id);
            }
            cx.action(RoomPickerModalAction::Close);
            return;
        }
        if should_redraw {
            self.view.redraw(cx);
        }
    }
}

impl RoomPickerModal {
    fn show(&mut self, cx: &mut Cx, content: RoomPickerContent) {
        self.view.label(cx, ids!(title)).set_text(cx, &content.title);
        self.content = content;
        let input = self.view.text_input(cx, ids!(filter_bar.input));
        input.set_text(cx, "");
        self.view.button(cx, ids!(filter_bar.clear_button)).set_visible(cx, false);
        self.load_rooms(cx, "");
        self.view.portal_list(cx, ids!(list)).set_first_id_and_scroll(0, 0.0);
        self.view.button(cx, ids!(cancel_button)).reset_hover(cx);
        self.view.redraw(cx);
        input.set_key_focus(cx);
    }

    fn load_rooms(&mut self, cx: &mut Cx, keywords: &str) {
        self.rooms = if cx.has_global::<RoomsListRef>() {
            cx.get_global::<RoomsListRef>().joined_rooms_matching(keywords)
        } else {
            Vec::new()
        };
        self.hovered_index = None;
    }
}

impl RoomPickerModalRef {
    /// Fills the picker with the given content and the current joined rooms.
    pub fn show(&self, cx: &mut Cx, content: RoomPickerContent) {
        let Some(mut inner) = self.borrow_mut() else { return };
        inner.show(cx, content);
    }
}
