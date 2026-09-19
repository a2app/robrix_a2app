use super::*;

pub(super) fn bullets(labels: &[String]) -> String {
    labels.iter().map(|label| format!("• {label}")).collect::<Vec<_>>().join("\n")
}

impl DataSharing {
    fn selected_context(&self, cx: &Cx) -> Result<&ContextId, String> {
        self.view.drop_down(cx, ids!(context_choice)).selected_item().checked_sub(1)
            .and_then(|index| self.snapshots.get(index))
            .map(|snapshot| &snapshot.context).ok_or_else(|| "Start an app or agent, then select its context.".into())
    }

    fn selected_decision(&self, cx: &Cx) -> Option<&FlowDecision> {
        self.view.drop_down(cx, ids!(decision_choice)).selected_item().checked_sub(1)
            .and_then(|index| self.decisions.get(index))
    }

    fn selected_action(&self, cx: &Cx) -> Option<&ActionDecision> {
        self.view.drop_down(cx, ids!(action_choice)).selected_item().checked_sub(1)
            .and_then(|index| self.action_decisions.get(index))
    }

    pub(super) fn selected_reader(&self, cx: &Cx) -> Result<ReaderScope, String> {
        match self.view.drop_down(cx, ids!(reader_kind)).selected_item() {
            0 => Ok(ReaderScope::Context(self.selected_context(cx)?.clone())),
            1 => self.apps.get(self.view.drop_down(cx, ids!(reader_app)).selected_item())
                .map(|(app, _)| ReaderScope::App { account: self.account.clone(), app: app.clone() })
                .ok_or_else(|| "Select an installed mini-app.".into()),
            2 if !self.account.is_empty() => Ok(ReaderScope::AllReaders),
            _ => Err("Sign in and choose who may share this source.".into()),
        }
    }

    fn selected_duration(&self, cx: &Cx) -> Result<SharingDuration, String> {
        match self.view.drop_down(cx, ids!(sharing_lifetime)).selected_item() {
            0 => Ok(SharingDuration::RobrixSession),
            1 => self.rooms.get(self.view.drop_down(cx, ids!(lifetime_room)).selected_item())
                .map(|(room, _)| SharingDuration::RoomSession { account: self.account.clone(), room: room.clone() })
                .ok_or_else(|| "Select the room whose close will end this rule.".into()),
            2 => Ok(SharingDuration::Permanent),
            _ => Err("Select how long the sharing rule lasts.".into()),
        }
    }

    pub(super) fn sharing_action(&self, cx: &Cx) -> Result<A2AppOp, String> {
        let source = self.selected_source(cx)?;
        let source_account = match &source {
            Source::Account { account } | Source::Room { account, .. } => account,
            Source::UnknownPrivate => return Err("Unknown private sources cannot be released.".into()),
        };
        if source_account != &self.account { return Err("Sign in to the source account to change its sharing rules.".into()); }
        Ok(A2AppOp::GrantFlowSharing {
            source, recipient: self.selected_recipient(cx)?, reader: self.selected_reader(cx)?, duration: self.selected_duration(cx)?,
        })
    }

    pub(super) fn authority_action(&self, cx: &Cx) -> Result<A2AppOp, String> {
        let decision = self.selected_action(cx)
            .ok_or("Select a blocked sensitive action to review.")?;
        let snapshot = self.snapshots.iter().find(|snapshot| snapshot.context == decision.context && snapshot.epoch == decision.epoch)
            .ok_or("This context is no longer active. Restart it and review the new request.")?;
        let session = match self.view.drop_down(cx, ids!(action_session)).selected_item() {
            0 => AuthoritySession::RoomSession {
                account: decision.context.account().to_string(),
                room: decision.context.room().ok_or("This context has no room. Choose Until Robrix closes.")?.to_string(),
            },
            1 => AuthoritySession::RobrixSession,
            _ => return Err("Select a session for this action permission.".into()),
        };
        Ok(A2AppOp::GrantFlowAuthority {
            context: decision.context.clone(), action: decision.action.clone(), session,
            expected_influences: snapshot.influences.clone(), expected_epoch: snapshot.epoch,
        })
    }

