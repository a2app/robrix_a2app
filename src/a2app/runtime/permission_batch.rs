//! Consent batches bind every answer to the displayed app activation and call.
use super::*;

pub(super) struct PermissionSetup {
    request: SplashHostRequest,
    context: a2app_core::information_flow::ContextId,
    epoch: u64,
    perms: Vec<Permission>,
    pending: HashMap<u64, Permission>,
    answers: serde_json::Map<String, serde_json::Value>,
}

fn gesture_is_current(state: &A2AppState, context: &a2app_core::information_flow::ContextId) -> bool {
    state.permission_gestures.get(context).is_some_and(|(epoch, time)|
        time.elapsed() < Duration::from_secs(1)
            && a2app_core::information_flow::ensure_context_epoch(context, *epoch).is_ok())
}

pub(super) fn clear_dismissals(subject: &str) {
    with_a2app(|state| {
        state.permissions.clear_tool_denials_for(subject);
        state.dismissed_prompts.retain(|(app, _)| app != subject);
        state.dismissed_net_hosts.retain(|(app, _)| app != subject);
        state.dismissed_effects.retain(|(context, _)| context.app() != Some(subject));
    });
}

pub(super) fn forget_gesture(subject: &str) {
    with_a2app(|state| state.permission_gestures.retain(|context, _| context.app() != Some(subject)));
}

pub(super) fn retry_parked(subject: &str, perm: Permission, parked: &ParkedRequest) {
    let context = match parked {
        ParkedRequest::Bridge(Some(request)) => super::super::information_flow::context_for_heap(request.heap_key).ok(),
        ParkedRequest::AppMedia(post) => post.activation().map(|(_, _, context, _)| context),
        _ => None,
    };
    let Some(context) = context else { return };
    with_a2app(|state| {
        if gesture_is_current(state, &context) {
            if perm == Permission::McpTools { state.permissions.clear_tool_denials_for(subject); }
            state.dismissed_prompts.remove(&(subject.into(), perm));
            if let Some(url) = parked_network_url(parked)
                && let Ok(url) = url::Url::parse(&url) && let Some(host) = url.host_str()
            { state.dismissed_net_hosts.remove(&(subject.into(), host.into())); }
        }
    });
}

fn effect_key(review: &a2app_core::information_flow::EffectReview) -> String {
    let payload = serde_json::from_str::<serde_json::Value>(&review.payload).unwrap_or_default();
    serde_json::json!([review.recipient, review.action, payload["operation"]]).to_string()
}

pub(super) fn effect_dismissed(review: &a2app_core::information_flow::EffectReview) -> bool {
    with_a2app(|state| {
        let key = (review.context.clone(), effect_key(review));
        if gesture_is_current(state, &review.context) { state.dismissed_effects.remove(&key); }
        state.dismissed_effects.contains(&key)
    }).unwrap_or(true)
}

pub(super) fn dismiss_effect(review: &a2app_core::information_flow::EffectReview) {
    with_a2app(|state| {
        state.permission_gestures.remove(&review.context);
        state.dismissed_effects.insert((review.context.clone(), effect_key(review)));
    });
}

fn identity(prompt: &PermissionPrompt) -> Option<(a2app_core::information_flow::ContextId, u64)> {
    if let Some(flow) = &prompt.flow {
        if matches!(flow, FlowContinuation::Generated { .. }) { return None; }
        let review = flow.review();
        return Some((review.context.clone(), review.epoch));
    }
    prompt.activations.first().map(|(_, _, context, epoch)| (context.clone(), *epoch))
}

fn live(prompt: &PermissionPrompt) -> bool {
    if with_a2app(|state| room_session::prompt_is_closing(state, prompt)).unwrap_or(true) { return false; }
    if let Some(flow) = &prompt.flow { return flow.can_prompt() && !flow.is_cancelled(); }
    !prompt.parked.is_empty() && prompt.parked.iter().all(|parked| match parked {
        ParkedRequest::AppMedia(post) => post.can_prompt(),
        ParkedRequest::Bridge(Some(request)) => bridge_activation_can_prompt(request, &prompt.activations),
        _ => true,
    })
}

