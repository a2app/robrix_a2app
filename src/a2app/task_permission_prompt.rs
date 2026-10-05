//! The upfront task permission prompt: one modal per task, showing the agent's
//! own plain-language explanation and a collapsible Details list of exactly
//! what Robrix will grant.
//!
//! Only the explanation comes from the agent. The Details list is Robrix's
//! own: the exact capability, scope, URL or tool for each row, with no agent
//! prose. What the user checks is what is applied.

use makepad_widgets::*;

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
        scroll_bars: ScrollBars { show_scroll_x: false, show_scroll_y: false }

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
        details_toggle := RobrixNeutralIconButton {
            margin: Inset{top: 10}
            text: "Details (0 items)"
            icon_walk: Walk{width: 0, height: 0, margin: 0}
        }
        sharing_toggle := RobrixNeutralIconButton {
            visible: false
            margin: Inset{top: 6}
            text: "Information sharing this requires"
            icon_walk: Walk{width: 0, height: 0, margin: 0}
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
    /// A rule Robrix derived from a requested read, rather than something the
    /// agent asked for. These are grouped under their own collapsed section so
    /// a large label's implied rules cannot bury the agent's own needs.
    pub implied: bool,
}

/// What the modal shows for one resolved task plan.
#[derive(Clone, Debug)]
pub struct TaskPromptInfo {
    /// The agent's paragraph, shown verbatim and quoted.
    pub explanation: String,
    pub items: Vec<TaskItemView>,
    /// Robrix's own warning, shown only for high-risk or unusually broad asks.
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
    /// Whether the implied "Information sharing this requires" rows are shown
    /// inside the Details list. Starts collapsed.
    #[rust] implied_expanded: bool,
    #[rust] items: Vec<TaskItemView>,
}

impl Widget for TaskPermissionPrompt {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.handle_event(cx, event, scope);

        if let Event::Actions(actions) = event {
            if self.view.button(cx, ids!(details_toggle)).clicked(actions) {
                self.expanded = !self.expanded;
                self.apply_expanded(cx);
            }
            if self.view.button(cx, ids!(sharing_toggle)).clicked(actions) {
                self.implied_expanded = !self.implied_expanded;
                // The implied rows live inside the Details list, so revealing
                // them also opens it; hiding them leaves Details as it was.
                if self.implied_expanded {
                    self.expanded = true;
                }
                self.apply_expanded(cx);
            }
            let mut changed = false;
            // The PortalList index is a position in the visible subset, which
            // excludes implied rows until their section is opened, so map it
            // back to the source item before mutating a checkbox.
            let visible: Vec<usize> = self.items.iter().enumerate()
                .filter(|(_, item)| !item.implied || self.implied_expanded)
                .map(|(index, _)| index)
                .collect();
            for (index, row) in self.view.portal_list(cx, ids!(items)).items_with_actions(actions) {
                let Some(&source) = visible.get(index) else { continue };
                let Some(item) = self.items.get_mut(source) else { continue };
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
                // Requested rows are always shown; the implied flow rules are
                // hidden until the user opens their section.
                let visible: Vec<usize> = self.items.iter().enumerate()
                    .filter(|(_, item)| !item.implied || self.implied_expanded)
                    .map(|(index, _)| index)
                    .collect();
                list.set_item_range(cx, 0, visible.len());
                while let Some(index) = list.next_visible_item(cx) {
                    let Some(&source) = visible.get(index) else { continue };
                    let Some(item) = self.items.get(source).cloned() else { continue };
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
        // The two counts partition the grantable rows: the agent's requested
        // needs under Details, Robrix's derived rules under their own heading.
        let grantable = self.items.iter().filter(|item| item.grantable && !item.implied).count();
        let text = if self.expanded {
            "Hide details".to_string()
        } else {
            format!("Details ({grantable} items)")
        };
        self.view.button(cx, ids!(details_toggle)).set_text(cx, &text);
        self.refresh_sharing_toggle(cx);
        self.refresh_allow(cx);
        self.view.redraw(cx);
    }

    fn refresh_sharing_toggle(&self, cx: &mut Cx) {
        let implied = self.items.iter().filter(|item| item.implied).count();
        if implied == 0 {
            self.view.widget(cx, ids!(sharing_toggle)).set_visible(cx, false);
            return;
        }
        self.view.widget(cx, ids!(sharing_toggle)).set_visible(cx, true);
        let text = if self.implied_expanded {
            format!("Hide information sharing ({implied})")
        } else {
            format!("Information sharing this requires ({implied})")
        };
        self.view.button(cx, ids!(sharing_toggle)).set_text(cx, &text);
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
        inner.implied_expanded = false;
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