    pub(super) fn refresh_diagnostics(&mut self, cx: &mut Cx) {
        let snapshots = flow::contexts().unwrap_or_default().into_iter()
            .filter(|snapshot| snapshot.context.account() == self.account).collect::<Vec<_>>();
        let previous = self.selected_context(cx).ok().cloned();
        if self.snapshots != snapshots {
            let labels = std::iter::once("Select an app or agent context".into())
                .chain(snapshots.iter().map(|snapshot| self.context_label(&snapshot.context))).collect();
            self.view.drop_down(cx, ids!(context_choice)).set_labels(cx, labels);
            let index = previous.as_ref().and_then(|context| snapshots.iter().position(|snapshot| &snapshot.context == context)).map(|index| index + 1).unwrap_or(0);
            self.snapshots = snapshots;
            self.view.drop_down(cx, ids!(context_choice)).set_selected_item(cx, index);
        }
        let decisions = flow::recent_decisions().unwrap_or_default().into_iter().rev()
            .filter(|decision| decision.context.account() == self.account).collect::<Vec<_>>();
        if self.decisions != decisions {
            let selected = self.selected_decision(cx);
            let index = selected.and_then(|selected| decisions.iter().position(|decision| decision == selected)).map(|index| index + 1).unwrap_or(0);
            let labels = std::iter::once("Select a sharing decision".into()).chain(decisions.iter().map(|decision| format!("{} · {} → {}",
                if decision.allowed { "Allowed" } else { "Blocked" }, self.context_label(&decision.context), self.recipient_label(&decision.recipient)))).collect();
            self.decisions = decisions;
            self.view.drop_down(cx, ids!(decision_choice)).set_labels(cx, labels);
            self.view.drop_down(cx, ids!(decision_choice)).set_selected_item(cx, index);
        }
        let decisions = flow::recent_action_decisions().unwrap_or_default().into_iter().rev()
            .filter(|decision| !decision.allowed && decision.context.account() == self.account).collect::<Vec<_>>();
        if self.action_decisions != decisions {
            let selected = self.selected_action(cx);
            let index = selected.and_then(|selected| decisions.iter().position(|decision| decision == selected)).map(|index| index + 1).unwrap_or(0);
            let labels = std::iter::once("Select a blocked sensitive action".into()).chain(decisions.iter().map(|decision| format!("{} · {} → {}",
                self.context_label(&decision.context), decision.action.kind, decision.action.target))).collect();
            self.action_decisions = decisions;
            self.view.drop_down(cx, ids!(action_choice)).set_labels(cx, labels);
            self.view.drop_down(cx, ids!(action_choice)).set_selected_item(cx, index);
        }
        let authorities = flow::authorities().unwrap_or_default().into_iter()
            .filter(|grant| grant.context.account() == self.account).collect::<Vec<_>>();
        if self.authorities != authorities {
            let labels = authorities.iter().map(|grant| format!("{} · {} → {} · {}",
                self.context_label(&grant.context), grant.action.kind, grant.action.target,
                match &grant.session {
                    AuthoritySession::RobrixSession => "Until Robrix closes".into(),
                    AuthoritySession::RoomSession { room, .. } => format!("Until {} closes", self.room_label(room)),
                })).collect::<Vec<_>>();
            self.view.label(cx, ids!(authority_rules)).set_text(cx, &if labels.is_empty() { "No session action permissions.".into() } else { bullets(&labels) });
            self.view.drop_down(cx, ids!(authority_remove_choice)).set_labels(cx, labels);
            self.view.drop_down(cx, ids!(authority_remove_choice)).set_selected_item(cx, 0);
            self.authorities = authorities;
        }
        self.view.widget(cx, ids!(authority_remove_section)).set_visible(cx, !self.authorities.is_empty());
        self.update_diagnostic_details(cx);
    }

