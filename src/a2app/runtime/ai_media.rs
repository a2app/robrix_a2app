//! Session-owned media drafts and the capability boundaries of their effects.

use super::*;
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use a2app_core::information_flow::{self as flow, ContextId, Label, Influences};
use crate::a2app::ai::{media::PreparedMedia, tools::{MediaDraftRequest, MediaSource}};

const MAX_DRAFTS: usize = 16;
const MAX_DRAFT_BYTES: usize = 64 * 1024 * 1024;

pub(super) struct Draft {
    media: Arc<PreparedMedia>,
    context: ContextId,
    epoch: u64,
    sources: Label,
    influences: Influences,
}

pub(super) struct Pending {
    room: OwnedRoomId,
    context: ContextId,
    epoch: u64,
    tool: &'static str,
    answer: Sender<Result<String, String>>,
    alive: Arc<AtomicBool>,
}

#[derive(Clone, Debug)]
pub(super) struct MediaResult {
    id: u64,
    result: Result<MediaOutcome, String>,
}

#[derive(Clone, Debug)]
enum MediaOutcome {
    Draft(Arc<PreparedMedia>),
    Posted(String),
}

fn draft(room: &OwnedRoomId, id: &str) -> Result<Arc<PreparedMedia>, String> {
    let context = super::super::information_flow::agent_context(room.as_str())?;
    with_a2app(|state| {
        let draft = state.ai_media_drafts.get(&(room.clone(), id.to_string()))
            .ok_or("Unknown media draft. Use draft_media in this agent session first.")?;
        if draft.context != context { return Err("This media draft belongs to another session.".into()); }
        flow::ensure_context_epoch(&context, draft.epoch)?;
        // A retained attachment carries its original sources into later turns.
        flow::add_sources_for_activation(&context, draft.epoch, draft.sources.iter().cloned())?;
        flow::add_influences_for_activation(&context, draft.epoch, draft.influences.iter().cloned())?;
        Ok(draft.media.clone())
    }).unwrap_or_else(|| Err("Mini Apps is unavailable.".into()))
}

pub(super) fn restore_provenance(room: &OwnedRoomId, job: &SessionJob) -> Result<(), String> {
    match job {
        SessionJob::AttachMedia { draft_id, .. } | SessionJob::PostRoomMedia { draft_id, .. } => { draft(room, draft_id)?; }
        _ => {}
    }
    Ok(())
}

pub(super) fn exact_action(room: &OwnedRoomId, job: &SessionJob) -> Option<(flow::SensitiveAction, serde_json::Value)> {
    match job {
        SessionJob::DraftMessage { text, .. } => Some((flow::SensitiveAction {
            kind: "host.composer.insert".into(), target: room.to_string(),
        }, serde_json::json!({ "text": text }))),
        SessionJob::AttachMedia { draft_id, .. } => {
            let media = draft(room, draft_id).ok()?;
            Some((flow::SensitiveAction { kind: "host.composer.attach".into(), target: room.to_string() },
                serde_json::json!({ "media": media.metadata() })))
        }
        _ => None,
    }
}

