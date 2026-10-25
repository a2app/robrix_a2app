use super::*;

pub(super) fn bullets(labels: &[String]) -> String {
    labels.iter().map(|label| format!("• {label}")).collect::<Vec<_>>().join("\n")
}

/// A sharing request is identified by who asked and where the data was going.
pub(super) type DecisionKey = (ContextId, Recipient);

pub(super) fn decision_key(decision: &FlowDecision) -> DecisionKey {
    (decision.context.clone(), decision.recipient.clone())
}

pub(super) fn is_decision_for(decision: &FlowDecision, (context, recipient): &DecisionKey) -> bool {
    &decision.context == context && &decision.recipient == recipient
}

fn unresolved_sharing_decisions(decisions: Vec<FlowDecision>, account: &str) -> Vec<FlowDecision> {
    let mut seen = std::collections::BTreeSet::new();
    decisions.into_iter().rev().filter(|decision| decision.context.account() == account)
        .filter(|decision| seen.insert((decision.context.clone(), decision.recipient.clone())))
        .filter(|decision| !decision.allowed).collect()
}

/// Splits the denied sources still not allowed for a request into those its context still holds
/// (all of them when the label is unknown), which still block it, and those it no longer holds.
pub(super) fn remaining_denials(denied: &flow::Label, label: Option<&flow::Label>, is_allowed: impl Fn(&Source) -> bool) -> (flow::Label, flow::Label) {
    denied.iter().filter(|source| !is_allowed(source)).cloned().partition(|source| label.is_none_or(|label| label.contains(source)))
}

pub(super) fn is_allowed_now(source: &Source, decision: &FlowDecision) -> bool {
    flow::sharing_allows_for_reader(source, &decision.recipient, &decision.context).unwrap_or(false)
}

pub(super) fn allowed_now_text(context: &ContextId) -> &'static str {
    if matches!(context, ContextId::Agent { .. }) { "Allowed now. Send your message again." } else { "Allowed now. Try again in the app." }
}

/// No rule allows this data yet; only its context's label dropped it.
pub(super) fn no_longer_held_text(context: &ContextId) -> &'static str {
    if matches!(context, ContextId::Agent { .. }) {
        "The agent no longer holds this data. If it reads it again and is blocked, it appears in Needs attention."
    } else { "This app no longer holds this data. If it is blocked again, it appears in Needs attention." }
}

