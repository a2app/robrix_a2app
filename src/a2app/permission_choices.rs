//! Visible, mutually exclusive choices for permission settings.

use makepad_widgets::*;

script_mod! {
    use mod.prelude.widgets.*
    use mod.widgets.*

    mod.widgets.PermissionChoiceRow = RobrixSettingsRadioButton {
        width: Fill, height: Fit{min: FitBound.Abs(40.0)}
        flow: Flow.Right{wrap: true}
        padding: Inset{top: 10, bottom: 10, left: 4, right: 4}
        label_walk: Walk{width: Fill, height: Fit, margin: Inset{left: 24}}
    }
    mod.widgets.PermissionHorizontalChoiceRow = mod.widgets.PermissionChoiceRow {
        width: Fit, margin: Inset{right: 12}
        label_walk: Walk{width: Fit, height: Fit, margin: Inset{left: 24}}
    }
    mod.widgets.PermissionChoiceTab = RadioButtonTabFlat {
        width: Fit, height: Fit{min: FitBound.Abs(40.0)}
        flow: Right
        padding: Inset{top: 10, bottom: 10, left: 12, right: 12}
        margin: Inset{right: 6, bottom: 4}
        icon_walk: Walk{width: 0, height: 0, margin: 0}
        label_walk: Walk{width: Fit, height: Fit, margin: 0}
        draw_text +: {
            text_style: SETTINGS_REGULAR_TEXT_STYLE {}
            color: (MESSAGE_TEXT_COLOR), color_hover: (COLOR_ACTIVE_PRIMARY_DARKER)
            color_active: (COLOR_ACTIVE_PRIMARY_DARKER), color_focus: (COLOR_ACTIVE_PRIMARY_DARKER)
            color_down: (COLOR_ACTIVE_PRIMARY_DARKER), color_disabled: (MESSAGE_TEXT_COLOR)
        }
        draw_bg +: {
            color: (COLOR_PRIMARY), color_active: (COLOR_BG_PREVIEW), color_disabled: (COLOR_BG_PREVIEW)
            border_size: 1.0, border_radius: 4.0
            border_color: (COLOR_SECONDARY_DARKER), border_color_hover: (COLOR_ACTIVE_PRIMARY)
            border_color_active: (COLOR_ACTIVE_PRIMARY_DARKER), border_color_focus: (COLOR_ACTIVE_PRIMARY_DARKER)
            border_color_down: (COLOR_ACTIVE_PRIMARY_DARKER), border_color_disabled: (COLOR_SECONDARY_DARKER)
        }
    }
    mod.widgets.PermissionChoices = set_type_default() do #(PermissionChoices::register_widget(vm)) {
        width: Fill, height: Fit, flow: Down, spacing: 2
    }
}

#[derive(Clone, Debug, Default)]
pub enum PermissionChoicesAction {
    Changed(usize),
    #[default]
    None,
}

#[derive(Script, Widget)]
pub struct PermissionChoices {
    #[deref] view: View,
    #[live] labels: Vec<String>,
    #[imperative]
    #[apply_state]
    #[live] selected_item: usize,
    #[live] horizontal: bool,
    #[live] tabs: bool,
    #[rust] disabled: bool,
}

impl ScriptHook for PermissionChoices {
    fn on_after_apply(&mut self, vm: &mut ScriptVm, _apply: &Apply, _scope: &mut Scope, _value: ScriptValue) {
        self.rebuild(vm);
    }
}

impl PermissionChoices {
    fn rebuild(&mut self, vm: &mut ScriptVm) {
        self.view.children.clear();
        self.view.layout.flow = if self.horizontal || self.tabs { Flow::right_wrap() } else { Flow::Down };
        for (index, label) in self.labels.iter().enumerate() {
            let value = if self.tabs {
                script_eval!(vm, { mod.widgets.PermissionChoiceTab {} })
            } else if self.horizontal {
                script_eval!(vm, { mod.widgets.PermissionHorizontalChoiceRow {} })
            } else {
                script_eval!(vm, { mod.widgets.PermissionChoiceRow {} })
            };
            let row = WidgetRef::script_from_value(vm, value);
            row.set_text(vm.cx_mut(), label);
            row.as_radio_button().set_active(vm.cx_mut(), index == self.selected_item, Animate::No);
            row.set_disabled(vm.cx_mut(), self.disabled);
            self.view.children.push((LiveId(index as u64 + 1), row));
        }
    }

