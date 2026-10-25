use super::*;
use super::diagnostics::{allowed_now_text, is_allowed_now, no_longer_held_text, remaining_denials};
use a2app_core::{information_flow::Label, protection_audit::{self, ActivityKind, ActivityOutcome}};

impl DataSharing {
    pub(super) fn sharing_remedy(&self, blocked: &Label, recipient: &Recipient) -> String {
        if blocked.contains(&Source::UnknownPrivate) {
            return "Robrix cannot identify where this old data or app source came from. No sharing rule can release it. For a public task, use an unchanged built-in app with separately tracked public data.".into();
        }
        if *recipient == Recipient::External {
            return "The app has not identified a destination for this data. It must choose a specific website or room before you can allow sharing.".into();
        }
        let mut text = if blocked.len() == 1 {
            "Sharing needs your approval. Choose Review sharing rule, check the destination and duration, then choose Allow sharing. Return to the app and try again."
        } else {
            "Sharing rules need your approval. Choose Allow all listed data, or select one data source and choose Review sharing rule. Then return to the app and try again."
        }.to_string();
        if blocked.iter().any(|source| match source { Source::Room { account, .. } | Source::Account { account } | Source::RoomDirectory { account } => account != &self.account, Source::UnknownPrivate => false }) {
            text.push_str("\nSome data belongs to another account. Sign in to that account to review its sharing permission.");
        }
        if let Recipient::ModelProvider(id) = recipient {
            if self.model.is_none() {
                text.push('\n');
                text.push_str(&self.model_unavailable_text());
            } else if self.model.as_ref().is_some_and(|model| &model.id != id) {
                text.push_str("\nYour AI service has changed. Restart the agent and review its new request.");
            }
        }
        text
    }

    /// A blocked History record as a request Needs attention can review: the data today's rules still
    /// don't allow, even data its context no longer holds, so it can be allowed ahead of time.
    fn history_request(&self, entry: &protection_audit::Activity) -> Option<FlowDecision> {
        if entry.kind != ActivityKind::SharingCheck || entry.outcome != ActivityOutcome::Blocked
            || entry.blocked_sources.contains(&Source::UnknownPrivate) || entry.recipient == Some(Recipient::External)
        { return None; }
        let (context, recipient) = (entry.context.clone()?, entry.recipient.clone()?);
        if context.account() != self.account || !self.snapshots.iter().any(|snapshot| snapshot.context == context) { return None; }
        let mut request = FlowDecision { context, epoch: 0, recipient, sources: entry.sources.clone(), denied_sources: Label::new(), allowed: false };
        request.denied_sources = remaining_denials(&entry.blocked_sources, None, |source| is_allowed_now(source, &request)).0;
        request.allowed = request.denied_sources.is_empty();
        Some(request)
    }

    /// Whether the request's context no longer holds any of its blocked data.
    fn is_no_longer_held(&self, request: &FlowDecision) -> bool {
        self.snapshots.iter().find(|snapshot| snapshot.context == request.context)
            .is_some_and(|snapshot| !request.denied_sources.iter().any(|source| snapshot.label.contains(source)))
    }

    pub(super) fn selected_history_request(&self) -> Option<FlowDecision> {
        let entry = self.activities.iter().find(|entry| Some(entry.id) == self.activity_selection)?;
        self.history_request(entry).filter(|request| !request.allowed)
    }

    /// Opens a History record in Needs attention, which lists it until it is resolved.
    pub(super) fn review_history_request(&mut self, cx: &mut Cx, request: FlowDecision) {
        match self.recalled_decisions.iter_mut().find(|recalled| is_decision_for(recalled, &decision_key(&request))) {
            Some(recalled) => recalled.denied_sources.extend(request.denied_sources.iter().cloned()),
            None => self.recalled_decisions.push(request.clone()),
        }
        self.activity_selection = None;
        self.view.permission_choices(cx, ids!(page_choice)).set_selected_item(cx, 1);
        self.refresh_diagnostics(cx);
        self.open_card(cx, &SharingCardTarget::Decision(request));
        self.update_page(cx);
    }

    /// History has no review controls, so its advice points to Needs attention.
    fn history_remedy(&self, entry: &protection_audit::Activity) -> String {
        let Some(recipient) = entry.recipient.as_ref().filter(|_| !entry.blocked_sources.is_empty()) else { return String::new() };
        // These explanations name no controls.
        if entry.blocked_sources.contains(&Source::UnknownPrivate) || *recipient == Recipient::External {
            return self.sharing_remedy(&entry.blocked_sources, recipient);
        }
        match self.history_request(entry) {
            Some(request) if request.allowed => allowed_now_text(&request.context).into(),
            Some(request) if self.is_no_longer_held(&request) =>
                format!("{} To allow it now, choose Review in Needs attention.", no_longer_held_text(&request.context)),
            Some(_) => "To allow this sharing, choose Review in Needs attention.".into(),
            None => "This request can't be reviewed from here. Run the app or send your message again; if it is blocked again, it appears in Needs attention.".into(),
        }
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
        self.view.widget(cx, ids!(history_more)).set_visible(cx, self.activities.len() > 20);
        if self.activities.len() <= 20 && self.history_expanded {
            self.history_expanded = false;
            self.view.collapsible_header(cx, ids!(history_more)).set_expanded(cx, false, Animate::No);
        }
        self.view.widget(cx, ids!(history_more_section)).set_visible(cx, self.activities.len() > 20 && self.history_expanded);
        self.view.widget(cx, ids!(history_overview)).set_visible(cx, self.activity_selection.is_none());
        self.view.widget(cx, ids!(history_details)).set_visible(cx, self.activity_selection.is_some());
        let entry = self.activity_selection.and_then(|id| self.activities.iter().find(|entry| entry.id == id));
        let details = entry.map(|entry| {
            let context = entry.context.as_ref().map(|context| self.context_label(context)).unwrap_or_else(|| "Account protection settings".into());
            let recipient = entry.recipient.as_ref().map(|recipient| self.recipient_label(recipient)).unwrap_or_else(|| "No external recipient".into());
            let sources = entry.sources.iter().map(|source| self.source_label(source)).collect::<Vec<_>>();
            let remedy = self.history_remedy(entry);
            format!("{}\n{context}\n{}\n{}\nRecipient: {recipient}\nSources:\n{}{}",
                activity_time(entry.timestamp_ms), activity_kind(entry.kind), entry.outcome.label(),
                if sources.is_empty() { "None recorded".into() } else { bullets(&sources) },
                if remedy.is_empty() { remedy } else { format!("\n\n{remedy}") })
        }).unwrap_or_else(|| "This activity record is no longer available.".into());
        self.view.label(cx, ids!(activity_details)).set_text(cx, &details);
        let is_reviewable = self.selected_history_request().is_some();
        self.view.widget(cx, ids!(history_review)).set_visible(cx, is_reviewable);
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