fn unresolved_action_decisions(decisions: Vec<ActionDecision>, snapshots: &[ContextSnapshot], account: &str) -> Vec<ActionDecision> {
    let mut seen = std::collections::BTreeSet::new();
    decisions.into_iter().rev().filter(|decision| decision.context.account() == account)
        .filter(|decision| seen.insert((decision.context.clone(), decision.action.kind.clone(), decision.action.target.clone())))
        .filter(|decision| !decision.allowed && snapshots.iter().any(|snapshot|
            snapshot.epoch != 0 && snapshot.context == decision.context && snapshot.epoch == decision.epoch)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(room: &str, epoch: u64) -> ContextSnapshot {
        ContextSnapshot {
            context: ContextId::App { account: "alice".into(), app: "dice".into(), room: Some(room.into()) }, epoch,
            label: [Source::Account { account: "alice".into() }].into(), clearance: None,
            influences: [Influence::InternetOrigin("https://example.org".into())].into(),
        }
    }

    #[test]
    fn sharing_attention_keeps_only_latest_status_per_compartment_and_destination() {
        let first = snapshot("one", 7);
        let second = snapshot("two", 0);
        let blocked = FlowDecision {
            context: first.context, epoch: first.epoch, recipient: Recipient::NetworkOrigin("https://example.org".into()),
            sources: first.label.clone(), denied_sources: first.label, allowed: false,
        };
        let other_room = FlowDecision { context: second.context, epoch: second.epoch, ..blocked.clone() };
        let allowed = FlowDecision { allowed: true, denied_sources: Default::default(), ..blocked.clone() };
        let other_site = FlowDecision { recipient: Recipient::NetworkOrigin("https://other.example".into()), ..blocked.clone() };
        let mut latest = other_site.clone();
        latest.denied_sources.insert(Source::Room { account: "alice".into(), room: "three".into() });
        let decisions = unresolved_sharing_decisions(vec![blocked, other_room.clone(), allowed, other_site, latest.clone()], "alice");
        assert_eq!(decisions, vec![latest, other_room], "successful retries supersede denials without merging room or website identities");
    }

    #[test]
    fn action_attention_removes_successful_stopped_and_superseded_requests() {
        let active = snapshot("one", 7);
        let stopped = snapshot("two", 0);
        let blocked = ActionDecision {
            context: active.context.clone(), epoch: active.epoch,
            action: flow::SensitiveAction { kind: "network.GET".into(), target: "https://example.org".into() },
            influences: active.influences.clone(), allowed: false,
            request: Some(flow::ActionRequest { id: 1, payload: "{}".into() }),
        };
        let allowed = ActionDecision { allowed: true, ..blocked.clone() };
        let stopped_request = ActionDecision { context: stopped.context.clone(), epoch: 6, ..blocked.clone() };
        let old_activation = ActionDecision {
            epoch: 6, action: flow::SensitiveAction { kind: "network.PUT".into(), target: "https://example.org".into() }, ..blocked.clone()
        };
        let mut older = blocked.clone();
        older.action.kind = "network.POST".into();
        let latest = ActionDecision { request: Some(flow::ActionRequest { id: 2, payload: "{\"body\":\"latest\"}".into() }), ..older.clone() };
        let decisions = unresolved_action_decisions(vec![blocked, older, allowed, stopped_request, old_activation, latest.clone()], &[active, stopped], "alice");
        assert_eq!(decisions, vec![latest]);
    }

    #[test]
    fn remaining_denials_drop_allowed_sources_and_sources_the_context_no_longer_holds() {
        let room = |id: &str| Source::Room { account: "alice".into(), room: id.into() };
        let directory = Source::RoomDirectory { account: "alice".into() };
        let denied: flow::Label = [room("a"), room("b"), directory.clone()].into();
        let label: flow::Label = [room("a"), directory.clone()].into();
        let is_allowed = |source: &Source| source == &room("a");
        assert_eq!(remaining_denials(&denied, Some(&label), is_allowed), ([directory.clone()].into(), [room("b")].into()),
            "an allowed source stops blocking; one a new session no longer holds stops blocking but is still not allowed");
        assert_eq!(remaining_denials(&denied, None, is_allowed), ([room("b"), directory].into(), flow::Label::new()),
            "an unknown label keeps every unallowed source");
        assert_eq!(remaining_denials(&denied, Some(&label), |_| true), Default::default());
    }
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
        self.view.drop_down2(cx, ids!(context_choice)).selected_item().checked_sub(1)
            .and_then(|index| self.snapshots.get(index))
            .map(|snapshot| &snapshot.context).ok_or_else(|| "Choose the mini-app or agent that may share this data.".into())
    }

    pub(super) fn selected_decision(&self, _cx: &Cx) -> Option<&FlowDecision> {
        let key = self.decision_selection.as_ref()?;
        self.decisions.iter().find(|decision| is_decision_for(decision, key))
    }

    fn selected_action(&self, _cx: &Cx) -> Option<&ActionDecision> {
        self.action_selection.checked_sub(1)
            .and_then(|index| self.action_decisions.get(index))
    }

    pub(super) fn selected_reader(&self, cx: &Cx) -> Result<ReaderScope, String> {
        match self.view.permission_choices(cx, ids!(reader_kind)).selected_item() {
            0 => Ok(ReaderScope::Context(self.selected_context(cx)?.clone())),
            1 => self.apps.get(self.view.drop_down2(cx, ids!(reader_app)).selected_item())
                .map(|(app, _)| ReaderScope::App { account: self.account.clone(), app: app.clone() })
                .ok_or_else(|| "Select an installed mini-app.".into()),
            2 if !self.account.is_empty() => Ok(ReaderScope::AllReaders),
            _ => Err("Sign in and choose who may share this source.".into()),
        }
    }

    fn selected_duration(&self, cx: &Cx) -> Result<SharingDuration, String> {
        match self.view.permission_choices(cx, ids!(sharing_lifetime)).selected_item() {
            0 => Ok(SharingDuration::RobrixSession),
            1 => self.rooms.get(self.view.drop_down2(cx, ids!(lifetime_room)).selected_item())
                .map(|(room, _)| SharingDuration::RoomSession { account: self.account.clone(), room: room.clone() })
                .ok_or_else(|| "Select the room whose close will end this rule.".into()),
            2 => Ok(SharingDuration::Permanent),
            _ => Err("Select how long the sharing rule lasts.".into()),
        }
    }

    pub(super) fn sharing_action(&self, cx: &Cx) -> Result<A2AppOp, String> {
        let source = self.selected_source(cx)?;
        let source_account = match &source {
            Source::Account { account } | Source::Room { account, .. } | Source::RoomDirectory { account } => account,
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
        let snapshot = self.snapshots.iter().find(|snapshot| snapshot.epoch != 0 && snapshot.context == decision.context && snapshot.epoch == decision.epoch)
            .ok_or("This app or agent has stopped. Run it again and review its new request.")?;
        let session = match self.view.permission_choices(cx, ids!(action_session)).selected_item() {
            0 => {
                let request = decision.request.as_ref().ok_or("This operation has no exact contents to review. Retry it to capture a request, or explicitly choose a session permission.")?;
                return Ok(A2AppOp::GrantExactFlowAuthority {
                    context: decision.context.clone(), request_id: request.id,
                    expected_influences: snapshot.influences.clone(), expected_epoch: snapshot.epoch,
                });
            }
            1 => AuthoritySession::RoomSession {
                account: decision.context.account().to_string(),
                room: decision.context.room().ok_or("This app has no attached room. Choose Until Robrix closes.")?.to_string(),
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
        // Sharing rules apply to a saved compartment across restarts.
        //
        // Action approvals still require its current, nonzero activation epoch.
        let snapshots = flow::retained_contexts().unwrap_or_default().into_iter()
            .filter(|snapshot| snapshot.context.account() == self.account).collect::<Vec<_>>();
        let previous = self.selected_context(cx).ok().cloned();
        if self.snapshots != snapshots {
            let labels = std::iter::once("Choose a mini-app or agent".into())
                .chain(snapshots.iter().map(|snapshot| format!("{}{}", self.context_choice_label(&snapshot.context), if snapshot.epoch == 0 { " (stopped)" } else { "" }))).collect();
            self.view.drop_down2(cx, ids!(context_choice)).set_labels(cx, labels);
            let index = previous.as_ref().and_then(|context| snapshots.iter().position(|snapshot| &snapshot.context == context)).map(|index| index + 1).unwrap_or(0);
            self.snapshots = snapshots;
            self.view.drop_down2(cx, ids!(context_choice)).set_selected_item(cx, index);
        }
        let mut decisions = unresolved_sharing_decisions(flow::recent_decisions().unwrap_or_default(), &self.account);
        for recalled in &self.recalled_decisions {
            match decisions.iter_mut().find(|decision| is_decision_for(decision, &decision_key(recalled))) {
                Some(decision) => decision.denied_sources.extend(recalled.denied_sources.iter().cloned()),
                None => decisions.push(recalled.clone()),
            }
        }
        // List only data that still blocks under today's rules and label, except that a request
        // reopened from History keeps data its context no longer holds. An open request stays.
        let mut no_longer_held = BTreeSet::new();
        let decisions = decisions.into_iter().filter_map(|mut decision| {
            let key = decision_key(&decision);
            let is_recalled = self.recalled_decisions.iter().any(|recalled| is_decision_for(recalled, &key));
            let label = self.snapshots.iter().find(|snapshot| snapshot.context == decision.context)
                .map(|snapshot| &snapshot.label).filter(|_| !is_recalled);
            let (blocking, unheld) = remaining_denials(&decision.denied_sources, label, |source| is_allowed_now(source, &decision));
            decision.allowed = blocking.is_empty();
            decision.denied_sources = blocking;
            if decision.allowed && !unheld.is_empty() { no_longer_held.insert(key.clone()); }
            (!decision.allowed || self.decision_selection.as_ref() == Some(&key)).then_some(decision)
        }).collect::<Vec<_>>();
        self.no_longer_held = no_longer_held;
        self.recalled_decisions.retain(|recalled| decisions.iter().any(|decision| is_decision_for(decision, &decision_key(recalled))));
        self.decisions = decisions;
        if self.selected_decision(cx).is_none() { self.decision_selection = None; }
        let decisions = unresolved_action_decisions(flow::recent_action_decisions().unwrap_or_default(), &self.snapshots, &self.account);
        if self.action_decisions != decisions {
            self.action_selection = self.selected_action(cx).and_then(|selected|
                decisions.iter().position(|decision| decision == selected)).map(|index| index + 1).unwrap_or(0);
            self.action_decisions = decisions;
        }
        self.authorities = flow::authorities().unwrap_or_default().into_iter()
            .filter(|grant| grant.context.account() == self.account).collect();
        self.effect_authorities = flow::effect_authorities().unwrap_or_default().into_iter()
            .filter(|grant| grant.context.account() == self.account).collect();
        let count = self.decisions.iter().filter(|decision| !decision.allowed).count() + self.action_decisions.len();
        self.view.label(cx, ids!(attention_summary)).set_text(cx, if count == 0 {
            "Nothing needs your attention. Blocked sharing requests and actions will appear here."
        } else { "Choose a request to see why it was blocked. Reviewing a request does not approve it." });
        self.view.widget(cx, ids!(attention_more)).set_visible(cx, count > 20);
        if count <= 20 && self.attention_expanded {
            self.attention_expanded = false;
            self.view.collapsible_header(cx, ids!(attention_more)).set_expanded(cx, false, Animate::No);
        }
        self.view.widget(cx, ids!(attention_more_section)).set_visible(cx, count > 20 && self.attention_expanded);
        let reviewing = self.decision_selection.is_some() || self.action_selection != 0;
        self.view.widget(cx, ids!(attention_overview)).set_visible(cx, !reviewing);
        self.view.widget(cx, ids!(attention_details)).set_visible(cx, reviewing);
        self.view.widget(cx, ids!(sharing_details)).set_visible(cx, self.decision_selection.is_some());
        self.view.widget(cx, ids!(action_review)).set_visible(cx, self.action_selection != 0);
        self.update_diagnostic_details(cx);
    }

    pub(super) fn update_diagnostic_details(&mut self, cx: &mut Cx) {
        let snapshot = self.selected_context(cx).ok().and_then(|context| self.snapshots.iter().find(|snapshot| &snapshot.context == context));
        let details = match snapshot {
            Some(snapshot) => {
                let sources = snapshot.label.iter().map(|source| self.source_label(source)).collect::<Vec<_>>();
                let influences = snapshot.influences.iter().map(|influence| self.influence_label(influence)).collect::<Vec<_>>();
                let clearance = if snapshot.clearance.is_some() { "Public mode: this app cannot receive private room or account data." } else { "Robrix remembers where data came from before the app or agent receives it." };
                let activation = if snapshot.epoch == 0 { "Stopped. Sharing rules will apply when you run it again." } else { "Currently running." };
                format!("{}\n{activation}\n{clearance}\nData it may know:\n{}\nContent that may influence its actions:\n{}",
                    self.context_label(&snapshot.context), if sources.is_empty() { "None".into() } else { bullets(&sources) },
                    if influences.is_empty() { "None".into() } else { bullets(&influences) })
            }
            None => "Choose a mini-app or agent under Sharing rules to see the data it may know.".into(),
        };
        self.view.label(cx, ids!(context_details)).set_text(cx, &details);
        let decision = self.selected_decision(cx);
        let details = decision.map(|decision| {
            let blocked = decision.denied_sources.iter().map(|source| self.source_choice_label(source)).collect::<Vec<_>>();
            format!("{}\nDestination: {}\n{}",
                self.context_choice_label(&decision.context), self.recipient_choice_label(&decision.recipient),
                if decision.allowed && self.no_longer_held.contains(&decision_key(decision)) { no_longer_held_text(&decision.context).into() }
                else if decision.allowed { allowed_now_text(&decision.context).into() }
                else { format!("Data needing permission:\n{}\n\n{}", bullets(&blocked), self.sharing_remedy(&decision.denied_sources, &decision.recipient)) })
        }).unwrap_or_else(|| if self.decisions.is_empty() { "No sharing requests have been checked in this account yet.".into() } else { "Choose a sharing decision to see its destination and the exact reason it was allowed or blocked.".into() });
        self.view.label(cx, ids!(decision_details)).set_text(cx, &details);
        let identifiers = decision.filter(|decision| !decision.allowed).map(|decision| format!("{}\nDestination: {}\nData sources:\n{}",
            self.context_label(&decision.context), self.recipient_label(&decision.recipient),
            bullets(&decision.denied_sources.iter().map(|source| self.source_label(source)).collect::<Vec<_>>()))).unwrap_or_default();
        self.view.label(cx, ids!(decision_identifiers)).set_text(cx, &identifiers);
        self.view.widget(cx, ids!(decision_identifiers)).set_visible(cx, self.decision_identifiers_open && !identifiers.is_empty());
        self.view.widget(cx, ids!(decision_identifiers_toggle)).set_visible(cx, !identifiers.is_empty());
        let blocked = decision.map(|decision| decision.denied_sources.iter().cloned().collect::<Vec<_>>()).unwrap_or_default();
        if blocked != self.blocked_sources {
            // Keep the chosen source when others are allowed meanwhile.
            let previous = self.blocked_sources.get(self.view.permission_choices(cx, ids!(blocked_source_choice)).selected_item()).cloned();
            let index = previous.and_then(|previous| blocked.iter().position(|source| source == &previous)).unwrap_or(0);
            let labels = blocked.iter().map(|source| self.source_choice_label(source)).collect();
            self.blocked_sources = blocked;
            self.view.permission_choices(cx, ids!(blocked_source_choice)).set_labels(cx, labels);
            self.view.permission_choices(cx, ids!(blocked_source_choice)).set_selected_item(cx, index);
        }
        self.view.widget(cx, ids!(review_section)).set_visible(cx, !self.blocked_sources.is_empty());
        self.view.widget(cx, ids!(blocked_source_prompt)).set_visible(cx, self.blocked_sources.len() > 1);
        self.view.widget(cx, ids!(blocked_source_choice)).set_visible(cx, self.blocked_sources.len() > 1);
        let review_problem = self.sharing_review_problem(cx);
        let is_reviewable = self.review_blocker(cx).is_none();
        self.view.button(cx, ids!(review_button)).set_enabled(cx, is_reviewable);
        self.view.widget(cx, ids!(review_button)).set_disabled(cx, !is_reviewable);
        self.view.widget(cx, ids!(review_problem)).set_visible(cx, review_problem.is_some() && !self.blocked_sources.is_empty());
        self.view.label(cx, ids!(review_problem)).set_text(cx, review_problem.as_deref().unwrap_or_default());
        let decision = self.selected_decision(cx);
        let allow_all = self.allow_all_grants(cx);
        let preview = match (&allow_all, decision) {
            (Ok(grants), Some(decision)) => {
                let sources = grants.iter().filter_map(|grant| match grant {
                    A2AppOp::GrantFlowSharing { source, .. } => Some(self.source_choice_label(source)),
                    _ => None,
                }).collect::<Vec<_>>();
                format!("{} will be able to share with {}:\n{}", self.context_choice_label(&decision.context), self.recipient_choice_label(&decision.recipient), bullets(&sources))
            }
            (Err(problem), _) => problem.clone(),
            _ => String::new(),
        };
        // A request no rule can unblock already says why under Review sharing rule.
        let is_unapprovable = decision.is_some_and(|decision| self.decision_problem(decision).is_some()
            || decision.denied_sources.iter().any(|source| self.source_problem(source).is_some()));
        self.view.widget(cx, ids!(allow_all_section)).set_visible(cx, !self.blocked_sources.is_empty() && !is_unapprovable);
        self.view.label(cx, ids!(allow_all_preview)).set_text(cx, &preview);
        let is_allow_all_enabled = allow_all.is_ok() || decision.is_some_and(|decision| self.model_wait(decision).is_some());
        self.view.button(cx, ids!(allow_all_button)).set_enabled(cx, is_allow_all_enabled);
        self.view.widget(cx, ids!(allow_all_button)).set_disabled(cx, !is_allow_all_enabled);
        let agent_room = decision.and_then(|decision| self.decision_agent_room(decision));
        self.view.widget(cx, ids!(decision_restore_section)).set_visible(cx, cfg!(unix) && agent_room.is_some());
        if let (Some(decision), Some(room)) = (decision, agent_room) {
            let is_covered = decision.denied_sources.iter().any(|source| self.is_agent_default_pair(room, source, &decision.recipient));
            self.view.label(cx, ids!(decision_restore_text)).set_text(cx, if is_covered {
                "An AI room needs its default rules to read its room and reply. Restoring them keeps your other choices."
            } else { "An AI room needs its default rules to read its room and reply. Restoring them keeps your other choices, but they don't cover the data listed above." });
        }
        let action = self.selected_action(cx);
        let details = action.map(|decision| {
            let snapshot = self.snapshots.iter().find(|snapshot| snapshot.epoch != 0 && snapshot.context == decision.context && snapshot.epoch == decision.epoch);
            let influences = snapshot.map(|snapshot| &snapshot.influences).unwrap_or(&decision.influences);
            let labels = influences.iter().map(|influence| self.influence_label(influence)).collect::<Vec<_>>();
            let warning = if self.view.permission_choices(cx, ids!(action_session)).selected_item() == 0 {
                "Once authorizes only one unchanged retry of the complete request shown below. Other requests, paths, contents and targets need their own approval."
            } else if self.view.permission_choices(cx, ids!(action_session)).selected_item() == 1 && decision.context.room().is_none() {
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
            self.snapshots.iter().any(|snapshot| snapshot.epoch != 0 && snapshot.context == decision.context && snapshot.epoch == decision.epoch)
                && match self.view.permission_choices(cx, ids!(action_session)).selected_item() {
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
            if self.view.permission_choices(cx, ids!(action_session)).selected_item() == 0 { "Allow this exact action once" } else { "Allow for this session" });
    }

    pub(super) fn review_decision(&mut self, cx: &mut Cx) -> Result<(), String> {
        self.refresh_model(cx);
        if let Some(problem) = self.sharing_review_problem(cx) { return Err(problem); }
        let decision = self.selected_decision(cx)
            .ok_or("Select a blocked sharing decision.")?.clone();
        let source = self.blocked_sources.get(self.view.permission_choices(cx, ids!(blocked_source_choice)).selected_item())
            .ok_or("Select one blocked source to review.")?.clone();
        let index = if let Some(index) = self.sources.iter().position(|(candidate, _)| candidate == &source) { index }
        else {
            self.sources.push((source.clone(), self.source_label(&source)));
            self.view.drop_down2(cx, ids!(source_choice)).set_labels(cx, self.sources.iter().map(|(source, _)| self.source_choice_label(source)).collect());
            self.sources.len() - 1
        };
        self.view.drop_down2(cx, ids!(source_choice)).set_selected_item(cx, index);
        if let Some(index) = self.snapshots.iter().position(|snapshot| snapshot.context == decision.context) {
            self.view.drop_down2(cx, ids!(context_choice)).set_selected_item(cx, index + 1);
        } else { return Err("This mini-app or agent's saved data is no longer available. Run it again, then review its new request.".into()); }
        self.view.permission_choices(cx, ids!(reader_kind)).set_selected_item(cx, 0);
        self.view.permission_choices(cx, ids!(sharing_lifetime)).set_selected_item(cx, 0);
        match &decision.recipient {
            Recipient::NetworkOrigin(_) if self.homeserver.as_ref() == Some(&decision.recipient) => {
                self.view.permission_choices(cx, ids!(recipient_kind)).set_selected_item(cx, 4);
            }
            Recipient::NetworkOrigin(origin) => {
                self.view.permission_choices(cx, ids!(recipient_kind)).set_selected_item(cx, 1);
                self.view.text_input(cx, ids!(network_url)).set_text(cx, origin);
            }
            Recipient::ModelProvider(id) if self.model.as_ref().is_some_and(|model| &model.id == id) => {
                self.view.permission_choices(cx, ids!(recipient_kind)).set_selected_item(cx, 0);
            }
            Recipient::ModelProvider(_) => return Err("This request used a different model configuration. Restart the agent with the currently configured service before approving it.".into()),
            Recipient::MatrixRoom { account, room } => {
                if account != &self.account { return Err("The recipient belongs to a different account.".into()); }
                let index = self.rooms.iter().position(|(id, _)| id == room).ok_or("The destination room is no longer joined.")?;
                self.view.permission_choices(cx, ids!(recipient_kind)).set_selected_item(cx, 2);
                self.view.drop_down2(cx, ids!(target_room)).set_selected_item(cx, index);
            }
            Recipient::External => return Err("Unrestricted external sharing cannot be approved here.".into()),
            Recipient::Clipboard => self.view.permission_choices(cx, ids!(recipient_kind)).set_selected_item(cx, 3),
        }
        self.update_recipient_form(cx);
        self.update_diagnostic_details(cx);
        self.wizard_return_page = 1;
        self.wizard_open = true;
        self.wizard_step = 2;
        self.reader_options_open = false;
        self.rule_identifiers_open = false;
        self.view.permission_choices(cx, ids!(page_choice)).set_selected_item(cx, 0);
        self.update_page(cx);
        Ok(())
    }

    fn sharing_review_problem(&self, cx: &Cx) -> Option<String> {
        self.review_blocker(cx).or_else(|| self.selected_decision(cx).and_then(|decision| self.model_wait(decision)))
    }

    /// What keeps the chosen source from being reviewed. Waiting for the AI service is not one:
    /// pressing the button reads it again.
    fn review_blocker(&self, cx: &Cx) -> Option<String> {
        let decision = self.selected_decision(cx)?;
        let source = self.blocked_sources.get(self.view.permission_choices(cx, ids!(blocked_source_choice)).selected_item())?;
        self.source_problem(source).or_else(|| self.decision_problem(decision))
    }

    fn model_wait(&self, decision: &FlowDecision) -> Option<String> {
        (matches!(decision.recipient, Recipient::ModelProvider(_)) && self.model.is_none()).then(|| self.model_unavailable_text())
    }

    fn source_problem(&self, source: &Source) -> Option<String> {
        match source {
            Source::UnknownPrivate => Some("Robrix cannot identify where this old data or app source came from. It stays protected. For a public task, use an unchanged built-in app with separately tracked public data.".into()),
            Source::Account { account } | Source::Room { account, .. } | Source::RoomDirectory { account } if account != &self.account => Some("Sign in to the account that owns this data to review its sharing permission.".into()),
            _ => None,
        }
    }

    fn decision_problem(&self, decision: &FlowDecision) -> Option<String> {
        if !self.snapshots.iter().any(|snapshot| snapshot.context == decision.context) {
            return Some("This mini-app or agent's saved data is no longer available. Run it again, then review its new request.".into());
        }
        match &decision.recipient {
            Recipient::External => Some("This app has not identified a destination for the data. It must choose a specific website or room before you can allow sharing.".into()),
            Recipient::MatrixRoom { account, .. } if account != &self.account => Some("This destination belongs to a different account. Sign in to that account and try again.".into()),
            Recipient::MatrixRoom { room, .. } if !self.rooms.iter().any(|(id, _)| id == room) => Some("You no longer belong to the destination room. Join it again before allowing sharing.".into()),
            Recipient::ModelProvider(id) if self.model.as_ref().is_some_and(|model| &model.id != id) => Some("Your AI service has changed. Restart the agent and review its new request.".into()),
            _ => None,
        }
    }

    /// A transient status (such as Claude Code still checking its sign-in) is not a changed service.
    pub(super) fn model_unavailable_text(&self) -> String {
        if self.model_error.is_empty() { "Configure a supported AI model service first.".into() }
        else { format!("Robrix can't confirm your AI service right now: {}", self.model_error) }
    }

    /// One sharing rule per still-blocked source of the open request, all for its own reader.
    pub(super) fn allow_all_grants(&self, cx: &Cx) -> Result<Vec<A2AppOp>, String> {
        let decision = self.selected_decision(cx).filter(|decision| !decision.allowed).ok_or("Select a blocked sharing request.")?;
        if let Some(problem) = decision.denied_sources.iter().find_map(|source| self.source_problem(source))
            .or_else(|| self.decision_problem(decision)).or_else(|| self.model_wait(decision))
        { return Err(problem); }
        // Re-check now: a source allowed since the list was built needs no second rule.
        let (sources, _) = remaining_denials(&decision.denied_sources, None, |source| is_allowed_now(source, decision));
        if sources.is_empty() { return Err(allowed_now_text(&decision.context).into()); }
        let duration = match self.view.permission_choices(cx, ids!(allow_all_lifetime)).selected_item() {
            0 => SharingDuration::RobrixSession,
            1 => SharingDuration::RoomSession {
                account: self.account.clone(),
                room: decision.context.room().ok_or("This app has no room. Choose another duration.")?.into(),
            },
            2 => SharingDuration::Permanent,
            _ => return Err("Select how long these rules last.".into()),
        };
        Ok(sources.into_iter().map(|source| A2AppOp::GrantFlowSharing {
            source, recipient: decision.recipient.clone(), reader: ReaderScope::Context(decision.context.clone()), duration: duration.clone(),
        }).collect())
    }

    /// The AI room whose agent made this still-blocked request.
    pub(super) fn decision_agent_room<'a>(&self, decision: &'a FlowDecision) -> Option<&'a str> {
        match &decision.context {
            ContextId::Agent { account, room } if account == &self.account && !decision.allowed => Some(room),
            _ => None,
        }
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
            Source::RoomDirectory { .. } => "Room and space directory".into(),
            Source::UnknownPrivate => "Unknown private data".into(),
        }
    }

    pub(super) fn recipient_choice_label(&self, recipient: &Recipient) -> String {
        match recipient {
            Recipient::NetworkOrigin(_) if self.homeserver.as_ref() == Some(recipient) => "My Matrix homeserver".into(),
            Recipient::NetworkOrigin(origin) => origin.clone(),
            Recipient::ModelProvider(id) => self.model.as_ref().filter(|model| &model.id == id)
                .map(|model| model.label.clone()).unwrap_or_else(|| "Other AI service".into()),
            Recipient::MatrixRoom { room, .. } => format!("Room · {}", self.room_name(room)),
            Recipient::External => "Unrestricted sharing".into(),
            Recipient::Clipboard => "System clipboard".into(),
        }
    }

    pub(super) fn action_choice_label(&self, action: &flow::SensitiveAction) -> String {
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
            Source::RoomDirectory { account } => format!("Room and space directory · {account}"),
            Source::UnknownPrivate => "Private data with an unknown source (sharing blocked)".into(),
        }
    }

    pub(super) fn influence_label(&self, influence: &Influence) -> String {
        match influence {
            Influence::RoomContent { account, room } => format!("Room content · {} · {account}", self.room_label(room)),
            Influence::RoomDirectory { account } => format!("Room names and directory · {account}"),
            Influence::InternetOrigin(origin) => format!("Website content · {origin}"),
            Influence::MiniApp { account, app } => format!("Mini-app content · {} · {account}", self.app_label(app)),
            Influence::Model(id) => self.recipient_label(&Recipient::ModelProvider(id.clone())),
            Influence::Unknown => "Unknown stored influences".into(),
        }
    }

    pub(super) fn reader_choice_label(&self, reader: &ReaderScope) -> String {
        match reader {
            ReaderScope::AllReaders => "All mini-apps and agents".into(),
            ReaderScope::App { app, .. } => format!("{} everywhere it runs", self.apps.iter().find(|(id, _)| id == app).map(|(_, name)| name.as_str()).unwrap_or(app)),
            ReaderScope::Context(context) => self.context_choice_label(context),
        }
    }

    pub(super) fn authority_duration_label(&self, session: &AuthoritySession) -> String {
        match session {
            AuthoritySession::Once { .. } => "One unchanged action, once".into(),
            AuthoritySession::RobrixSession => "Until Robrix closes".into(),
            AuthoritySession::RoomSession { room, .. } => format!("Until {} closes", self.room_label(room)),
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

    pub(super) fn effect_operation_label(&self, grant: &EffectAuthority) -> String {
        let operation = grant.operation.strip_prefix("operation:").or_else(|| grant.operation.strip_prefix("action:"));
        if let Some(capability) = operation.and_then(a2app_core::capabilities::by_id) { return capability.title.into(); }
        if let Some(action) = &grant.action { return self.action_choice_label(action); }
        "This exact request".into()
    }

    pub(super) fn effect_duration_label(&self, duration: &SharingDuration) -> String {
        match duration {
            SharingDuration::Permanent => "Forever (until you remove this permission)".into(),
            SharingDuration::RoomSession { room, .. } => format!("Until {} closes", self.room_name(room)),
            SharingDuration::RobrixSession => "Until Robrix closes".into(),
        }
    }
}
