use super::*;
use a2app_core::{information_flow::Label, protection_audit::{self, ActivityKind}};

impl DataSharing {
    pub(super) fn sharing_remedy(&self, blocked: &Label, recipient: &Recipient) -> String {
        if blocked.contains(&Source::UnknownPrivate) {
            return "Blocked control: retained data has unknown private sources. No sharing toggle can release it. Use a separate public instance with no private input, or a new app with known sources. Clearing storage does not erase retained source labels.".into();
        }
        if *recipient == Recipient::External {
            return "Blocked control: this operation exports to an uncontrolled recipient. It cannot be approved with an internet or room permission. Use an operation with an explicit website origin or Matrix room, then review that destination's sharing rule.".into();
        }
        let mut text = "Blocked control: Private data sharing. At this check, no matching source-to-recipient allowance covered this app or agent. Every blocked source needs its own rule.\n\nTo change it: select a blocked source above and press Review this source and recipient below. Check Data source, Who may share this source, Allowed recipient and lifetime, then press Allow sharing with this recipient. Repeat for each blocked source. Room read/write and internet permissions do not substitute for these rules. A historical block remains in this history after settings change; retry the operation to check the new rules.".to_string();
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
        let filter = self.view.drop_down(cx, ids!(activity_filter)).selected_item();
        let context = self.view.drop_down(cx, ids!(context_choice)).selected_item().checked_sub(1)
            .and_then(|index| self.snapshots.get(index)).map(|snapshot| snapshot.context.clone());
        let source = self.selected_source(cx).ok();
        let recipient = self.selected_recipient(cx).ok();
        let key = (protection_audit::revision(), filter, context.clone(), source.clone(), recipient.clone());
        if self.activity_key.as_ref() != Some(&key) {
            let selected = self.view.drop_down(cx, ids!(activity_choice)).selected_item().checked_sub(1)
                .and_then(|index| self.activities.get(index)).map(|entry| entry.id);
            let (entries, warning) = protection_audit::recent(&self.account)
                .unwrap_or_else(|error| (Vec::new(), Some(error)));
            self.activities = entries.into_iter().filter(|entry| match filter {
                0 => true,
                1 => context.is_some() && entry.context == context,
                2 => source.as_ref().is_some_and(|source| entry.sources.contains(source)),
                3 => recipient.is_some() && entry.recipient == recipient,
                _ => false,
            }).collect();
            let labels = std::iter::once("Select an activity record".into()).chain(self.activities.iter().map(|entry|
                format!("{} · {} · {}", activity_time(entry.timestamp_ms), activity_kind(entry.kind), entry.outcome.label()))).collect();
            self.view.drop_down(cx, ids!(activity_choice)).set_labels(cx, labels);
            let index = selected.and_then(|id| self.activities.iter().position(|entry| entry.id == id)).map(|index| index + 1).unwrap_or(0);
            self.view.drop_down(cx, ids!(activity_choice)).set_selected_item(cx, index);
            self.view.widget(cx, ids!(activity_warning)).set_visible(cx, warning.is_some());
            self.view.label(cx, ids!(activity_warning)).set_text(cx, warning.as_deref().unwrap_or_default());
            self.activity_key = Some(key);
        }
        let entry = self.view.drop_down(cx, ids!(activity_choice)).selected_item().checked_sub(1).and_then(|index| self.activities.get(index));
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
        }).unwrap_or_else(|| "Select a record to inspect its source identities, actual recipient and outcome. History is bounded; older records expire. Filter selections refer to the controls on this page.".into());
        self.view.label(cx, ids!(activity_details)).set_text(cx, &details);
    }
}

fn activity_time(timestamp_ms: u64) -> String {
    chrono::DateTime::from_timestamp_millis(timestamp_ms.min(i64::MAX as u64) as i64)
        .map(|time| time.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S %Z").to_string())
        .unwrap_or_else(|| "Unknown time".into())
}

fn activity_kind(kind: ActivityKind) -> &'static str {
    match kind {
        ActivityKind::SharingCheck => "Sharing policy check",
        ActivityKind::ModelRequest => "AI model request",
        ActivityKind::HttpRequest => "Website request",
        ActivityKind::MatrixOperation => "Matrix operation",
        ActivityKind::ToolCall => "Tool call",
        ActivityKind::PolicyChange => "Protection change",
    }
}