fn ordinary_dismissed(state: &A2AppState, prompt: &PermissionPrompt) -> bool {
    state.dismissed_prompts.contains(&(prompt.subject.clone(), prompt.perm))
        || prompt.parked.iter().filter_map(parked_network_url).any(|url|
            url::Url::parse(&url).ok().and_then(|url| url.host_str().map(str::to_owned))
                .is_some_and(|host| state.dismissed_net_hosts.contains(&(prompt.subject.clone(), host))))
}

fn prepare(cx: &mut Cx, ui: &WidgetRef, mut prompt: PermissionPrompt) -> Option<PermissionPrompt> {
    if !live(&prompt) { refuse_prompt(cx, prompt, "This permission request was cancelled."); return None; }
    // A refusal also applies to work queued before that refusal. Checking
    // only when enqueueing lets queued overflow open the same popup again.
    let dismissed = if let Some(flow) = &prompt.flow { effect_dismissed(flow.review()) }
        else { with_a2app(|state| ordinary_dismissed(state, &prompt)).unwrap_or(true) };
    if dismissed { refuse_prompt(cx, prompt, "This request was not approved."); return None; }
    if let Some(flow) = prompt.flow.as_mut() {
        let review = flow.review();
        let updated = serde_json::from_str(&review.payload).map_err(|_| "This permission request is invalid.".to_owned())
            .and_then(|payload| a2app_core::information_flow::prepare_effect_for_activation(
                &review.context, review.epoch, review.recipient.as_ref(), review.action.as_ref(), &payload));
        match updated {
            Ok(updated) => flow.replace_review(updated),
            Err(error) => { refuse_prompt(cx, prompt, &error); return None; }
        }
        if flow.review().allowed && flow_bridge_permission_granted(flow) {
            resume_flow(cx, ui, prompt.flow.take().unwrap());
            return None;
        }
    } else if prompt.setup.is_none()
        && with_a2app(|state| prompt_already_granted(state, &prompt)).unwrap_or(false)
        && prompt.parked.iter().all(|request| matches!(request, ParkedRequest::Bridge(_) | ParkedRequest::AppMedia(_)))
    {
        for parked in prompt.parked {
            match parked {
                ParkedRequest::Bridge(Some(request)) => replay_bridge_request(cx, ui, request),
                ParkedRequest::AppMedia(post) => post.resume(cx, ui),
                _ => {},
            }
        }
        return None;
    }
    Some(prompt)
}

fn info(state: &A2AppState, rooms: Option<&RoomsListRef>, prompt: &PermissionPrompt) -> PermissionPromptInfo {
    if let Some(flow) = &prompt.flow {
        let mut info = flow_prompt_info(state, rooms, flow);
        info.prompt_id = prompt.id;
        PermissionPromptInfo::Flow(info)
    } else {
        let mut info = prompt_info_for(state, rooms, &prompt.subject, prompt.perm, &prompt.parked, prompt.tool.as_ref());
        info.prompt_id = prompt.id;
        info.enable_writes = prompt.enable_writes;
        // Explicit setup describes a declared group, rather than pretending
        // that one specific immutable outgoing action has already happened.
        if prompt.setup.is_some() {
            info.can_allow_once = false;
            info.collection = matches!(prompt.perm, Permission::MatrixRoomsList | Permission::MatrixRoomsRead | Permission::MatrixRoomsSend | Permission::MatrixSpaces);
            if info.collection { info.scope = Some(RoomScope::AllRooms); info.scope_targets.clear(); }
        }
        PermissionPromptInfo::Ordinary(info)
    }
}

pub(super) fn show_next(cx: &mut Cx, ui: &WidgetRef) {
    show_next_permission_prompt(cx, ui);
}

