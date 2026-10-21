//! The upfront task permission prompt: one modal per task, showing the agent's
//! own plain-language explanation and a collapsible Details list of exactly
//! what Robrix will grant.
//!
//! Only the explanation comes from the agent. The Details list is Robrix's
//! own: the exact capability, scope, URL or tool for each row, with no agent
//! prose. What the user checks is what is applied.

use makepad_widgets::*;
use crate::shared::collapsible_header::CollapsibleHeaderWidgetExt;

script_mod! {
    use mod.prelude.widgets.*
    use mod.widgets.*

    mod.widgets.TaskPromptBody = Label {
        width: Fill, height: Fit, padding: 0, margin: 0
        flow: Flow.Right{wrap: true}
        draw_text +: { text_style: SETTINGS_REGULAR_TEXT_STYLE {}, color: (MESSAGE_TEXT_COLOR) }
    }

    mod.widgets.TaskPermissionPrompt = set_type_default() do #(TaskPermissionPrompt::register_widget(vm)) {
        ..mod.widgets.SmallModal

        width: Fill { max: 660 }
        height: Fill { max: 780 }
        padding: 20
        // Keep SmallModal's vertical scrolling: the explanation and expanded
        // Details may be taller than a narrow or short window.

        prompt_title := ModalTitle {
            margin: Inset{bottom: 10}
            text: "Your room's AI is asking for access"
        }
        agent_paragraph := ModalBody {
            draw_text +: { text_style: SETTINGS_REGULAR_TEXT_STYLE {}, color: (MESSAGE_TEXT_COLOR) }
        }
        risk_strip := RoundedView {
            visible: false
            width: Fill, height: Fit, padding: 12, margin: Inset{top: 10}
            draw_bg +: { color: (COLOR_BG_PREVIEW), border_radius: 4.0 }
            risk_text := mod.widgets.TaskPromptBody {}
        }
        details_toggle := RobrixSettingsCollapsibleHeader {
            margin: Inset{top: 10}
            title: "Details (0 items)"
        }
        details := ScrollYView {
            visible: false
            width: Fill, height: 340, flow: Down
            margin: Inset{top: 6}
            items := PortalList {
                width: Fill, height: Fill, flow: Down
                item := View {
                    width: Fill, height: Fit, flow: Down, spacing: 2
                    padding: Inset{top: 6, bottom: 6}
                    check := RobrixSettingsCheckBox {}
                    detail := mod.widgets.TaskPromptBody { visible: false }
                    LineH { height: 1 }
                }
            }
        }
        lasts := ModalBody {
            margin: Inset{top: 10}
            text: "This access lasts until this turn ends. Anything that should persist lives in Mini Apps."
        }

        ModalButtonsRow {
            spacing: 8, padding: Inset{top: 16, bottom: 0}
            allow_button := RobrixPositiveIconButton {
                padding: 12
                icon_walk: Walk{width: 0, height: 0, margin: 0}
                text: "Allow"
            }
            not_now_button := RobrixNeutralIconButton {
                padding: 12
                icon_walk: Walk{width: 0, height: 0, margin: 0}
                text: "Not now"
            }
        }
    }
}

/// One grantable row in the Details list: what the user can check or uncheck.
#[derive(Clone, Debug)]
pub struct TaskItemView {
    pub id: String,
    /// Robrix's own title for the exact operation (never the agent's prose).
    pub title: String,
    /// Robrix's exact, checkable detail line: ids, capability id, URL.
    pub detail: String,
    /// The state chip (Already allowed, Will be granted, Blocked and why).
    pub chip: String,
    /// Whether this item starts checked. Already-allowed and non-grantable
    /// items start unchecked and disabled, so Allow never re-grants them.
    pub grantable: bool,
    pub checked: bool,
}

/// What the modal shows for one resolved task plan.
#[derive(Clone, Debug)]
pub struct TaskPromptInfo {
    /// The agent's paragraph, shown verbatim and quoted.
    pub explanation: String,
    pub items: Vec<TaskItemView>,
    /// An optional host notice for the exact action being reviewed.
    pub risk: Option<String>,
}