    fn select(&mut self, cx: &mut Cx, index: usize) {
        // Keep an invalid index invalid. Silently selecting the first item could broaden access.
        if self.selected_item == index { return }
        self.selected_item = index;
        for (row_index, (_, row)) in self.view.children.iter().enumerate() {
            row.as_radio_button().set_active(cx, row_index == index, Animate::No);
        }
        self.view.redraw(cx);
    }
}

impl Widget for PermissionChoices {
    fn set_disabled(&mut self, cx: &mut Cx, disabled: bool) {
        if self.disabled == disabled { return }
        self.disabled = disabled;
        for (_, row) in &self.view.children { row.set_disabled(cx, disabled); }
        self.view.redraw(cx);
    }

    fn disabled(&self, _cx: &Cx) -> bool { self.disabled }

    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.handle_event(cx, event, scope);
        if self.disabled || !self.view.visible { return }
        let Event::Actions(actions) = event else { return };
        let clicked = self.view.children.iter().position(|(_, row)| row.as_radio_button().clicked(actions));
        if let Some(index) = clicked {
            self.select(cx, index);
            cx.widget_action(self.widget_uid(), PermissionChoicesAction::Changed(index));
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        self.view.draw_walk(cx, scope, walk)
    }
}

impl PermissionChoicesRef {
    pub fn selected_item(&self) -> usize {
        self.borrow().map(|inner| inner.selected_item).unwrap_or(usize::MAX)
    }

    pub fn set_selected_item(&self, cx: &mut Cx, index: usize) {
        if let Some(mut inner) = self.borrow_mut() { inner.select(cx, index); }
    }

    pub fn set_labels(&self, cx: &mut Cx, labels: Vec<String>) {
        if let Some(mut inner) = self.borrow_mut() {
            if inner.labels == labels { return }
            inner.labels = labels;
            cx.with_vm(|vm| inner.rebuild(vm));
            inner.view.redraw(cx);
        }
    }