    pub(super) fn update_diagnostic_details(&mut self, cx: &mut Cx) {
        let snapshot = self.selected_context(cx).ok().and_then(|context| self.snapshots.iter().find(|snapshot| &snapshot.context == context));
        let details = match snapshot {
            Some(snapshot) => {
                let sources = snapshot.label.iter().map(|source| self.source_label(source)).collect::<Vec<_>>();
                let influences = snapshot.influences.iter().map(|influence| self.influence_label(influence)).collect::<Vec<_>>();
                let clearance = if snapshot.clearance.is_some() { "Public context: private inputs are blocked." } else { "Sources accumulate before private inputs are delivered." };
                format!("{}\n{clearance}\nSources:\n{}\nOutside influences:\n{}",
                    self.context_label(&snapshot.context), if sources.is_empty() { "None".into() } else { bullets(&sources) },
                    if influences.is_empty() { "None".into() } else { bullets(&influences) })
            }
            None => "Select an active context for this account to inspect its sources. Start a mini-app or agent if none is available.".into(),
        };
        self.view.label(cx, ids!(context_details)).set_text(cx, &details);
        let decision = self.selected_decision(cx);
        let details = decision.map(|decision| {
            let blocked = decision.denied_sources.iter().map(|source| self.source_label(source)).collect::<Vec<_>>();
            format!("{}\nActual recipient: {}\n{}",
                self.context_label(&decision.context), self.recipient_label(&decision.recipient),
                if decision.allowed { "Every source allowed this recipient when checked.".into() }
                else { format!("Blocked sources:\n{}", bullets(&blocked)) })
        }).unwrap_or_else(|| "No recent sharing decisions for this account.".into());
        self.view.label(cx, ids!(decision_details)).set_text(cx, &details);
        let blocked = decision.map(|decision| decision.denied_sources.iter().cloned().collect::<Vec<_>>()).unwrap_or_default();
        if blocked != self.blocked_sources {
            let labels = blocked.iter().map(|source| self.source_label(source)).collect();
            self.blocked_sources = blocked;
            self.view.drop_down(cx, ids!(blocked_source_choice)).set_labels(cx, labels);
            self.view.drop_down(cx, ids!(blocked_source_choice)).set_selected_item(cx, 0);
        }
        self.view.widget(cx, ids!(review_section)).set_visible(cx, !self.blocked_sources.is_empty());
        let action = self.selected_action(cx);
        let details = action.map(|decision| {
            let snapshot = self.snapshots.iter().find(|snapshot| snapshot.context == decision.context && snapshot.epoch == decision.epoch);
            let influences = snapshot.map(|snapshot| &snapshot.influences).unwrap_or(&decision.influences);
            let labels = influences.iter().map(|influence| self.influence_label(influence)).collect::<Vec<_>>();
            let warning = if decision.action.kind.starts_with("network.") {
                "This HTTP method permission covers every path on this exact origin. Internet and source sharing rules still apply."
            } else { "Only this action and target are authorized. Room permissions and source sharing rules still apply." };
            let activation = if snapshot.is_some() { "Current activation" } else { "Closed activation: this request can no longer be approved" };
            format!("{}\n{activation}\nAction: {}\nTarget: {}\n{warning}\nCurrent influences to review:\n{}",
                self.context_label(&decision.context), decision.action.kind, decision.action.target, bullets(&labels))
        }).unwrap_or_else(|| "No blocked sensitive actions for this account.".into());
        self.view.label(cx, ids!(action_details)).set_text(cx, &details);
        self.view.widget(cx, ids!(authority_button)).set_visible(cx, action.is_some());
        self.view.widget(cx, ids!(action_session)).set_visible(cx, action.is_some());
    }

