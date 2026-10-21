//! This module defines a collapsible header wrapper with a triangle icon
//! that indicates whether the header is expanded or collapsed.
//!
//! This widget can be clicked to toggle between expanded and collapsed.
//!
//! The collapsible header is *just* the header, it doesn't actually contain any content.
//! This design is necessary because the header is drawn within a PortalList,
//! and its content is also drawn within that PortalList separately from its content.

use makepad_widgets::*;
use makepad_widgets::animator::Animate;


use super::expand_arrow::ExpandArrow;
use super::unread_badge::UnreadBadgeWidgetRefExt as _;

script_mod! {
    use mod.prelude.widgets.*
    use mod.widgets.*


    mod.widgets.CollapsibleHeader = set_type_default() do #(CollapsibleHeader::register_widget(vm)) {
        ..mod.widgets.RoundedView

        width: Fill,
        height: 35,
        align: Align{ x: 0.0, y: 0.5 },
        margin: Inset{top: 3, bottom: 3, left: 0, right: 0},
        padding: 5
        flow: Right,

        cursor: MouseCursor.Hand,
        draw_bg +: {
            border_radius: 4.0
            color: mod.widgets.COLOR_HEADER_BG
        }

        collapse_icon := mod.widgets.ExpandArrow {
            width: 20, height: 20,
            margin: Inset{left: 5, right: 6, top: 0, bottom: 0},
            draw_bg.color: mod.widgets.COLOR_HEADER_FG
        }

        label := Label {
            padding: 0,
            width: Fill,
            height: Fit,
            flow: Flow.Right { wrap: false }
            text: "",
            text_overflow: TextOverflow.Ellipsis,
            max_lines: 1
            draw_text +: {
                text_style: mod.widgets.REGULAR_TEXT {font_size: 11},
                color: (mod.widgets.COLOR_HEADER_FG),
            }
        }

        unread_badge := UnreadBadge {
            margin: Inset{right: 5.5},
        }
    }

    // The room-category header reused for disclosures in settings and dialogs.
    mod.widgets.RobrixSettingsCollapsibleHeader = mod.widgets.CollapsibleHeader {
        height: 40, margin: 0
        title: "Details"
        is_expanded: false
        draw_bg +: { color: (COLOR_BG_PREVIEW) }
        collapse_icon +: { draw_bg.color: (MESSAGE_TEXT_COLOR) }
        label +: {
            draw_text +: {
                color: (MESSAGE_TEXT_COLOR)
                text_style: SETTINGS_BOLD_TEXT_STYLE {}
            }
        }
        unread_badge +: { visible: false }
    }
}

/// The categories of collapsible headers in the rooms list.
#[derive(Copy, Clone, Debug, Default)]
pub enum HeaderCategory {
    /// Rooms the user has been invited to but has not yet joined.
    Invites,
    /// Joined rooms that the user has marked as favorites.
    Favorites,
    /// Joined rooms that are direct messages with other users.
    DirectRooms,
    /// Joined rooms that are not direct messages or favorites.
    RegularRooms,
    /// Joined rooms that the user has marked as low priority.
    LowPriority,
    /// Rooms that the user has left.
    LeftRooms,
    #[default]
    None,
}
impl HeaderCategory {
    fn as_str(&self) -> &'static str {
        match self {
            HeaderCategory::Invites => "Invites",
            HeaderCategory::Favorites => "Favorites",
            HeaderCategory::RegularRooms => "Rooms",
            HeaderCategory::DirectRooms => "People",
            HeaderCategory::LowPriority => "Low Priority",
            HeaderCategory::LeftRooms => "Left Rooms",
            HeaderCategory::None => "",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub enum CollapsibleHeaderAction {
    /// The header was clicked to toggled its expanded/collapsed state.
    Toggled {
        category: HeaderCategory,
    },
    /// A standalone disclosure header changed its expanded state.
    ExpansionChanged(bool),
    #[default]
    None,
}

#[derive(Script, ScriptHook, Widget)]
pub struct CollapsibleHeader {
    #[deref] view: View,
    /// A standalone disclosure title; empty uses the room-category name.
    #[live] title: String,
    #[live(true)] is_expanded: bool,
    #[rust] category: HeaderCategory,
    #[rust] num_unread_mentions: u64,
    #[rust] num_unread_messages: u64,
}

impl Widget for CollapsibleHeader {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        if !self.view.visible() { return; }
        // Handle hits on this view as a whole before passing the event to the inner view.
        match event.hits(cx, self.view.area()) {
            Hit::FingerDown(..) => {
                cx.set_key_focus(self.view.area());
            }
            Hit::FingerUp(fe)
                if fe.is_over && fe.is_primary_hit() && fe.was_tap() =>
            {
                self.toggle_collapse(cx, scope);
            }
            Hit::KeyDown(key) if !key.is_repeat
                && matches!(key.key_code, KeyCode::Space | KeyCode::ReturnKey | KeyCode::NumpadEnter) =>
            {
                self.toggle_collapse(cx, scope);
            }
            _ => { }
        }
        self.view.handle_event(cx, event, scope);
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        if !self.view.visible() { return DrawStep::done(); }
        // Set arrow and label state during draw to ensure child widgets are available.
        if let Some(mut arrow) = self.view.child_by_path(ids!(collapse_icon)).borrow_mut::<ExpandArrow>() {
            arrow.set_is_open_no_animate(self.is_expanded);
        }
        let title = if self.title.is_empty() { self.category.as_str() } else { &self.title };
        self.view.child_by_path(ids!(label)).set_text(cx, title);
        self.view.child_by_path(ids!(unread_badge))
            .as_unread_badge()
            .update_counts(false, self.num_unread_mentions, self.num_unread_messages);
        let step = self.view.draw_walk(cx, scope, walk);
        cx.add_nav_stop(self.view.area(), NavRole::TextInput, Inset::default());
        step
    }
}

impl CollapsibleHeader {
    fn set_expanded(&mut self, cx: &mut Cx, is_expanded: bool, animate: Animate) {
        if self.is_expanded == is_expanded { return; }
        self.is_expanded = is_expanded;
        if let Some(mut arrow) = self.view.child_by_path(ids!(collapse_icon)).borrow_mut::<ExpandArrow>() {
            arrow.set_is_open(cx, is_expanded, animate);
        }
        self.redraw(cx);
    }

