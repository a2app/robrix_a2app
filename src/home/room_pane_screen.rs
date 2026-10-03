//! A room's pane (e.g., its member list or a mini-app) that was popped out of its room
//! into its own dock tab (desktop) or stack view (mobile).

use makepad_widgets::*;
use matrix_sdk_ui::sync_service::State as SyncServiceState;

use crate::{
    app::{AppStateAction, SelectedRoom},
    home::{rooms_list::RoomsListAction, rooms_list_header::RoomsListHeaderAction},
    profile::user_profile::UserProfileSlidingPaneWidgetExt,
    room::{
        room_members_list::{RoomMembersChanged, RoomMembersFetchAction, RoomMembersListAction, RoomMembersListWidgetRefExt, show_member_profile},
        pane_dock::{SavedPaneContent, populate_content, restore_content, save_content, set_pane_header},
        pinned_messages_list::PinnedMessagesListWidgetRefExt,
        room_pane::{self, RoomPaneKind, RoomPaneOp, RoomPaneRequest, mini_app_panes},
        threads_list::ThreadsListWidgetRefExt,
    },
    sliding_sync::{MatrixRequest, submit_async_request},
    utils::RoomNameId,
};

script_mod! {
    use mod.prelude.widgets.*
    use mod.widgets.*

    mod.widgets.RoomPaneScreen = set_type_default() do #(RoomPaneScreen::register_widget(vm)) {
        ..mod.widgets.SolidView
        width: Fill, height: Fill
        flow: Overlay
        show_bg: true
        draw_bg +: { color: (COLOR_PRIMARY) }

        pane_screen_content := View {
            width: Fill, height: Fill
            flow: Down

            header := SolidView {
                width: Fill, height: Fit
                flow: Right
                spacing: 6
                padding: Inset{top: 8, right: 10, bottom: 8, left: 10}
                show_bg: true
                draw_bg +: { color: (COLOR_BG_LAVENDER) }

                title_row := mod.widgets.RoomPaneTitle {}

                return_button := RobrixIconButton {
                    padding: Inset{top: 6, bottom: 6, left: 10, right: 12}
                    spacing: 6
                    draw_icon.svg: (ICON_JUMP)
                    icon_walk: Walk{width: 12, height: 12}
                    text: "Back to room"
                }
            }

            content := View {
                width: Fill, height: Fill
                flow: Down
                padding: Inset{top: 8, right: 0, bottom: 8, left: 10}
                room_members := mod.widgets.RoomMembersList { visible: false }
                pinned_messages := mod.widgets.PinnedMessagesList { visible: false }
                mini_app_host := mod.widgets.MiniAppHostArea { visible: false }
                threads := mod.widgets.ThreadsList { visible: false }
            }
        }

        // Shown when clicking on a member, on top of all other content.
        user_profile_sliding_pane := mod.widgets.UserProfileSlidingPane { }
    }
}

/// Widget actions emitted by a [`RoomPaneScreen`].
#[derive(Clone, Debug, Default)]
pub enum RoomPaneScreenAction {
    /// The user asked to dock this pane back into its room.
    /// The popped-out pane's tab or view should be closed, and the room shown.
    ReturnToRoom {
        room_name_id: RoomNameId,
        kind: RoomPaneKind,
    },
    /// This pane's content went away (e.g., its mini-app was closed),
    /// so its tab or view should be closed.
    Closed {
        room_name_id: RoomNameId,
        kind: RoomPaneKind,
    },
    #[default]
    None,
}

#[derive(Script, ScriptHook, Widget)]
pub struct RoomPaneScreen {
    #[deref] view: View,
    /// The room and kind of pane being displayed.
    #[rust] displayed: Option<(RoomNameId, RoomPaneKind)>,
    /// Whether this screen let go of its mini-app (e.g., to return it to its room),
    /// such that it must not show or quit it again.
    #[rust] is_vacated: bool,
}

impl Drop for RoomPaneScreen {
    fn drop(&mut self) {
        self.save_popped_out_state();
        mini_app_panes::release_all_shown_by(self.widget_uid());
    }
}

impl Widget for RoomPaneScreen {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        // A reapply resets the title's padding that we set, so we set it again.
        if let Event::ScriptReapply = event
            && let Some((_, kind)) = self.displayed.as_ref()
        {
            set_pane_header(cx, &self.view.widget(cx, ids!(title_row)), kind);
        }