/// Return the immutable request when all needed capabilities are live. A
/// parked post carries the specific capability currently being asked about.
fn gate(cx: &mut Cx, ui: &WidgetRef, room: &OwnedRoomId, mut job: SessionJob, target: &OwnedRoomId, caps: &[&'static str]) -> Option<SessionJob> {
    let subject = agent_subject(room.as_str());
    for id in caps {
        let cap = a2app_core::capabilities::by_id(id)?;
        let permission_target = if cap.scope == a2app_core::capabilities::Scope::Account { room } else { target };
        if let SessionJob::PostRoomMedia { authorizations, .. } = &job
            && let Some(auth) = authorizations.iter().find(|auth| auth.capability == *id)
        {
            let allowed = auth.check_context().is_ok() && auth.target_room.as_deref() == Some(permission_target.as_str())
                && with_a2app(|state| auth.permits_request(&state.permissions)).unwrap_or(false);
            if allowed { continue; }
        }
        let verdict = with_a2app(|state| state.permissions.effective_capability_for_in_context(
            &subject, ai_room_declares_perm, ai_room_declares_cap, cap,
            PermissionContext { origin_room: Some(room.as_str()), target_room: Some(permission_target.as_str()) },
        )).unwrap_or(Effective::Denied);
        match verdict {
            Effective::Granted => {
                if let SessionJob::PostRoomMedia { authorizations, .. } = &mut job {
                    let capture = super::super::information_flow::agent_context(room.as_str())
                        .and_then(|context| authorization(room, permission_target, id, &context));
                    match capture {
                        Ok(auth) => { authorizations.retain(|auth| auth.capability != *id); authorizations.push(auth); }
                        Err(error) => { answer_session_job(job, Err(error)); return None; }
                    }
                }
                with_a2app(|state| {
                    if let Some(group) = cap.group { state.permissions.record_access(&subject, group, versions::now_unix()); }
                    state.perms_dirty = true;
                });
            }
            Effective::NeedsPrompt => {
                if let SessionJob::PostRoomMedia { permission, .. } = &mut job { *permission = id; }
                queue_permission_prompt(cx, ui, subject, cap.group?, ParkedRequest::AiTool { room_id: room.clone(), job }, None);
                return None;
            }
            Effective::Denied | Effective::Undeclared => {
                if let SessionJob::PostRoomMedia { permission, .. } = &mut job { *permission = id; }
                let enable_writes = with_a2app(|state| can_enable_writes(state, &subject, cap, room, target)).unwrap_or(false);
                if enable_writes {
                    queue_permission_prompt(cx, ui, subject, cap.group?, ParkedRequest::AiTool { room_id: room.clone(), job }, None);
                    return None;
                }
                answer_session_job(job, Err(ai_tool_refused_text(cap.group?)));
                return None;
            }
        }
    }
    Some(job)
}

pub(super) fn can_enable_writes(state: &A2AppState, subject: &str, cap: &a2app_core::capabilities::Capability, room: &OwnedRoomId, target: &OwnedRoomId) -> bool {
    if state.permissions.matrix_write() || a2app_core::permissions::capability_room_access(cap) != Some(RoomAccess::Write) { return false; }
    let mut enabled = state.permissions.clone();
    enabled.set_matrix_write(true);
    !matches!(enabled.effective_capability_for_in_context(subject, ai_room_declares_perm, ai_room_declares_cap, cap,
        PermissionContext { origin_room: Some(room.as_str()), target_room: Some(target.as_str()) }), Effective::Denied | Effective::Undeclared)
}

fn authorization(room: &OwnedRoomId, target: &OwnedRoomId, capability: &str, context: &ContextId) -> Result<matrix::policy::MatrixAuthorization, String> {
    with_a2app(|state| {
        let mut auth = matrix::policy::MatrixAuthorization::new(&agent_subject(room.as_str()), capability, Some(room.as_str()), &state.permissions).with_flow(context.clone());
        auth.target_room = Some(target.to_string());
        auth
    }).ok_or_else(|| "Permissions are unavailable.".into())
}

fn pending(room: &OwnedRoomId, tool: &'static str, answer: Sender<Result<String, String>>) -> Result<(u64, ContextId, Arc<AtomicBool>), String> {
    let context = super::super::information_flow::agent_context(room.as_str())?;
    let epoch = flow::context_epoch(&context)?;
    let id = NEXT_AI_TOOL_ID.fetch_add(1, Ordering::Relaxed);
    let alive = Arc::new(AtomicBool::new(true));
    with_a2app(|state| state.ai_media_pending.insert(id, Pending { room: room.clone(), context: context.clone(), epoch, tool, answer, alive: alive.clone() }));
    Ok((id, context, alive))
}

pub(super) fn run(cx: &mut Cx, ui: &WidgetRef, room: &OwnedRoomId, job: SessionJob) {
    match job {
        SessionJob::DraftMedia { request, answer } => prepare(cx, ui, room, request, answer),
        SessionJob::DraftMessage { .. } | SessionJob::AttachMedia { .. } => {
            let cap = if matches!(job, SessionJob::DraftMessage { .. }) { "host.composer.insert" } else { "host.composer.attach" };
            let Some(job) = gate(cx, ui, room, job, room, &[cap]) else { return };
            let checked = super::super::information_flow::agent_context(room.as_str()).and_then(|context| {
                authorization(room, room, cap, &context)
            });
            let auth = match checked { Ok(auth) => auth, Err(error) => { answer_session_job(job, Err(error)); return; } };
            let action = match job {
                SessionJob::DraftMessage { text, answer } => RoomAction::DraftAgentMessage { text, answer },
                SessionJob::AttachMedia { draft_id, answer } => match draft(room, &draft_id) {
                    Ok(media) => RoomAction::AttachAgentMedia { media, answer },
                    Err(error) => { let _ = answer.send(Err(error)); return; }
                },
                _ => unreachable!(),
            };
            if let Err(error) = queue_authorized_room_action(cx, room.clone(), action.clone(), Some(auth)) {
                refuse_room_action(room, &action, &error);
            }
        }
        SessionJob::PostRoomMedia { ref room_id, .. } => {
            let target = match room_id.as_deref().map(OwnedRoomId::try_from).transpose() {
                Ok(target) => target.unwrap_or_else(|| room.clone()),
                Err(_) => { answer_session_job(job, Err("Invalid Matrix target room id.".into())); return; }
            };
            if let Err(error) = joined_room_name(cx, &target) { answer_session_job(job, Err(error)); return; }
            let mut caps = vec!["matrix.media.upload", "matrix.media.send"];
            if target != *room { caps.push("matrix.rooms.message.send"); }
            let Some(job) = gate(cx, ui, room, job, &target, &caps) else { return };
            let SessionJob::PostRoomMedia { draft_id, answer, mut authorizations, .. } = job else { unreachable!() };
            authorizations.sort_by_key(|auth| auth.capability != "matrix.media.send");
            let prepared = draft(room, &draft_id).and_then(|media| {
                Ok((media, authorizations))
            });
            let (media, authorizations) = match prepared { Ok(prepared) => prepared, Err(error) => { let _ = answer.send(Err(error)); return; } };
            let Ok((id, _, alive)) = pending(room, "post_room_media", answer) else { return };
            crate::sliding_sync::spawn_async_task(async move {
                let result = if alive.load(Ordering::Acquire) { media.post(&target, &authorizations).await.map(MediaOutcome::Posted) }
                    else { Err("This agent session stopped.".into()) };
                Cx::post_action(MediaResult { id, result });
            });
        }
        _ => unreachable!(),
    }
}

fn prepare(cx: &mut Cx, ui: &WidgetRef, room: &OwnedRoomId, request: MediaDraftRequest, answer: Sender<Result<String, String>>) {
    let context = match super::super::information_flow::agent_context(room.as_str()) { Ok(context) => context, Err(error) => { let _ = answer.send(Err(error)); return; } };
    let has_room = with_a2app(|state| state.ai_media_drafts.keys().filter(|(owner, _)| owner == room).count() < MAX_DRAFTS
        && state.ai_media_drafts.values().map(|draft| draft.media.bytes().len()).sum::<usize>() < MAX_DRAFT_BYTES).unwrap_or(false);
    if !has_room { let _ = answer.send(Err("Media draft storage is full. Stop this agent session to clear its drafts.".into())); return; }
    match &request.source {
        MediaSource::Base64(_) => {
            let result = PreparedMedia::from_base64(&request).map(Arc::new).and_then(|media| save_draft(room, &context, media));
            note_ai_tool_call(room, "draft_media", result.is_ok(), "");
            let _ = answer.send(result);
        }
        MediaSource::Url(url) => {
            let prepared = super::super::network::Request::parse(&serde_json::json!({ "url": url }));
            let network_request = match prepared { Ok(prepared) => prepared, Err(error) => { let _ = answer.send(Err(error)); return; } };
            let subject = agent_subject(room.as_str());
            let consent = with_a2app(|state| {
                if state.permissions.is_restricted(&subject) || state.permissions.state(&subject, Permission::Network) == GrantState::Denied
                    || state.permissions.capability_state(&subject, "network.http") == GrantState::Denied
                { return Err("Internet permission is blocked for this agent."); }
                Ok(state.permissions.is_url_allowed(&subject, url, PermissionContext { origin_room: Some(room.as_str()), target_room: Some(room.as_str()) }).then(|| state.permissions.clone()))
            }).unwrap_or(Err("Permissions are unavailable."));
            let consent = match consent {
                Ok(Some(consent)) => consent,
                Ok(None) => { queue_permission_prompt(cx, ui, subject, Permission::Network, ParkedRequest::AiTool { room_id: room.clone(), job: SessionJob::DraftMedia { request, answer } }, None); return; }
                Err(error) => { let _ = answer.send(Err(error.into())); return; }
            };
            let Ok((id, context, alive)) = pending(room, "draft_media", answer) else { return };
            let origin = Some(room.to_string());
            crate::sliding_sync::spawn_async_task(async move {
                let result = crate::a2app::network::run_bytes(network_request, context, subject, origin, consent, Some(alive)).await
                    .and_then(|response| {
                        if !(200..300).contains(&response.status) { return Err(format!("The media download returned HTTP {}.", response.status)); }
                        PreparedMedia::from_bytes(&request, response.body).map(|media| MediaOutcome::Draft(Arc::new(media)))
                    });
                Cx::post_action(MediaResult { id, result });
            });
        }
        MediaSource::MatrixUri(uri) => {
            let job = SessionJob::DraftMedia { request: request.clone(), answer };
            let Some(job) = gate(cx, ui, room, job, room, &["matrix.media.download"]) else { return };
            let SessionJob::DraftMedia { answer, .. } = job else { unreachable!() };
            // The worker resolves this URI only against actual attachments in
            // the authorized room, so an unknown URI cannot cross room policy.
            let labeled = flow::add_sources(&context, [super::super::information_flow::room_source(&context, room.as_str())])
                .and_then(|_| flow::add_influences(&context, [flow::Influence::RoomContent { account: context.account().into(), room: room.to_string() }]));
            if let Err(error) = labeled { let _ = answer.send(Err(error)); return; }
            let auth = match authorization(room, room, "matrix.media.download", &context) { Ok(auth) => auth, Err(error) => { let _ = answer.send(Err(error)); return; } };
            let uri = uri.clone();
            let Ok((id, _, _)) = pending(room, "draft_media", answer) else { return };
            crate::sliding_sync::spawn_async_task(async move {
                let result = crate::a2app::ai::media::download_mxc(&uri, auth).await
                    .and_then(|bytes| PreparedMedia::from_bytes(&request, bytes)).map(|media| MediaOutcome::Draft(Arc::new(media)));
                Cx::post_action(MediaResult { id, result });
            });
        }
        MediaSource::MatrixEvent { event_id, room_id } => {
            let source_room = match room_id.as_deref().map(OwnedRoomId::try_from).transpose() {
                Ok(source_room) => source_room.unwrap_or_else(|| room.clone()),
                Err(_) => { let _ = answer.send(Err("Invalid Matrix source room id.".into())); return; }
            };
            if let Err(error) = joined_room_name(cx, &source_room) { let _ = answer.send(Err(error)); return; }
            let job = SessionJob::DraftMedia { request: request.clone(), answer };
            let Some(job) = gate(cx, ui, room, job, &source_room, &["matrix.media.download"]) else { return };
            let SessionJob::DraftMedia { answer, .. } = job else { unreachable!() };
            let labeled = flow::add_sources(&context, [super::super::information_flow::room_source(&context, source_room.as_str())])
                .and_then(|_| flow::add_influences(&context, [flow::Influence::RoomContent { account: context.account().into(), room: source_room.to_string() }]));
            if let Err(error) = labeled { let _ = answer.send(Err(error)); return; }
            let auth = match authorization(room, &source_room, "matrix.media.download", &context) { Ok(auth) => auth, Err(error) => { let _ = answer.send(Err(error)); return; } };
            let event_id = event_id.clone();
            let Ok((id, _, _)) = pending(room, "draft_media", answer) else { return };
            crate::sliding_sync::spawn_async_task(async move {
                let result = crate::a2app::ai::media::download_event_media(&source_room, &event_id, auth).await
                    .and_then(|bytes| PreparedMedia::from_bytes(&request, bytes)).map(|media| MediaOutcome::Draft(Arc::new(media)));
                Cx::post_action(MediaResult { id, result });
            });
        }
    }
}

fn save_draft(room: &OwnedRoomId, context: &ContextId, media: Arc<PreparedMedia>) -> Result<String, String> {
    let epoch = flow::context_epoch(context)?;
    let sources = flow::labels(context)?;
    let influences = flow::influences(context)?;
    let id = format!("media_{:032x}", rand::random::<u128>());
    let summary = serde_json::json!({ "draft_id": id, "status": "drafted", "media": media.metadata() }).to_string();
    with_a2app(|state| {
        let bytes: usize = state.ai_media_drafts.values().map(|draft| draft.media.bytes().len()).sum();
        if bytes.saturating_add(media.bytes().len()) > MAX_DRAFT_BYTES || state.ai_media_drafts.keys().filter(|(owner, _)| owner == room).count() >= MAX_DRAFTS {
            return Err("Media draft storage is full.".into());
        }
        state.ai_media_drafts.insert((room.clone(), id), Draft { media, context: context.clone(), epoch, sources, influences });
        Ok(summary)
    }).unwrap_or_else(|| Err("Mini Apps is unavailable.".into()))
}

pub(super) fn apply_result(response: MediaResult) {
    let Some(pending) = with_a2app(|state| state.ai_media_pending.remove(&response.id)).flatten() else { return };
    pending.alive.store(false, Ordering::Release);
    let result = super::super::information_flow::current_context(&pending.context)
        .and_then(|_| flow::ensure_context_epoch(&pending.context, pending.epoch))
        .and_then(|_| response.result)
        .and_then(|result| match result {
            MediaOutcome::Draft(media) => save_draft(&pending.room, &pending.context, media),
            MediaOutcome::Posted(text) => Ok(text),
        });
    note_ai_tool_call(&pending.room, pending.tool, result.is_ok(), "");
    let _ = pending.answer.send(result);
}

pub(super) fn cancel(state: &mut A2AppState, room: &OwnedRoomId) {
    state.ai_media_drafts.retain(|(owner, _), _| owner != room);
    state.ai_media_pending.retain(|_, pending| {
        if pending.room != *room { return true; }
        pending.alive.store(false, Ordering::Release);
        let _ = pending.answer.send(Err("This agent session stopped.".into()));
        false
    });
    if state.room_action.as_ref().is_some_and(|action| action.room_id == *room
        && matches!(action.action, RoomAction::AttachAgentMedia { .. } | RoomAction::DraftAgentMessage { .. }))
        && let Some(action) = state.room_action.take()
    { refuse_room_action(room, &action.action, "This agent session stopped."); }
}

pub(super) fn refuse_room_action(room: &RoomId, action: &RoomAction, error: &str) {
    match action {
        RoomAction::AttachAgentMedia { answer, .. } => finish_composer(room, "attach_media", answer, Err(error.into())),
        RoomAction::DraftAgentMessage { answer, .. } => finish_composer(room, "draft_message", answer, Err(error.into())),
        _ => {}
    }
}

pub(crate) fn finish_composer(room: &RoomId, tool: &str, answer: &Sender<Result<String, String>>, result: Result<String, String>) {
    // This callback can run inside state teardown; recording is handled by
    // the room screen after ordinary consumption, not by this send helper.
    let _ = (room, tool);
    let _ = answer.send(result);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{channel, TryRecvError};

    struct Fixture {
        previous_state: Option<A2AppState>,
        previous_account: Option<String>,
        rooms: [OwnedRoomId; 2],
        contexts: [ContextId; 2],
    }

    impl Fixture {
        fn new() -> Self {
            let previous_state = A2APP.with(|state| state.replace(None));
            let account = format!("media-test-{:032x}", rand::random::<u128>());
            let previous_account = crate::a2app::information_flow::TEST_ACCOUNT.with(|value| value.replace(Some(account)));
            initialize_state(AppRegistry::new(Vec::new()), PermissionStore::default(), A2AppPersistedState::default(), Default::default());
            let rooms = [OwnedRoomId::try_from("!media:test").unwrap(), OwnedRoomId::try_from("!other:test").unwrap()];
            let contexts = rooms.each_ref().map(|room| crate::a2app::information_flow::prepare_agent(room.as_str()).unwrap());
            Self { previous_state, previous_account, rooms, contexts }
        }

        fn media(&self) -> Arc<PreparedMedia> {
            let request = MediaDraftRequest {
                source: MediaSource::Base64("bWVkaWE=".into()), filename: "fixture.txt".into(),
                mime_type: "text/plain".into(), caption: Some("Fixture".into()),
            };
            Arc::new(PreparedMedia::from_base64(&request).unwrap())
        }

        fn save(&self, room: usize, media: Arc<PreparedMedia>) -> String {
            let result = save_draft(&self.rooms[room], &self.contexts[room], media).unwrap();
            serde_json::from_str::<serde_json::Value>(&result).unwrap()["draft_id"].as_str().unwrap().to_string()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            with_a2app(|state| {
                for room in &self.rooms { cancel(state, room); }
            });
            for context in &self.contexts { let _ = flow::remove_context(context); }
            A2APP.with(|state| { state.replace(self.previous_state.take()); });
            crate::a2app::information_flow::TEST_ACCOUNT.with(|value| { value.replace(self.previous_account.take()); });
        }
    }

    #[test]
    fn handles_cannot_cross_rooms_or_accounts() {
        let fixture = Fixture::new();
        let media = fixture.media();
        let id = fixture.save(0, media.clone());
        assert!(Arc::ptr_eq(&draft(&fixture.rooms[0], &id).unwrap(), &media));
        assert!(draft(&fixture.rooms[1], &id).is_err(), "knowing a handle must not authorize another room's draft");
        crate::a2app::information_flow::TEST_ACCOUNT.with(|value| { value.replace(Some("another-account".into())); });
        assert!(draft(&fixture.rooms[0], &id).is_err(), "same room id under another account must not reuse media");
    }

    #[test]
    fn drafts_and_late_workers_cannot_reach_a_replacement_activation() {
        let fixture = Fixture::new();
        let media = fixture.media();
        let draft_id = fixture.save(0, media.clone());
        let epoch = flow::context_epoch(&fixture.contexts[0]).unwrap();
        let (answer, result) = channel();
        let (worker_id, _, alive) = pending(&fixture.rooms[0], "draft_media", answer).unwrap();
        flow::remove_context_for_activation(&fixture.contexts[0], epoch).unwrap();
        assert!(draft(&fixture.rooms[0], &draft_id).is_err());
        flow::register_context(&fixture.contexts[0]).unwrap();
        flow::begin_agent_session(&fixture.contexts[0]).unwrap();
        let replacement_epoch = flow::context_epoch(&fixture.contexts[0]).unwrap();
        assert_ne!(epoch, replacement_epoch);
        assert!(draft(&fixture.rooms[0], &draft_id).is_err(), "restarting the same room must not revive the old handle");
        apply_result(MediaResult { id: worker_id, result: Ok(MediaOutcome::Draft(media)) });
        assert!(result.try_recv().unwrap().is_err(), "a completed old worker must report failure");
        assert!(!alive.load(Ordering::Acquire));
        assert_eq!(with_a2app(|state| state.ai_media_drafts.len()).unwrap(), 1,
            "an old result must not install another draft into the replacement");
        assert!(flow::ensure_context_epoch(&fixture.contexts[0], replacement_epoch).is_ok());
    }

    #[test]
    fn retained_media_restores_its_sources_and_untrusted_influence() {
        let fixture = Fixture::new();
        let source = flow::Source::Room { account: fixture.contexts[0].account().into(), room: fixture.rooms[1].to_string() };
        let influence = flow::Influence::InternetOrigin("https://media.example.org".into());
        flow::add_sources(&fixture.contexts[0], [source.clone()]).unwrap();
        flow::add_influences(&fixture.contexts[0], [influence.clone()]).unwrap();
        let other_label = flow::labels(&fixture.contexts[1]).unwrap();
        let other_influences = flow::influences(&fixture.contexts[1]).unwrap();
        let id = fixture.save(0, fixture.media());
        let epoch = flow::context_epoch(&fixture.contexts[0]).unwrap();
        // Exercise the stored draft provenance independently of the current
        // live label; ordinary turns keep their label, but must also retain it
        // when a draft itself is the source of a later attachment or post.
        flow::reset_agent_session_provenance(&fixture.contexts[0]).unwrap();
        assert!(!flow::labels(&fixture.contexts[0]).unwrap().contains(&source));
        assert!(!flow::influences(&fixture.contexts[0]).unwrap().contains(&influence));
        let (answer, _) = channel();
        restore_provenance(&fixture.rooms[0], &SessionJob::PostRoomMedia {
            draft_id: id, room_id: None, permission: "matrix.media.upload", authorizations: Vec::new(), answer,
        }).unwrap();
        assert!(flow::labels(&fixture.contexts[0]).unwrap().contains(&source));
        assert!(flow::influences(&fixture.contexts[0]).unwrap().contains(&influence));
        assert_eq!(flow::context_epoch(&fixture.contexts[0]).unwrap(), epoch);
        assert_eq!(flow::labels(&fixture.contexts[1]).unwrap(), other_label,
            "restoring a draft must not taint another agent");
        assert_eq!(flow::influences(&fixture.contexts[1]).unwrap(), other_influences);
    }

    #[test]
    fn draft_count_limit_applies_again_when_an_async_download_finishes() {
        let fixture = Fixture::new();
        let media = fixture.media();
        let (answer, result) = channel();
        let (id, _, alive) = pending(&fixture.rooms[0], "draft_media", answer).unwrap();
        for _ in 0..MAX_DRAFTS { fixture.save(0, media.clone()); }
        assert!(save_draft(&fixture.rooms[0], &fixture.contexts[0], media.clone()).is_err());
        // Starting a download before the final free slot is consumed must not
        // let its later completion bypass the count limit.
        apply_result(MediaResult { id, result: Ok(MediaOutcome::Draft(media.clone())) });
        assert!(result.try_recv().unwrap().unwrap_err().contains("storage is full"));
        assert!(!alive.load(Ordering::Acquire));
        assert_eq!(with_a2app(|state| state.ai_media_drafts.len()).unwrap(), MAX_DRAFTS);
        let other = fixture.save(1, media);
        assert!(draft(&fixture.rooms[1], &other).is_ok(), "the per-room count must not block another room");
    }

    #[test]
    fn cancelling_media_answers_waiters_once_and_preserves_other_rooms() {
        let fixture = Fixture::new();
        let media = fixture.media();
        let removed = fixture.save(0, media.clone());
        let kept = fixture.save(1, media.clone());
        let (answer, result) = channel();
        let (id, _, alive) = pending(&fixture.rooms[0], "draft_media", answer).unwrap();
        let (other_answer, other_result) = channel();
        let (other_id, _, other_alive) = pending(&fixture.rooms[1], "post_room_media", other_answer).unwrap();
        let (composer_answer, composer_result) = channel();
        with_a2app(|state| {
            state.room_action = Some(PendingRoomAction {
                room_id: fixture.rooms[0].clone(),
                action: RoomAction::AttachAgentMedia { media: media.clone(), answer: composer_answer },
                since: Instant::now(), authorization: None, close_after: None,
            });
            // This runs while the RefCell is borrowed: cancellation callbacks
            // must send their answer without borrowing A2APP again.
            cancel(state, &fixture.rooms[0]);
            assert!(state.room_action.is_none());
        });
        assert_eq!(result.try_recv().unwrap().unwrap_err(), "This agent session stopped.");
        assert_eq!(composer_result.try_recv().unwrap().unwrap_err(), "This agent session stopped.");
        assert!(!alive.load(Ordering::Acquire));
        assert!(other_alive.load(Ordering::Acquire));
        assert!(draft(&fixture.rooms[0], &removed).is_err());
        assert!(draft(&fixture.rooms[1], &kept).is_ok());
        apply_result(MediaResult { id, result: Ok(MediaOutcome::Draft(media)) });
        assert!(matches!(result.try_recv(), Err(TryRecvError::Disconnected)), "a late result cannot answer a cancelled caller twice");
        assert!(matches!(other_result.try_recv(), Err(TryRecvError::Empty)));
        apply_result(MediaResult { id: other_id, result: Ok(MediaOutcome::Posted("other sent".into())) });
        assert_eq!(other_result.try_recv().unwrap().unwrap(), "other sent");
        assert!(!other_alive.load(Ordering::Acquire));
    }

    #[test]
    fn cancelling_a_queued_text_draft_delivers_its_failure_without_reentry() {
        let fixture = Fixture::new();
        let (answer, result) = channel();
        with_a2app(|state| {
            state.room_action = Some(PendingRoomAction {
                room_id: fixture.rooms[0].clone(), action: RoomAction::DraftAgentMessage { text: "draft".into(), answer },
                since: Instant::now(), authorization: None, close_after: None,
            });
            cancel(state, &fixture.rooms[0]);
        });
        assert_eq!(result.try_recv().unwrap().unwrap_err(), "This agent session stopped.");
        assert!(with_a2app(|state| state.room_action.is_none()).unwrap());
    }

    #[test]
    fn an_upload_once_receipt_survives_the_next_prompt_but_cannot_authorize_another_job() {
        let fixture = Fixture::new();
        let subject = agent_subject(fixture.rooms[0].as_str());
        let once = with_a2app(|state| {
            state.permissions.set_matrix_write(true);
            state.permissions.set_global_policy(RoomAccess::Write, PolicyDecision::Ask);
            // Keep prompts queued so this test models the next prompt waiting
            // for the user without needing a rendered permission modal.
            state.permission_batch_busy = true;
            let id = state.permissions.grant_scoped(&subject, Permission::MatrixMedia, Some("matrix.media.upload"),
                RoomScope::room(fixture.rooms[0].as_str()), GrantDuration::RobrixSession, Some(fixture.rooms[0].as_str())).unwrap();
            state.permissions.mark_request_once(id);
            id
        }).unwrap();
        let receipt = authorization(&fixture.rooms[0], &fixture.rooms[0], "matrix.media.upload", &fixture.contexts[0]).unwrap();
        with_a2app(|state| { state.permissions.remove_scoped_grant(once); });
        assert!(with_a2app(|state| receipt.permits_request(&state.permissions)).unwrap());
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let ui = WidgetRef::empty();
        let job = |answer, authorizations| SessionJob::PostRoomMedia {
            draft_id: "draft".into(), room_id: Some(fixture.rooms[1].to_string()),
            permission: "matrix.media.send", authorizations, answer,
        };
        let (answer, _) = channel();
        let resumed = gate(&mut cx, &ui, &fixture.rooms[0], job(answer, vec![receipt.clone()]), &fixture.rooms[1], &["matrix.media.upload"]);
        let Some(SessionJob::PostRoomMedia { authorizations, .. }) = resumed else { panic!("the in-progress job must keep its consumed upload approval") };
        assert_eq!(authorizations.len(), 1);
        let (answer, result) = channel();
        assert!(gate(&mut cx, &ui, &fixture.rooms[0], job(answer, Vec::new()), &fixture.rooms[1], &["matrix.media.upload"]).is_none(),
            "a fresh job must ask again because it has no request-bound receipt");
        assert!(matches!(result.try_recv(), Err(TryRecvError::Empty)));
        assert_eq!(with_a2app(|state| state.prompts.len()).unwrap(), 1);

        with_a2app(|state| state.permissions.set(&subject, Permission::MatrixMedia, GrantState::Denied));
        let (answer, result) = channel();
        assert!(gate(&mut cx, &ui, &fixture.rooms[0], job(answer, vec![receipt]), &fixture.rooms[1], &["matrix.media.upload"]).is_none());
        assert!(result.try_recv().unwrap().is_err(), "explicit Deny must revoke the captured receipt");
        assert_eq!(with_a2app(|state| state.prompts.len()).unwrap(), 1, "Deny must not create a new prompt");
    }

    #[test]
    fn a_send_once_receipt_cannot_follow_a_changed_destination() {
        let fixture = Fixture::new();
        let subject = agent_subject(fixture.rooms[0].as_str());
        let once = with_a2app(|state| {
            state.permissions.set_matrix_write(true);
            state.permissions.set_global_policy(RoomAccess::Write, PolicyDecision::Ask);
            state.permission_batch_busy = true;
            let id = state.permissions.grant_scoped(&subject, Permission::MatrixMedia, Some("matrix.media.send"),
                RoomScope::room(fixture.rooms[0].as_str()), GrantDuration::RobrixSession, Some(fixture.rooms[0].as_str())).unwrap();
            state.permissions.mark_request_once(id);
            id
        }).unwrap();
        let receipt = authorization(&fixture.rooms[0], &fixture.rooms[0], "matrix.media.send", &fixture.contexts[0]).unwrap();
        with_a2app(|state| { state.permissions.remove_scoped_grant(once); });
        let (answer, result) = channel();
        let job = SessionJob::PostRoomMedia {
            draft_id: "draft".into(), room_id: Some(fixture.rooms[1].to_string()),
            permission: "matrix.media.send", authorizations: vec![receipt], answer,
        };
        let mut cx = Cx::new(Box::new(|_, _| {}));
        assert!(gate(&mut cx, &WidgetRef::empty(), &fixture.rooms[0], job, &fixture.rooms[1], &["matrix.media.send"]).is_none(),
            "an approval for the old destination must not skip the new destination's prompt");
        assert!(matches!(result.try_recv(), Err(TryRecvError::Empty)));
        assert_eq!(with_a2app(|state| state.prompts.len()).unwrap(), 1);
    }

    #[test]
    fn staging_drafts_needs_no_send_or_room_sharing_authority() {
        let fixture = Fixture::new();
        let room = &fixture.rooms[0];
        let app_context = ContextId::App { account: fixture.contexts[0].account().into(), app: "draft-test-app".into(), room: Some(room.to_string()) };
        flow::register_context(&app_context).unwrap();
        for context in [&fixture.contexts[0], &app_context] {
            let subject = context.app().map(str::to_string).unwrap_or_else(|| agent_subject(room.as_str()));
            flow::add_sources(context, [flow::Source::Room { account: context.account().into(), room: fixture.rooms[1].to_string() }]).unwrap();
            flow::add_influences(context, [flow::Influence::InternetOrigin("https://draft-source.test".into())]).unwrap();
            let recipient = flow::Recipient::MatrixRoom { account: context.account().into(), room: room.to_string() };
            assert!(flow::ensure_allowed(context, &recipient).is_err());
            let media = fixture.media();
            let (answer, _) = channel();
            let actions = [
                ("host.composer.insert", RoomAction::DraftAgentMessage { text: "review me".into(), answer }, serde_json::json!({ "text": "review me" })),
                ("host.composer.attach", RoomAction::StagePreparedMedia { media: media.clone(), reply: AppComposerCompletion::new(Reply { heap_key: 0, req_id: 1 }) }, serde_json::json!({ "media": media.metadata() })),
            ];
            for (capability, action, payload) in actions {
                with_a2app(|state| {
                    state.permissions.set(&subject, Permission::MatrixMedia, GrantState::Denied);
                    state.permissions.set(&subject, Permission::MatrixRoomSend, GrantState::Denied);
                    state.permissions.set(&subject, Permission::MatrixRoomsSend, GrantState::Denied);
                    state.permissions.set_room_policy(room.as_str(), RoomAccess::Write, PolicyDecision::Deny);
                    state.permissions.grant_scoped(&subject, Permission::RobrixComposer, Some(capability), RoomScope::room(room.as_str()),
                        GrantDuration::RobrixSession, Some(room.as_str())).unwrap();
                    let cap = a2app_core::capabilities::by_id(capability).unwrap();
                    assert!(!can_enable_writes(state, &subject, cap, room, room), "drafting must never ask to enable sending");
                });
                let auth = with_a2app(|state| matrix::policy::MatrixAuthorization::new(&subject, capability, Some(room.as_str()), &state.permissions)).unwrap().with_flow(context.clone());
                let sensitive = flow::SensitiveAction { kind: capability.into(), target: room.to_string() };
                let epoch = flow::context_epoch(context).unwrap();
                if matches!(context, ContextId::Agent { .. }) {
                    assert!(flow::check_exact_action_for_activation(context, epoch, &sensitive, &payload).is_err());
                    let (request, influences) = pending_exact_decision(context, epoch, &sensitive).unwrap();
                    flow::grant_exact_action_for_activation(context, request, &influences, epoch).unwrap();
                } else {
                    let review = flow::prepare_effect_for_activation(context, epoch, None, Some(&sensitive), &payload).unwrap();
                    assert!(!review.allowed, "untrusted draft content still needs local exact-action review");
                    flow::approve_effect_once(&review).unwrap();
                }
                let pending = PendingRoomAction { room_id: room.clone(), action, since: Instant::now(), authorization: Some(auth), close_after: None };
                pending.check().expect("approved local draft must work while sends and remote sharing stay denied");
                assert!(!with_a2app(|state| state.permissions.matrix_write()).unwrap());
                assert!(flow::ensure_allowed(context, &recipient).is_err(), "draft review must not release data to the room");
            }
        }
        flow::remove_context(&app_context).unwrap();
    }

    #[test]
    fn enabling_room_changes_never_overrides_media_or_composer_denials() {
        let fixture = Fixture::new();
        let subject = agent_subject(fixture.rooms[0].as_str());
        for id in ["matrix.media.send", "matrix.rooms.message.send"] {
            let cap = a2app_core::capabilities::by_id(id).unwrap();
            let target = &fixture.rooms[1];
            with_a2app(|state| {
                state.permissions = PermissionStore::default();
                assert!(!state.permissions.matrix_write());
                assert!(can_enable_writes(state, &subject, cap, &fixture.rooms[0], target),
                    "{id} can offer to enable room changes when no other rule blocks it");
                assert!(!state.permissions.matrix_write(), "checking eligibility must not change the saved switch");
                state.permissions.set_room_policy(target.as_str(), RoomAccess::Write, PolicyDecision::Deny);
                assert!(!can_enable_writes(state, &subject, cap, &fixture.rooms[0], target), "a hard room block wins");
                state.permissions.set_room_policy(target.as_str(), RoomAccess::Write, PolicyDecision::Ask);
                state.permissions.set_room_spaces(target.as_str(), vec!["!protected:test".into()]);
                state.permissions.set_space_policy("!protected:test", RoomAccess::Write, PolicyDecision::Deny);
                assert!(!can_enable_writes(state, &subject, cap, &fixture.rooms[0], target), "a parent space block wins");
                state.permissions.set_space_policy("!protected:test", RoomAccess::Write, PolicyDecision::Ask);
                state.permissions.set_capability(&subject, id, GrantState::Denied);
                assert!(!can_enable_writes(state, &subject, cap, &fixture.rooms[0], target), "an explicit capability denial wins");
                state.permissions.set_capability(&subject, id, GrantState::Ask);
                state.permissions.set(&subject, cap.group.unwrap(), GrantState::Denied);
                assert!(!can_enable_writes(state, &subject, cap, &fixture.rooms[0], target), "an explicit permission denial wins");
                state.permissions.set(&subject, cap.group.unwrap(), GrantState::Ask);
                assert!(can_enable_writes(state, &subject, cap, &fixture.rooms[0], target));
                state.permissions.set_matrix_write(true);
                assert!(!can_enable_writes(state, &subject, cap, &fixture.rooms[0], target), "an already enabled switch needs no enable action");
            });
        }
    }

    #[test]
    fn parked_media_prompts_use_source_rooms_and_the_current_permission_target() {
        let fixture = Fixture::new();
        let parked = |job| ParkedRequest::AiTool { room_id: fixture.rooms[0].clone(), job };
        let request = MediaDraftRequest {
            source: MediaSource::MatrixEvent { event_id: "$attachment".into(), room_id: Some(fixture.rooms[1].to_string()) },
            filename: "fixture.txt".into(), mime_type: "text/plain".into(), caption: None,
        };
        let (answer, _) = channel();
        let download = parked(SessionJob::DraftMedia { request, answer });
        assert_eq!(parked_rooms(&download), (Some(fixture.rooms[0].to_string()), Some(fixture.rooms[1].to_string())));
        assert_eq!(parked_capability(&download).unwrap().id, "matrix.media.download");
        for (permission, target) in [
            ("matrix.media.upload", &fixture.rooms[0]),
            ("matrix.media.send", &fixture.rooms[1]),
            ("matrix.rooms.message.send", &fixture.rooms[1]),
        ] {
            let (answer, _) = channel();
            let post = parked(SessionJob::PostRoomMedia {
                draft_id: "draft".into(), room_id: Some(fixture.rooms[1].to_string()),
                permission, authorizations: Vec::new(), answer,
            });
            assert_eq!(parked_rooms(&post), (Some(fixture.rooms[0].to_string()), Some(target.to_string())),
                "{permission} must show the target for this permission phase");
            assert_eq!(parked_capability(&post).unwrap().id, permission);
        }
    }
}