    pub fn changed(&self, actions: &Actions) -> Option<usize> {
        let inner = self.borrow()?;
        if inner.disabled || !inner.view.visible { return None }
        match actions.find_widget_action(self.widget_uid()).cast() {
            PermissionChoicesAction::Changed(index) if index < inner.labels.len() => Some(index),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigation_tabs_keep_choice_actions_scoped_and_select_one_page() {
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let tabs = cx.with_vm(|vm| {
            makepad_widgets::script_mod(vm);
            crate::shared::script_mod(vm);
            super::script_mod(vm);
            let value = script_eval!(vm, { mod.widgets.PermissionChoices { tabs: true, labels: ["Rules", "History"] } });
            WidgetRef::script_from_value(vm, value).as_permission_choices()
        });
        assert_eq!(tabs.borrow().unwrap().view.layout.flow, Flow::right_wrap());
        let history = tabs.borrow().unwrap().view.children[1].1.clone();
        assert_eq!(history.text(), "History");
        assert!(history.walk(&mut cx).width.is_fit());
        let clicked = cx.capture_actions(|cx| cx.widget_action(history.widget_uid(), RadioButtonAction::Clicked));
        let changed = cx.capture_actions(|cx| tabs.borrow_mut().unwrap().handle_event(cx, &Event::Actions(clicked), &mut Scope::empty()));
        assert_eq!(tabs.changed(&changed), Some(1));
        assert_eq!(tabs.selected_item(), 1);
        assert!(history.as_radio_button().active(&cx));
        assert!(!tabs.borrow().unwrap().view.children[0].1.as_radio_button().active(&cx));
    }

    #[test]
    fn horizontal_choices_keep_visible_labels_and_fit_width_after_runtime_updates() {
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let choices = cx.with_vm(|vm| {
            makepad_widgets::script_mod(vm);
            crate::shared::script_mod(vm);
            super::script_mod(vm);
            let value = script_eval!(vm, { mod.widgets.PermissionChoices { horizontal: true, labels: ["Read", "Write"] } });
            WidgetRef::script_from_value(vm, value).as_permission_choices()
        });
        choices.set_labels(&mut cx, vec!["Read".into(), "Write".into(), "Both".into()]);
        let inner = choices.borrow().unwrap();
        assert_eq!(inner.view.layout.flow, Flow::right_wrap());
        assert_eq!(inner.view.children.len(), 3);
        for ((_, row), label) in inner.view.children.iter().zip(["Read", "Write", "Both"]) {
            assert_eq!(row.text(), label);
            let walk = row.walk(&mut cx);
            assert!(walk.width.is_fit());
            assert_eq!(walk.margin.right, 12.0);
            assert!(matches!(walk.height, Size::Fit { min: Some(FitBound::Abs(40.0)), .. }));
        }
    }

    #[test]
    fn choices_scope_actions_and_preserve_invalid_selection_when_labels_change() {
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let (first, second) = cx.with_vm(|vm| {
            makepad_widgets::script_mod(vm);
            crate::shared::script_mod(vm);
            super::script_mod(vm);
            let value = script_eval!(vm, { mod.widgets.PermissionChoices { labels: ["Ask", "Allow", "Block"] } });
            let first = WidgetRef::script_from_value(vm, value).as_permission_choices();
            let value = script_eval!(vm, { mod.widgets.PermissionChoices { labels: ["Ask", "Allow", "Block"] } });
            (first, WidgetRef::script_from_value(vm, value).as_permission_choices())
        });
        let second_row = first.borrow().unwrap().view.children[1].1.widget_uid();
        let clicked = cx.capture_actions(|cx| cx.widget_action(second_row, RadioButtonAction::Clicked));
        let changed = cx.capture_actions(|cx| first.borrow_mut().unwrap().handle_event(cx, &Event::Actions(clicked), &mut Scope::empty()));
        assert_eq!(first.changed(&changed), Some(1));
        assert_eq!(second.changed(&changed), None);
        assert_eq!(first.selected_item(), 1);
        assert_eq!(second.selected_item(), 0);
        first.set_selected_item(&mut cx, 2);
        let invalid = cx.capture_actions(|cx| cx.widget_action(first.widget_uid(), PermissionChoicesAction::Changed(99)));
        assert_eq!(first.changed(&invalid), None);
        assert_eq!(first.selected_item(), 2, "an invalid choice must preserve the existing Block selection");
        first.set_selected_item(&mut cx, 99);
        first.set_labels(&mut cx, vec!["Ask".into(), "Allow".into()]);
        assert_eq!(first.selected_item(), 99);
        assert!(first.borrow().unwrap().view.children.iter().all(|(_, row)| !row.as_radio_button().active(&cx)));

        first.borrow_mut().unwrap().set_disabled(&mut cx, true);
        assert!(first.borrow().unwrap().view.children.iter().all(|(_, row)| row.disabled(&cx)));
        let first_row = first.borrow().unwrap().view.children[0].1.widget_uid();
        let clicked = cx.capture_actions(|cx| cx.widget_action(first_row, RadioButtonAction::Clicked));
        let changed = cx.capture_actions(|cx| first.borrow_mut().unwrap().handle_event(cx, &Event::Actions(clicked), &mut Scope::empty()));
        assert_eq!(first.changed(&changed), None, "a stale radio action cannot change a disabled permission");
        assert_eq!(first.selected_item(), 99);
        first.borrow_mut().unwrap().set_disabled(&mut cx, false);
        let clicked = cx.capture_actions(|cx| cx.widget_action(first_row, RadioButtonAction::Clicked));
        let changed = cx.capture_actions(|cx| first.borrow_mut().unwrap().handle_event(cx, &Event::Actions(clicked), &mut Scope::empty()));
        assert_eq!(first.changed(&changed), Some(0), "the first option remains selectable from an unset state");
    }
}
