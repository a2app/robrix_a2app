use super::*;
use a2app_core::{information_flow::Label, protection_audit::{self, ActivityKind, ActivityOutcome}};

impl DataSharing {
    pub(super) fn sharing_remedy(&self, blocked: &Label, recipient: &Recipient) -> String {
        if blocked.contains(&Source::UnknownPrivate) {
            return "Blocked control: retained data has unknown private sources. No sharing toggle can release it. Use a separate public instance with no private input, or a new app with known sources. Clearing storage does not erase retained source labels.".into();
        }
        if *recipient == Recipient::External {
            return "Blocked control: this operation exports to an uncontrolled recipient. It cannot be approved with an internet or room permission. Use an operation with an explicit website origin or Matrix room, then review that destination's sharing rule.".into();
        }
        let mut text = "Blocked by: Sharing rules. This app or agent did not have permission to send data from these sources to this destination. Every blocked source needs its own rule.\n\nIn Needs attention, choose the blocked data source and select Review sharing rule. Check who can share it, the destination and duration before selecting Allow sharing. Repeat for each blocked source. Room and internet permissions do not replace sharing permission. Retry the operation after changing a rule; the historical block remains in History.".to_string();
        if blocked.iter().any(|source| match source { Source::Room { account, .. } | Source::Account { account } => account != &self.account, Source::UnknownPrivate => false }) {
            text.push_str("\nThis also includes another account's source. Sign in to that source account to review its rules; the current account cannot authorize it.");
        }
        if let Recipient::ModelProvider(id) = recipient
            && self.model.as_ref().is_none_or(|model| &model.id != id)
        {
            text.push_str("\nThis model recipient differs from the current AI provider configuration. Restart the agent with the intended provider and review its new recipient. An old approval cannot follow changed credentials, model or endpoint.");
        }
        text
    }

    pub(super) fn refresh_activity(&mut self, cx: &mut Cx) {
        let filter = self.view.permission_choices(cx, ids!(activity_filter)).selected_item();
        let key = (protection_audit::revision(), filter, None, None, None);
        if self.activity_key.as_ref() != Some(&key) {
            let (entries, warning) = protection_audit::recent(&self.account)
                .unwrap_or_else(|error| (Vec::new(), Some(error)));
            self.activities = entries.into_iter().filter(|entry| match filter {
                0 => true,
                1 => entry.outcome == ActivityOutcome::Blocked,
                2 => matches!(entry.kind, ActivityKind::ModelRequest | ActivityKind::HttpRequest | ActivityKind::MatrixOperation | ActivityKind::ToolCall),
                3 => entry.kind == ActivityKind::PolicyChange,
                _ => false,
            }).collect();
            if self.activity_selection.is_some_and(|id| !self.activities.iter().any(|entry| entry.id == id)) {
                self.activity_selection = None;
            }
            self.view.widget(cx, ids!(activity_warning)).set_visible(cx, warning.is_some());
            self.view.label(cx, ids!(activity_warning)).set_text(cx, warning.as_deref().unwrap_or_default());
            self.activity_key = Some(key);
        }
        self.view.label(cx, ids!(activity_summary)).set_text(cx, if self.activities.is_empty() {
            "No activity matches this filter."
        } else { "Choose a record to see its data sources, destination and result." });
        self.view.widget(cx, ids!(history_more)).set_visible(cx, self.activities.len() > self.history_limit);
        self.view.widget(cx, ids!(history_overview)).set_visible(cx, self.activity_selection.is_none());
        self.view.widget(cx, ids!(history_details)).set_visible(cx, self.activity_selection.is_some());
        let entry = self.activity_selection.and_then(|id| self.activities.iter().find(|entry| entry.id == id));
        let details = entry.map(|entry| {
            let context = entry.context.as_ref().map(|context| self.context_label(context)).unwrap_or_else(|| "Account protection settings".into());
            let recipient = entry.recipient.as_ref().map(|recipient| self.recipient_label(recipient)).unwrap_or_else(|| "No external recipient".into());
            let sources = entry.sources.iter().map(|source| self.source_label(source)).collect::<Vec<_>>();
            let remedy = if !entry.blocked_sources.is_empty() {
                entry.recipient.as_ref().map(|recipient| format!("\n\n{}", self.sharing_remedy(&entry.blocked_sources, recipient))).unwrap_or_default()
            } else { String::new() };
            format!("{}\n{context}\n{}\n{}\nRecipient: {recipient}\nSources:\n{}{remedy}",
                activity_time(entry.timestamp_ms), activity_kind(entry.kind), entry.outcome.label(),
                if sources.is_empty() { "None recorded".into() } else { bullets(&sources) })
        }).unwrap_or_else(|| "This activity record is no longer available.".into());
        self.view.label(cx, ids!(activity_details)).set_text(cx, &details);
    }
}

pub(super) fn activity_time(timestamp_ms: u64) -> String {
    chrono::DateTime::from_timestamp_millis(timestamp_ms.min(i64::MAX as u64) as i64)
        .map(|time| time.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S %Z").to_string())
        .unwrap_or_else(|| "Unknown time".into())
}

pub(super) fn activity_kind(kind: ActivityKind) -> &'static str {
    match kind {
        ActivityKind::SharingCheck => "Sharing policy check",
        ActivityKind::ModelRequest => "AI model request",
        ActivityKind::HttpRequest => "Website request",
        ActivityKind::MatrixOperation => "Matrix operation",
        ActivityKind::ToolCall => "Tool call",
        ActivityKind::PolicyChange => "Protection change",
    }
}

pub(super) fn activity_outcome(outcome: ActivityOutcome) -> &'static str {
    match outcome {
        ActivityOutcome::Allowed => "Allowed",
        ActivityOutcome::Blocked => "Blocked",
        ActivityOutcome::Started => "Started",
        ActivityOutcome::Completed => "Completed",
        ActivityOutcome::Failed => "Failed",
        ActivityOutcome::Interrupted => "Interrupted",
        ActivityOutcome::Changed => "Changed",
    }
}
