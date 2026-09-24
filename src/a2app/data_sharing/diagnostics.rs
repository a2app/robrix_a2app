use super::*;

pub(super) fn bullets(labels: &[String]) -> String {
    labels.iter().map(|label| format!("• {label}")).collect::<Vec<_>>().join("\n")
}

pub(super) fn numbered(labels: &[String]) -> String {
    labels.iter().enumerate().map(|(index, label)| format!("{}. {label}", index + 1)).collect::<Vec<_>>().join("\n")
}

fn review_payload(payload: &str) -> String {
    // Render direction-changing/invisible controls explicitly. Literal JSON
    // backslashes are already escaped, so a user can distinguish the contents.
    let mut displayed = String::with_capacity(payload.len());
    for character in payload.chars() {
        if character.is_control() || matches!(character, '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}') {
            use std::fmt::Write;
            let _ = write!(displayed, "\\u{:04x}", character as u32);
        } else { displayed.push(character); }
    }
    displayed
}

#[cfg(test)]
#[test]
fn exact_review_exposes_direction_controls_without_confusing_literal_escapes() {
    let raw = serde_json::json!({"body": "invoice\u{202e}exe"}).to_string();
    let literal = serde_json::json!({"body": "invoice\\u202eexe"}).to_string();
    assert!(!review_payload(&raw).contains('\u{202e}'));
    assert!(review_payload(&raw).contains("\\u202e"));
    assert_ne!(review_payload(&raw), review_payload(&literal));
}