/// The user's answer, emitted as a global action for the runtime to apply.
#[derive(Clone, Debug, Default)]
pub enum TaskPermissionAction {
    /// The checked item ids the user approved.
    Allow(Vec<String>),
    /// Nothing persists; this exact plan is refused until the turn closes.
    NotNow,
    #[default]
    None,
}

#[derive(Script, ScriptHook, Widget)]
pub struct TaskPermissionPrompt {
    #[deref] view: View,
    #[rust] expanded: bool,
    #[rust] items: Vec<TaskItemView>,
}

impl Widget for TaskPermissionPrompt {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.handle_event(cx, event, scope);

        if let Event::Actions(actions) = event {
            if let Some(expanded) = self.view.collapsible_header(cx, ids!(details_toggle)).expansion_changed(actions) {
                self.expanded = expanded;
                self.apply_expanded(cx);
            }
            let mut changed = false;
            for (index, row) in self.view.portal_list(cx, ids!(items)).items_with_actions(actions) {
                let Some(item) = self.items.get_mut(index) else { continue };
                if !item.grantable { continue; }
                if let Some(checked) = row.check_box(cx, ids!(check)).changed(actions) {
                    item.checked = checked;
                    changed = true;
                }
            }
            if changed {
                self.refresh_allow(cx);
                self.view.redraw(cx);
            }
            if self.view.button(cx, ids!(allow_button)).clicked(actions) {
                let approved = self.items.iter().filter(|item| item.grantable && item.checked)
                    .map(|item| item.id.clone()).collect();
                cx.action(TaskPermissionAction::Allow(approved));
            } else if self.view.button(cx, ids!(not_now_button)).clicked(actions) {
                cx.action(TaskPermissionAction::NotNow);
            }
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        while let Some(widget) = self.view.draw_walk(cx, scope, walk).step() {
            if let Some(mut list) = widget.as_portal_list().borrow_mut() {
                list.set_item_range(cx, 0, self.items.len());
                while let Some(index) = list.next_visible_item(cx) {
                    let Some(item) = self.items.get(index).cloned() else { continue };
                    let row = list.item(cx, index, id!(item));
                    let check = row.check_box(cx, ids!(check));
                    check.set_text(&format!("{} — {}", item.title, item.chip));
                    check.set_active(cx, item.checked, Animate::No);
                    row.widget(cx, ids!(check)).set_disabled(cx, !item.grantable);
                    row.label(cx, ids!(detail)).set_text(cx, &item.detail);
                    row.widget(cx, ids!(detail)).set_visible(cx, !item.detail.is_empty());
                    row.draw_all(cx, scope);
                }
            }
        }
        DrawStep::done()
    }
}

impl TaskPermissionPrompt {
    fn apply_expanded(&mut self, cx: &mut Cx) {
        self.view.widget(cx, ids!(details)).set_visible(cx, self.expanded);
        let grantable = self.items.iter().filter(|item| item.grantable).count();
        self.view.collapsible_header(cx, ids!(details_toggle)).set_title(cx, &format!("Details ({grantable} items)"));
        self.refresh_allow(cx);
        self.view.redraw(cx);
    }

    fn refresh_allow(&self, cx: &mut Cx) {
        let any = self.items.iter().any(|item| item.grantable && item.checked);
        self.view.button(cx, ids!(allow_button)).set_enabled(cx, any);
        self.view.widget(cx, ids!(allow_button)).set_disabled(cx, !any);
    }
}

