//! User-owned background schedules; activation never grants app authority.

use makepad_widgets::*;
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
            width: Fill, height: Fill, flow: Down, spacing: 10, padding: 15
            mod.widgets.PermissionOptionLabel {
                text: "Run opted-in mini-apps on a schedule or when a room receives messages. Tasks run only while Robrix is running. Enabled tasks restore after launch; missed intervals are combined into one run."
            }
            mod.widgets.PermissionOptionLabel {
                text: "Enabling a task grants no read, write, internet or data-sharing permissions. Background work cannot open approval prompts. Review blocked permissions here; session permissions still expire."
            }
            refresh_tasks := RobrixNeutralIconButton {
                padding: 8, icon_walk: Walk{width: 0, height: 0, margin: 0}, text: "Refresh tasks and app versions"
            }
            task_error := mod.widgets.PermissionOptionLabel {
                visible: false
                draw_text +: { color: (COLOR_FG_DANGER_RED) }
            }
            SubsectionLabel { text: "Saved tasks for this account", margin: 0 }
            task_choice := mod.widgets.PermissionDropDown { labels: ["New background task"] }
            task_details := mod.widgets.PermissionOptionLabel {}
            task_actions := View {
                visible: false
                width: Fill, height: Fit, flow: Flow.Right{wrap: true}, spacing: 8
                pause_resume := RobrixNeutralIconButton {
                    padding: 8, icon_walk: Walk{width: 0, height: 0, margin: 0}, text: "Pause"
                }
                run_now := RobrixNeutralIconButton {
                    padding: 8, icon_walk: Walk{width: 0, height: 0, margin: 0}, text: "Run now"
                }
                remove_task := RobrixNegativeIconButton {
                    padding: 8, icon_walk: Walk{width: 0, height: 0, margin: 0}, text: "Remove task"
                }
            }
            SubsectionLabel { text: "Task configuration", margin: 0 }
            mod.widgets.PermissionOptionLabel { text: "Mini-app" }
            app_choice := mod.widgets.PermissionDropDown { labels: ["Choose a background-capable mini-app"] }
            no_background_apps := mod.widgets.PermissionOptionLabel {
                visible: false
                text: "No installed mini-app opts into background tasks. Install or create one with background support first."
            }
            mod.widgets.PermissionOptionLabel { text: "Run in this context" }
            context_choice := mod.widgets.PermissionDropDown { labels: ["Choose a context"] }
            mod.widgets.PermissionOptionLabel {
                text: "Each app and context has one task. Select New background task to use a different app or context. Configure the app itself by opening it below; its saved settings and data stay in that context's private storage."
            }
            mod.widgets.PermissionOptionLabel { text: "Run when" }
            trigger_choice := mod.widgets.PermissionDropDown {
                labels: ["An interval passes", "A UTC alarm is due", "New room messages"]
            }
            interval_section := View {
                width: Fill, height: Fit, flow: Down, spacing: 5
                interval_value := RobrixTextInput { width: Fill, text: "5", empty_text: "Whole number" }
                interval_unit := mod.widgets.PermissionDropDown { labels: ["Seconds", "Minutes", "Hours"] }
                mod.widgets.PermissionOptionLabel { text: "Intervals must be at least one minute." }
            }
            alarm_section := View {
                visible: false
                width: Fill, height: Fit, flow: Down, spacing: 5
                alarm_value := RobrixTextInput { width: Fill, empty_text: "YYYY-MM-DD HH:MM:SS" }
                mod.widgets.PermissionOptionLabel {
                    text: "Enter a future UTC time. A missed alarm runs once after Robrix restarts; a completed alarm stays disabled until you save a new alarm. Run now fires and consumes this one-time alarm immediately."
                }
            }
            messages_section := mod.widgets.PermissionOptionLabel {
                visible: false
                text: "Choose one room for this trigger. The mini-app decides which new live messages match its condition and still needs read permission. History is not replayed; messages can be dropped while a run is busy. Run now supplies an empty message batch. Space-wide message streams are not supported."
            }
            changed_version := View {
                visible: false
                width: Fill, height: Fit, flow: Down, spacing: 5
                mod.widgets.PermissionOptionLabel {
                    text: "This task was enabled for an earlier app version. Its code or declared permissions changed. Review the current app before enabling this task again."
                }
                reviewed_version := CheckBox {
                    width: Fill, height: Fit
                    text: "I reviewed the current app version and want this task to run it"
                }
            }
            save_task := RobrixPositiveIconButton {
                padding: 10, icon_walk: Walk{width: 0, height: 0, margin: 0}, text: "Save and enable task"
            }
            View {
                width: Fill, height: Fit, flow: Flow.Right{wrap: true}, spacing: 8
                open_task_app := RobrixNeutralIconButton {
                    padding: 8, icon_walk: Walk{width: 0, height: 0, margin: 0}, text: "Open app to configure it"
                }
                task_permissions := RobrixNeutralIconButton {
                    padding: 8, icon_walk: Walk{width: 0, height: 0, margin: 0}, text: "Review app and permissions"
                }
                task_sharing := RobrixNeutralIconButton {
                    padding: 8, icon_walk: Walk{width: 0, height: 0, margin: 0}, text: "Review data sharing and actions"
                }
            }
            room_action_help := mod.widgets.PermissionOptionLabel {
                visible: false
                text: "To review a blocked action for a room task: open the app in its room pane, keep that pane open, return here and Run now, then review the current action in Data sharing and retry. Closing the instance ends its action approvals."
            }
            mod.widgets.PermissionOptionLabel {
                text: "Action approvals belong to a live app instance; they do not authorize a later restored run. Hidden workers can close after a run, so unattended tasks must not depend on an approval surviving that closure."
            }
            mod.widgets.PermissionOptionLabel {
                text: "Tasks restore saved app settings and data, not a snapshot of an app's memory. Pausing or removing stops current and future background work and closes this app instance. Save its settings first; saved data is retained. Force Stop in app details disables its tasks."
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
}