pub(super) fn show_next_ordinary(cx: &mut Cx, ui: &WidgetRef) {
    if with_a2app(|state| permission_modal_is_open(state)).unwrap_or(true) { return; }
    with_a2app(|state| state.permission_batch_busy = true);
    let rooms = cx.has_global::<RoomsListRef>().then(|| cx.get_global::<RoomsListRef>().clone());
    let first = loop {
        let next = with_a2app(|state| state.prompts.pop_front()).flatten();
        let Some(next) = next else {
            with_a2app(|state| state.permission_batch_busy = false);
            return;
        };
        if let Some(prompt) = prepare(cx, ui, next) { break prompt; }
    };
    let first_identity = identity(&first);
    let candidates = with_a2app(|state| {
        let mut mine = Vec::new();
        let mut rest = VecDeque::new();
        for next in state.prompts.drain(..) {
            if mine.len() < 31 && next.subject == first.subject && first_identity.is_some() && identity(&next) == first_identity {
                mine.push(next);
            } else { rest.push_back(next); }
        }
        state.prompts = rest;
        mine
    }).unwrap_or_default();
    let mut prompts = vec![first];
    for prompt in candidates { if let Some(prompt) = prepare(cx, ui, prompt) { prompts.push(prompt); } }
    let group = with_a2app(|state| PermissionPromptGroupInfo {
        group_id: prompts[0].id,
        requests: prompts.iter().map(|prompt| info(state, rooms.as_ref(), prompt)).collect(),
    }).unwrap();
    let mut prompts = prompts.into_iter();
    with_a2app(|state| {
        state.active_prompt = prompts.next();
        state.active_prompt_batch = prompts.collect();
        state.permission_batch_busy = false;
    });
    let widget = ui.mini_app_permission_prompt(cx, ids!(a2app_permission_modal.content));
    if group.requests.len() > 1 { widget.show_group(cx, &group); }
    else { match &group.requests[0] {
        PermissionPromptInfo::Ordinary(info) => widget.show(cx, info),
        PermissionPromptInfo::Flow(info) => widget.show_flow(cx, info),
    } }
    let ready = with_a2app(|state| state.policy_spaces_ready
        && state.policy_spaces_revision == matrix::spaces::policy_spaces_revision()).unwrap_or(false);
    widget.set_space_membership_ready(cx, ready);
    ui.modal(cx, ids!(a2app_permission_modal)).open(cx);
}

pub(super) fn answer_group(cx: &mut Cx, ui: &WidgetRef, response: PermissionPromptGroupResponse) {
    room_session::cancel_closing_prompts(cx, ui);
    let matches = with_a2app(|state| state.active_prompt.as_ref().is_some_and(|prompt| prompt.id == response.group_id)
        && !state.active_prompt_batch.is_empty()).unwrap_or(false);
    if !matches { return; }
    let prompts = with_a2app(|state| {
        let ids = state.active_prompt.iter().chain(state.active_prompt_batch.iter()).map(|prompt| prompt.id).collect::<HashSet<_>>();
        if !complete_group_response(&ids, &response.responses) { return None; }
        let mut prompts = state.active_prompt.take().into_iter().collect::<Vec<_>>();
        prompts.append(&mut state.active_prompt_batch);
        state.permission_batch_busy = true;
        Some(prompts)
    }).flatten();
    let Some(prompts) = prompts else { return };
    // Validate every member before the first grant or replay. A response is
    // never redirected to another activation or an undisplayed new request.
    if prompts.iter().any(|prompt| !live(prompt)) {
        for prompt in prompts { refuse_prompt(cx, prompt, "This permission request changed. Try again."); }
        ui.modal(cx, ids!(a2app_permission_modal)).close(cx);
    } else {
        for prompt in prompts {
            let answer = response.responses.iter().find(|answer| answer.prompt_id == prompt.id).unwrap().clone();
            with_a2app(|state| state.active_prompt = Some(prompt));
            answer_permission_prompt(cx, ui, answer);
        }
    }
    with_a2app(|state| state.permission_batch_busy = false);
    show_next(cx, ui);
}

