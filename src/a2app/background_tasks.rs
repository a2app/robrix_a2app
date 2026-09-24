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
use super::permission_choices::*;

script_mod! {
    use mod.prelude.widgets.*
    use mod.widgets.*

    mod.widgets.BackgroundTaskRow = #(BackgroundTaskRow::register_widget(vm)) {
        width: Fill, height: Fit, flow: Down, spacing: 8, padding: 12
        task_name := SubsectionLabel { width: Fill, margin: 0, flow: Flow.Right{wrap: true} }
        task_summary := mod.widgets.PermissionOptionLabel {}
        View {
            width: Fill, height: Fit, flow: Flow.Right{wrap: true}, spacing: 8
            pause := RobrixNeutralIconButton {
                padding: 10, icon_walk: Walk{width: 0, height: 0, margin: 0}, text: "Pause"
            }
            edit := RobrixNeutralIconButton {
                padding: 10, icon_walk: Walk{width: 0, height: 0, margin: 0}, text: "Edit"
            }
            details := RobrixNeutralIconButton {
                padding: 10, icon_walk: Walk{width: 0, height: 0, margin: 0}, text: "Details"
            }
        }
        LineH { margin: Inset{top: 4} }
    }

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
                    text: "Mini-apps can check for updates or respond to messages while Robrix is open. Enabled tasks resume when you next sign in."
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
                    text: "Install or create a mini-app with background support to get started."
                }
                no_tasks := mod.widgets.PermissionOptionLabel {
                    text: "No tasks yet. Try checking a website every 12 hours and posting updates in a room."
                }
                task_list := FlatList {
                    width: Fill, height: Fit, flow: Down
                    task_row := mod.widgets.BackgroundTaskRow {}
                }
            }
            task_overview := View {
                visible: false, width: Fill, height: Fit, flow: Down, spacing: 10
                close_details := RobrixNeutralIconButton {
                    padding: 10, icon_walk: Walk{width: 0, height: 0, margin: 0}, text: "Back to tasks"
                }
                task_details := mod.widgets.PermissionOptionLabel {}
                View {
                    width: Fill, height: Fit, flow: Flow.Right{wrap: true}, spacing: 8
                    pause_resume := RobrixNeutralIconButton {
                        padding: 10, icon_walk: Walk{width: 0, height: 0, margin: 0}, text: "Pause"
                    }
                    edit_task := RobrixNeutralIconButton {
                        padding: 10, icon_walk: Walk{width: 0, height: 0, margin: 0}, text: "Edit schedule"
                    }
                    run_now := RobrixNeutralIconButton {
                        padding: 10, icon_walk: Walk{width: 0, height: 0, margin: 0}, text: "Run now"
                    }
                }
                runtime_details := mod.widgets.PermissionOptionLabel {}
                remove_task := RobrixNegativeIconButton {
                    padding: 10, icon_walk: Walk{width: 0, height: 0, margin: 0}, text: "Remove task"
                }
            }
            task_editor := View {
                visible: false, width: Fill, height: Fit, flow: Down, spacing: 12
                View {
                    width: Fill, height: Fit, flow: Flow.Right{wrap: true}, spacing: 8, align: Align{y: 0.5}
                    editor_title := SubsectionLabel { width: Fit, text: "New task", margin: 0 }
                    cancel_edit := RobrixNeutralIconButton {
                        padding: 10, icon_walk: Walk{width: 0, height: 0, margin: 0}, text: "Cancel"
                    }
                }
                step_title := SubsectionLabel { width: Fill, margin: 0, flow: Flow.Right{wrap: true} }
                app_step := View {
                    width: Fill, height: Fit, flow: Down, spacing: 8
                    mod.widgets.PermissionOptionLabel { text: "Which mini-app should work in the background?" }
                    app_choice := mod.widgets.PermissionChoices {}
                }
                context_step := View {
                    visible: false, width: Fill, height: Fit, flow: Down, spacing: 8
                    mod.widgets.PermissionOptionLabel { text: "Choose the room or space where it should run." }
                    context_choice := mod.widgets.PermissionDropDown { labels: ["Choose a room or space…"] }
                    mod.widgets.PermissionOptionLabel { text: "It uses the mini-app's saved settings here. Each mini-app can have one task per room, space, or account." }
                }
                schedule_step := View {
                    visible: false, width: Fill, height: Fit, flow: Down, spacing: 10
                    schedule_choice := mod.widgets.PermissionChoices {
                        labels: ["Every hour", "Every 12 hours", "Every day", "Once, at a set time", "When new room messages arrive", "Custom interval"]
                    }
                    interval_section := View {
                        visible: false, width: Fill, height: Fit, flow: Down, spacing: 6
                        mod.widgets.PermissionOptionLabel { text: "Repeat every" }
                        interval_value := RobrixTextInput { width: Fill, text: "5", empty_text: "Enter a whole number" }
                        interval_unit := mod.widgets.PermissionChoices { horizontal: true, labels: ["Seconds", "Minutes", "Hours", "Days"] }
                        mod.widgets.PermissionOptionLabel { text: "At least one minute between runs." }
                    }
                    alarm_section := View {
                        visible: false, width: Fill, height: Fit, flow: Down, spacing: 6
                        mod.widgets.PermissionOptionLabel { text: "Local date and time (24-hour clock)" }
                        alarm_value := RobrixTextInput { width: Fill, empty_text: "YYYY-MM-DD HH:MM", autocorrect: Disabled }
                        alarm_timezone := mod.widgets.PermissionOptionLabel {}
                    }
                    messages_section := mod.widgets.PermissionOptionLabel {
                        visible: false
                        text: "Requires a single room and permission to read it. The mini-app checks new messages for its conditions. Old messages are not replayed, and messages received during a run may be skipped."
                    }
                    schedule_error := mod.widgets.PermissionOptionLabel { visible: false, draw_text +: {color: (COLOR_FG_DANGER_RED)} }
                }
                review_step := View {
                    visible: false, width: Fill, height: Fit, flow: Down, spacing: 10
                    review_summary := mod.widgets.PermissionOptionLabel {}
                    mod.widgets.PermissionOptionLabel {
                        text: "Runs while Robrix is open and you are signed in. Missed scheduled runs are combined into one run when you return."
                    }
                    mod.widgets.PermissionOptionLabel {
                        text: "This task gets no new permissions. Set up the mini-app and its access before leaving it to run."
                    }
                    changed_version := View {
                        visible: false, width: Fill, height: Fit, flow: Down, spacing: 6
                        mod.widgets.PermissionOptionLabel { text: "The mini-app changed. Open it and review its permissions before enabling this version." }
                        reviewed_version := RobrixSettingsCheckBox {
                            width: Fill, height: Fit, text: "I reviewed this version and allow it to run"
                        }
                    }
                    review_error := mod.widgets.PermissionOptionLabel { visible: false, draw_text +: {color: (COLOR_FG_DANGER_RED)} }
                    save_task := RobrixPositiveIconButton {
                        padding: 10, icon_walk: Walk{width: 0, height: 0, margin: 0}, text: "Enable task"
                    }
                }
                View {
                    width: Fill, height: Fit, flow: Flow.Right{wrap: true}, spacing: 8
                    step_back := RobrixNeutralIconButton {
                        padding: 10, icon_walk: Walk{width: 0, height: 0, margin: 0}, text: "Back"
                    }
                    next_section := View {
                        width: Fit, height: Fit
                        step_next := RobrixPositiveIconButton {
                            padding: 10, icon_walk: Walk{width: 0, height: 0, margin: 0}, text: "Continue"
                        }
                    }
                }
            }
            app_settings := View {
                visible: false, width: Fill, height: Fit, flow: Down, spacing: 10
                LineH { width: Fill, margin: Inset{top: 4, bottom: 4} }
                SubsectionLabel { text: "Mini-app settings and access", margin: 0 }
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
                    text: "To review a blocked action, keep the mini-app open in its room, return here and choose Run now. Then review the action in Data sharing and retry. Background tasks cannot ask for permission while running."
                }
            }
            show_help := RobrixNeutralIconButton {
                padding: 8, icon_walk: Walk{width: 0, height: 0, margin: 0}, text: "How background tasks work"
            }
            task_help := View {
                visible: false, width: Fill, height: Fit, flow: Down, spacing: 8
                mod.widgets.PermissionOptionLabel {
                    text: "Pausing or removing a task stops its work and closes the mini-app instance. Saved settings and data are kept. Force Stop in the mini-app's details also disables its tasks."
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum TaskStep { #[default] App, Context, Schedule, Review }

#[derive(Clone, Copy, Debug)]
enum TaskCommand { PauseResume, Edit, Details }

#[derive(Clone, Debug)]
struct TaskRowAction { account: String, id: u64, command: TaskCommand }

#[derive(Script, ScriptHook, Widget)]
pub struct BackgroundTaskRow {
    #[deref] view: View,
    #[rust] account: String,
    #[rust] task_id: u64,
}

impl Widget for BackgroundTaskRow {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.handle_event(cx, event, scope);
        let Event::Actions(actions) = event else { return };
        for (id, command) in [(ids!(pause), TaskCommand::PauseResume), (ids!(edit), TaskCommand::Edit), (ids!(details), TaskCommand::Details)] {
            if self.view.button(cx, id).clicked(actions) {
                cx.action(TaskRowAction { account: self.account.clone(), id: self.task_id, command });
            }
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        self.view.draw_walk(cx, scope, walk)
    }
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
    #[rust] step: TaskStep,
    #[rust] selected_task_id: Option<u64>,
    #[rust] selected_app_id: Option<String>,
    #[rust] selected_context_id: Option<JobContext>,
    #[rust] showing_details: bool,
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
        for action in actions.iter() {
            if let Some(action) = action.downcast_ref::<TaskRowAction>() {
                if action.account != self.account || !self.tasks.iter().any(|task| task.job.id == action.id) { continue; }
                match action.command {
                    TaskCommand::PauseResume => { let result = self.pause_action(action.id); self.submit(cx, result); }
                    TaskCommand::Edit => self.open_task(cx, action.id, true),
                    TaskCommand::Details => self.open_task(cx, action.id, false),
                }
            }
        }
        if self.view.button(cx, ids!(refresh_tasks)).clicked(actions) { self.configure(cx); }
        if self.view.button(cx, ids!(new_task)).clicked(actions) {
            self.selected_task_id = None;
            self.showing_details = false;
            self.step = TaskStep::App;
            self.view.widget(cx, ids!(task_error)).set_visible(cx, false);
            self.set_editing(cx, true);
            self.load_selected_task(cx);
        }
        if self.view.button(cx, ids!(edit_task)).clicked(actions) {
            if let Some(id) = self.selected_task_id { self.open_task(cx, id, true); }
        }
        if self.view.button(cx, ids!(cancel_edit)).clicked(actions) {
            self.close_task(cx);
            return;
        }
        if self.view.button(cx, ids!(close_details)).clicked(actions) {
            self.back(cx);
            return;
        }
        if let Some(index) = self.view.permission_choices(cx, ids!(app_choice)).changed(actions) {
            self.selected_app_id = self.apps.get(index).map(|app| app.id.clone());
            self.load_contexts(cx, None);
            self.view.check_box(cx, ids!(reviewed_version)).set_active(cx, false, Animate::No);
            self.update_form(cx);
        }
        if let Some(index) = self.view.drop_down(cx, ids!(context_choice)).changed(actions) {
            self.selected_context_id = index.checked_sub(1).and_then(|index| self.contexts.get(index)).map(|choice| choice.context.clone());
            self.update_form(cx);
        }
        if self.view.permission_choices(cx, ids!(schedule_choice)).changed(actions).is_some()
            || self.view.permission_choices(cx, ids!(interval_unit)).changed(actions).is_some()
            || self.view.text_input(cx, ids!(interval_value)).changed(actions).is_some()
            || self.view.text_input(cx, ids!(alarm_value)).changed(actions).is_some()
            || self.view.check_box(cx, ids!(reviewed_version)).changed(actions).is_some()
        { self.update_form(cx); }
        if self.view.button(cx, ids!(step_next)).clicked(actions) {
            match self.advance_step(cx, now_ms()) {
                Ok(()) => {},
                Err(error) => self.show_error(cx, &error),
            }
        }
        if self.view.button(cx, ids!(step_back)).clicked(actions) {
            self.back(cx);
            return;
        }
        if self.view.button(cx, ids!(save_task)).clicked(actions) {
            let result = self.save_action(cx, now_ms());
            self.submit(cx, result);
        }
        if self.view.button(cx, ids!(pause_resume)).clicked(actions) {
            let result = self.selected_task_id.ok_or_else(|| "Select a task.".into()).and_then(|id| self.pause_action(id));
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
            if let Some(app) = self.selected_app(cx) { cx.action(BackgroundTasksAction::AppPermissions(app.id.clone())); }
        }
        if self.view.button(cx, ids!(task_sharing)).clicked(actions) { cx.action(BackgroundTasksAction::Sharing); }
        if self.view.button(cx, ids!(show_help)).clicked(actions) {
            let visible = !self.view.widget(cx, ids!(task_help)).visible();
            self.view.widget(cx, ids!(task_help)).set_visible(cx, visible);
            self.view.button(cx, ids!(show_help)).set_text(cx, if visible { "Hide help" } else { "How background tasks work" });
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        if self.account != super::information_flow::account().unwrap_or_default() { self.configure(cx); }
        else if self.revision != Some(background::revision()) { self.refresh_tasks(cx); }
        while let Some(item) = self.view.draw_walk(cx, scope, walk).step() {
            if let Some(mut list) = item.borrow_mut::<FlatList>() {
                for task in &self.tasks {
                    let Some(row) = list.item(cx, LiveId(task.job.id), id!(task_row)) else { continue };
                    if let Some(mut card) = row.borrow_mut::<BackgroundTaskRow>() {
                        card.account = self.account.clone();
                        card.task_id = task.job.id;
                        let app = self.apps.iter().find(|app| app.id == task.job.binding.app_id)
                            .map(|app| app.name.as_str()).unwrap_or(&task.job.binding.app_id);
                        card.view.label(cx, ids!(task_name)).set_text(cx, app);
                        let mut summary = format!("{}\n{} · {}", self.context_label(&task.job.binding.context), trigger_label(&task.job.trigger), task_state_label(&task.job));
                        if task.job.enabled && let Some(next) = task.job.next_due_ms { summary.push_str(&format!("\nNext: {}", local_label(next))); }
                        card.view.label(cx, ids!(task_summary)).set_text(cx, &summary);
                        card.view.button(cx, ids!(pause)).set_text(cx, if task_is_active(&task.job) { "Pause" } else { "Resume" });
                        set_button_enabled(&card.view, cx, ids!(pause), task_is_active(&task.job) || can_resume(task, now_ms()));
                        set_button_enabled(&card.view, cx, ids!(edit), task.job.in_flight.is_none());
                    }
                    row.draw_all(cx, &mut Scope::empty());
                }
            }
        }
        DrawStep::done()
    }
}

impl BackgroundTasks {
    pub fn back(&mut self, cx: &mut Cx) -> bool {
        if self.account != super::information_flow::account().unwrap_or_default() {
            self.configure(cx);
            return true;
        }
        if self.editing {
            self.step = match self.step {
                TaskStep::Review => TaskStep::Schedule,
                TaskStep::Schedule if self.selected_task_id.is_none() => TaskStep::Context,
                TaskStep::Context => TaskStep::App,
                _ => { self.close_task(cx); return true; },
            };
            self.view.widget(cx, ids!(task_error)).set_visible(cx, false);
            self.reset_scroll(cx);
            self.update_form(cx);
            true
        } else if self.showing_details {
            self.close_task(cx);
            true
        } else {
            false
        }
    }

    fn reset_scroll(&self, cx: &mut Cx) {
        self.view.view(cx, ids!(content)).set_scroll_pos(cx, Vec2d::default());
    }

    fn set_editing(&mut self, cx: &mut Cx, editing: bool) {
        self.editing = editing;
        self.reset_scroll(cx);
    }

    fn open_task(&mut self, cx: &mut Cx, id: u64, edit: bool) {
        let Some(task) = self.tasks.iter().find(|task| task.job.id == id) else { return };
        if edit && task.job.in_flight.is_some() { self.show_error(cx, "Pause the task before editing its schedule."); return; }
        self.selected_task_id = Some(id);
        self.showing_details = !edit;
        self.step = TaskStep::Schedule;
        self.set_editing(cx, edit);
        self.load_selected_task(cx);
        self.view.widget(cx, ids!(task_error)).set_visible(cx, false);
    }

    fn close_task(&mut self, cx: &mut Cx) {
        self.selected_task_id = None;
        self.showing_details = false;
        self.set_editing(cx, false);
        self.load_selected_task(cx);
        self.view.widget(cx, ids!(task_error)).set_visible(cx, false);
    }

    fn advance_step(&mut self, cx: &mut Cx, now: u64) -> Result<(), String> {
        self.step = match self.step {
            TaskStep::App => {
                self.selected_app(cx).ok_or("Choose a mini-app first.")?;
                TaskStep::Context
            }
            TaskStep::Context => {
                let binding = self.selected_binding(cx)?;
                if let Some(id) = self.tasks.iter().find(|task| task.job.binding == binding).map(|task| task.job.id) {
                    self.open_task(cx, id, true);
                    return Ok(());
                }
                TaskStep::Schedule
            }
            TaskStep::Schedule => { self.selected_trigger(cx, now)?; TaskStep::Review }
            TaskStep::Review => TaskStep::Review,
        };
        self.view.widget(cx, ids!(task_error)).set_visible(cx, false);
        self.reset_scroll(cx);
        self.update_form(cx);
        Ok(())
    }

    fn pause_action(&self, id: u64) -> Result<A2AppOp, String> {
        let task = self.tasks.iter().find(|task| task.job.id == id).ok_or("This task is no longer available.")?;
        let enabled = !task_is_active(&task.job);
        if enabled && !can_resume(task, now_ms()) {
            return Err("Open task details to review the mini-app version or choose a future alarm before resuming.".into());
        }
        Ok(A2AppOp::SetBackgroundTaskEnabled { id, enabled, expected_fingerprint: task.current_fingerprint.clone().unwrap_or_default() })
    }

    fn configure(&mut self, cx: &mut Cx) {
        self.account = super::information_flow::account().unwrap_or_default();
        self.pending_save = None;
        self.selected_task_id = None;
        self.selected_app_id = None;
        self.selected_context_id = None;
        self.showing_details = false;
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
        self.view.permission_choices(cx, ids!(app_choice)).set_labels(cx, self.apps.iter().map(|app| {
            if app_names[app.name.as_str()] > 1 { format!("{} ({})", app.name, app.id) }
            else { app.name.clone() }
        }).collect());
        self.view.widget(cx, ids!(no_background_apps)).set_visible(cx, self.apps.is_empty());
        self.refresh_tasks(cx);
        self.load_selected_task(cx);
    }

    fn refresh_tasks(&mut self, cx: &mut Cx) {
        let error = match background::snapshot() {
            Ok(tasks) => { self.tasks = tasks; self.view.widget(cx, ids!(task_error)).set_visible(cx, false); None }
            Err(error) => { self.tasks.clear(); Some(error) }
        };
        self.revision = Some(background::revision());
        let saved = self.pending_save.take().is_some_and(|binding| self.tasks.iter().any(|task| task.job.binding == binding));
        let removed = self.selected_task_id.is_some_and(|id| !self.tasks.iter().any(|task| task.job.id == id));
        if saved || removed { self.close_task(cx); }
        if let Some(error) = error { self.show_error(cx, &error); }
        self.update_form(cx);
    }

    fn selected_task(&self, _cx: &Cx) -> Option<&TaskView> {
        self.selected_task_id.and_then(|id| self.tasks.iter().find(|task| task.job.id == id))
    }

    fn selected_app(&self, _cx: &Cx) -> Option<&AppChoice> {
        self.selected_app_id.as_ref().and_then(|id| self.apps.iter().find(|app| &app.id == id))
    }

    fn selected_context(&self, _cx: &Cx) -> Option<&ContextChoice> {
        self.selected_context_id.as_ref().and_then(|context| self.contexts.iter().find(|choice| &choice.context == context))
    }

    fn selected_binding(&self, cx: &Cx) -> Result<JobBinding, String> {
        if self.account.is_empty() { return Err("Sign in before configuring a background task.".into()); }
        let app = self.selected_app(cx).ok_or("Choose an installed mini-app with background support.")?;
        let context = self.selected_context(cx).ok_or("Choose a room, space, or account for this task.")?;
        if !context.available { return Err("The saved room or space is no longer available. Rejoin it or create a task in another context.".into()); }
        Ok(JobBinding { account: self.account.clone(), app_id: app.id.clone(), context: context.context.clone() })
    }

    fn load_selected_task(&mut self, cx: &mut Cx) {
        let selected = self.selected_task(cx).map(|task| task.job.clone());
        self.view.check_box(cx, ids!(reviewed_version)).set_active(cx, false, Animate::No);
        self.selected_app_id = selected.as_ref().map(|job| job.binding.app_id.clone());
        let app = self.selected_app_id.as_ref().and_then(|id| self.apps.iter().position(|app| &app.id == id)).unwrap_or(usize::MAX);
        self.view.permission_choices(cx, ids!(app_choice)).set_selected_item(cx, app);
        self.load_contexts(cx, selected.as_ref().map(|job| &job.binding.context));
        let (amount, units, alarm) = match selected.as_ref().map(|job| &job.trigger) {
            Some(Trigger::Interval { seconds }) if seconds % 86_400 == 0 => ((seconds / 86_400).to_string(), 3, String::new()),
            Some(Trigger::Interval { seconds }) if seconds % 3600 == 0 => ((seconds / 3600).to_string(), 2, String::new()),
            Some(Trigger::Interval { seconds }) if seconds % 60 == 0 => ((seconds / 60).to_string(), 1, String::new()),
            Some(Trigger::Interval { seconds }) => (seconds.to_string(), 0, String::new()),
            Some(Trigger::Alarm { unix_ms }) => ("5".into(), 1, local_input(*unix_ms)),
            _ => ("5".into(), 1, String::new()),
        };
        let schedule = match selected.as_ref().map(|job| &job.trigger) {
            Some(Trigger::Interval { seconds: 3600 }) => 0,
            Some(Trigger::Interval { seconds: 43200 }) => 1,
            Some(Trigger::Interval { seconds: 86400 }) => 2,
            Some(Trigger::Alarm { .. }) => 3,
            Some(Trigger::RoomMessages) => 4,
            Some(Trigger::Interval { .. }) => 5,
            None => 1,
        };
        self.view.permission_choices(cx, ids!(schedule_choice)).set_selected_item(cx, schedule);
        self.view.text_input(cx, ids!(interval_value)).set_text(cx, &amount);
        self.view.permission_choices(cx, ids!(interval_unit)).set_selected_item(cx, units);
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
        self.selected_context_id = index.checked_sub(1).and_then(|index| self.contexts.get(index)).map(|choice| choice.context.clone());
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
        let schedule = self.view.permission_choices(cx, ids!(schedule_choice)).selected_item();
        self.view.widget(cx, ids!(interval_section)).set_visible(cx, schedule == 5);
        self.view.widget(cx, ids!(alarm_section)).set_visible(cx, schedule == 3);
        self.view.widget(cx, ids!(messages_section)).set_visible(cx, schedule == 4);
        let selected = self.selected_task(cx);
        let landing = !self.editing && !self.showing_details;
        self.view.widget(cx, ids!(no_tasks)).set_visible(cx, self.tasks.is_empty());
        self.view.widget(cx, ids!(overview_header)).set_visible(cx, landing);
        self.view.widget(cx, ids!(task_overview)).set_visible(cx, selected.is_some() && self.showing_details && !self.editing);
        self.view.widget(cx, ids!(task_editor)).set_visible(cx, self.editing);
        self.view.widget(cx, ids!(app_settings)).set_visible(cx, self.selected_app(cx).is_some()
            && (self.showing_details || (self.editing && self.step == TaskStep::Review)));
        self.view.label(cx, ids!(editor_title)).set_text(cx, if selected.is_some() { "Edit task" } else { "New task" });
        let title = match (selected.is_some(), self.step) {
            (_, TaskStep::App) => "1 of 4 · Choose a mini-app",
            (_, TaskStep::Context) => "2 of 4 · Choose where it runs",
            (false, TaskStep::Schedule) => "3 of 4 · Choose a schedule",
            (false, TaskStep::Review) => "4 of 4 · Review your task",
            (true, TaskStep::Schedule) => "1 of 2 · Choose a schedule",
            (true, TaskStep::Review) => "2 of 2 · Review your changes",
        };
        self.view.label(cx, ids!(step_title)).set_text(cx, title);
        for (id, step) in [(ids!(app_step), TaskStep::App), (ids!(context_step), TaskStep::Context),
            (ids!(schedule_step), TaskStep::Schedule), (ids!(review_step), TaskStep::Review)]
        { self.view.widget(cx, id).set_visible(cx, self.step == step); }
        self.view.widget(cx, ids!(next_section)).set_visible(cx, self.step != TaskStep::Review);
        self.view.button(cx, ids!(step_back)).set_text(cx,
            if self.step == TaskStep::App || (selected.is_some() && self.step == TaskStep::Schedule) { "Back to tasks" } else { "Back" });
        let trigger = self.selected_trigger(cx, now_ms());
        let next = match self.step {
            TaskStep::App => self.selected_app(cx).is_some(),
            TaskStep::Context => self.selected_binding(cx).is_ok(),
            TaskStep::Schedule => trigger.is_ok(),
            TaskStep::Review => false,
        };
        set_button_enabled(&self.view, cx, ids!(step_next), next);
        self.view.widget(cx, ids!(schedule_error)).set_visible(cx, trigger.is_err());
        self.view.label(cx, ids!(schedule_error)).set_text(cx, trigger.as_ref().err().map(String::as_str).unwrap_or_default());
        let app = self.selected_app(cx).map(|app| app.name.as_str()).unwrap_or("Mini-app unavailable");
        let context = self.selected_context(cx).map(|context| context.label.as_str()).unwrap_or("Room or space unavailable");
        let summary = format!("{app}\n{context}\n{}", trigger.as_ref().map(trigger_label).unwrap_or_else(Clone::clone));
        self.view.label(cx, ids!(review_summary)).set_text(cx, &summary);
        self.view.button(cx, ids!(save_task)).set_text(cx, if selected.is_some_and(|task| task.job.enabled) { "Save changes" } else { "Enable task" });
        self.view.label(cx, ids!(alarm_timezone)).set_text(cx, &format!("Local time now: {}.", chrono::Local::now().format("%Y-%m-%d %H:%M (%:z)")));
        self.view.widget(cx, ids!(changed_version)).set_visible(cx, self.version_changed(cx));
        self.view.widget(cx, ids!(room_action_help)).set_visible(cx, self.showing_details && self.selected_context(cx).is_some_and(|choice| matches!(choice.context, JobContext::Room { .. })));
        let details = selected.map(|task| {
            let next = task.job.next_due_ms.map(local_label).unwrap_or_else(|| if matches!(task.job.trigger, Trigger::RoomMessages) && task.job.enabled { "Waiting for room messages".into() } else { "Not scheduled".into() });
            let last = task.job.last_run.as_ref().map(|run| format!("{} at {}", outcome_label(&run.outcome), local_label(run.finished_ms))).unwrap_or_else(|| "No run recorded".into());
            format!("{app}\n{}\n{} · {}\nNext run: {next}\nLast run: {last}\n\n{}", self.context_label(&task.job.binding.context), trigger_label(&task.job.trigger), task_state_label(&task.job), saved_status(&task.job))
        }).unwrap_or_default();
        self.view.label(cx, ids!(task_details)).set_text(cx, &details);
        self.view.label(cx, ids!(runtime_details)).set_text(cx, selected.map(|task| task.status.as_str()).unwrap_or_default());
        self.view.button(cx, ids!(run_now)).set_text(cx, if selected.is_some_and(|task| matches!(task.job.trigger, Trigger::Alarm { .. })) { "Run now (uses alarm)" } else { "Run now" });
        set_button_enabled(&self.view, cx, ids!(new_task), !self.apps.is_empty());
        set_button_enabled(&self.view, cx, ids!(edit_task), selected.is_some_and(|task| task.job.in_flight.is_none()));
        self.view.button(cx, ids!(pause_resume)).set_text(cx, if selected.is_some_and(|task| !task_is_active(&task.job)) { "Resume" } else { "Pause" });
        set_button_enabled(&self.view, cx, ids!(pause_resume), selected.is_some_and(|task| task_is_active(&task.job) || can_resume(task, now_ms())));
        set_button_enabled(&self.view, cx, ids!(run_now), selected.is_some_and(|task| task.job.enabled && task.job.in_flight.is_none() && task.current_fingerprint.as_ref() == Some(&task.job.fingerprint)));
        let save = self.save_action(cx, now_ms());
        self.view.widget(cx, ids!(review_error)).set_visible(cx, save.is_err());
        self.view.label(cx, ids!(review_error)).set_text(cx, save.as_ref().err().map(String::as_str).unwrap_or_default());
        set_button_enabled(&self.view, cx, ids!(save_task), save.is_ok());
        set_button_enabled(&self.view, cx, ids!(open_task_app), self.selected_binding(cx).is_ok());
        set_button_enabled(&self.view, cx, ids!(task_permissions), self.selected_app(cx).is_some());
        self.view.redraw(cx);
    }

    fn selected_trigger(&self, cx: &Cx, now: u64) -> Result<Trigger, String> {
        let context = self.selected_context(cx).ok_or("Choose where the task runs first.")?;
        let (kind, amount, units) = match self.view.permission_choices(cx, ids!(schedule_choice)).selected_item() {
            0 => (0, "1".into(), 2),
            1 => (0, "12".into(), 2),
            2 => (0, "1".into(), 3),
            3 => (1, String::new(), 0),
            4 => (2, String::new(), 0),
            5 => (0, self.view.text_input(cx, ids!(interval_value)).text(), self.view.permission_choices(cx, ids!(interval_unit)).selected_item()),
            _ => return Err("Choose a schedule.".into()),
        };
        parse_trigger(kind, &amount, units, &self.view.text_input(cx, ids!(alarm_value)).text(), &context.context, now)
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
                return Err("The mini-app changed after this page was loaded. Cancel, choose Refresh in the task list, then review the new version before enabling the task.".into());
            }
            if task.job.fingerprint != *fingerprint && !self.view.check_box(cx, ids!(reviewed_version)).active(cx) {
                return Err("Review the current app version, then check the version confirmation before enabling this task.".into());
            }
        }
        let trigger = self.selected_trigger(cx, now_ms)?;
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
    pub fn back(&self, cx: &mut Cx) -> bool {
        self.borrow_mut().is_some_and(|mut inner| inner.back(cx))
    }

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
            super::super::permission_choices::script_mod(vm);
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
        editor.selected_app_id = Some(binding.app_id.clone());
        editor.selected_context_id = Some(binding.context.clone());
        editor.apps.push(AppChoice { id: binding.app_id, name: "Reminder".into(), fingerprint: Ok("a".repeat(64)) });
        editor.contexts.push(ContextChoice { context: binding.context, label: "Room: Test".into(), available: true });
        editor.view.permission_choices(cx, ids!(app_choice)).set_labels(cx, vec!["Reminder".into()]);
        editor.view.permission_choices(cx, ids!(app_choice)).set_selected_item(cx, 0);
        editor.view.drop_down(cx, ids!(context_choice)).set_labels(cx, vec!["Choose".into(), "Room: Test".into()]);
        editor.view.drop_down(cx, ids!(context_choice)).set_selected_item(cx, 1);
        editor.view.text_input(cx, ids!(interval_value)).set_text(cx, "5");
        editor.view.permission_choices(cx, ids!(interval_unit)).set_selected_item(cx, 1);
        editor.view.permission_choices(cx, ids!(schedule_choice)).set_selected_item(cx, 5);
    }

    fn select_task(editor: &mut BackgroundTasks, _cx: &mut Cx, task: TaskView) {
        editor.selected_task_id = Some(task.job.id);
        editor.showing_details = true;
        editor.tasks.push(task);
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
        assert!(!editor.view.widget(&cx, ids!(task_overview)).visible());
        assert!(editor.selected_task_id.is_none());
        assert!(editor.selected_app_id.is_none());
        assert!(!actions.iter().any(|action| action.downcast_ref::<A2AppOp>().is_some()));
    }

    #[test]
    fn creation_reviews_the_selected_binding_and_schedule_before_enabling() {
        let _account = AccountGuard::set("@alice:example.org");
        let (mut cx, widget) = editor();
        let mut editor = widget.borrow_mut::<BackgroundTasks>().unwrap();
        set_form(&mut editor, &mut cx);
        editor.set_editing(&mut cx, true);
        editor.step = TaskStep::App;
        editor.update_form(&mut cx);
        for expected in [TaskStep::Context, TaskStep::Schedule, TaskStep::Review] {
            let uid = editor.view.button(&cx, ids!(step_next)).widget_uid();
            let clicks = cx.capture_actions(|cx| cx.widget_action(uid, ButtonAction::Clicked(Default::default())));
            let actions = cx.capture_actions(|cx| editor.handle_event(cx, &Event::Actions(clicks), &mut Scope::empty()));
            assert_eq!(editor.step, expected);
            assert!(!actions.iter().any(|action| action.downcast_ref::<A2AppOp>().is_some()));
        }
        assert!(editor.view.widget(&cx, ids!(review_step)).visible());
        assert!(!editor.view.widget(&cx, ids!(schedule_step)).visible());
        let uid = editor.view.button(&cx, ids!(save_task)).widget_uid();
        let clicks = cx.capture_actions(|cx| cx.widget_action(uid, ButtonAction::Clicked(Default::default())));
        let actions = cx.capture_actions(|cx| editor.handle_event(cx, &Event::Actions(clicks), &mut Scope::empty()));
        assert!(actions.iter().any(|action| matches!(action.downcast_ref::<A2AppOp>(),
            Some(A2AppOp::SaveBackgroundTask { binding: actual, trigger: Trigger::Interval { seconds: 300 }, .. }) if actual == &binding())));
    }

    #[test]
    fn back_retraces_task_steps_before_leaving_the_task_list() {
        let _account = AccountGuard::set("@alice:example.org");
        let (mut cx, widget) = editor();
        let mut editor = widget.borrow_mut::<BackgroundTasks>().unwrap();
        set_form(&mut editor, &mut cx);
        editor.set_editing(&mut cx, true);
        editor.step = TaskStep::Review;
        let actions = cx.capture_actions(|cx| {
            for expected in [TaskStep::Schedule, TaskStep::Context, TaskStep::App] {
                assert!(editor.back(cx));
                assert_eq!(editor.step, expected);
                assert!(editor.editing);
            }
            assert!(editor.back(cx));
            assert!(!editor.editing);
            assert!(!editor.showing_details);
            assert!(editor.selected_app_id.is_none());
            assert!(!editor.back(cx));

            select_task(&mut editor, cx, task());
            assert!(editor.back(cx));
            assert!(editor.selected_task_id.is_none());
            assert!(!editor.showing_details);
            assert!(!editor.back(cx));

            editor.selected_task_id = Some(7);
            editor.set_editing(cx, true);
            editor.step = TaskStep::Review;
            assert!(editor.back(cx));
            assert_eq!(editor.step, TaskStep::Schedule);
            assert!(editor.back(cx));
            assert!(editor.selected_task_id.is_none());
            assert!(!editor.editing);
            assert!(!editor.back(cx));
        });
        assert!(!actions.iter().any(|action| action.downcast_ref::<A2AppOp>().is_some()));
    }

    #[test]
    fn task_cards_address_stable_ids_and_reject_another_accounts_actions() {
        let _account = AccountGuard::set("@alice:example.org");
        let (mut cx, widget) = editor();
        let mut editor = widget.borrow_mut::<BackgroundTasks>().unwrap();
        set_form(&mut editor, &mut cx);
        select_task(&mut editor, &mut cx, task());
        let mut second = task();
        second.job.id = 8;
        editor.tasks.insert(0, second);
        for (account, id, expected) in [("@alice:example.org", 8, true), ("@bob:example.org", 7, false), ("@alice:example.org", 99, false)] {
            let clicks = cx.capture_actions(|cx| cx.action(TaskRowAction { account: account.into(), id, command: TaskCommand::PauseResume }));
            let actions = cx.capture_actions(|cx| editor.handle_event(cx, &Event::Actions(clicks), &mut Scope::empty()));
            let paused = actions.iter().any(|action| matches!(action.downcast_ref::<A2AppOp>(), Some(A2AppOp::SetBackgroundTaskEnabled { id: 8, enabled: false, .. })));
            assert_eq!(paused, expected);
            if !expected { assert!(!actions.iter().any(|action| action.downcast_ref::<A2AppOp>().is_some())); }
        }
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
        editor.selected_task_id = None;
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
        assert!(!editor.editing);
        assert!(editor.showing_details);
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