impl Widget for BackgroundTasks {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.handle_event(cx, event, scope);
        let Event::Actions(actions) = event else { return };
        if self.view.button(cx, ids!(refresh_tasks)).clicked(actions) {
            self.configure(cx);
        }
        if self.view.drop_down(cx, ids!(task_choice)).changed(actions).is_some() {
            self.view.widget(cx, ids!(task_error)).set_visible(cx, false);
            self.load_selected_task(cx);
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
            let result = self.selected_task(cx).map(|task| A2AppOp::RemoveBackgroundTask(task.job.id)).ok_or_else(|| "Select a saved task.".into());
            self.submit(cx, result);
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
    fn configure(&mut self, cx: &mut Cx) {
        self.account = super::information_flow::account().unwrap_or_default();
        self.pending_save = None;
        self.targets = if cx.has_global::<RoomsListRef>() { cx.get_global::<RoomsListRef>().permission_targets() }
            else { Vec::new() };
        let apps = with_a2app(|state| state.registry.iter().filter(|app| app.supports_background())
            .map(|app| (app.id.clone(), app.name.clone())).collect::<Vec<_>>()).unwrap_or_default();
        self.apps = apps.into_iter().map(|(id, name)| AppChoice {
            fingerprint: background::app_fingerprint(&id), id, name,
        }).collect();
        self.apps.sort_by_cached_key(|app| (app.name.to_lowercase(), app.id.clone()));
        self.view.drop_down(cx, ids!(app_choice)).set_labels(cx, std::iter::once("Choose a background-capable mini-app".into())
            .chain(self.apps.iter().map(|app| format!("{} ({})", app.name, app.id))).collect());
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
        self.view.drop_down(cx, ids!(task_choice)).set_labels(cx, std::iter::once("New background task".into()).chain(self.tasks.iter().map(|task| {
            let app = self.apps.iter().find(|app| app.id == task.job.binding.app_id).map(|app| app.name.as_str()).unwrap_or(&task.job.binding.app_id);
            format!("{app} · {} · {}", self.context_label(&task.job.binding.context), task_state_label(&task.job))
        })).collect());
        let saved = self.pending_save.take().and_then(|binding| self.tasks.iter().position(|task| task.job.binding == binding));
        let index = saved.or_else(|| selected.and_then(|id| self.tasks.iter().position(|task| task.job.id == id))).map(|index| index + 1).unwrap_or(0);
        self.view.drop_down(cx, ids!(task_choice)).set_selected_item(cx, index);
        self.revision = Some(background::revision());
        if saved.is_some() || (selected.is_some() && index == 0) { self.load_selected_task(cx); }
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
        let context = self.selected_context(cx).ok_or("Choose a context for this task.")?;
        if !context.available { return Err("The saved room or space is no longer available. Rejoin it or create a task in another context.".into()); }
        Ok(JobBinding { account: self.account.clone(), app_id: app.id.clone(), context: context.context.clone() })
    }

    fn select_existing_binding(&mut self, cx: &mut Cx) {
        let Ok(binding) = self.selected_binding(cx) else { return };
        if let Some(index) = self.tasks.iter().position(|task| task.job.binding == binding) {
            self.view.drop_down(cx, ids!(task_choice)).set_selected_item(cx, index + 1);
            self.load_selected_task(cx);
        }
    }

    fn load_selected_task(&mut self, cx: &mut Cx) {
        let selected = self.selected_task(cx).map(|task| task.job.clone());
        self.view.check_box(cx, ids!(reviewed_version)).set_active(cx, false, Animate::No);
        let app = selected.as_ref().and_then(|job| self.apps.iter().position(|app| app.id == job.binding.app_id)).map(|index| index + 1).unwrap_or(0);
        self.view.drop_down(cx, ids!(app_choice)).set_selected_item(cx, app);
        self.load_contexts(cx, selected.as_ref().map(|job| &job.binding.context));
        let (trigger, amount, units, alarm) = match selected.as_ref().map(|job| &job.trigger) {
            Some(Trigger::Interval { seconds }) if seconds % 3600 == 0 => (0, (seconds / 3600).to_string(), 2, String::new()),
            Some(Trigger::Interval { seconds }) if seconds % 60 == 0 => (0, (seconds / 60).to_string(), 1, String::new()),
            Some(Trigger::Interval { seconds }) => (0, seconds.to_string(), 0, String::new()),
            Some(Trigger::Alarm { unix_ms }) => (1, "5".into(), 1, utc_input(*unix_ms)),
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
        self.contexts = app.as_ref().and_then(|id| with_a2app(|state| {
            state.registry.get(id).map(|app| {
                let mut contexts = Vec::new();
                if app.can_run_background_in_context(None, false) {
                    contexts.push(ContextChoice { context: JobContext::Account, label: "Account context".into(), available: true });
                }
                for (id, name, space) in &self.targets {
                    if app.can_run_background_in_context(Some(id), *space) {
                        contexts.push(ContextChoice {
                            context: if *space { JobContext::Space { space_id: id.clone() } } else { JobContext::Room { room_id: id.clone() } },
                            label: format!("{}: {name} ({id})", if *space { "Space" } else { "Room" }), available: true,
                        });
                    }
                }
                contexts
            })
        }).flatten()).unwrap_or_default();
        if let Some(selected) = selected && !self.contexts.iter().any(|choice| &choice.context == selected) {
            self.contexts.push(ContextChoice { context: selected.clone(), label: format!("Unavailable: {}", self.context_label(selected)), available: false });
        }
        self.view.drop_down(cx, ids!(context_choice)).set_labels(cx, std::iter::once("Choose a context".into()).chain(self.contexts.iter().map(|choice| choice.label.clone())).collect());
        let index = selected.and_then(|selected| self.contexts.iter().position(|choice| &choice.context == selected)).map(|index| index + 1)
            .unwrap_or_else(|| usize::from(self.contexts.len() == 1));
        self.view.drop_down(cx, ids!(context_choice)).set_selected_item(cx, index);
    }

    fn context_label(&self, context: &JobContext) -> String {
        match context {
            JobContext::Account => "Account context".into(),
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
        self.view.widget(cx, ids!(task_actions)).set_visible(cx, selected.is_some());
        self.view.widget(cx, ids!(app_choice)).set_disabled(cx, selected.is_some());
        self.view.widget(cx, ids!(context_choice)).set_disabled(cx, selected.is_some());
        self.view.widget(cx, ids!(changed_version)).set_visible(cx, self.version_changed(cx));
        self.view.widget(cx, ids!(room_action_help)).set_visible(cx, self.selected_context(cx).is_some_and(|choice| matches!(choice.context, JobContext::Room { .. })));
        let details = selected.map(|task| {
            let next = task.job.next_due_ms.map(utc_label).unwrap_or_else(|| if matches!(task.job.trigger, Trigger::RoomMessages) && task.job.enabled { "Waiting for room messages".into() } else { "Not scheduled".into() });
            let last = task.job.last_run.as_ref().map(|run| format!("{} at {}", outcome_label(&run.outcome), utc_label(run.finished_ms))).unwrap_or_else(|| "No run recorded".into());
            format!("{}\n{}\nNext run: {next}\nLast run: {last}\n{}\n{}", self.context_label(&task.job.binding.context), trigger_label(&task.job.trigger), saved_status(&task.job), task.status)
        }).unwrap_or_else(|| "Choose an app, context and trigger, then explicitly save and enable the task. No task is created until you do.".into());
        self.view.label(cx, ids!(task_details)).set_text(cx, &details);
        let resume = selected.is_some_and(|task| !task_is_active(&task.job));
        self.view.button(cx, ids!(pause_resume)).set_text(cx, if resume { "Resume" } else { "Pause" });
        set_button_enabled(&self.view, cx, ids!(pause_resume), selected.is_some_and(|task|
            task_is_active(&task.job) || can_resume(task, now_ms())));
        set_button_enabled(&self.view, cx, ids!(run_now), selected.is_some_and(|task|
            task.job.enabled && task.job.in_flight.is_none() && task.current_fingerprint.as_ref() == Some(&task.job.fingerprint)));
        set_button_enabled(&self.view, cx, ids!(save_task), self.selected_app(cx).is_some() && selected.is_none_or(|task| task.job.in_flight.is_none()));
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
                return Err("The app changed after this page was loaded. Review the app, refresh tasks and app versions, then enable it.".into());
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
            let multiplier = match units { 0 => 1, 1 => 60, 2 => 3600, _ => return Err("Choose seconds, minutes or hours.".into()) };
            let seconds = amount.checked_mul(multiplier).ok_or("The interval is too large.")?;
            if seconds < MIN_INTERVAL_SECONDS { return Err(format!("Intervals must be at least {MIN_INTERVAL_SECONDS} seconds.")); }
            if seconds.checked_mul(1000).and_then(|millis| now_ms.checked_add(millis)).is_none() { return Err("The interval is too large.".into()); }
            Ok(Trigger::Interval { seconds })
        }
        1 => {
            let date = chrono::NaiveDateTime::parse_from_str(alarm.trim(), "%Y-%m-%d %H:%M:%S")
                .map_err(|_| "Enter a valid UTC time as YYYY-MM-DD HH:MM:SS.".to_string())?;
            let unix_ms = u64::try_from(date.and_utc().timestamp_millis()).map_err(|_| "Enter a future UTC alarm time.".to_string())?;
            if unix_ms <= now_ms { return Err("The alarm must be in the future. Enter a new UTC time.".into()); }
            Ok(Trigger::Alarm { unix_ms })
        }
        2 if matches!(context, JobContext::Room { .. }) => Ok(Trigger::RoomMessages),
        2 => Err("Message-triggered tasks require one room. Select New background task and choose a room context.".into()),
        _ => Err("Choose a trigger.".into()),
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|duration| duration.as_millis().min(u64::MAX as u128) as u64).unwrap_or(0)
}

fn utc_input(unix_ms: u64) -> String {
    i64::try_from(unix_ms).ok().and_then(chrono::DateTime::from_timestamp_millis)
        .map(|date| date.format("%Y-%m-%d %H:%M:%S").to_string()).unwrap_or_default()
}

fn utc_label(unix_ms: u64) -> String {
    let date = utc_input(unix_ms);
    if date.is_empty() { "Unavailable time".into() } else { format!("{date} UTC") }
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
        Some(PauseReason::AppChanged) => "Paused: app source or permissions changed. Review this version, refresh, confirm and save to enable it.",
        Some(PauseReason::AppMissing) => "Paused: the app is missing. Restore the app and review its version before enabling this task.",
        Some(PauseReason::TimedOut) => "Paused: the last run exceeded its time limit. Review the app's settings or code before resuming.",
        Some(PauseReason::HookUnavailable) => "Paused: the app could not handle background work. Review its settings or code before resuming.",
        None if job.enabled => "Enabled. Background runs use the existing permissions for this context.",
        None if matches!(job.trigger, Trigger::Alarm { .. }) => "Alarm disabled. Enter a future UTC alarm and save to schedule it again.",
        None => "Paused. Resume to restart this schedule with the existing app version.",
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
        Trigger::Interval { seconds } if seconds % 3600 == 0 => format!("Every {} hour(s)", seconds / 3600),
        Trigger::Interval { seconds } if seconds % 60 == 0 => format!("Every {} minute(s)", seconds / 60),
        Trigger::Interval { seconds } => format!("Every {seconds} seconds"),
        Trigger::Alarm { unix_ms } => format!("Alarm: {}", utc_label(*unix_ms)),
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
        for (amount, units, seconds) in [("60", 0, 60), ("5", 1, 300), ("2", 2, 7200)] {
            assert_eq!(parse_trigger(0, amount, units, "", &context, 1_000).unwrap(), Trigger::Interval { seconds });
        }
        for (amount, units, now) in [("59", 0, 0), ("0", 1, 0), ("1.5", 1, 0), ("-1", 1, 0), ("1", 3, 0), ("18446744073709551615", 2, 0), ("60", 0, u64::MAX)] {
            assert!(parse_trigger(0, amount, units, "", &context, now).is_err());
        }
    }

    #[test]
    fn validates_utc_alarms_and_exact_room_message_contexts() {
        let future = 1_800_000_000_000;
        assert_eq!(parse_trigger(1, "", 0, &utc_input(future), &JobContext::Account, future - 1).unwrap(), Trigger::Alarm { unix_ms: future });
        for alarm in [utc_input(future), "2026-02-30 12:00:00".into(), "2026-09-19 12:00:00 PDT".into(), "1960-01-01 00:00:00".into()] {
            assert!(parse_trigger(1, "", 0, &alarm, &JobContext::Account, future).is_err());
        }
        assert_eq!(parse_trigger(2, "", 0, "", &binding().context, 0).unwrap(), Trigger::RoomMessages);
        for context in [JobContext::Account, JobContext::Space { space_id: "!space:example.org".into() }] {
            assert!(parse_trigger(2, "", 0, "", &context, 0).is_err());
        }
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
        assert!(details.contains("Enter a future UTC alarm"));
        assert!(!editor.view.button(&cx, ids!(pause_resume)).borrow().unwrap().enabled());
        for reason in [PauseReason::AppChanged, PauseReason::AppMissing, PauseReason::TimedOut, PauseReason::HookUnavailable] {
            editor.tasks[0].job.pause_reason = Some(reason);
            editor.update_form(&mut cx);
            assert!(editor.view.label(&cx, ids!(task_details)).text().contains("Paused:"));
        }
    }
}