fn complete_group_response(ids: &HashSet<u64>, responses: &[PermissionPromptResponse]) -> bool {
    let answered = responses.iter().map(|answer| answer.prompt_id).collect::<HashSet<_>>();
    responses.len() == ids.len() && *ids == answered
        && responses.iter().all(|answer| !matches!(answer.answer, PermissionPromptAction::None))
}

pub(super) fn queue_setup(cx: &mut Cx, ui: &WidgetRef, subject: String, request: SplashHostRequest, perms: Vec<Permission>) {
    let Ok(context) = super::super::information_flow::context_for_heap(request.heap_key) else { Broker::respond_denied(cx, &request); return };
    let Ok(epoch) = a2app_core::information_flow::context_epoch(&context) else { Broker::respond_denied(cx, &request); return };
    if !instances::context_can_prompt(&context, epoch) { Broker::respond_denied(cx, &request); return; }
    let key = (request.heap_key, request.req_id);
    let mut setup = PermissionSetup { request: request.clone(), context: context.clone(), epoch, perms: perms.clone(), pending: HashMap::new(), answers: serde_json::Map::new() };
    let mut prompts = Vec::new();
    for perm in perms {
        let mut item = request.clone();
        item.args_json = serde_json::json!({ "perm": perm.as_str() }).to_string();
        let parked = ParkedRequest::Bridge(Some(item));
        retry_parked(&subject, perm, &parked);
        let (effective, dismissed, enable_writes) = with_a2app(|state| {
            let Some(manifest) = state.registry.get(&subject) else { return (Effective::Undeclared, true, false) };
            let effective = services::permission_setup_status(&state.permissions, manifest, perm,
                PermissionContext { origin_room: context.room(), target_room: context.room() });
            let enable_writes = parked_can_enable_writes(state, &subject, perm, &parked)
                && setup_group_can_enable_writes(&state.permissions, manifest, perm,
                    PermissionContext { origin_room: context.room(), target_room: context.room() });
            (effective, state.dismissed_prompts.contains(&(subject.clone(), perm)), enable_writes)
        }).unwrap_or((Effective::Undeclared, true, false));
        if effective == Effective::Granted {
            setup.answers.insert(perm.as_str().into(), true.into());
        } else if !dismissed && (effective == Effective::NeedsPrompt || enable_writes) {
            let id = next_permission_prompt_id();
            setup.pending.insert(id, perm);
            prompts.push(PermissionPrompt { id, setup: Some(key), subject: subject.clone(), perm, parked: vec![parked], tool: None, flow: None,
                enable_writes, activations: vec![(request.heap_key, request.req_id, context.clone(), epoch)] });
        } else { setup.answers.insert(perm.as_str().into(), false.into()); }
    }
    let empty = setup.pending.is_empty();
    with_a2app(|state| {
        state.permission_setups.insert(key, setup);
        state.prompts.extend(prompts);
    });
    if empty { finish_setup(cx, key); }
    show_next(cx, ui);
}