impl TaskPermissionPromptRef {
    /// Populates the modal for one resolved plan. The Details list starts
    /// collapsed; each grantable item starts checked, so the default is the
    /// plan the agent asked for and the user narrows it if they wish.
    pub fn show(&self, cx: &mut Cx, info: &TaskPromptInfo) {
        let Some(mut inner) = self.borrow_mut() else { return };
        inner.items = info.items.clone();
        inner.expanded = false;
        inner.view.collapsible_header(cx, ids!(details_toggle)).set_expanded(cx, false, Animate::No);
        inner.view.label(cx, ids!(agent_paragraph)).set_text(cx, &info.explanation);
        match &info.risk {
            Some(risk) => {
                inner.view.label(cx, ids!(risk_strip.risk_text)).set_text(cx, risk);
                inner.view.widget(cx, ids!(risk_strip)).set_visible(cx, true);
            }
            None => inner.view.widget(cx, ids!(risk_strip)).set_visible(cx, false),
        }
        inner.apply_expanded(cx);
        inner.view.redraw(cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::collapsible_header::CollapsibleHeaderAction;

    #[test]
    fn task_details_disclose_without_changing_approvals_and_reset_for_the_next_plan() {
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let prompt = cx.with_vm(|vm| {
            makepad_widgets::script_mod(vm);
            makepad_code_editor::script_mod(vm);
            crate::shared::script_mod(vm, crate::settings::app_preferences::Appearance::Light);
            crate::a2app::script_mod(vm);
            let value = script_eval!(vm, { mod.widgets.TaskPermissionPrompt {} });
            WidgetRef::script_from_value(vm, value).as_task_permission_prompt()
        });
        let item = |id: &str, grantable, checked| TaskItemView {
            id: id.into(), title: format!("Operation {id}"), detail: format!("Exact scope for {id}"),
            chip: if grantable { "Will be granted".into() } else { "Blocked".into() }, grantable, checked,
        };
        let info = TaskPromptInfo {
            explanation: "I need access for this task.".into(),
            items: vec![item("selected", true, true), item("unchecked", true, false), item("blocked", false, false)],
            risk: None,
        };
        prompt.show(&mut cx, &info);
        let header = prompt.borrow().unwrap().view.collapsible_header(&cx, ids!(details_toggle));
        assert!(header.borrow().is_some(), "task details use the shared disclosure header");
        assert!(!prompt.borrow().unwrap().view.widget(&cx, ids!(details)).visible());
        for expanded in [true, false, true] {
            let change = cx.capture_actions(|cx| cx.widget_action(header.widget_uid(), CollapsibleHeaderAction::ExpansionChanged(expanded)));
            let emitted = cx.capture_actions(|cx| prompt.borrow_mut().unwrap().handle_event(cx, &Event::Actions(change), &mut Scope::empty()));
            let inner = prompt.borrow().unwrap();
            assert_eq!(inner.expanded, expanded);
            assert_eq!(inner.view.widget(&cx, ids!(details)).visible(), expanded);
            assert_eq!(inner.items.iter().map(|item| (&item.id, item.grantable, item.checked)).collect::<Vec<_>>(),
                info.items.iter().map(|item| (&item.id, item.grantable, item.checked)).collect::<Vec<_>>());
            assert!(!emitted.iter().any(|action| action.downcast_ref::<TaskPermissionAction>().is_some()),
                "disclosing details cannot approve or refuse a task");
        }
        let allow = prompt.borrow().unwrap().view.button(&cx, ids!(allow_button)).widget_uid();
        let click = cx.capture_actions(|cx| cx.widget_action(allow, ButtonAction::Clicked(Default::default())));
        let emitted = cx.capture_actions(|cx| prompt.borrow_mut().unwrap().handle_event(cx, &Event::Actions(click), &mut Scope::empty()));
        let answer = emitted.iter().find_map(|action| action.downcast_ref::<TaskPermissionAction>());
        assert!(matches!(answer, Some(TaskPermissionAction::Allow(ids)) if ids == &vec!["selected".to_string()]));

        prompt.show(&mut cx, &TaskPromptInfo { items: vec![item("next", true, false)], ..info });
        let inner = prompt.borrow().unwrap();
        assert!(!inner.expanded);
        assert!(!inner.view.widget(&cx, ids!(details)).visible());
        assert_eq!(inner.items.len(), 1);
        assert_eq!(inner.items[0].id, "next");
        assert!(!inner.items[0].checked);
        assert!(inner.view.widget(&cx, ids!(allow_button)).disabled(&cx));
    }
}