        if let Event::Actions(actions) = event {
            let mut members_changed = false;
            for action in actions {
                if let Some(
                    AppStateAction::RoomNameUpdated(new_name)
                    | AppStateAction::RoomLoadedSuccessfully { room_name_id: new_name, .. }
                ) = action.downcast_ref()
                    && let Some((room_name_id, kind)) = self.displayed.clone()
                    && room_name_id.room_id() == new_name.room_id()
                {
                    self.set_displayed(cx, new_name, kind);
                }

                if let Some(RoomPaneRequest { room_id, kind, op }) = action.downcast_ref()
                    && let Some((room_name_id, displayed_kind)) = self.displayed.clone()
                    && room_name_id.room_id() == room_id
                    && &displayed_kind == kind
                    && !self.is_vacated
                {
                    self.handle_request(cx, room_name_id, displayed_kind, *op);
                }

                // Without a timeline, we fetch this room's members ourselves.
                let Some((room_name_id, RoomPaneKind::Members)) = self.displayed.as_ref() else { continue };
                let members_list = self.view.child_by_path(ids!(content.room_members)).as_room_members_list();
                match action.downcast_ref() {
                    Some(RoomMembersFetchAction::Fetched { room_id, members }) if room_id == room_name_id.room_id() => {
                        members_list.set_members(cx, room_name_id, Some(members.clone()));
                    }
                    Some(RoomMembersFetchAction::Failed { room_id, error }) if room_id == room_name_id.room_id() => {
                        error!("Failed to fetch members of room {room_id}: {error}");
                        members_list.set_error(cx, error.clone());
                    }
                    _ => {}
                }
                members_changed |= action.downcast_ref::<RoomMembersChanged>()
                    .is_some_and(|changed| changed.room_id == *room_name_id.room_id());
                // Retry syncing members that failed to sync while we were offline.
                members_changed |= matches!(
                    action.downcast_ref(),
                    Some(RoomsListHeaderAction::StateUpdate(state)) if !matches!(state, SyncServiceState::Offline)
                );
            }
            if members_changed {
                self.fetch_members(false);
            }
        }

        let profile_pane = self.view.user_profile_sliding_pane(cx, ids!(user_profile_sliding_pane));
        let actions = cx.capture_actions(|cx| {
            // While the profile pane is shown, it gets all of the user's input.
            if profile_pane.is_currently_shown(cx) && crate::utils::is_interactive_hit_event(event) {
                profile_pane.handle_event(cx, event, scope);
            } else {
                self.view.handle_event(cx, event, scope);
            }
        });

        let mut unhandled = ActionsBuf::new();
        for action in actions {
            if let RoomMembersListAction::MemberClicked { room_name_id, member } = action.as_widget_action().cast() {
                show_member_profile(cx, &profile_pane, &room_name_id, member);
                // There's no timeline here in which to jump to a read receipt.
                profile_pane.button(cx, ids!(jump_to_read_receipt_button)).set_visible(cx, false);
                self.redraw(cx);
                continue;
            }
            unhandled.push(action);
        }
        let return_clicked = self.view.button(cx, ids!(return_button)).clicked(&unhandled);
        cx.extend_actions(unhandled);

        // Whoever handles this also calls `vacate()`, once it's sure to return the pane to its room.
        if return_clicked && let Some((room_name_id, kind)) = self.displayed.clone() {
            cx.widget_action(self.widget_uid(), RoomPaneScreenAction::ReturnToRoom { room_name_id, kind });
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        let step = self.view.draw_walk(cx, scope, walk);
        if let Some((room_name_id, RoomPaneKind::MiniApp(app_id))) = self.displayed.as_ref()
            && !self.is_vacated
        {
            let host_area = self.view.widget(cx, ids!(content.mini_app_host));
            mini_app_panes::note_size(&host_area, room_name_id.room_id(), app_id);
        }
        step
    }
}