    pub(super) fn review_decision(&mut self, cx: &mut Cx) -> Result<(), String> {
        let decision = self.selected_decision(cx)
            .ok_or("Select a blocked sharing decision.")?.clone();
        let source = self.blocked_sources.get(self.view.drop_down(cx, ids!(blocked_source_choice)).selected_item())
            .ok_or("Select one blocked source to review.")?.clone();
        let index = if let Some(index) = self.sources.iter().position(|(candidate, _)| candidate == &source) { index }
        else {
            self.sources.push((source.clone(), self.source_label(&source)));
            self.view.drop_down(cx, ids!(source_choice)).set_labels(cx, self.sources.iter().map(|(_, label)| label.clone()).collect());
            self.sources.len() - 1
        };
        self.view.drop_down(cx, ids!(source_choice)).set_selected_item(cx, index);
        if let Some(index) = self.snapshots.iter().position(|snapshot| snapshot.context == decision.context && snapshot.epoch == decision.epoch) {
            self.view.drop_down(cx, ids!(context_choice)).set_selected_item(cx, index + 1);
        } else { return Err("This context is no longer active. Its sources remain protected; restart it before adding a context rule.".into()); }
        self.view.drop_down(cx, ids!(reader_kind)).set_selected_item(cx, 0);
        self.view.drop_down(cx, ids!(sharing_lifetime)).set_selected_item(cx, 0);
        match &decision.recipient {
            Recipient::NetworkOrigin(origin) => {
                self.view.drop_down(cx, ids!(recipient_kind)).set_selected_item(cx, 1);
                self.view.text_input(cx, ids!(network_url)).set_text(cx, origin);
            }
            Recipient::ModelProvider(id) if self.model.as_ref().is_some_and(|model| &model.id == id) => {
                self.view.drop_down(cx, ids!(recipient_kind)).set_selected_item(cx, 0);
            }
            Recipient::ModelProvider(_) => return Err("This request used a different model configuration. Restart the agent with the currently configured service before approving it.".into()),
            Recipient::MatrixRoom { account, room } => {
                if account != &self.account { return Err("The recipient belongs to a different account.".into()); }
                let index = self.rooms.iter().position(|(id, _)| id == room).ok_or("The destination room is no longer joined.")?;
                self.view.drop_down(cx, ids!(recipient_kind)).set_selected_item(cx, 2);
                self.view.drop_down(cx, ids!(target_room)).set_selected_item(cx, index);
            }
            Recipient::External => return Err("Unrestricted external sharing cannot be approved here.".into()),
        }
        self.update_recipient_form(cx);
        self.update_diagnostic_details(cx);
        Ok(())
    }

    fn room_label(&self, room: &str) -> String {
        self.rooms.iter().find(|(id, _)| id == room).map(|(_, name)| format!("{name} ({room})")).unwrap_or_else(|| room.to_owned())
    }

    fn app_label(&self, app: &str) -> String {
        self.apps.iter().find(|(id, _)| id == app).map(|(_, name)| format!("{name} ({app})")).unwrap_or_else(|| app.to_owned())
    }

    fn context_label(&self, context: &ContextId) -> String {
        match context {
            ContextId::App { account, app, room } => format!("Mini-app {} · {} · {account}", self.app_label(app), room.as_deref().map(|room| self.room_label(room)).unwrap_or_else(|| "Account context".into())),
            ContextId::PublicApp { account, app } => format!("Public mini-app {} · {account}", self.app_label(app)),
            ContextId::Agent { account, room } => format!("Agent · {} · {account}", self.room_label(room)),
        }
    }

    fn source_label(&self, source: &Source) -> String {
        match source {
            Source::Account { account } => format!("Account data · {account}"),
            Source::Room { account, room } => format!("Room · {} · {account}", self.room_label(room)),
            Source::UnknownPrivate => "Unknown private sources (cannot be released)".into(),
        }
    }

    fn influence_label(&self, influence: &Influence) -> String {
        match influence {
            Influence::RoomContent { account, room } => format!("Room content · {} · {account}", self.room_label(room)),
            Influence::InternetOrigin(origin) => format!("Website content · {origin}"),
            Influence::MiniApp { account, app } => format!("Mini-app content · {} · {account}", self.app_label(app)),
            Influence::Model(id) => self.recipient_label(&Recipient::ModelProvider(id.clone())),
            Influence::Unknown => "Unknown stored influences".into(),
        }
    }

    pub(super) fn reader_label(&self, reader: &ReaderScope) -> String {
        match reader {
            ReaderScope::AllReaders => "All mini-apps and agents".into(),
            ReaderScope::App { account, app } => format!("Mini-app {} in all contexts · {account}", self.app_label(app)),
            ReaderScope::Context(context) => self.context_label(context),
        }
    }

    pub(super) fn duration_label(&self, duration: &SharingDuration) -> String {
        match duration {
            SharingDuration::Permanent => "Always (until removed)".into(),
            SharingDuration::RobrixSession => "Until Robrix closes".into(),
            SharingDuration::RoomSession { account, room } => format!("Until {} closes · {account}", self.room_label(room)),
        }
    }
}