    fn toggle_collapse(&mut self, cx: &mut Cx, _scope: &mut Scope) {
        self.is_expanded = !self.is_expanded;
        if let Some(mut arrow) = self.view.child_by_path(ids!(collapse_icon)).borrow_mut::<ExpandArrow>() {
            arrow.set_is_open(cx, self.is_expanded, Animate::Yes);
        }
        self.redraw(cx);
        let action = if self.title.is_empty() {
            CollapsibleHeaderAction::Toggled {
                category: self.category,
            }
        } else {
            CollapsibleHeaderAction::ExpansionChanged(self.is_expanded)
        };
        cx.widget_action(self.widget_uid(), action);
    }
}

impl CollapsibleHeaderRef {
    /// Updates a standalone disclosure's title without changing its state.
    pub fn set_title(&self, cx: &mut Cx, title: &str) {
        if let Some(mut inner) = self.borrow_mut()
            && inner.title != title
        {
            inner.title = title.to_string();
            inner.redraw(cx);
        }
    }

    /// Sets the disclosure's state without changing its title or category.
    pub fn set_expanded(&self, cx: &mut Cx, is_expanded: bool, animate: Animate) {
        if let Some(mut inner) = self.borrow_mut() {
            inner.set_expanded(cx, is_expanded, animate);
        }
    }

    /// Returns the new state when a standalone disclosure was toggled.
    pub fn expansion_changed(&self, actions: &Actions) -> Option<bool> {
        match actions.find_widget_action(self.widget_uid()).cast() {
            CollapsibleHeaderAction::ExpansionChanged(expanded) => Some(expanded),
            _ => None,
        }
    }

    /// Sets the category and expanded state of the header.
    pub fn set_details(
        &self,
        _cx: &mut Cx,
        is_expanded: bool,
        category: HeaderCategory,
        num_unread_mentions: u64,
        num_unread_messages: u64,
    ) {
        if let Some(mut inner) = self.borrow_mut() {
            inner.is_expanded = is_expanded;
            inner.category = category;
            inner.num_unread_mentions = num_unread_mentions;
            inner.num_unread_messages = num_unread_messages;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_disclosures_start_closed_and_silent_sync_preserves_title() {
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let (settings, category) = cx.with_vm(|vm| {
            makepad_widgets::script_mod(vm);
            crate::shared::script_mod(vm, crate::settings::app_preferences::Appearance::Light);
            let settings = script_eval!(vm, { mod.widgets.RobrixSettingsCollapsibleHeader { title: "Individual abilities" } });
            let category = script_eval!(vm, { mod.widgets.CollapsibleHeader {} });
            (WidgetRef::script_from_value(vm, settings).as_collapsible_header(),
                WidgetRef::script_from_value(vm, category).as_collapsible_header())
        });
        assert!(!settings.borrow().unwrap().is_expanded);
        assert!(category.borrow().unwrap().is_expanded, "room categories keep their existing default");
        let actions = cx.capture_actions(|cx| {
            settings.set_expanded(cx, true, Animate::No);
            settings.set_title(cx, "Permission details");
        });
        assert!(actions.is_empty(), "render-state synchronization must not act like user approval");
        assert!(settings.borrow().unwrap().is_expanded);
        assert_eq!(settings.borrow().unwrap().title, "Permission details");
        for expanded in [false, true, false] {
            let actions = cx.capture_actions(|cx| settings.borrow_mut().unwrap().toggle_collapse(cx, &mut Scope::empty()));
            assert_eq!(settings.expansion_changed(&actions), Some(expanded));
            assert_eq!(settings.borrow().unwrap().title, "Permission details");
        }
        category.set_details(&mut cx, true, HeaderCategory::Favorites, 0, 0);
        let actions = cx.capture_actions(|cx| category.borrow_mut().unwrap().toggle_collapse(cx, &mut Scope::empty()));
        assert!(matches!(actions.find_widget_action(category.widget_uid()).cast(),
            CollapsibleHeaderAction::Toggled { category: HeaderCategory::Favorites }));
    }
}