impl RoomPaneScreen {
    /// Displays the given kind of pane for the given room; call it again whenever the room's name changes.
    /// Returns false (and emits `Closed`) if its mini-app isn't running or is shown elsewhere.
    pub fn set_displayed(&mut self, cx: &mut Cx, room_name_id: &RoomNameId, kind: RoomPaneKind) -> bool {
        let is_same = self.displayed.as_ref()
            .is_some_and(|(r, k)| r.room_id() == room_name_id.room_id() && *k == kind);
        if !is_same {
            self.hide_displayed(cx);
        }
        set_pane_header(cx, &self.view.widget(cx, ids!(title_row)), &kind);
        self.view.label(cx, ids!(pane_room)).set_text(cx, &room_name_id.to_string());
        self.view.child_by_path(ids!(content.room_members)).set_visible(cx, kind == RoomPaneKind::Members);
        self.view.child_by_path(ids!(content.pinned_messages)).set_visible(cx, kind == RoomPaneKind::PinnedMessages);
        self.view.child_by_path(ids!(content.threads)).set_visible(cx, kind == RoomPaneKind::Threads);
        let host_area = self.view.widget(cx, ids!(content.mini_app_host));
        host_area.set_visible(cx, matches!(kind, RoomPaneKind::MiniApp(_)));
        self.displayed = Some((room_name_id.clone(), kind.clone()));
        if let RoomPaneKind::MiniApp(app_id) = &kind {
            // A vacated screen is about to close, so it must not take its app back.
            let room_id = room_name_id.room_id();
            if !self.is_vacated
                && !mini_app_panes::is_shown_by(room_id, app_id, self.widget_uid())
                && !mini_app_panes::attach(cx, &host_area, self.widget_uid(), room_id, app_id, None, false)
            {
                self.is_vacated = true;
                cx.widget_action(self.widget_uid(), RoomPaneScreenAction::Closed { room_name_id: room_name_id.clone(), kind });
                return false;
            }
        } else {
            // If we just popped out or hid a room pane, restore its previous state/content.
            let frame = self.view.child_by_path(ids!(pane_screen_content));
            match room_pane::take_popped_out_state(room_name_id.room_id(), &kind) {
                Some(saved) => restore_content(cx, &frame, room_name_id, saved, &None),
                None => populate_content(cx, &kind, &frame, room_name_id, &None),
            }
        }
        // Also re-fetch members upon re-showing the pane, in case we missed some membership changes.
        self.fetch_members(false);
        self.redraw(cx);
        true
    }

    /// Applies a request to this screen's pane, e.g., from its mini-app asking to be closed.
    fn handle_request(&mut self, cx: &mut Cx, room_name_id: RoomNameId, kind: RoomPaneKind, op: RoomPaneOp) {
        match op {
            RoomPaneOp::Close | RoomPaneOp::Remove => {
                self.detach_mini_app(cx, matches!(op, RoomPaneOp::Close));
                self.is_vacated = true;
                cx.widget_action(self.widget_uid(), RoomPaneScreenAction::Closed { room_name_id, kind });
            }
            RoomPaneOp::Focus => {
                cx.widget_action(self.widget_uid(), RoomsListAction::Selected(SelectedRoom::RoomPane { room_name_id, kind }));
            }
            RoomPaneOp::Open | RoomPaneOp::MoveTo(_) | RoomPaneOp::PopOut | RoomPaneOp::Minimize => {}
        }
    }

    /// Stops showing this screen's mini-app, if any, and either quits or parks it.
    fn detach_mini_app(&mut self, cx: &mut Cx, is_quitting: bool) {
        if let Some((room_name_id, RoomPaneKind::MiniApp(app_id))) = self.displayed.as_ref() {
            let host_area = self.view.widget(cx, ids!(content.mini_app_host));
            mini_app_panes::detach(cx, &host_area, self.widget_uid(), room_name_id.room_id(), app_id, is_quitting);
        }
    }

    /// Lets go of this screen's mini-app, if any, so it can be docked in its room again.
    pub fn vacate(&mut self, cx: &mut Cx) {
        self.detach_mini_app(cx, false);
        self.is_vacated = true;
    }

    /// Closes this screen's content for good: its mini-app, if any, is quit.
    pub fn close_content(&mut self, cx: &mut Cx) {
        if self.is_vacated {
            return;
        }
        if let Some((room_name_id, RoomPaneKind::MiniApp(app_id))) = self.displayed.clone() {
            if mini_app_panes::is_shown_by(room_name_id.room_id(), &app_id, self.widget_uid()) {
                self.detach_mini_app(cx, true);
            } else {
                // E.g., a mobile view whose app was parked while another screen covered it.
                mini_app_panes::quit_if_parked(cx, room_name_id.room_id(), &app_id);
            }
        }
        self.is_vacated = true;
    }

    /// Fetches the displayed room's members.
    fn fetch_members(&self, local_only: bool) {
        if let Some((room_name_id, RoomPaneKind::Members)) = self.displayed.as_ref() {
            submit_async_request(MatrixRequest::GetRoomMembersList {
                room_id: room_name_id.room_id().clone(),
                local_only,
            });
        }
    }

