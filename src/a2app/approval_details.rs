//! Searchable, grouped sources behind a saved approval's compact summary.

use makepad_widgets::*;
use a2app_core::information_flow::{Label as SourceLabel, Source};
use super::permission_choices::PermissionChoicesWidgetExt;

script_mod! {
    use mod.prelude.widgets.*
    use mod.widgets.*

    mod.widgets.ApprovalDetails = set_type_default() do #(ApprovalDetails::register_widget(vm)) {
        ..mod.widgets.RoundedView
        width: Fill{max: 800}, height: Fill, margin: 20
        flow: Down, spacing: 12, padding: 20
        show_bg: true
        draw_bg +: { color: (COLOR_PRIMARY), border_radius: 6.0 }
        View {
            width: Fill, height: Fit, flow: Right, spacing: 10
            title := TitleLabel { width: Fill, margin: 0, text: "Approval details" }
            close_button := RobrixNeutralIconButton {
                padding: 8, draw_icon +: { svg: (ICON_CLOSE) }
                icon_walk: Walk{width: 14, height: 14, margin: 0}, text: ""
            }
        }
        ScrollYView {
            width: Fill, height: Fit{max: FitBound.Abs(180.0)}, flow: Down
            summary := mod.widgets.PermissionOptionLabel {}
        }
        category := mod.widgets.PermissionChoices {
            horizontal: true
            labels: ["All data", "Rooms", "Conversations", "Account and other"]
        }
        search := RobrixTextInput {
            width: Fill, height: Fit
            empty_text: "Search names or room IDs…"
        }
        sources := PortalList {
            width: Fill, height: Fill, flow: Down
            SourceRow := View {
                width: Fill, height: Fit, flow: Down, spacing: 6, padding: 8
                name := mod.widgets.PermissionOptionLabel {}
                identity := mod.widgets.PermissionOptionLabel {}
                LineH {}
            }
            Empty := mod.widgets.PermissionOptionLabel { padding: 8 }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SourceCategory { Room, Conversation, Account, Directory, Other }

#[derive(Clone, Debug)]
pub(super) struct ApprovalSource {
    category: SourceCategory,
    name: String,
    identity: String,
}

/// Resolve presentation metadata once when opening details, without changing
/// the approval or expanding its scope to newly joined rooms.
pub(super) fn approval_sources(
    sources: &SourceLabel,
    mut room_info: impl FnMut(&str) -> (String, bool),
) -> Vec<ApprovalSource> {
    let mut rows = sources.iter().map(|source| match source {
        Source::Account { account } => ApprovalSource {
            category: SourceCategory::Account, name: "Your account information".into(), identity: account.clone(),
        },
        Source::Room { account, room } => {
            let (name, direct) = room_info(room);
            ApprovalSource { category: if direct { SourceCategory::Conversation } else { SourceCategory::Room },
                name, identity: format!("{room}\nAccount: {account}") }
        }
        Source::UnknownPrivate => ApprovalSource {
            category: SourceCategory::Other, name: "Previously received private data".into(),
            identity: "Its original source is not recorded.".into(),
        },
        Source::RoomDirectory { account } => ApprovalSource {
            category: SourceCategory::Directory, name: "Room and space directory".into(),
            identity: format!("Account: {account}"),
        },
    }).collect::<Vec<_>>();
    rows.sort_by_key(|row| (match row.category {
        SourceCategory::Account => 0, SourceCategory::Room => 1,
        SourceCategory::Conversation => 2, SourceCategory::Directory => 3, SourceCategory::Other => 4,
    }, row.name.to_lowercase(), row.identity.clone()));
    rows
}

pub(super) fn source_summary(rows: &[ApprovalSource]) -> String {
    let count = |category| rows.iter().filter(|row| row.category == category).count();
    let mut parts = Vec::new();
    if count(SourceCategory::Account) > 0 { parts.push("your account information".into()); }
    for (category, singular, plural) in [
        (SourceCategory::Room, "room", "rooms"),
        (SourceCategory::Conversation, "direct conversation", "direct conversations"),
    ] {
        let count = count(category);
        if count > 0 { parts.push(format!("data from {count} {}", if count == 1 { singular } else { plural })); }
    }
    if count(SourceCategory::Directory) > 0 { parts.push("your room and space directory".into()); }
    if count(SourceCategory::Other) > 0 { parts.push("previously received private data".into()); }
    if parts.is_empty() { "No private data sources in this approval.".into() }
    else { format!("May use {}.", parts.join("; ")) }
}

fn matches_filter(row: &ApprovalSource, category: usize, query: &str) -> bool {
    let category_matches = match category {
        1 => row.category == SourceCategory::Room,
        2 => row.category == SourceCategory::Conversation,
        3 => matches!(row.category, SourceCategory::Account | SourceCategory::Directory | SourceCategory::Other),
        _ => true,
    };
    category_matches && (row.name.to_lowercase().contains(query) || row.identity.to_lowercase().contains(query))
}

#[derive(Script, ScriptHook, Widget)]
pub struct ApprovalDetails {
    #[deref] view: View,
    #[rust] sources: Vec<ApprovalSource>,
}

impl Widget for ApprovalDetails {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.handle_event(cx, event, scope);
        if let Event::Actions(actions) = event {
            if self.view.permission_choices(cx, ids!(category)).changed(actions).is_some()
                || self.view.text_input(cx, ids!(search)).changed(actions).is_some()
            {
                self.view.portal_list(cx, ids!(sources)).set_first_id_and_scroll(0, 0.0);
                self.view.redraw(cx);
            }
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        let category = self.view.permission_choices(cx, ids!(category)).selected_item();
        let query = self.view.text_input(cx, ids!(search)).text().trim().to_lowercase();
        let rows = self.sources.iter().filter(|row| matches_filter(row, category, &query)).collect::<Vec<_>>();
        while let Some(widget) = self.view.draw_walk(cx, scope, walk).step() {
            let portal = widget.as_portal_list();
            let Some(mut list) = portal.borrow_mut() else { continue };
            let count = rows.len().max(1);
            list.set_item_range(cx, 0, count);
            while let Some(index) = list.next_visible_item(cx) {
                if index >= count { continue; }
                let item = if let Some(row) = rows.get(index) {
                    let item = list.item(cx, index, id!(SourceRow));
                    let kind = match row.category {
                        SourceCategory::Room => "Room", SourceCategory::Conversation => "Direct conversation",
                        SourceCategory::Account => "Account", SourceCategory::Directory => "Directory",
                        SourceCategory::Other => "Other private data",
                    };
                    item.label(cx, ids!(name)).set_text(cx, &format!("{kind}: {}", row.name));
                    item.label(cx, ids!(identity)).set_text(cx, &row.identity);
                    item
                } else {
                    let item = list.item(cx, index, id!(Empty));
                    item.set_text(cx, if self.sources.is_empty() { "No private data sources in this approval." }
                        else { "No sources match these filters." });
                    item
                };
                item.draw_all(cx, &mut Scope::empty());
            }
        }
        DrawStep::done()
    }
}

impl ApprovalDetailsRef {
    pub(super) fn configure(&self, cx: &mut Cx, summary: &str, sources: Vec<ApprovalSource>) {
        let Some(mut inner) = self.borrow_mut() else { return };
        inner.sources = sources;
        inner.view.label(cx, ids!(summary)).set_text(cx, summary);
        inner.view.permission_choices(cx, ids!(category)).set_selected_item(cx, 0);
        inner.view.text_input(cx, ids!(search)).set_text(cx, "");
        inner.view.portal_list(cx, ids!(sources)).set_first_id_and_scroll(0, 0.0);
        inner.view.redraw(cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_is_bounded_and_details_keep_every_source_searchable() {
        let mut sources = SourceLabel::from([Source::Account { account: "@me:example.org".into() }]);
        for index in 0..60 {
            sources.insert(Source::Room { account: "@me:example.org".into(), room: format!("!room{index}:example.org") });
        }
        let rows = approval_sources(&sources, |id| (format!("Name for {id}"), id == "!room0:example.org"));
        assert_eq!(source_summary(&rows), "May use your account information; data from 59 rooms; data from 1 direct conversation.");
        assert_eq!(rows.iter().filter(|row| matches_filter(row, 1, "")).count(), 59);
        assert_eq!(rows.iter().filter(|row| matches_filter(row, 2, "room0")).count(), 1);
        assert_eq!(rows.iter().filter(|row| matches_filter(row, 3, "@me")).count(), 1);
        assert_eq!(rows.iter().filter(|row| matches_filter(row, 0, "!room59:")).count(), 1);
        assert!(!rows.iter().any(|row| matches_filter(row, 1, "room0:")));
    }

    #[test]
    fn unknown_and_empty_sources_remain_explicit() {
        let rows = approval_sources(&SourceLabel::from([Source::UnknownPrivate]), |_| unreachable!());
        assert_eq!(source_summary(&rows), "May use previously received private data.");
        assert!(matches_filter(&rows[0], 3, "source is not recorded"));
        assert_eq!(source_summary(&[]), "No private data sources in this approval.");
    }

    #[test]
    fn filtering_after_scrolling_returns_to_the_first_matching_source() {
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let widget = cx.with_vm(|vm| {
            makepad_widgets::script_mod(vm);
            crate::shared::script_mod(vm);
            super::super::permission_choices::script_mod(vm);
            super::script_mod(vm);
            let value = script_eval!(vm, { mod.widgets.ApprovalDetails {} });
            WidgetRef::script_from_value(vm, value)
        });
        let mut details = widget.borrow_mut::<ApprovalDetails>().unwrap();
        let list = details.view.portal_list(&cx, ids!(sources));
        list.set_first_id_and_scroll(40, -20.0);
        let search_uid = details.view.text_input(&cx, ids!(search)).widget_uid();
        let changed = cx.capture_actions(|cx| cx.widget_action(search_uid, TextInputAction::Changed("Alice".into())));
        details.handle_event(&mut cx, &Event::Actions(changed), &mut Scope::empty());
        assert_eq!(list.first_id(), 0);
        list.set_first_id_and_scroll(30, -10.0);
        let category_uid = details.view.permission_choices(&cx, ids!(category)).widget_uid();
        let changed = cx.capture_actions(|cx| cx.widget_action(category_uid,
            super::super::permission_choices::PermissionChoicesAction::Changed(2)));
        details.handle_event(&mut cx, &Event::Actions(changed), &mut Scope::empty());
        assert_eq!(list.first_id(), 0);
    }
}
