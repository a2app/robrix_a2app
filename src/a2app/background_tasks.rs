//! User-owned background schedules; activation never grants app authority.

use makepad_widgets::*;
use std::cell::RefCell;
use std::collections::HashMap;
use crate::app::ConfirmDeleteAction;
use crate::shared::confirmation_modal::ConfirmationModalContent;
use a2app_core::background::{Job, JobBinding, JobContext, PauseReason, RunOutcome, Trigger, MIN_INTERVAL_SECONDS};
use crate::home::rooms_list::RoomsListRef;
use super::background::{self, TaskView};
use super::runtime::{with_a2app, A2AppOp};

script_mod! {
    use mod.prelude.widgets.*
    use mod.widgets.*

    mod.widgets.BackgroundTasks = set_type_default() do #(BackgroundTasks::register_widget(vm)) {
        width: Fill, height: Fill, flow: Down
        content := ScrollYView {
            width: Fill, height: Fill, flow: Down, spacing: 12, padding: 15
            task_error := mod.widgets.PermissionOptionLabel {
                visible: false
                draw_text +: { color: (COLOR_FG_DANGER_RED) }
            }
            overview_header := View {
                width: Fill, height: Fit, flow: Down, spacing: 12
                mod.widgets.PermissionOptionLabel {
                    text: "Let a mini-app work for you on a schedule or when new messages arrive. For example, check a website every 12 hours and post updates in a room."
                }
                mod.widgets.PermissionOptionLabel {
                    text: "Tasks run while Robrix is open and you are signed in. Enabled tasks resume the next time you open Robrix, in the same room or space."
                }
                View {
                    width: Fill, height: Fit, flow: Flow.Right{wrap: true}, spacing: 8
                    new_task := RobrixPositiveIconButton {
                        padding: 10, icon_walk: Walk{width: 0, height: 0, margin: 0}, text: "New task"
                    }
                    refresh_tasks := RobrixNeutralIconButton {
                        padding: 10, icon_walk: Walk{width: 0, height: 0, margin: 0}, text: "Refresh"
                    }
                }
                no_background_apps := mod.widgets.PermissionOptionLabel {
                    visible: false
                    text: "None of your installed mini-apps supports background tasks yet. Install or create a mini-app with background support to get started."
                }
                no_tasks := mod.widgets.PermissionOptionLabel {
                    text: "No background tasks yet. Choose New task to set one up."
                }
                task_list := View {
                    visible: false
                    width: Fill, height: Fit, flow: Down, spacing: 8
                    SubsectionLabel { text: "Your tasks", margin: 0 }
                    task_choice := mod.widgets.PermissionDropDown { labels: ["Choose a task…"] }
                }
            }
            task_overview := View {
                visible: false
                width: Fill, height: Fit, flow: Down, spacing: 10
                task_details := mod.widgets.PermissionOptionLabel {}
                task_actions := View {
                    width: Fill, height: Fit, flow: Flow.Right{wrap: true}, spacing: 8
                    pause_resume := RobrixNeutralIconButton {
                        padding: 10, icon_walk: Walk{width: 0, height: 0, margin: 0}, text: "Pause"
                    }
                    run_now := RobrixNeutralIconButton {
                        padding: 10, icon_walk: Walk{width: 0, height: 0, margin: 0}, text: "Run now"
                    }
                    edit_task := RobrixNeutralIconButton {
                        padding: 10, icon_walk: Walk{width: 0, height: 0, margin: 0}, text: "Edit schedule"
                    }
                    remove_task := RobrixNegativeIconButton {
                        padding: 10, icon_walk: Walk{width: 0, height: 0, margin: 0}, text: "Remove task"
                    }
                }
                show_details := RobrixNeutralIconButton {
                    padding: 8, icon_walk: Walk{width: 0, height: 0, margin: 0}, text: "Show task details"
                }
                runtime_details := mod.widgets.PermissionOptionLabel { visible: false }
            }
            task_editor := View {
                visible: false
                width: Fill, height: Fit, flow: Down, spacing: 10
                View {
                    width: Fill, height: Fit, flow: Flow.Right{wrap: true}, spacing: 8, align: Align{y: 0.5}
                    editor_title := SubsectionLabel { width: Fit, text: "New task", margin: 0 }
                    cancel_edit := RobrixNeutralIconButton {
                        padding: 10, icon_walk: Walk{width: 0, height: 0, margin: 0}, text: "Cancel"
                    }
                }
                mod.widgets.PermissionOptionLabel { text: "Mini-app" }
                app_choice_section := View {
                    width: Fill, height: Fit
                    app_choice := mod.widgets.PermissionDropDown { labels: ["Choose a mini-app…"] }
                }
                saved_app := mod.widgets.PermissionOptionLabel { visible: false }
                mod.widgets.PermissionOptionLabel { text: "Room or space" }
                context_choice_section := View {
                    width: Fill, height: Fit
                    context_choice := mod.widgets.PermissionDropDown { labels: ["Choose where this task runs…"] }
                }
                saved_context := mod.widgets.PermissionOptionLabel { visible: false }
                mod.widgets.PermissionOptionLabel {
                    text: "The task uses this mini-app's saved settings in the selected room or space. Each mini-app can have one task per room, space, or account."
                }
                mod.widgets.PermissionOptionLabel { text: "When to run" }
                trigger_choice := mod.widgets.PermissionDropDown {
                    labels: ["Repeat on a schedule", "Once at a set time", "When new messages arrive"]
                }
                interval_section := View {
                    width: Fill, height: Fit, flow: Down, spacing: 6
                    mod.widgets.PermissionOptionLabel { text: "Repeat every" }
                    interval_value := RobrixTextInput { width: Fill, text: "5", empty_text: "Enter a whole number" }
                    interval_unit := mod.widgets.PermissionDropDown { labels: ["Seconds", "Minutes", "Hours", "Days"] }
                    mod.widgets.PermissionOptionLabel { text: "At least one minute. Missed runs are combined into one run when Robrix reopens." }
                }
                alarm_section := View {
                    visible: false
                    width: Fill, height: Fit, flow: Down, spacing: 6
                    mod.widgets.PermissionOptionLabel { text: "Date and time on this device (24-hour clock)" }
                    alarm_value := RobrixTextInput { width: Fill, empty_text: "YYYY-MM-DD HH:MM", autocorrect: Disabled }
                    alarm_timezone := mod.widgets.PermissionOptionLabel {}
                    mod.widgets.PermissionOptionLabel {
                        text: "Runs once. If Robrix is closed at that time, it runs when you next open Robrix. Run now uses up the scheduled run."
                    }
                }
                messages_section := mod.widgets.PermissionOptionLabel {
                    visible: false
                    text: "Requires a single room and permission to read it. The mini-app checks new messages for its conditions. Old messages are not replayed, and messages received during a run may be skipped. Run now checks without a message."
                }
                changed_version := View {
                    visible: false
                    width: Fill, height: Fit, flow: Down, spacing: 6
                    mod.widgets.PermissionOptionLabel {
                        text: "The mini-app changed since you enabled this task. Open it and review its permissions before allowing the new version to run."
                    }
                    reviewed_version := RobrixSettingsCheckBox {
                        width: Fill, height: Fit
                        text: "I reviewed this version and allow it to run"
                    }
                }
                save_task := RobrixPositiveIconButton {
                    padding: 10, icon_walk: Walk{width: 0, height: 0, margin: 0}, text: "Save and enable task"
                }
            }
            app_settings := View {
                visible: false
                width: Fill, height: Fit, flow: Down, spacing: 10
                LineH { width: Fill, margin: Inset{top: 4, bottom: 4} }
                SubsectionLabel { text: "Mini-app settings and access", margin: 0 }
                mod.widgets.PermissionOptionLabel {
                    text: "Saving a task does not give it new permissions. Set up the mini-app and allow the access it needs before leaving it to run. Background tasks cannot ask you for permission while running."
                }
                View {
                    width: Fill, height: Fit, flow: Flow.Right{wrap: true}, spacing: 8
                    open_task_app := RobrixNeutralIconButton {
                        padding: 10, icon_walk: Walk{width: 0, height: 0, margin: 0}, text: "Open mini-app"
                    }
                    task_permissions := RobrixNeutralIconButton {
                        padding: 10, icon_walk: Walk{width: 0, height: 0, margin: 0}, text: "Permissions"
                    }
                    task_sharing := RobrixNeutralIconButton {
                        padding: 10, icon_walk: Walk{width: 0, height: 0, margin: 0}, text: "Data sharing"
                    }
                }
                room_action_help := mod.widgets.PermissionOptionLabel {
                    visible: false
                    text: "If an action is blocked: open the mini-app in its room, keep its pane open, return here and choose Run now. Then review the blocked action in Data sharing and retry."
                }
            }
            show_help := RobrixNeutralIconButton {
                padding: 8, icon_walk: Walk{width: 0, height: 0, margin: 0}, text: "How background tasks work"
            }
            task_help := View {
                visible: false
                width: Fill, height: Fit, flow: Down, spacing: 8
                mod.widgets.PermissionOptionLabel {
                    text: "Pausing or removing a task stops its current work and closes the mini-app instance. Its saved settings and data are kept. Force Stop in the mini-app's details also disables its tasks."
                }
                mod.widgets.PermissionOptionLabel {
                    text: "Tasks restore saved settings and data, not unsaved work. Session permissions and action approvals can expire when the mini-app or Robrix closes. A task may need your attention before it can run again."
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
pub enum BackgroundTasksAction {
    OpenApp(JobBinding),
    AppPermissions(String),
    Sharing,
}

#[derive(Clone)]
struct AppChoice {
    id: String,
    name: String,
    fingerprint: Result<String, String>,
}

#[derive(Clone)]
struct ContextChoice {
    context: JobContext,
    label: String,
    available: bool,
}

#[derive(Script, ScriptHook, Widget)]
pub struct BackgroundTasks {
    #[deref] view: View,
    #[rust] account: String,
    #[rust] tasks: Vec<TaskView>,
    #[rust] apps: Vec<AppChoice>,
    #[rust] contexts: Vec<ContextChoice>,
    #[rust] targets: Vec<(String, String, bool)>,
    #[rust] revision: Option<u64>,
    #[rust] pending_save: Option<JobBinding>,
    #[rust] editing: bool,
}

impl Widget for BackgroundTasks {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        if self.account != super::information_flow::account().unwrap_or_default() {
            self.configure(cx);
            // The event belongs to the previous account's visible controls.
            return;
        }
        self.view.handle_event(cx, event, scope);
        let Event::Actions(actions) = event else { return };
        if self.view.button(cx, ids!(refresh_tasks)).clicked(actions) {
            self.configure(cx);
        }
        if self.view.button(cx, ids!(new_task)).clicked(actions) {
            self.view.widget(cx, ids!(task_error)).set_visible(cx, false);
            self.view.drop_down(cx, ids!(task_choice)).set_selected_item(cx, 0);
            self.set_editing(cx, true);
            self.load_selected_task(cx);
        }
        if self.view.drop_down(cx, ids!(task_choice)).changed(actions).is_some() {
            self.set_editing(cx, false);
            self.view.widget(cx, ids!(task_error)).set_visible(cx, false);
            self.load_selected_task(cx);
        }
        if self.view.button(cx, ids!(edit_task)).clicked(actions) {
            self.set_editing(cx, true);
            self.update_form(cx);
        }
        if self.view.button(cx, ids!(cancel_edit)).clicked(actions) {
            self.set_editing(cx, false);
            self.view.widget(cx, ids!(task_error)).set_visible(cx, false);
            self.load_selected_task(cx);
        }
        for (button, section, closed, opened) in [
            (ids!(show_help), ids!(task_help), "How background tasks work", "Hide background task help"),
            (ids!(show_details), ids!(runtime_details), "Show task details", "Hide task details"),
        ] {
            if self.view.button(cx, button).clicked(actions) {
                let visible = !self.view.widget(cx, section).visible();
                self.view.widget(cx, section).set_visible(cx, visible);
                self.view.button(cx, button).set_text(cx, if visible { opened } else { closed });
            }
        }
        if self.view.drop_down(cx, ids!(app_choice)).changed(actions).is_some() {
            self.load_contexts(cx, None);
            self.view.check_box(cx, ids!(reviewed_version)).set_active(cx, false, Animate::No);
        }
        if self.view.drop_down(cx, ids!(app_choice)).changed(actions).is_some()
            || self.view.drop_down(cx, ids!(context_choice)).changed(actions).is_some()
        {
            self.select_existing_binding(cx);
        }
        if self.view.drop_down(cx, ids!(trigger_choice)).changed(actions).is_some()
            || self.view.drop_down(cx, ids!(app_choice)).changed(actions).is_some()
            || self.view.drop_down(cx, ids!(context_choice)).changed(actions).is_some()
        {
            self.update_form(cx);
        }
        if self.view.button(cx, ids!(save_task)).clicked(actions) {
            let result = self.save_action(cx, now_ms());
            self.submit(cx, result);
        }
        if self.view.button(cx, ids!(pause_resume)).clicked(actions) {
            let result = self.selected_task(cx).ok_or_else(|| "Select a saved task.".to_string()).and_then(|task| {
                let fingerprint = task.current_fingerprint.clone().unwrap_or_default();
                let enabled = !task_is_active(&task.job);
                if enabled && !can_resume(task, now_ms()) {
                    return Err("This task cannot resume with its saved app version or alarm. Review the current app version and enter a future alarm if needed, then save and enable.".into());
                }
                Ok(A2AppOp::SetBackgroundTaskEnabled { id: task.job.id, enabled, expected_fingerprint: fingerprint })
            });
            self.submit(cx, result);
        }
        if self.view.button(cx, ids!(run_now)).clicked(actions) {
            let result = self.selected_task(cx).map(|task| A2AppOp::RunBackgroundTask(task.job.id)).ok_or_else(|| "Select an enabled task.".into());
            self.submit(cx, result);
        }
        if self.view.button(cx, ids!(remove_task)).clicked(actions) {
            if let Some(task) = self.selected_task(cx) {
                let id = task.job.id;
                let account = self.account.clone();
                let app = self.apps.iter().find(|app| app.id == task.job.binding.app_id)
                    .map(|app| app.name.as_str()).unwrap_or(&task.job.binding.app_id);
                cx.action(ConfirmDeleteAction::Show(RefCell::new(Some(ConfirmationModalContent {
                    title_text: "Remove background task?".into(),
                    body_text: format!("Stop {app}'s background work in {} and remove its schedule? The mini-app's saved settings and data will be kept.", self.context_label(&task.job.binding.context)).into(),
                    accept_button_text: Some("Remove task".into()),
                    on_accept_clicked: Some(Box::new(move |cx| {
                        if super::information_flow::account().ok().as_deref() == Some(account.as_str()) {
                            cx.action(A2AppOp::RemoveBackgroundTask(id));
                        } else {
                            crate::shared::popup_list::enqueue_popup_notification("The signed-in account changed. Reopen Background tasks before removing this task.", crate::shared::popup_list::PopupKind::Warning, Some(5.0));
                        }
                    })),
                    ..Default::default()
                }))));
            }
        }
        if self.view.button(cx, ids!(open_task_app)).clicked(actions) {
            match self.selected_binding(cx) {
                Ok(binding) => cx.action(BackgroundTasksAction::OpenApp(binding)),
                Err(error) => self.show_error(cx, &error),
            }
        }
        if self.view.button(cx, ids!(task_permissions)).clicked(actions) {
            if let Some(app) = self.selected_app(cx) {
                cx.action(BackgroundTasksAction::AppPermissions(app.id.clone()));
            }
        }
        if self.view.button(cx, ids!(task_sharing)).clicked(actions) {
            cx.action(BackgroundTasksAction::Sharing);
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        if self.account != super::information_flow::account().unwrap_or_default() { self.configure(cx); }
        else if self.revision != Some(background::revision()) { self.refresh_tasks(cx); }
        self.view.draw_walk(cx, scope, walk)
    }
}

impl BackgroundTasks {
    fn set_editing(&mut self, cx: &mut Cx, editing: bool) {
        self.editing = editing;
        self.view.view(cx, ids!(content)).set_scroll_pos(cx, Vec2d::default());
    }

    fn configure(&mut self, cx: &mut Cx) {
        self.account = super::information_flow::account().unwrap_or_default();
        self.pending_save = None;
        self.set_editing(cx, false);
        self.targets = if cx.has_global::<RoomsListRef>() { cx.get_global::<RoomsListRef>().permission_targets() }
            else { Vec::new() };
        let apps = with_a2app(|state| state.registry.iter().filter(|app| app.supports_background())
            .map(|app| (app.id.clone(), app.name.clone())).collect::<Vec<_>>()).unwrap_or_default();
        self.apps = apps.into_iter().map(|(id, name)| AppChoice {
            fingerprint: background::app_fingerprint(&id), id, name,
        }).collect();
        self.apps.sort_by_cached_key(|app| (app.name.to_lowercase(), app.id.clone()));
        let mut app_names = HashMap::new();
        for app in &self.apps { *app_names.entry(app.name.as_str()).or_insert(0) += 1; }
        self.view.drop_down(cx, ids!(app_choice)).set_labels(cx, std::iter::once("Choose a mini-app…".into())
            .chain(self.apps.iter().map(|app| {
                if app_names[app.name.as_str()] > 1 { format!("{} ({})", app.name, app.id) }
                else { app.name.clone() }
            })).collect());
        self.view.widget(cx, ids!(no_background_apps)).set_visible(cx, self.apps.is_empty());
        self.refresh_tasks(cx);
        self.load_selected_task(cx);
    }

    fn refresh_tasks(&mut self, cx: &mut Cx) {
        let selected = self.selected_task(cx).map(|task| task.job.id);
        match background::snapshot() {
            Ok(tasks) => {
                self.tasks = tasks;
                self.view.widget(cx, ids!(task_error)).set_visible(cx, false);
            }
            Err(error) => { self.tasks.clear(); self.show_error(cx, &error); }
        }
        self.view.drop_down(cx, ids!(task_choice)).set_labels(cx, std::iter::once("Choose a task…".into()).chain(self.tasks.iter().map(|task| {
            let app = self.apps.iter().find(|app| app.id == task.job.binding.app_id).map(|app| app.name.as_str()).unwrap_or(&task.job.binding.app_id);
            format!("{app} · {} · {}", self.context_label(&task.job.binding.context), task_state_label(&task.job))
        })).collect());
        let saved = self.pending_save.take().and_then(|binding| self.tasks.iter().position(|task| task.job.binding == binding));
        let index = saved.or_else(|| selected.and_then(|id| self.tasks.iter().position(|task| task.job.id == id))).map(|index| index + 1).unwrap_or(0);
        self.view.drop_down(cx, ids!(task_choice)).set_selected_item(cx, index);
        self.revision = Some(background::revision());
        if saved.is_some() || (selected.is_some() && index == 0) {
            self.set_editing(cx, false);
            self.load_selected_task(cx);
        }
        self.update_form(cx);
    }

    fn selected_task(&self, cx: &Cx) -> Option<&TaskView> {
        self.view.drop_down(cx, ids!(task_choice)).selected_item().checked_sub(1).and_then(|index| self.tasks.get(index))
    }

    fn selected_app(&self, cx: &Cx) -> Option<&AppChoice> {
        self.view.drop_down(cx, ids!(app_choice)).selected_item().checked_sub(1).and_then(|index| self.apps.get(index))
    }

    fn selected_context(&self, cx: &Cx) -> Option<&ContextChoice> {
        self.view.drop_down(cx, ids!(context_choice)).selected_item().checked_sub(1).and_then(|index| self.contexts.get(index))
    }

    fn selected_binding(&self, cx: &Cx) -> Result<JobBinding, String> {
        if self.account.is_empty() { return Err("Sign in before configuring a background task.".into()); }
        let app = self.selected_app(cx).ok_or("Choose an installed mini-app with background support.")?;
        let context = self.selected_context(cx).ok_or("Choose a room, space, or account for this task.")?;
        if !context.available { return Err("The saved room or space is no longer available. Rejoin it or create a task in another context.".into()); }
        Ok(JobBinding { account: self.account.clone(), app_id: app.id.clone(), context: context.context.clone() })
    }

    fn select_existing_binding(&mut self, cx: &mut Cx) {
        let Ok(binding) = self.selected_binding(cx) else { return };
        if let Some(index) = self.tasks.iter().position(|task| task.job.binding == binding) {
            self.view.drop_down(cx, ids!(task_choice)).set_selected_item(cx, index + 1);
            self.set_editing(cx, false);
            self.load_selected_task(cx);
        }
    }

    fn load_selected_task(&mut self, cx: &mut Cx) {
        let selected = self.selected_task(cx).map(|task| task.job.clone());
        self.view.check_box(cx, ids!(reviewed_version)).set_active(cx, false, Animate::No);
        self.view.widget(cx, ids!(runtime_details)).set_visible(cx, false);
        self.view.button(cx, ids!(show_details)).set_text(cx, "Show task details");
        let app = selected.as_ref().and_then(|job| self.apps.iter().position(|app| app.id == job.binding.app_id)).map(|index| index + 1).unwrap_or(0);
        self.view.drop_down(cx, ids!(app_choice)).set_selected_item(cx, app);
        self.load_contexts(cx, selected.as_ref().map(|job| &job.binding.context));
        let (trigger, amount, units, alarm) = match selected.as_ref().map(|job| &job.trigger) {
            Some(Trigger::Interval { seconds }) if seconds % 86_400 == 0 => (0, (seconds / 86_400).to_string(), 3, String::new()),
            Some(Trigger::Interval { seconds }) if seconds % 3600 == 0 => (0, (seconds / 3600).to_string(), 2, String::new()),
            Some(Trigger::Interval { seconds }) if seconds % 60 == 0 => (0, (seconds / 60).to_string(), 1, String::new()),
            Some(Trigger::Interval { seconds }) => (0, seconds.to_string(), 0, String::new()),
            Some(Trigger::Alarm { unix_ms }) => (1, "5".into(), 1, local_input(*unix_ms)),
            Some(Trigger::RoomMessages) => (2, "5".into(), 1, String::new()),
            None => (0, "5".into(), 1, String::new()),
        };
        self.view.drop_down(cx, ids!(trigger_choice)).set_selected_item(cx, trigger);
        self.view.text_input(cx, ids!(interval_value)).set_text(cx, &amount);
        self.view.drop_down(cx, ids!(interval_unit)).set_selected_item(cx, units);
        self.view.text_input(cx, ids!(alarm_value)).set_text(cx, &alarm);
        self.update_form(cx);
    }

    fn load_contexts(&mut self, cx: &mut Cx, selected: Option<&JobContext>) {
        let app = self.selected_app(cx).map(|app| app.id.clone());
        let mut target_names = HashMap::new();
        for (_, name, space) in &self.targets { *target_names.entry((name.as_str(), *space)).or_insert(0) += 1; }
        self.contexts = app.as_ref().and_then(|id| with_a2app(|state| {
            state.registry.get(id).map(|app| {
                let mut contexts = Vec::new();
                if app.can_run_background_in_context(None, false) {
                    contexts.push(ContextChoice { context: JobContext::Account, label: "This account (no room)".into(), available: true });
                }
                for (id, name, space) in &self.targets {
                    if app.can_run_background_in_context(Some(id), *space) {
                        contexts.push(ContextChoice {
                            context: if *space { JobContext::Space { space_id: id.clone() } } else { JobContext::Room { room_id: id.clone() } },
                            label: if target_names[&(name.as_str(), *space)] > 1 {
                                format!("{}: {name} ({id})", if *space { "Space" } else { "Room" })
                            } else { format!("{}: {name}", if *space { "Space" } else { "Room" }) }, available: true,
                        });
                    }
                }
                contexts
            })
        }).flatten()).unwrap_or_default();
        if let Some(selected) = selected && !self.contexts.iter().any(|choice| &choice.context == selected) {
            self.contexts.push(ContextChoice { context: selected.clone(), label: format!("Unavailable: {}", self.context_label(selected)), available: false });
        }
        self.view.drop_down(cx, ids!(context_choice)).set_labels(cx, std::iter::once("Choose where this task runs…".into()).chain(self.contexts.iter().map(|choice| choice.label.clone())).collect());
        let index = selected.and_then(|selected| self.contexts.iter().position(|choice| &choice.context == selected)).map(|index| index + 1)
            .unwrap_or_else(|| usize::from(self.contexts.len() == 1));
        self.view.drop_down(cx, ids!(context_choice)).set_selected_item(cx, index);
    }

    fn context_label(&self, context: &JobContext) -> String {
        match context {
            JobContext::Account => "This account (no room)".into(),
            JobContext::Room { room_id } | JobContext::Space { space_id: room_id } => {
                let name = self.targets.iter().find(|target| &target.0 == room_id).map(|target| target.1.as_str()).unwrap_or(room_id);
                format!("{}: {name}", if matches!(context, JobContext::Space { .. }) { "Space" } else { "Room" })
            }
        }
    }

    fn version_changed(&self, cx: &Cx) -> bool {
        self.selected_task(cx).is_some_and(|task| task.current_fingerprint.as_ref().is_some_and(|fingerprint| fingerprint != &task.job.fingerprint))
    }

    fn update_form(&mut self, cx: &mut Cx) {
        let trigger = self.view.drop_down(cx, ids!(trigger_choice)).selected_item();
        self.view.widget(cx, ids!(interval_section)).set_visible(cx, trigger == 0);
        self.view.widget(cx, ids!(alarm_section)).set_visible(cx, trigger == 1);
        self.view.widget(cx, ids!(messages_section)).set_visible(cx, trigger == 2);
        let selected = self.selected_task(cx);
        self.view.widget(cx, ids!(task_list)).set_visible(cx, !self.tasks.is_empty());
        self.view.widget(cx, ids!(no_tasks)).set_visible(cx, self.tasks.is_empty() && !self.editing && !self.apps.is_empty());
        self.view.widget(cx, ids!(overview_header)).set_visible(cx, !self.editing);
        self.view.widget(cx, ids!(task_overview)).set_visible(cx, selected.is_some() && !self.editing);
        self.view.widget(cx, ids!(task_editor)).set_visible(cx, self.editing);
        self.view.widget(cx, ids!(app_settings)).set_visible(cx, self.selected_app(cx).is_some());
        self.view.label(cx, ids!(editor_title)).set_text(cx, if selected.is_some() { "Edit schedule" } else { "New task" });
        self.view.button(cx, ids!(save_task)).set_text(cx, if selected.is_some_and(|task| task.job.enabled) { "Save schedule" } else { "Save and enable task" });
        self.view.label(cx, ids!(alarm_timezone)).set_text(cx, &format!("Local time now: {}.", chrono::Local::now().format("%Y-%m-%d %H:%M (%:z)")));
        for id in [ids!(app_choice), ids!(context_choice)] {
            self.view.widget(cx, id).set_disabled(cx, selected.is_some());
        }
        for id in [ids!(app_choice_section), ids!(context_choice_section)] {
            self.view.widget(cx, id).set_visible(cx, selected.is_none());
        }
        for id in [ids!(saved_app), ids!(saved_context)] {
            self.view.widget(cx, id).set_visible(cx, selected.is_some());
        }
        if let Some(task) = selected {
            let app = self.selected_app(cx).map(|app| app.name.clone())
                .unwrap_or_else(|| format!("{} (not installed)", task.job.binding.app_id));
            let context = self.selected_context(cx).map(|context| context.label.clone())
                .unwrap_or_else(|| self.context_label(&task.job.binding.context));
            self.view.label(cx, ids!(saved_app)).set_text(cx, &app);
            self.view.label(cx, ids!(saved_context)).set_text(cx, &context);
        }
        self.view.widget(cx, ids!(changed_version)).set_visible(cx, self.version_changed(cx));
        self.view.widget(cx, ids!(room_action_help)).set_visible(cx, self.selected_context(cx).is_some_and(|choice| matches!(choice.context, JobContext::Room { .. })));
        let details = selected.map(|task| {
            let next = task.job.next_due_ms.map(local_label).unwrap_or_else(|| if matches!(task.job.trigger, Trigger::RoomMessages) && task.job.enabled { "Waiting for room messages".into() } else { "Not scheduled".into() });
            let last = task.job.last_run.as_ref().map(|run| format!("{} at {}", outcome_label(&run.outcome), local_label(run.finished_ms))).unwrap_or_else(|| "No run recorded".into());
            format!("{} · {}\n{}\nNext run: {next}\nLast run: {last}\n\n{}", task_state_label(&task.job), self.context_label(&task.job.binding.context), trigger_label(&task.job.trigger), saved_status(&task.job))
        }).unwrap_or_default();
        self.view.label(cx, ids!(task_details)).set_text(cx, &details);
        self.view.label(cx, ids!(runtime_details)).set_text(cx, selected.map(|task| task.status.as_str()).unwrap_or_default());
        self.view.widget(cx, ids!(show_details)).set_visible(cx, selected.is_some_and(|task| !task.status.is_empty()));
        self.view.button(cx, ids!(run_now)).set_text(cx, if selected.is_some_and(|task| matches!(task.job.trigger, Trigger::Alarm { .. })) { "Run now (uses alarm)" } else { "Run now" });
        set_button_enabled(&self.view, cx, ids!(new_task), !self.apps.is_empty());
        set_button_enabled(&self.view, cx, ids!(edit_task), selected.is_some_and(|task| task.job.in_flight.is_none()));
        let resume = selected.is_some_and(|task| !task_is_active(&task.job));
        self.view.button(cx, ids!(pause_resume)).set_text(cx, if resume { "Resume" } else { "Pause" });
        set_button_enabled(&self.view, cx, ids!(pause_resume), selected.is_some_and(|task|
            task_is_active(&task.job) || can_resume(task, now_ms())));
        set_button_enabled(&self.view, cx, ids!(run_now), selected.is_some_and(|task|
            task.job.enabled && task.job.in_flight.is_none() && task.current_fingerprint.as_ref() == Some(&task.job.fingerprint)));
        set_button_enabled(&self.view, cx, ids!(save_task), self.selected_binding(cx).is_ok() && selected.is_none_or(|task| task.job.in_flight.is_none()));
        set_button_enabled(&self.view, cx, ids!(open_task_app), self.selected_app(cx).is_some() && self.selected_context(cx).is_some_and(|context| context.available));
        set_button_enabled(&self.view, cx, ids!(task_permissions), self.selected_app(cx).is_some());
        self.view.redraw(cx);
    }

    fn save_action(&self, cx: &Cx, now_ms: u64) -> Result<A2AppOp, String> {
        let binding = self.selected_binding(cx)?;
        let app = self.selected_app(cx).ok_or("Choose a mini-app.")?;
        let fingerprint = app.fingerprint.as_ref().map_err(|error| format!("Cannot review the app version: {error}"))?;
        if let Some(task) = self.tasks.iter().find(|task| task.job.binding == binding) {
            if task.job.in_flight.is_some() {
                return Err("Pause this task before changing its settings.".into());
            }
            if task.current_fingerprint.as_ref() != Some(fingerprint) {
                return Err("The mini-app changed after this page was loaded. Choose Refresh, review the new version, then enable the task.".into());
            }
            if task.job.fingerprint != *fingerprint && !self.view.check_box(cx, ids!(reviewed_version)).active(cx) {
                return Err("Review the current app version, then check the version confirmation before enabling this task.".into());
            }
        }
        let trigger = parse_trigger(
            self.view.drop_down(cx, ids!(trigger_choice)).selected_item(),
            &self.view.text_input(cx, ids!(interval_value)).text(),
            self.view.drop_down(cx, ids!(interval_unit)).selected_item(),
            &self.view.text_input(cx, ids!(alarm_value)).text(), &binding.context, now_ms,
        )?;
        Ok(A2AppOp::SaveBackgroundTask { binding, trigger, expected_fingerprint: fingerprint.clone() })
    }

    fn submit(&mut self, cx: &mut Cx, action: Result<A2AppOp, String>) {
        match action {
            Ok(action) => {
                self.pending_save = match &action { A2AppOp::SaveBackgroundTask { binding, .. } => Some(binding.clone()), _ => None };
                self.view.widget(cx, ids!(task_error)).set_visible(cx, false);
                cx.action(action);
            }
            Err(error) => self.show_error(cx, &error),
        }
    }

    fn show_error(&mut self, cx: &mut Cx, error: &str) {
        self.view.label(cx, ids!(task_error)).set_text(cx, error);
        self.view.widget(cx, ids!(task_error)).set_visible(cx, true);
        self.view.redraw(cx);
    }
}

fn set_button_enabled(view: &View, cx: &mut Cx, id: &[LiveId], enabled: bool) {
    view.button(cx, id).set_enabled(cx, enabled);
    view.widget(cx, id).set_disabled(cx, !enabled);
}

fn parse_trigger(kind: usize, amount: &str, units: usize, alarm: &str, context: &JobContext, now_ms: u64) -> Result<Trigger, String> {
    match kind {
        0 => {
            let amount: u64 = amount.trim().parse().map_err(|_| "Enter a positive whole-number interval.".to_string())?;
            let multiplier = match units { 0 => 1, 1 => 60, 2 => 3600, 3 => 86_400, _ => return Err("Choose seconds, minutes, hours, or days.".into()) };
            let seconds = amount.checked_mul(multiplier).ok_or("The interval is too large.")?;
            if seconds < MIN_INTERVAL_SECONDS { return Err(format!("Intervals must be at least {MIN_INTERVAL_SECONDS} seconds.")); }
            if seconds.checked_mul(1000).and_then(|millis| now_ms.checked_add(millis)).is_none() { return Err("The interval is too large.".into()); }
            Ok(Trigger::Interval { seconds })
        }
        1 => parse_alarm(alarm, now_ms, &chrono::Local),
        2 if matches!(context, JobContext::Room { .. }) => Ok(Trigger::RoomMessages),
        2 => Err("New-message tasks need a single room. Choose New task and select a room, rather than a space or the whole account.".into()),
        _ => Err("Choose when the task should run.".into()),
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|duration| duration.as_millis().min(u64::MAX as u128) as u64).unwrap_or(0)
}

fn parse_alarm<T: chrono::TimeZone>(alarm: &str, now_ms: u64, timezone: &T) -> Result<Trigger, String> {
    let date = chrono::NaiveDateTime::parse_from_str(alarm.trim(), "%Y-%m-%d %H:%M")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(alarm.trim(), "%Y-%m-%d %H:%M:%S"))
        .map_err(|_| "Enter a date and local time as YYYY-MM-DD HH:MM (24-hour clock), for example 2026-12-31 14:30.".to_string())?;
    let date = match timezone.from_local_datetime(&date) {
        chrono::LocalResult::Single(date) => date,
        chrono::LocalResult::Ambiguous(..) => return Err("This local time occurs twice when the clocks change. Choose a time outside the clock change so the alarm is unambiguous.".into()),
        chrono::LocalResult::None => return Err("This local time does not exist in your device's time zone, possibly because the clocks change. Choose another time.".into()),
    };
    let unix_ms = u64::try_from(date.timestamp_millis()).map_err(|_| "Choose a future date and time.".to_string())?;
    if unix_ms <= now_ms { return Err("The alarm must be in the future. Choose a later date or time.".into()); }
    Ok(Trigger::Alarm { unix_ms })
}

fn local_input(unix_ms: u64) -> String {
    i64::try_from(unix_ms).ok().and_then(chrono::DateTime::from_timestamp_millis)
        .map(|date| date.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S").to_string()).unwrap_or_default()
}

fn local_label(unix_ms: u64) -> String {
    i64::try_from(unix_ms).ok().and_then(chrono::DateTime::from_timestamp_millis)
        .map(|date| date.with_timezone(&chrono::Local).format("%b %-d, %Y at %H:%M %Z").to_string())
        .unwrap_or_else(|| "Unavailable time".into())
}

fn task_is_active(job: &Job) -> bool { job.enabled || job.in_flight.is_some() }

fn can_resume(task: &TaskView, now_ms: u64) -> bool {
    task.current_fingerprint.as_ref() == Some(&task.job.fingerprint)
        && !matches!(task.job.trigger, Trigger::Alarm { unix_ms } if unix_ms <= now_ms)
}

fn task_state_label(job: &Job) -> &'static str {
    if job.in_flight.is_some() { "Running" }
    else if job.enabled { "Enabled" }
    else if job.pause_reason.is_some() { "Needs attention" }
    else if matches!(job.trigger, Trigger::Alarm { .. }) {
        if job.last_run.as_ref().is_some_and(|run| run.outcome == RunOutcome::Succeeded) { "Completed" }
        else { "Alarm disabled" }
    } else { "Paused" }
}

fn saved_status(job: &Job) -> &'static str {
    if job.in_flight.is_some() { return "A run is in progress. Pause stops it; wait for completion before changing the schedule."; }
    match job.pause_reason {
        Some(PauseReason::AppChanged) => "Paused: the mini-app or its permissions changed. Choose Refresh, open the mini-app and review its permissions, then Edit schedule to confirm and enable the new version.",
        Some(PauseReason::AppMissing) => "Paused: the mini-app is no longer installed. Reinstall it, choose Refresh, and review its permissions before enabling this task.",
        Some(PauseReason::TimedOut) => "Paused: the last run took too long. Open the mini-app to check its settings before choosing Resume.",
        Some(PauseReason::HookUnavailable) => "Paused: the mini-app could not run in the background. Open it to check its settings or install a newer version before choosing Resume.",
        None if job.enabled => "Enabled. Runs with this mini-app's existing permissions for the selected room, space, or account.",
        None if matches!(job.trigger, Trigger::Alarm { .. }) => "This alarm is no longer scheduled. Choose Edit schedule and enter a future date and time to schedule it again.",
        None => "Paused. Choose Resume to restart this schedule with the existing mini-app version.",
    }
}

fn outcome_label(outcome: &RunOutcome) -> &'static str {
    match outcome {
        RunOutcome::Succeeded => "Completed",
        RunOutcome::Failed => "Failed",
        RunOutcome::Blocked => "Blocked by permissions or data protection",
        RunOutcome::Cancelled => "Cancelled",
        RunOutcome::Interrupted => "Interrupted before completion",
    }
}

fn trigger_label(trigger: &Trigger) -> String {
    match trigger {
        Trigger::Interval { seconds } => {
            let (amount, unit) = if seconds % 86_400 == 0 { (seconds / 86_400, "day") }
                else if seconds % 3600 == 0 { (seconds / 3600, "hour") }
                else if seconds % 60 == 0 { (seconds / 60, "minute") }
                else { (*seconds, "second") };
            format!("Every {amount} {unit}{}", if amount == 1 { "" } else { "s" })
        }
        Trigger::Alarm { unix_ms } => format!("Alarm: {}", local_label(*unix_ms)),
        Trigger::RoomMessages => "When the room receives messages".into(),
    }
}

impl BackgroundTasksRef {
    pub fn configure(&self, cx: &mut Cx) {
        if let Some(mut inner) = self.borrow_mut() { inner.configure(cx); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use a2app_core::background::{RunMetadata, RunRecord};

    struct AccountGuard(Option<String>);

    impl AccountGuard {
        fn set(account: &str) -> Self {
            Self(super::super::information_flow::TEST_ACCOUNT.with(|current| current.replace(Some(account.into()))))
        }
    }

    impl Drop for AccountGuard {
        fn drop(&mut self) {
            super::super::information_flow::TEST_ACCOUNT.with(|current| current.replace(self.0.take()));
        }
    }

    fn editor() -> (Cx, WidgetRef) {
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let widget = cx.with_vm(|vm| {
            makepad_widgets::script_mod(vm);
            makepad_code_editor::script_mod(vm);
            crate::shared::script_mod(vm);
            super::super::permission_prompt::script_mod(vm);
            super::script_mod(vm);
            let value = script_eval!(vm, { mod.widgets.BackgroundTasks {} });
            WidgetRef::script_from_value(vm, value)
        });
        (cx, widget)
    }

    fn binding() -> JobBinding {
        JobBinding { account: "@alice:example.org".into(), app_id: "reminder".into(), context: JobContext::Room { room_id: "!test:example.org".into() } }
    }

    fn task() -> TaskView {
        TaskView {
            job: serde_json::from_value(serde_json::json!({
                "id": 7, "binding": binding(), "trigger": {"Interval": {"seconds": 300}},
                "enabled": true, "fingerprint": "a".repeat(64), "next_due_ms": 300_000,
                "in_flight": null, "last_run": null, "pause_reason": null, "recent_events": [],
            })).unwrap(),
            current_fingerprint: Some("a".repeat(64)), status: "Scheduled".into(),
        }
    }

    fn set_form(editor: &mut BackgroundTasks, cx: &mut Cx) {
        let binding = binding();
        editor.account = binding.account;
        editor.apps.push(AppChoice { id: binding.app_id, name: "Reminder".into(), fingerprint: Ok("a".repeat(64)) });
        editor.contexts.push(ContextChoice { context: binding.context, label: "Room: Test".into(), available: true });
        editor.view.drop_down(cx, ids!(app_choice)).set_labels(cx, vec!["Choose".into(), "Reminder".into()]);
        editor.view.drop_down(cx, ids!(app_choice)).set_selected_item(cx, 1);
        editor.view.drop_down(cx, ids!(context_choice)).set_labels(cx, vec!["Choose".into(), "Room: Test".into()]);
        editor.view.drop_down(cx, ids!(context_choice)).set_selected_item(cx, 1);
        editor.view.text_input(cx, ids!(interval_value)).set_text(cx, "5");
        editor.view.drop_down(cx, ids!(interval_unit)).set_selected_item(cx, 1);
    }

    fn select_task(editor: &mut BackgroundTasks, cx: &mut Cx, task: TaskView) {
        editor.tasks.push(task);
        editor.view.drop_down(cx, ids!(task_choice)).set_labels(cx, vec!["New".into(), "Saved task".into()]);
        editor.view.drop_down(cx, ids!(task_choice)).set_selected_item(cx, 1);
    }

    #[test]
    fn validates_intervals_without_overflow_or_rapid_background_work() {
        let context = JobContext::Account;
        for (amount, units, seconds) in [("60", 0, 60), ("5", 1, 300), ("2", 2, 7200), ("2", 3, 172_800)] {
            assert_eq!(parse_trigger(0, amount, units, "", &context, 1_000).unwrap(), Trigger::Interval { seconds });
        }
        for (amount, units, now) in [("59", 0, 0), ("0", 1, 0), ("1.5", 1, 0), ("-1", 1, 0), ("1", 4, 0), ("18446744073709551615", 2, 0), ("60", 0, u64::MAX)] {
            assert!(parse_trigger(0, amount, units, "", &context, now).is_err());
        }
    }

    #[test]
    fn validates_local_alarms_and_exact_room_message_contexts() {
        let future = 1_800_000_000_000;
        assert_eq!(parse_trigger(1, "", 0, &local_input(future), &JobContext::Account, future - 1).unwrap(), Trigger::Alarm { unix_ms: future });
        for alarm in [local_input(future), "2026-02-30 12:00:00".into(), "2026-09-19 12:00:00 PDT".into(), "1960-01-01 00:00:00".into()] {
            assert!(parse_trigger(1, "", 0, &alarm, &JobContext::Account, future).is_err());
        }
        assert_eq!(parse_trigger(2, "", 0, "", &binding().context, 0).unwrap(), Trigger::RoomMessages);
        for context in [JobContext::Account, JobContext::Space { space_id: "!space:example.org".into() }] {
            assert!(parse_trigger(2, "", 0, "", &context, 0).is_err());
        }
    }

    #[test]
    fn local_alarms_are_converted_to_an_absolute_time() {
        use chrono::TimeZone;
        let timezone = chrono::FixedOffset::west_opt(7 * 3600).unwrap();
        let expected = chrono::Utc.with_ymd_and_hms(2026, 12, 31, 21, 30, 0).unwrap().timestamp_millis() as u64;
        assert_eq!(parse_alarm("2026-12-31 14:30", expected - 1, &timezone).unwrap(), Trigger::Alarm { unix_ms: expected });
        assert_eq!(parse_alarm("2026-12-31 14:30:00", expected - 1, &timezone).unwrap(), Trigger::Alarm { unix_ms: expected });
        assert!(parse_alarm("2026-12-31 14:30", expected, &timezone).is_err());
        assert!(parse_alarm("2026-12-31 25:30", 0, &timezone).is_err());
    }

    #[test]
    fn schedule_editor_requires_explicit_entry_and_cancel_does_not_save() {
        let _account = AccountGuard::set("@alice:example.org");
        let (mut cx, widget) = editor();
        let mut editor = widget.borrow_mut::<BackgroundTasks>().unwrap();
        set_form(&mut editor, &mut cx);
        select_task(&mut editor, &mut cx, task());
        editor.update_form(&mut cx);
        assert!(editor.view.widget(&cx, ids!(task_overview)).visible());
        assert!(!editor.view.widget(&cx, ids!(task_editor)).visible());
        let edit_uid = editor.view.button(&cx, ids!(edit_task)).widget_uid();
        let clicks = cx.capture_actions(|cx| cx.widget_action(edit_uid, ButtonAction::Clicked(Default::default())));
        editor.handle_event(&mut cx, &Event::Actions(clicks), &mut Scope::empty());
        assert!(editor.view.widget(&cx, ids!(task_editor)).visible());
        assert!(!editor.view.widget(&cx, ids!(overview_header)).visible());
        assert!(!editor.view.widget(&cx, ids!(task_overview)).visible());
        editor.view.text_input(&cx, ids!(interval_value)).set_text(&mut cx, "42");
        let cancel_uid = editor.view.button(&cx, ids!(cancel_edit)).widget_uid();
        let clicks = cx.capture_actions(|cx| cx.widget_action(cancel_uid, ButtonAction::Clicked(Default::default())));
        let actions = cx.capture_actions(|cx| editor.handle_event(cx, &Event::Actions(clicks), &mut Scope::empty()));
        assert!(!editor.view.widget(&cx, ids!(task_editor)).visible());
        assert!(editor.view.widget(&cx, ids!(overview_header)).visible());
        assert!(editor.view.widget(&cx, ids!(task_overview)).visible());
        assert_eq!(editor.view.text_input(&cx, ids!(interval_value)).text(), "5");
        assert!(!actions.iter().any(|action| action.downcast_ref::<A2AppOp>().is_some()));
    }

    #[test]
    fn task_removal_uses_the_standard_confirmation_before_stopping_work() {
        let _account = AccountGuard::set("@alice:example.org");
        let (mut cx, widget) = editor();
        let mut editor = widget.borrow_mut::<BackgroundTasks>().unwrap();
        set_form(&mut editor, &mut cx);
        select_task(&mut editor, &mut cx, task());
        editor.update_form(&mut cx);
        let uid = editor.view.button(&cx, ids!(remove_task)).widget_uid();
        let clicks = cx.capture_actions(|cx| cx.widget_action(uid, ButtonAction::Clicked(Default::default())));
        let actions = cx.capture_actions(|cx| editor.handle_event(cx, &Event::Actions(clicks), &mut Scope::empty()));
        assert!(!actions.iter().any(|action| matches!(action.downcast_ref::<A2AppOp>(), Some(A2AppOp::RemoveBackgroundTask(..)))));
        assert!(actions.iter().any(|action| {
            let Some(ConfirmDeleteAction::Show(content)) = action.downcast_ref() else { return false };
            let content = content.borrow();
            let content = content.as_ref().unwrap();
            content.title_text == "Remove background task?" && content.on_accept_clicked.is_some()
        }));
    }

    #[test]
    fn account_change_discards_old_task_actions() {
        let _account = AccountGuard::set("@bob:example.org");
        let (mut cx, widget) = editor();
        let mut editor = widget.borrow_mut::<BackgroundTasks>().unwrap();
        set_form(&mut editor, &mut cx);
        select_task(&mut editor, &mut cx, task());
        editor.update_form(&mut cx);
        let uid = editor.view.button(&cx, ids!(pause_resume)).widget_uid();
        let clicks = cx.capture_actions(|cx| cx.widget_action(uid, ButtonAction::Clicked(Default::default())));
        let actions = cx.capture_actions(|cx| editor.handle_event(cx, &Event::Actions(clicks), &mut Scope::empty()));
        assert_eq!(editor.account, "@bob:example.org");
        assert!(!actions.iter().any(|action| action.downcast_ref::<A2AppOp>().is_some()));
        assert!(!editor.editing);
    }

    #[test]
    fn new_task_requires_explicit_valid_binding_and_emits_only_enable_operation() {
        let (mut cx, widget) = editor();
        let mut editor = widget.borrow_mut::<BackgroundTasks>().unwrap();
        editor.update_form(&mut cx);
        assert!(editor.save_action(&cx, 0).is_err());
        assert!(!editor.view.button(&cx, ids!(save_task)).borrow().unwrap().enabled());
        assert!(editor.view.widget(&cx, ids!(save_task)).disabled(&cx));
        set_form(&mut editor, &mut cx);
        match editor.save_action(&cx, 0).unwrap() {
            A2AppOp::SaveBackgroundTask { binding: actual, trigger, expected_fingerprint } => {
                assert_eq!(actual, binding());
                assert_eq!(trigger, Trigger::Interval { seconds: 300 });
                assert_eq!(expected_fingerprint, "a".repeat(64));
            }
            _ => panic!("task activation must not grant app permissions"),
        }
        editor.contexts[0].available = false;
        assert!(editor.save_action(&cx, 0).is_err());
        editor.contexts[0].available = true;
        editor.account.clear();
        assert!(editor.save_action(&cx, 0).is_err());
    }

    #[test]
    fn changed_source_requires_review_even_when_new_task_targets_saved_binding() {
        let (mut cx, widget) = editor();
        let mut editor = widget.borrow_mut::<BackgroundTasks>().unwrap();
        set_form(&mut editor, &mut cx);
        let mut changed = task();
        changed.job.fingerprint = "b".repeat(64);
        changed.job.enabled = false;
        select_task(&mut editor, &mut cx, changed);
        editor.update_form(&mut cx);
        assert!(editor.view.widget(&cx, ids!(changed_version)).visible());
        assert!(!editor.view.button(&cx, ids!(pause_resume)).borrow().unwrap().enabled());
        assert!(editor.save_action(&cx, 0).unwrap_err().contains("version confirmation"));
        editor.view.drop_down(&cx, ids!(task_choice)).set_selected_item(&mut cx, 0);
        assert!(editor.save_action(&cx, 0).is_err(), "New must not bypass existing source review");
        editor.view.check_box(&cx, ids!(reviewed_version)).set_active(&mut cx, true, Animate::No);
        assert!(editor.save_action(&cx, 0).is_ok());
        editor.tasks[0].current_fingerprint = Some("c".repeat(64));
        assert!(editor.save_action(&cx, 0).unwrap_err().contains("after this page"));
    }

    #[test]
    fn running_and_paused_tasks_expose_only_valid_controls() {
        let _account = AccountGuard::set("@alice:example.org");
        let (mut cx, widget) = editor();
        let mut editor = widget.borrow_mut::<BackgroundTasks>().unwrap();
        set_form(&mut editor, &mut cx);
        select_task(&mut editor, &mut cx, task());
        editor.tasks[0].job.in_flight = Some(RunMetadata { run_id: 1, started_ms: 1, scheduled_ms: 1, deadline_ms: 120_001 });
        editor.update_form(&mut cx);
        assert!(!editor.view.button(&cx, ids!(save_task)).borrow().unwrap().enabled());
        assert!(!editor.view.button(&cx, ids!(run_now)).borrow().unwrap().enabled());
        assert!(editor.view.button(&cx, ids!(pause_resume)).borrow().unwrap().enabled());
        assert!(editor.save_action(&cx, 0).is_err());
        editor.tasks[0].job.enabled = false;
        editor.tasks[0].job.trigger = Trigger::Alarm { unix_ms: 1_000 };
        editor.update_form(&mut cx);
        assert_eq!(editor.view.button(&cx, ids!(pause_resume)).text(), "Pause", "a claimed alarm is disabled but its current run must remain pausable");
        assert!(editor.view.button(&cx, ids!(pause_resume)).borrow().unwrap().enabled());
        let uid = editor.view.button(&cx, ids!(pause_resume)).widget_uid();
        let click = cx.capture_actions(|cx| cx.widget_action(uid, ButtonAction::Clicked(Default::default())));
        let actions = cx.capture_actions(|cx| editor.handle_event(cx, &Event::Actions(click), &mut Scope::empty()));
        assert!(actions.iter().any(|action| matches!(action.downcast_ref::<A2AppOp>(), Some(A2AppOp::SetBackgroundTaskEnabled { id: 7, enabled: false, .. }))));
        editor.tasks[0].job.trigger = Trigger::Interval { seconds: 300 };
        editor.tasks[0].job.in_flight = None;
        editor.tasks[0].job.enabled = false;
        editor.update_form(&mut cx);
        assert_eq!(editor.view.button(&cx, ids!(pause_resume)).text(), "Resume");
        assert!(editor.view.button(&cx, ids!(pause_resume)).borrow().unwrap().enabled());
        assert!(!editor.view.button(&cx, ids!(run_now)).borrow().unwrap().enabled());
        assert!(editor.view.widget(&cx, ids!(app_choice)).disabled(&cx));
        assert!(editor.view.widget(&cx, ids!(context_choice)).disabled(&cx));
        assert!(!editor.view.widget(&cx, ids!(app_choice_section)).visible());
        assert!(!editor.view.widget(&cx, ids!(context_choice_section)).visible());
        assert!(editor.view.widget(&cx, ids!(saved_app)).visible());
        assert!(editor.view.widget(&cx, ids!(saved_context)).visible());
        editor.view.drop_down(&cx, ids!(task_choice)).set_selected_item(&mut cx, 0);
        editor.update_form(&mut cx);
        assert!(editor.view.widget(&cx, ids!(app_choice_section)).visible());
        assert!(editor.view.widget(&cx, ids!(context_choice_section)).visible());
        assert!(!editor.view.widget(&cx, ids!(saved_app)).visible());
        assert!(!editor.view.widget(&cx, ids!(saved_context)).visible());
    }

    #[test]
    fn persisted_failures_and_completed_alarms_remain_actionable_after_restart() {
        let (mut cx, widget) = editor();
        let mut editor = widget.borrow_mut::<BackgroundTasks>().unwrap();
        set_form(&mut editor, &mut cx);
        let mut completed = task();
        completed.job.enabled = false;
        completed.job.trigger = Trigger::Alarm { unix_ms: 1_000 };
        completed.job.next_due_ms = None;
        completed.job.last_run = Some(RunRecord { run_id: 1, started_ms: 1_000, finished_ms: 1_001, outcome: RunOutcome::Succeeded });
        completed.status.clear();
        assert_eq!(task_state_label(&completed.job), "Completed");
        select_task(&mut editor, &mut cx, completed);
        editor.update_form(&mut cx);
        let details = editor.view.label(&cx, ids!(task_details)).text();
        assert!(details.contains("Completed at"));
        assert!(details.contains("Edit schedule and enter a future date and time"));
        assert!(!editor.view.button(&cx, ids!(pause_resume)).borrow().unwrap().enabled());
        for reason in [PauseReason::AppChanged, PauseReason::AppMissing, PauseReason::TimedOut, PauseReason::HookUnavailable] {
            editor.tasks[0].job.pause_reason = Some(reason);
            editor.update_form(&mut cx);
            assert!(editor.view.label(&cx, ids!(task_details)).text().contains("Paused:"));
        }
    }
}