    /// Saves and returns the state of this displayed pane, which takes its data subscription with it.
    fn save_state(&self) -> Option<SavedPaneContent> {
        let (_, kind) = self.displayed.as_ref()?;
        Some(save_content(kind, &self.view.child_by_path(ids!(pane_screen_content))))
    }

    /// Saves our displayed pane's state for the next time it's shown.
    fn save_popped_out_state(&self) {
        if self.is_vacated {
            return;
        }
        if let Some((room_name_id, kind)) = self.displayed.as_ref()
            && let Some(saved) = self.save_state()
        {
            room_pane::save_popped_out_state(room_name_id.room_id(), kind, saved);
        }
    }

    /// Stops displaying this screen's pane, keeping its state in case it's shown again.
    pub fn hide_displayed(&mut self, cx: &mut Cx) {
        self.save_popped_out_state();
        self.detach_mini_app(cx, false);
        self.view.user_profile_sliding_pane(cx, ids!(user_profile_sliding_pane)).reset(cx);
        self.view.child_by_path(ids!(content.room_members)).as_room_members_list().reset(cx);
        self.view.child_by_path(ids!(content.pinned_messages)).as_pinned_messages_list().reset(cx);
        self.view.child_by_path(ids!(content.threads)).as_threads_list().reset(cx);
        self.displayed = None;
        self.is_vacated = false;
    }
}

impl RoomPaneScreenRef {
    /// See [`RoomPaneScreen::set_displayed()`].
    pub fn set_displayed(&self, cx: &mut Cx, room_name_id: &RoomNameId, kind: RoomPaneKind) -> bool {
        self.borrow_mut().is_some_and(|mut inner| inner.set_displayed(cx, room_name_id, kind))
    }

    /// Returns whether this screen was given a pane to display.
    pub fn is_displaying(&self) -> bool {
        self.borrow().is_some_and(|inner| inner.displayed.is_some())
    }

    /// See [`RoomPaneScreen::vacate()`].
    pub fn vacate(&self, cx: &mut Cx) {
        if let Some(mut inner) = self.borrow_mut() {
            inner.vacate(cx);
        }
    }

    /// See [`RoomPaneScreen::close_content()`].
    pub fn close_content(&self, cx: &mut Cx) {
        if let Some(mut inner) = self.borrow_mut() {
            inner.close_content(cx);
        }
    }

    /// See [`RoomPaneScreen::hide_displayed()`].
    pub fn hide_displayed(&self, cx: &mut Cx) {
        if let Some(mut inner) = self.borrow_mut() {
            inner.hide_displayed(cx);
        }
    }

    /// Returns the state of this screen's pane, e.g., for docking it back into its timeline.
    /// See [`RoomPaneScreen::save_state()`].
    pub fn save_state(&self) -> Option<SavedPaneContent> {
        self.borrow()?.save_state()
    }

    /// Returns the saved state only if this screen currently displays the requested pane.
    pub fn save_state_for(&self, room_id: &ruma::RoomId, kind: &RoomPaneKind) -> Option<SavedPaneContent> {
        let inner = self.borrow()?;
        let (displayed_room, displayed_kind) = inner.displayed.as_ref()?;
        if displayed_room.room_id() != room_id || displayed_kind != kind {
            return None;
        }
        inner.save_state()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saving_requested_pane_ignores_a_different_room_or_kind() {
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let widget = cx.with_vm(|vm| {
            makepad_widgets::script_mod(vm);
            let pane_type = RoomPaneScreen::register_widget(vm);
            let value = script_eval!(vm, { #(pane_type) {} });
            WidgetRef::script_from_value(vm, value)
        });
        let screen = widget.as_room_pane_screen();
        let room = RoomNameId::empty("!pane-state:example.org".try_into().unwrap());
        let other_room: ruma::OwnedRoomId = "!other-pane:example.org".try_into().unwrap();
        assert!(screen.save_state_for(room.room_id(), &RoomPaneKind::PinnedMessages).is_none());
        widget.borrow_mut::<RoomPaneScreen>().unwrap().displayed = Some((room.clone(), RoomPaneKind::Members));
        assert!(screen.save_state_for(room.room_id(), &RoomPaneKind::PinnedMessages).is_none());
        widget.borrow_mut::<RoomPaneScreen>().unwrap().displayed = Some((room.clone(), RoomPaneKind::PinnedMessages));
        assert!(screen.save_state_for(&other_room, &RoomPaneKind::PinnedMessages).is_none());
        assert!(matches!(screen.save_state_for(room.room_id(), &RoomPaneKind::PinnedMessages), Some(SavedPaneContent::PinnedMessages(_))));
    }
}