fn setup_group_can_enable_writes(permissions: &PermissionStore, manifest: &MiniAppManifest,
    permission: Permission, context: PermissionContext<'_>) -> bool
{
    let mut enabled = permissions.clone();
    enabled.set_matrix_write(true);
    !matches!(services::permission_setup_status(&enabled, manifest, permission, context),
        Effective::Denied | Effective::Undeclared)
}

pub(super) fn complete_setup(cx: &mut Cx, key: (usize, u64), id: u64, granted: bool) {
    let finished = with_a2app(|state| {
        let Some(setup) = state.permission_setups.get_mut(&key) else { return false };
        record_setup_answer(setup, id, granted)
    }).unwrap_or(false);
    if finished { finish_setup(cx, key); }
}

fn record_setup_answer(setup: &mut PermissionSetup, id: u64, granted: bool) -> bool {
    let Some(perm) = setup.pending.remove(&id) else { return false };
    setup.answers.insert(perm.as_str().into(), granted.into());
    setup.pending.is_empty()
}

fn recheck_setup_answers(setup: &mut PermissionSetup, permissions: &PermissionStore, manifest: Option<&MiniAppManifest>) {
    for perm in &setup.perms {
        let was_approved = setup.answers.get(perm.as_str()).and_then(serde_json::Value::as_bool) == Some(true);
        let usable = was_approved && manifest.is_some_and(|manifest|
            services::permission_setup_status(permissions, manifest, *perm,
                PermissionContext { origin_room: setup.context.room(), target_room: setup.context.room() }) == Effective::Granted);
        setup.answers.insert(perm.as_str().into(), usable.into());
    }
}

fn finish_setup(cx: &mut Cx, key: (usize, u64)) {
    let Some(mut setup) = with_a2app(|state| state.permission_setups.remove(&key)).flatten() else { return };
    with_a2app(|state| state.broker.declined(&setup.request));
    if super::super::information_flow::current_context(&setup.context).is_err()
        || a2app_core::information_flow::ensure_context_epoch(&setup.context, setup.epoch).is_err()
        || super::super::information_flow::context_for_heap(setup.request.heap_key).as_ref() != Ok(&setup.context)
    { return; }
    // Successfully creating an allowance is not enough: a capability block
    // or changed room policy may still prevent the app from using it.
    with_a2app(|state| {
        let manifest = setup.context.app().and_then(|subject| state.registry.get(subject));
        recheck_setup_answers(&mut setup, &state.permissions, manifest);
    });
    let granted = setup.perms.iter().all(|perm| setup.answers.get(perm.as_str()).and_then(serde_json::Value::as_bool) == Some(true));
    let subject = setup.context.app().unwrap_or_default();
    for perm in &setup.perms { apply_permission_to_running(cx, &WidgetRef::empty(), subject, *perm); }
    services::respond(cx, Reply { heap_key: setup.request.heap_key, req_id: setup.request.req_id }, Ok(&serde_json::json!({ "granted": granted, "permissions": setup.answers }).to_string()));
}

pub(super) fn refuse_prompt(cx: &mut Cx, mut prompt: PermissionPrompt, reason: &str) {
    if let Some(key) = prompt.setup { complete_setup(cx, key, prompt.id, false); }
    else {
        for parked in prompt.parked {
            if let ParkedRequest::Bridge(Some(request)) = &parked
                && !bridge_activation_is_live(request, &prompt.activations)
            { continue; }
            refuse_parked_request(cx, prompt.perm, parked);
        }
    }
    if let Some(flow) = prompt.flow.take() { refuse_flow(cx, flow, reason); }
}

pub(super) fn cancel_subject(cx: &mut Cx, ui: &WidgetRef, subject: &str) {
    let (prompts, closed) = with_a2app(|state| {
        let closed = state.active_prompt.as_ref().is_some_and(|prompt| prompt.subject == subject);
        let mut mine = Vec::new();
        if closed {
            mine.extend(state.active_prompt.take());
            mine.append(&mut state.active_prompt_batch);
        }
        let (queued, rest): (Vec<_>, Vec<_>) = state.prompts.drain(..).partition(|prompt| prompt.subject == subject);
        mine.extend(queued);
        state.prompts = rest.into();
        (mine, closed)
    }).unwrap_or_default();
    for prompt in prompts { refuse_prompt(cx, prompt, "This permission request was cancelled."); }
    if closed { ui.modal(cx, ids!(a2app_permission_modal)).close(cx); }
}

/// Retire only the initiating room, including its queued setup and effect reviews.
/// A changed grouped display receives fresh IDs so a late answer cannot target survivors.
pub(super) fn cancel_room(cx: &mut Cx, ui: &WidgetRef, account: &str, room: &str) {
    let (prompts, closed) = with_a2app(|state| {
        let matches = |prompt: &PermissionPrompt| room_session::prompt_originates_in(prompt, account, room);
        let closed = state.active_prompt.iter().chain(state.active_prompt_batch.iter()).any(&matches);
        let mut mine = Vec::new();
        if closed {
            let mut displayed = state.active_prompt.take().into_iter().collect::<Vec<_>>();
            displayed.append(&mut state.active_prompt_batch);
            for mut prompt in displayed {
                if matches(&prompt) { mine.push(prompt); }
                else {
                    let old_id = prompt.id;
                    prompt.id = next_permission_prompt_id();
                    // Setup completion keys also capture the displayed member ID.
                    if let Some(key) = prompt.setup
                        && let Some(setup) = state.permission_setups.get_mut(&key)
                        && let Some(permission) = setup.pending.remove(&old_id)
                    { setup.pending.insert(prompt.id, permission); }
                    state.prompts.push_front(prompt);
                }
            }
        }
        let (queued, rest): (Vec<_>, Vec<_>) = state.prompts.drain(..).partition(matches);
        mine.extend(queued);
        state.prompts = rest.into();
        (mine, closed)
    }).unwrap_or_default();
    for prompt in prompts { refuse_prompt(cx, prompt, "The initiating room closed before this request was approved."); }
    if closed { ui.modal(cx, ids!(a2app_permission_modal)).close(cx); }
}

pub(super) fn sweep(cx: &mut Cx, ui: &WidgetRef) {
    room_session::cancel_closing_prompts(cx, ui);
    let stale = with_a2app(|state| {
        state.active_prompt.as_ref().filter(|prompt|
            !live_with_state(state, prompt) || state.active_prompt_batch.iter().any(|sibling| !live_with_state(state, sibling)))
            .map(|prompt| prompt.subject.clone())
    }).flatten();
    if let Some(subject) = stale { cancel_subject(cx, ui, &subject); show_next(cx, ui); }
}

// Avoid re-entering runtime state while checking a Generated continuation.
fn live_with_state(state: &A2AppState, prompt: &PermissionPrompt) -> bool {
    if room_session::prompt_is_closing(state, prompt) { return false; }
    if let Some(flow) = &prompt.flow {
        return flow.can_prompt_with_state(state)
            && !matches!(flow, FlowContinuation::Worker(worker) if worker.is_cancelled());
    }
    !prompt.parked.is_empty() && prompt.parked.iter().all(|parked| match parked {
        ParkedRequest::AppMedia(post) => post.can_prompt() && post.permits_send(&state.permissions),
        ParkedRequest::Bridge(Some(request)) => bridge_activation_can_prompt(request, &prompt.activations),
        _ => true,
    })
}

pub(super) fn refresh_targets(cx: &mut Cx, ui: &WidgetRef) {
    let group = with_a2app(|state| {
        let mut targets = Vec::new();
        for prompt in state.active_prompt.iter().chain(state.active_prompt_batch.iter()) {
            let rooms = if let Some(flow) = &prompt.flow {
                let review = flow.review();
                let payload = serde_json::from_str::<serde_json::Value>(&review.payload).unwrap_or_default();
                let capability = review.action.as_ref().and_then(|action| a2app_core::capabilities::by_id(&action.kind))
                    .or_else(|| payload["operation"].as_str().and_then(a2app_core::capabilities::by_id));
                flow_prompt_scope(state, flow, capability, &payload).1
            } else if prompt.setup.is_some() && matches!(prompt.perm, Permission::MatrixRoomsList | Permission::MatrixRoomsRead | Permission::MatrixRoomsSend | Permission::MatrixSpaces) {
                Vec::new()
            } else { parked_scope_targets(state, &prompt.parked) };
            targets.push((prompt.id, rooms));
        }
        (targets, state.policy_spaces_ready && state.policy_spaces_revision == matrix::spaces::policy_spaces_revision())
    });
    if let Some((targets, ready)) = group {
        let widget = ui.mini_app_permission_prompt(cx, ids!(a2app_permission_modal.content));
        widget.set_space_membership_ready(cx, ready);
        if targets.len() > 1 { widget.update_group_scope_targets(cx, &targets); }
        else if let Some((_, targets)) = targets.first() { widget.update_scope_targets(cx, targets); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(service: &str, args: serde_json::Value) -> SplashHostRequest {
        SplashHostRequest { app_tag: "reminder".into(), heap_key: 0, req_id: 1, service: service.into(), args_json: args.to_string(), may_prompt: true }
    }

    fn setup(perms: Vec<Permission>) -> PermissionSetup {
        PermissionSetup {
            request: request("permissions.request", serde_json::json!({ "perms": perms.iter().map(|perm| perm.as_str()).collect::<Vec<_>>() })),
            context: a2app_core::information_flow::ContextId::App { account: "test".into(), app: "reminder".into(), room: Some("!room:test".into()) },
            epoch: 1, perms, pending: HashMap::new(), answers: serde_json::Map::new(),
        }
    }

    #[test]
    fn group_answers_require_one_decision_per_displayed_member() {
        let ids = HashSet::from([1, 2]);
        let mut responses = vec![
            PermissionPromptResponse { prompt_id: 1, answer: PermissionPromptAction::AllowOnce },
            PermissionPromptResponse { prompt_id: 2, answer: PermissionPromptAction::NotNow },
        ];
        assert!(complete_group_response(&ids, &responses));
        assert!(!complete_group_response(&ids, &responses[..1]));
        responses[1].prompt_id = 1;
        assert!(!complete_group_response(&ids, &responses));
        responses[1].prompt_id = 3;
        assert!(!complete_group_response(&ids, &responses));
        responses[1].prompt_id = 2;
        responses[1].answer = PermissionPromptAction::None;
        assert!(!complete_group_response(&ids, &responses));
    }

    #[test]
    fn setup_partial_answers_complete_once_and_duplicates_cannot_change_a_denial() {
        let mut setup = setup(vec![Permission::Location, Permission::Camera]);
        setup.pending = HashMap::from([(1, Permission::Location), (2, Permission::Camera)]);
        assert!(!record_setup_answer(&mut setup, 1, false));
        assert!(!record_setup_answer(&mut setup, 1, true));
        assert_eq!(setup.answers["location"], false);
        assert!(record_setup_answer(&mut setup, 2, true));
        assert!(!record_setup_answer(&mut setup, 2, false));
        assert_eq!(setup.answers["camera"], true);
        assert!(setup.pending.is_empty());
    }

    #[test]
    fn accepted_setup_does_not_claim_access_after_capability_or_room_policy_revocation() {
        let mut manifest = builtin::stock("reminder").unwrap();
        manifest.permissions = vec!["matrix-room-read".into()];
        manifest.capabilities = vec!["matrix.room.messages.read".into(), "matrix.room.members.read".into()];
        let mut permissions = PermissionStore::default();
        permissions.grant_scoped(&manifest.id, Permission::MatrixRoomRead, None, RoomScope::AllRooms, GrantDuration::Always, None).unwrap();
        let mut setup = setup(vec![Permission::MatrixRoomRead]);
        setup.answers.insert("matrix-room-read".into(), true.into());
        recheck_setup_answers(&mut setup, &permissions, Some(&manifest));
        assert_eq!(setup.answers["matrix-room-read"], true);
        permissions.set_capability(&manifest.id, "matrix.room.members.read", GrantState::Denied);
        recheck_setup_answers(&mut setup, &permissions, Some(&manifest));
        assert_eq!(setup.answers["matrix-room-read"], false);
        permissions.set_capability(&manifest.id, "matrix.room.members.read", GrantState::Ask);
        setup.answers.insert("matrix-room-read".into(), true.into());
        permissions.set_room_policy("!room:test", RoomAccess::Read, PolicyDecision::Deny);
        recheck_setup_answers(&mut setup, &permissions, Some(&manifest));
        assert_eq!(setup.answers["matrix-room-read"], false);
    }

    #[test]
    fn setup_write_enablement_does_not_offer_a_group_with_another_blocked_ability() {
        let mut manifest = builtin::stock("roll-call").unwrap();
        manifest.capabilities = vec!["matrix.room.message.send".into(), "matrix.room.message.reply".into()];
        let mut permissions = PermissionStore::default();
        let context = PermissionContext { origin_room: Some("!room:test"), target_room: Some("!room:test") };
        assert!(services::can_enable_permission_writes(&permissions, &manifest, Permission::MatrixRoomSend, context));
        assert!(setup_group_can_enable_writes(&permissions, &manifest, Permission::MatrixRoomSend, context));
        permissions.set_capability(&manifest.id, "matrix.room.message.reply", GrantState::Denied);
        assert!(services::can_enable_permission_writes(&permissions, &manifest, Permission::MatrixRoomSend, context),
            "the send ability alone is usable after enabling writes");
        assert!(!setup_group_can_enable_writes(&permissions, &manifest, Permission::MatrixRoomSend, context),
            "the complete setup group still contains an explicitly blocked reply ability");
    }

    #[test]
    fn queued_ordinary_requests_respect_denials_recorded_after_enqueue() {
        initialize_background_test(builtin::stock("reminder").unwrap());
        let mut prompt = PermissionPrompt { id: 1, setup: None, subject: "reminder".into(), perm: Permission::Location,
            parked: vec![ParkedRequest::Bridge(Some(request("location.get", serde_json::Value::Null)))],
            tool: None, flow: None, enable_writes: false, activations: Vec::new() };
        with_a2app(|state| {
            assert!(!ordinary_dismissed(state, &prompt));
            state.dismissed_prompts.insert(("reminder".into(), Permission::Location));
            assert!(ordinary_dismissed(state, &prompt));
            state.dismissed_prompts.clear();
            prompt.perm = Permission::Network;
            prompt.parked = vec![ParkedRequest::Bridge(Some(request("network.http", serde_json::json!({ "url": "https://example.org/page" }))))];
            state.dismissed_net_hosts.insert(("reminder".into(), "example.org".into()));
            assert!(ordinary_dismissed(state, &prompt));
            state.dismissed_net_hosts.clear();
            assert!(!ordinary_dismissed(state, &prompt));
        });
        A2APP.with(|state| *state.borrow_mut() = None);
    }

    #[test]
    fn effect_denial_key_keeps_targets_distinct_without_keeping_private_text() {
        let context = a2app_core::information_flow::ContextId::App { account: "@test:server".into(), app: "search".into(), room: Some("!room:server".into()) };
        let root = std::env::temp_dir().join(format!("robrix-effect-denial-{}-{}", std::process::id(), next_permission_prompt_id()));
        let mut registry = a2app_core::information_flow::Registry::open(&root).unwrap();
        registry.register_context(&context).unwrap();
        registry.add_sources(&context, [a2app_core::information_flow::Source::Account { account: "@test:server".into() }]).unwrap();
        let epoch = registry.context_epoch(&context).unwrap();
        let first = registry.prepare_effect_for_activation(&context, epoch, Some(&a2app_core::information_flow::Recipient::network_origin("https://example.org").unwrap()), None,
            &serde_json::json!({ "operation": "network.http", "body": "private body" })).unwrap();
        let key = effect_key(&first);
        assert!(!key.contains("private body"));
        let second = registry.prepare_effect_for_activation(&context, epoch, Some(&a2app_core::information_flow::Recipient::network_origin("https://other.example").unwrap()), None,
            &serde_json::json!({ "operation": "network.http", "body": "private body" })).unwrap();
        assert_ne!(key, effect_key(&second));
        std::fs::remove_dir_all(root).unwrap();
    }
}