impl DataSharing {
    fn selected_context(&self, cx: &Cx) -> Result<&ContextId, String> {
        self.view.drop_down(cx, ids!(context_choice)).selected_item().checked_sub(1)
            .and_then(|index| self.snapshots.get(index))
            .map(|snapshot| &snapshot.context).ok_or_else(|| "Start a mini-app or agent, then choose it above.".into())
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
            .ok_or("Choose a blocked action to review.")?;
        let snapshot = self.snapshots.iter().find(|snapshot| snapshot.context == decision.context && snapshot.epoch == decision.epoch)
            .ok_or("This context is no longer active. Restart it and review the new request.")?;
        let session = match self.view.drop_down(cx, ids!(action_session)).selected_item() {
            0 => {
                let request = decision.request.as_ref().ok_or("This operation has no exact contents to review. Retry it to capture a request, or explicitly choose a session permission.")?;
                return Ok(A2AppOp::GrantExactFlowAuthority {
                    context: decision.context.clone(), request_id: request.id,
                    expected_influences: snapshot.influences.clone(), expected_epoch: snapshot.epoch,
                });
            }
            1 => AuthoritySession::RoomSession {
                account: decision.context.account().to_string(),
                room: decision.context.room().ok_or("This context has no room. Choose Until Robrix closes.")?.to_string(),
            },
            2 => AuthoritySession::RobrixSession,
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
            let labels = std::iter::once("Choose a running mini-app or agent".into())
                .chain(snapshots.iter().map(|snapshot| self.context_choice_label(&snapshot.context))).collect();
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
            let labels = std::iter::once("Choose a sharing decision".into()).chain(decisions.iter().enumerate().map(|(index, decision)| format!("{}. {} · {}",
                index + 1, if decision.allowed { "Allowed" } else { "Blocked" }, self.recipient_choice_label(&decision.recipient)))).collect();
            self.decisions = decisions;
            self.view.drop_down(cx, ids!(decision_choice)).set_labels(cx, labels);
            self.view.drop_down(cx, ids!(decision_choice)).set_selected_item(cx, index);
        }
        let decisions = flow::recent_action_decisions().unwrap_or_default().into_iter().rev()
            .filter(|decision| !decision.allowed && decision.context.account() == self.account).collect::<Vec<_>>();
        if self.action_decisions != decisions {
            let selected = self.selected_action(cx);
            let index = selected.and_then(|selected| decisions.iter().position(|decision| decision == selected)).map(|index| index + 1).unwrap_or(0);
            let labels = std::iter::once("Choose a blocked action".into()).chain(decisions.iter().enumerate().map(|(index, decision)| format!("{}. {}",
                index + 1, self.action_choice_label(&decision.action)))).collect();
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
                    AuthoritySession::Once { .. } => "One unchanged action, once".into(),
                    AuthoritySession::RobrixSession => "Until Robrix closes".into(),
                    AuthoritySession::RoomSession { room, .. } => format!("Until {} closes", self.room_label(room)),
                })).collect::<Vec<_>>();
            self.view.label(cx, ids!(authority_rules)).set_text(cx, &if labels.is_empty() { "No action approvals.".into() } else { numbered(&labels) });
            self.view.drop_down(cx, ids!(authority_remove_choice)).set_labels(cx, authorities.iter().enumerate().map(|(index, grant)|
                format!("{}. {} · {}", index + 1, self.action_choice_label(&grant.action), self.context_choice_label(&grant.context))).collect());
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
                let clearance = if snapshot.clearance.is_some() { "Public mode: this app cannot receive private room or account data." } else { "Robrix remembers where data came from before the app or agent receives it." };
                format!("{}\n{clearance}\nData it may know:\n{}\nContent that may influence its actions:\n{}",
                    self.context_label(&snapshot.context), if sources.is_empty() { "None".into() } else { bullets(&sources) },
                    if influences.is_empty() { "None".into() } else { bullets(&influences) })
            }
            None => "Choose a mini-app or agent under Sharing rules to see the data it may know. Start it first if it is not listed.".into(),
        };
        self.view.label(cx, ids!(context_details)).set_text(cx, &details);
        let decision = self.selected_decision(cx);
        let details = decision.map(|decision| {
            let blocked = decision.denied_sources.iter().map(|source| self.source_label(source)).collect::<Vec<_>>();
            format!("{}\nDestination: {}\n{}",
                self.context_label(&decision.context), self.recipient_label(&decision.recipient),
                if decision.allowed { "Sharing was allowed for every data source when checked.".into() }
                else { format!("Data sources that blocked sharing:\n{}\n\n{}", bullets(&blocked), self.sharing_remedy(&decision.denied_sources, &decision.recipient)) })
        }).unwrap_or_else(|| if self.decisions.is_empty() { "No sharing requests have been checked in this account yet.".into() } else { "Choose a sharing decision to see its destination and the exact reason it was allowed or blocked.".into() });
        self.view.label(cx, ids!(decision_details)).set_text(cx, &details);
        let blocked = decision.map(|decision| decision.denied_sources.iter().cloned().collect::<Vec<_>>()).unwrap_or_default();
        if blocked != self.blocked_sources {
            let labels = blocked.iter().map(|source| self.source_choice_label(source)).collect();
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
            let warning = if self.view.drop_down(cx, ids!(action_session)).selected_item() == 0 {
                "Once authorizes only one unchanged retry of the complete request shown below. Other requests, paths, contents and targets need their own approval."
            } else if self.view.drop_down(cx, ids!(action_session)).selected_item() == 1 && decision.context.room().is_none() {
                "This app has no attached room. Choose This exact action once or Until Robrix closes before approving it."
            } else if decision.action.kind.starts_with("network.") {
                "This HTTP method permission covers every path on this exact origin. Internet and source sharing rules still apply."
            } else { "Session permission covers repeated actions of this kind to this target, including different contents. Room permissions and source sharing rules still apply." };
            let activation = if snapshot.is_some() { "This app or agent is currently running" } else { "This app or agent has stopped. Run it again and review its new request." };
            let contents = decision.request.as_ref().map(|request| format!("\nExact request #{} (invisible controls are shown as Unicode escapes):\n{}", request.id, review_payload(&request.payload)))
                .unwrap_or_else(|| "\nExact contents have not been captured. Retry the operation to capture them before choosing Once.".into());
            let approved = snapshot.is_some() && self.authorities.iter().any(|grant| {
                grant.context == decision.context && grant.action == decision.action && match grant.session {
                    AuthoritySession::Once { request_id } => grant.influences == *influences && decision.request.as_ref().is_some_and(|request| request.id == request_id),
                    _ => influences.is_subset(&grant.influences),
                }
            });
            let status = if approved {
                "A matching action approval is now present. Retry the unchanged operation; all other permission and sharing checks still apply."
            } else {
                "Blocked by: action approval. Content read by this app or agent may have influenced this action. Review the contents below, choose how long to allow it, then press Allow this exact action once or Allow for this session."
            };
            format!("{}\n{activation}\nAction: {}\nTarget: {}\n{status}\n{warning}\nContent that may have influenced this action:\n{}{contents}",
                self.context_label(&decision.context), review_payload(&decision.action.kind), review_payload(&decision.action.target), bullets(&labels))
        }).unwrap_or_else(|| if self.action_decisions.is_empty() { "No actions need review in this account.".into() } else { "Choose a blocked action to review its destination, contents and approval options.".into() });
        self.view.label(cx, ids!(action_details)).set_text(cx, &details);
        self.view.widget(cx, ids!(authority_button)).set_visible(cx, action.is_some());
        let can_approve = action.is_some_and(|decision| {
            self.snapshots.iter().any(|snapshot| snapshot.context == decision.context && snapshot.epoch == decision.epoch)
                && match self.view.drop_down(cx, ids!(action_session)).selected_item() {
                    0 => decision.request.is_some(),
                    1 => decision.context.room().is_some(),
                    2 => true,
                    _ => false,
                }
        });
        self.view.button(cx, ids!(authority_button)).set_enabled(cx, can_approve);
        self.view.widget(cx, ids!(authority_button)).set_disabled(cx, !can_approve);
        self.view.widget(cx, ids!(action_session)).set_visible(cx, action.is_some());
        self.view.button(cx, ids!(authority_button)).set_text(cx,
            if self.view.drop_down(cx, ids!(action_session)).selected_item() == 0 { "Allow this exact action once" } else { "Allow for this session" });
    }

    pub(super) fn review_decision(&mut self, cx: &mut Cx) -> Result<(), String> {
        let decision = self.selected_decision(cx)
            .ok_or("Select a blocked sharing decision.")?.clone();
        let source = self.blocked_sources.get(self.view.drop_down(cx, ids!(blocked_source_choice)).selected_item())
            .ok_or("Select one blocked source to review.")?.clone();
        let index = if let Some(index) = self.sources.iter().position(|(candidate, _)| candidate == &source) { index }
        else {
            self.sources.push((source.clone(), self.source_label(&source)));
            self.view.drop_down(cx, ids!(source_choice)).set_labels(cx, self.sources.iter().map(|(source, _)| self.source_choice_label(source)).collect());
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
        self.view.drop_down(cx, ids!(page_choice)).set_selected_item(cx, 0);
        self.update_page(cx);
        Ok(())
    }

    fn room_label(&self, room: &str) -> String {
        self.rooms.iter().find(|(id, _)| id == room).map(|(_, name)| format!("{name} ({room})")).unwrap_or_else(|| room.to_owned())
    }

    pub(super) fn room_name<'a>(&'a self, room: &'a str) -> &'a str {
        self.rooms.iter().find(|(id, _)| id == room).map(|(_, name)| name.as_str()).unwrap_or(room)
    }

    pub(super) fn context_choice_label(&self, context: &ContextId) -> String {
        let name = |app: &str| self.apps.iter().find(|(id, _)| id == app)
            .map(|(_, name)| name.clone()).unwrap_or_else(|| app.into());
        match context {
            ContextId::App { app, room, .. } => format!("{} · {}", name(app), room.as_deref().map(|room| self.room_name(room)).unwrap_or("Account-wide")),
            ContextId::PublicApp { app, .. } => format!("{} · Public mode", name(app)),
            ContextId::Agent { room, .. } => format!("Agent · {}", self.room_name(room)),
        }
    }

    pub(super) fn source_choice_label(&self, source: &Source) -> String {
        match source {
            Source::Account { .. } => "Account data".into(),
            Source::Room { room, .. } => format!("Room · {}", self.room_name(room)),
            Source::UnknownPrivate => "Unknown private data".into(),
        }
    }

    pub(super) fn recipient_choice_label(&self, recipient: &Recipient) -> String {
        match recipient {
            Recipient::NetworkOrigin(origin) => origin.clone(),
            Recipient::ModelProvider(id) => self.model.as_ref().filter(|model| &model.id == id)
                .map(|model| model.label.clone()).unwrap_or_else(|| "Other AI service".into()),
            Recipient::MatrixRoom { room, .. } => format!("Room · {}", self.room_name(room)),
            Recipient::External => "Unrestricted sharing".into(),
        }
    }

    fn action_choice_label(&self, action: &flow::SensitiveAction) -> String {
        let action_name = a2app_core::capabilities::by_id(&action.kind).map(|cap| cap.title.to_string())
            .unwrap_or_else(|| action.kind.strip_prefix("network.").map(|method| format!("Website request ({method})")).unwrap_or_else(|| action.kind.clone()));
        format!("{action_name} · {}", self.room_name(&action.target))
    }

    fn app_label(&self, app: &str) -> String {
        self.apps.iter().find(|(id, _)| id == app).map(|(_, name)| format!("{name} ({app})")).unwrap_or_else(|| app.to_owned())
    }

    pub(super) fn context_label(&self, context: &ContextId) -> String {
        match context {
            ContextId::App { account, app, room } => format!("Mini-app {} · {} · {account}", self.app_label(app), room.as_deref().map(|room| self.room_label(room)).unwrap_or_else(|| "Account context".into())),
            ContextId::PublicApp { account, app } => format!("Public mini-app {} · {account}", self.app_label(app)),
            ContextId::Agent { account, room } => format!("Agent · {} · {account}", self.room_label(room)),
        }
    }

    pub(super) fn source_label(&self, source: &Source) -> String {
        match source {
            Source::Account { account } => format!("Account data · {account}"),
            Source::Room { account, room } => format!("Room · {} · {account}", self.room_label(room)),
            Source::UnknownPrivate => "Private data with an unknown source (sharing blocked)".into(),
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
            ReaderScope::App { account, app } => format!("Everywhere {} runs: rooms, spaces and account-wide use · {account}", self.app_label(app)),
            ReaderScope::Context(context) => self.context_label(context),
        }
    }

    pub(super) fn duration_label(&self, duration: &SharingDuration) -> String {
        match duration {
            SharingDuration::Permanent => "Until you remove this rule".into(),
            SharingDuration::RobrixSession => "Until Robrix closes".into(),
            SharingDuration::RoomSession { account, room } => format!("Until {} closes · {account}", self.room_label(room)),
        }
    }
}
