//! Room-close expiry uses the initiating compartment, independently of targets.
use super::*;
use a2app_core::information_flow::{self as flow, ContextId, SharingDuration};

pub(super) fn approval_origin(context: &ContextId) -> Option<String> {
    context.room().filter(|room| RoomId::parse(*room).is_ok()).map(str::to_owned)
}

/// A single display may contain several parked calls; room expiry is offered
/// only when every native-captured origin agrees and supports user prompting.
pub(super) fn parked_approval_origin(parked: &[ParkedRequest]) -> Option<String> {
    let origin = parked.first().and_then(|request| parked_rooms(request).0)
        .filter(|room| RoomId::parse(room).is_ok())?;
    parked.iter().all(|request| parked_rooms(request).0.as_deref() == Some(origin.as_str())
        && !matches!(request, ParkedRequest::Bridge(Some(request)) if !request.may_prompt)).then_some(origin)
}

pub(super) fn sharing_duration(context: &ContextId, duration: GrantDuration) -> Result<SharingDuration, String> {
    Ok(match duration {
        GrantDuration::RoomSession => SharingDuration::RoomSession {
            account: context.account().into(),
            room: approval_origin(context).ok_or("This request has no initiating room session.")?,
        },
        GrantDuration::RobrixSession => SharingDuration::RobrixSession,
        GrantDuration::Always => SharingDuration::Permanent,
    })
}

pub(super) fn approve_review(review: &flow::EffectReview, answer: &PermissionPromptAction, allow_once: bool) -> Result<(), String> {
    match answer {
        PermissionPromptAction::AllowFlowOnce if allow_once => flow::approve_effect_once(review),
        PermissionPromptAction::AllowFlowSession => flow::approve_effect_session(review, SharingDuration::RobrixSession),
        PermissionPromptAction::AllowFlow { duration: GrantDuration::Always } => flow::approve_effect_always(review),
        PermissionPromptAction::AllowFlow { duration } => flow::approve_effect_session(review, sharing_duration(&review.context, *duration)?),
        PermissionPromptAction::AllowFlowScoped { scope, duration } =>
            flow::approve_effect_scoped(review, scope.clone(), sharing_duration(&review.context, *duration)?),
        _ => Err("This request was not approved.".into()),
    }
}

fn context_originates_in(context: &ContextId, account: &str, room: &str) -> bool {
    context.account() == account && context.room() == Some(room)
}

pub(super) fn prompt_originates_in(prompt: &PermissionPrompt, account: &str, room: &str) -> bool {
    if let Some(flow) = &prompt.flow { return context_originates_in(&flow.review().context, account, room); }
    if !prompt.activations.is_empty() {
        return prompt.activations.iter().any(|(_, _, context, _)| context_originates_in(context, account, room));
    }
    // Agent calls have no Splash activation. Their native room is the origin,
    // even when the requested capability targets a different room.
    prompt.parked.iter().any(|parked| parked_rooms(parked).0.as_deref() == Some(room))
}

pub(super) fn prompt_is_closing(state: &A2AppState, prompt: &PermissionPrompt) -> bool {
    if state.closing_room_sessions.is_empty() { return false; }
    let Ok(account) = super::super::information_flow::account() else { return true };
    state.closing_room_sessions.iter().any(|(owner, room)| owner == &account && prompt_originates_in(prompt, &account, room.as_str()))
}

pub(super) fn cancel_closing_prompts(cx: &mut Cx, ui: &WidgetRef) {
    let Ok(account) = super::super::information_flow::account() else { return };
    let rooms = with_a2app(|state| state.closing_room_sessions.iter().filter(|(owner, _)| owner == &account).map(|(_, room)| room.clone()).collect::<Vec<_>>()).unwrap_or_default();
    for room in rooms { permission_batch::cancel_room(cx, ui, &account, room.as_str()); }
}

/// Invalidates captured approvals immediately, before action-batch processing
/// can replay a stale popup answer ahead of the queued RoomClosed operation.
pub(super) fn begin(cx: &mut Cx, room: &OwnedRoomId) -> Option<String> {
    let account = super::super::information_flow::account().ok()?;
    with_a2app(|state| { state.closing_room_sessions.insert((account.clone(), room.clone())); });
    {
        if let Err(error) = flow::close_room_session(&account, room.as_str()) {
            enqueue_popup_notification(error, PopupKind::Error, Some(6.0));
        }
        for context in flow::contexts().unwrap_or_default().into_iter().filter(|snapshot|
            context_originates_in(&snapshot.context, &account, room.as_str()))
        { let _ = flow::remove_context_for_activation(&context.context, context.epoch); }
    }
    let pending = with_a2app(|state| {
        state.permissions.clear_room_session(room.as_str());
        state.permission_gestures.retain(|context, _| context.room() != Some(room.as_str()));
        state.dismissed_effects.retain(|(context, _)| context.room() != Some(room.as_str()));
        state.room_action.take_if(|pending| pending.room_id == *room
            || pending.authorization.as_ref().and_then(|authorization| authorization.flow_context.as_ref())
                .is_some_and(|context| context.room() == Some(room.as_str())))
    }).flatten();
    if let Some(pending) = pending {
        refuse_composer_action(cx, &pending.room_id, &pending.action, "The room closed before the draft was attached.");
    }
    publish_grants(cx);
    Some(account)
}

/// Retire only this room's instances and work. Closing one room is not a
/// global policy edit and must preserve other rooms' active compartments.
pub(super) fn finish(cx: &mut Cx, ui: &WidgetRef, room: &OwnedRoomId, captured_account: Option<&str>) {
    let Some(account) = captured_account else { return };
    with_a2app(|state| { state.closing_room_sessions.remove(&(account.into(), room.clone())); });
    if super::super::information_flow::account().as_deref() != Ok(account) { return; }
    permission_batch::cancel_room(cx, ui, &account, room.as_str());
    super::super::background::room_closed(cx, room.as_str());
    #[cfg(unix)]
    abort_ai_room_work(cx, ui, room);
    let apps = with_a2app(|state| state.registry.iter().map(|manifest| manifest.id.clone()).collect::<Vec<_>>()).unwrap_or_default();
    let keys = apps.iter().flat_map(|app| instances::keys_of_app(app)).filter(|key|
        instances::context_of_key(key).is_some_and(|context| context_originates_in(&context, &account, room.as_str())))
        .collect::<Vec<_>>();
    for key in keys {
        // A modal borrowing this exact instance releases it too. Another
        // room's instance of the same app retains its surface and state.
        if instances::surface_of(&key) == Some(instances::Surface::Modal) {
            host_pane(cx, ui).close_active(cx, true);
            with_a2app(|state| state.foreground_app = None);
            ui.modal(cx, ids!(mini_app_host_modal)).close(cx);
        }
        if instances::terminate(cx, &key) { app_stopped(cx, &key.0); }
    }
    publish_grants(cx);
    prune_hook_subs();
    show_next_permission_prompt(cx, ui);
    ui.redraw(cx);
}

#[cfg(test)]
mod tests {
    use super::*;
    const ORIGIN: &str = "!origin:room-session-test";
    const OTHER: &str = "!other:room-session-test";

    struct Fixture {
        cx: Cx,
        account: String,
        manifest: MiniAppManifest,
        keys: Vec<instances::InstanceKey>,
        previous_state: Option<A2AppState>,
        previous_account: Option<String>,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let account = format!("@{name}:room-session-test");
            let previous_account = super::super::super::information_flow::TEST_ACCOUNT.with(|value| value.replace(Some(account.clone())));
            let previous_state = A2APP.with(|value| value.replace(None));
            let mut manifest = builtin::stock("simple-watcher").unwrap();
            manifest.id = format!("runtime-{name}");
            manifest.permissions = vec![Permission::MatrixRoomWatch.as_str().into()];
            manifest.capabilities = vec!["on_room_message".into()];
            manifest.source = r#"
fn ask_watch(){
    host.request("events.subscribe", {event:"on_room_message"}, fn(r){
        ui.status.set_text("callback")
    })
}
fn on_room_message(payload){ ui.hits.set_text("delivered") }
View{status := Label{text:"waiting"} hits := Label{text:"none"}}
"#.into();
            initialize_background_test(manifest.clone());
            let mut cx = Cx::new(Box::new(|_, _| {}));
            let (template, root) = cx.with_vm(|vm| {
                makepad_widgets::script_mod(vm);
                makepad_code_editor::script_mod(vm);
                crate::shared::script_mod(vm);
                super::super::super::host_set::script_mod(vm);
                let template = script_eval!(vm, { mod.widgets.MiniAppHost {} });
                let template = vm.bx.heap.new_object_ref(template.as_object().unwrap());
                let root = script_eval!(vm, { mod.widgets.View {} });
                (template, WidgetRef::script_from_value(vm, root))
            });
            makepad_widgets::widget_tree::set_ui_root(&mut cx, &root);
            instances::set_host_template(template);
            let keys = [ORIGIN, OTHER].map(|room| (manifest.id.clone(), Some(room.try_into().unwrap()))).to_vec();
            for key in &keys {
                let host = instances::ensure(&mut cx, key, &manifest, &[]).unwrap();
                instances::adopt(&mut cx, key, root.widget_uid(), instances::Surface::Tab).unwrap();
                host.widget(&cx, ids!(status));
                host.widget(&cx, ids!(hits));
            }
            Self { cx, account, manifest, keys, previous_state, previous_account }
        }

        fn context(&self, index: usize) -> ContextId { instances::context_of_key(&self.keys[index]).unwrap() }
        fn heap(&self, index: usize) -> usize { instances::heap_of(&self.keys[index]).unwrap() }
        fn host(&self, index: usize) -> WidgetRef { instances::host_of(&self.keys[index]).unwrap() }
        fn prompt(&mut self, index: usize) -> PermissionPrompt {
            let host = self.host(index);
            assert!(host.widget(&self.cx, ids!(splash)).borrow_mut::<Splash>().unwrap()
                .call_script_fn(&mut self.cx, id!(ask_watch), &[]));
            self.cx.with_vm_and_async(|_| {});
            let request = makepad_widgets::splash_host::take_splash_host_requests().into_iter()
                .find(|request| request.heap_key == self.heap(index) && request.service == "events.subscribe").unwrap();
            let context = self.context(index);
            let epoch = flow::context_epoch(&context).unwrap();
            PermissionPrompt { setup: None, id: next_permission_prompt_id(), subject: self.manifest.id.clone(),
                perm: Permission::MatrixRoomWatch, activations: vec![(request.heap_key, request.req_id, context, epoch)],
                parked: vec![ParkedRequest::Bridge(Some(request))], tool: None, flow: None, enable_writes: false }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            for key in &self.keys { instances::terminate(&mut self.cx, key); }
            instances::clear_host_template();
            A2APP.with(|value| { value.replace(self.previous_state.take()); });
            super::super::super::information_flow::TEST_ACCOUNT.with(|value| { value.replace(self.previous_account.take()); });
            makepad_widgets::splash_host::take_splash_host_requests();
        }
    }

    #[test]
    fn flow_room_duration_uses_origin_and_expires_without_broadening_target_scope() {
        let mut fixture = Fixture::new("flow-room-duration");
        let context = fixture.context(0);
        let epoch = flow::context_epoch(&context).unwrap();
        flow::add_influences(&context, [flow::Influence::InternetOrigin("https://review.example".into())]).unwrap();
        let action = flow::SensitiveAction { kind: "matrix.rooms.message.send".into(), target: OTHER.into() };
        let destination = flow::Recipient::MatrixRoom { account: fixture.account.clone(), room: OTHER.into() };
        let payload = serde_json::to_value(matrix_sdk::ruma::events::room::message::RoomMessageEventContent::text_plain("reviewed message")).unwrap();
        let review = flow::prepare_effect_for_activation(&context, epoch, Some(&destination), Some(&action), &payload).unwrap();
        assert!(!review.allowed);
        approve_review(&review, &PermissionPromptAction::AllowFlowScoped {
            scope: RoomScope::room(OTHER), duration: GrantDuration::RoomSession,
        }, true).unwrap();
        let grant = flow::effect_authorities().unwrap().into_iter().find(|grant| grant.context == context).unwrap();
        assert_eq!(grant.duration, SharingDuration::RoomSession { account: fixture.account.clone(), room: ORIGIN.into() });
        assert_eq!(grant.room_scope, Some(RoomScope::room(OTHER)));
        let clipboard = flow::Recipient::Clipboard;
        let clipboard_action = flow::SensitiveAction { kind: "clipboard.write".into(), target: "clipboard".into() };
        let clipboard_review = flow::prepare_effect_for_activation(&context, epoch, Some(&clipboard), Some(&clipboard_action),
            &serde_json::json!({"text":"different reviewed output"})).unwrap();
        approve_review(&clipboard_review, &PermissionPromptAction::AllowFlow { duration: GrantDuration::RoomSession }, true).unwrap();
        assert_eq!(flow::effect_authorities().unwrap().iter().filter(|grant| grant.context == context).count(), 2,
            "unscoped and scoped popup answers both retain the initiating room lifetime");
        begin(&mut fixture.cx, &OTHER.try_into().unwrap());
        assert!(flow::effect_authorities().unwrap().iter().any(|grant| grant.context == context));
        assert!(flow::ensure_context_epoch(&context, epoch).is_ok(), "closing a destination does not retire its reader's origin");
        begin(&mut fixture.cx, &ORIGIN.try_into().unwrap());
        assert!(!flow::effect_authorities().unwrap().iter().any(|grant| grant.context == context));
        assert!(approve_review(&review, &PermissionPromptAction::AllowFlowScoped {
            scope: RoomScope::AllRooms, duration: GrantDuration::RoomSession,
        }, true).is_err(), "an expired popup must not create another grant");
    }

    #[test]
    fn non_room_origins_and_mixed_parked_origins_cannot_offer_room_duration() {
        let context = |room: Option<&str>| ContextId::App { account: "alice".into(), app: "test".into(), room: room.map(str::to_owned) };
        assert!(sharing_duration(&context(None), GrantDuration::RoomSession).is_err());
        assert!(sharing_duration(&context(Some("background-run-1")), GrantDuration::RoomSession).is_err());
        assert_eq!(approval_origin(&context(Some(ORIGIN))), Some(ORIGIN.into()));
        let parked = |room: Option<&str>, may_prompt| ParkedRequest::Bridge(Some(SplashHostRequest {
            app_tag: a2app_core::manifest::instance_tag("test", room), heap_key: 0, req_id: 1,
            service: "events.subscribe".into(), args_json: "{}".into(), may_prompt,
        }));
        assert_eq!(parked_approval_origin(&[parked(Some(ORIGIN), true), parked(Some(ORIGIN), true)]), Some(ORIGIN.into()));
        for requests in [vec![parked(Some(ORIGIN), true), parked(Some(OTHER), true)],
            vec![parked(Some(ORIGIN), true), parked(None, true)], vec![parked(Some(ORIGIN), false)]]
        { assert_eq!(parked_approval_origin(&requests), None); }
    }

    #[test]
    fn close_rejects_same_pass_ordinary_approval_and_drops_stale_callbacks() {
        let mut fixture = Fixture::new("stale-room-answer");
        let prompt = fixture.prompt(0);
        let id = prompt.id;
        let host = fixture.host(0);
        with_a2app(|state| state.active_prompt = Some(prompt));
        on_room_closed(&mut fixture.cx, &ORIGIN.try_into().unwrap());
        // The deferred RoomClosed operation has not run yet.
        answer_permission_prompt(&mut fixture.cx, &WidgetRef::empty(), PermissionPromptResponse { prompt_id: id,
            answer: PermissionPromptAction::AllowScoped { scope: RoomScope::AllRooms, duration: GrantDuration::RoomSession, network: None } });
        fixture.cx.with_vm_and_async(|_| {});
        with_a2app(|state| {
            assert!(state.permissions.scoped_grants(&fixture.manifest.id).is_empty());
            assert!(state.active_prompt.is_none());
        });
        assert_eq!(host.widget(&fixture.cx, ids!(status)).text(), "waiting", "retired requests must not enter any old or replacement callback");
        apply_op(&mut fixture.cx, &WidgetRef::empty(), A2AppOp::RoomClosed { room_id: ORIGIN.try_into().unwrap(), account: Some(fixture.account.clone()) });
        assert!(instances::heap_of(&fixture.keys[0]).is_none());
        assert!(instances::heap_of(&fixture.keys[1]).is_some());
    }

    #[test]
    fn closing_one_room_preserves_other_instance_grants_and_hook_delivery() {
        let mut fixture = Fixture::new("independent-room-hooks");
        let heaps = [fixture.heap(0), fixture.heap(1)];
        let surviving_host = fixture.host(1);
        let context = fixture.context(1);
        let epoch = flow::context_epoch(&context).unwrap();
        let lifetime = instances::lifetime_of_heap(heaps[1]).unwrap();
        with_a2app(|state| {
            for (index, room) in [ORIGIN, OTHER].into_iter().enumerate() {
                state.permissions.grant_scoped(&fixture.manifest.id, Permission::MatrixRoomWatch, Some("on_room_message"),
                    RoomScope::room(room), GrantDuration::RoomSession, Some(room)).unwrap();
                state.hook_subs.insert(heaps[index], HookSubscription { app_id: fixture.manifest.id.clone(),
                    room_id: Some(room.try_into().unwrap()), hooks: HashSet::from(["on_room_message"]) });
            }
        });
        on_room_closed(&mut fixture.cx, &ORIGIN.try_into().unwrap());
        apply_op(&mut fixture.cx, &WidgetRef::empty(), A2AppOp::RoomClosed { room_id: ORIGIN.try_into().unwrap(), account: Some(fixture.account.clone()) });
        assert!(instances::heap_of(&fixture.keys[0]).is_none());
        assert_eq!(instances::heap_of(&fixture.keys[1]), Some(heaps[1]));
        assert_eq!(instances::host_of(&fixture.keys[1]), Some(surviving_host.clone()));
        assert!(lifetime.load(std::sync::atomic::Ordering::Acquire));
        assert!(flow::ensure_context_epoch(&context, epoch).is_ok());
        with_a2app(|state| {
            assert!(!state.hook_subs.contains_key(&heaps[0]));
            assert!(state.hook_subs.contains_key(&heaps[1]));
            let capability = a2app_core::capabilities::by_id("on_room_message").unwrap();
            assert_eq!(state.permissions.effective_capability_in_context(&fixture.manifest, capability,
                PermissionContext { origin_room: Some(OTHER), target_room: Some(OTHER) }), Effective::Granted);
            assert_eq!(state.permissions.effective_capability_in_context(&fixture.manifest, capability,
                PermissionContext { origin_room: Some(ORIGIN), target_room: Some(ORIGIN) }), Effective::NeedsPrompt);
        });
        deliver_room_hooks(&mut fixture.cx, &WidgetRef::empty(), Vec::new(), Vec::new(),
            vec![("on_room_message", serde_json::json!({"room_id":OTHER,"body":"after close"}))]);
        fixture.cx.with_vm_and_async(|_| {});
        assert_eq!(surviving_host.widget(&fixture.cx, ids!(hits)).text(), "delivered");
        assert!(!instances::call_hook(&mut fixture.cx, &fixture.keys[0], id!(on_room_message), &["[]"]));
    }

    #[test]
    fn deferred_close_is_account_bound_and_releases_markers_after_sign_out() {
        let mut fixture = Fixture::new("account-bound-close");
        let room: OwnedRoomId = ORIGIN.try_into().unwrap();
        let captured_account = begin(&mut fixture.cx, &room).unwrap();
        instances::terminate(&mut fixture.cx, &fixture.keys[0]);
        let replacement_account = "@replacement:room-session-test".to_string();
        super::super::super::information_flow::TEST_ACCOUNT.with(|value| { value.replace(Some(replacement_account)); });
        let host = instances::ensure(&mut fixture.cx, &fixture.keys[0], &fixture.manifest, &[]).unwrap();
        let owner = fixture.cx.widget_tree().root_uid();
        instances::adopt(&mut fixture.cx, &fixture.keys[0], owner, instances::Surface::Tab).unwrap();
        let context = fixture.context(0);
        let epoch = flow::context_epoch(&context).unwrap();
        finish(&mut fixture.cx, &WidgetRef::empty(), &room, Some(&captured_account));
        assert_eq!(fixture.host(0), host);
        assert!(flow::ensure_context_epoch(&context, epoch).is_ok(), "a previous account's queued close must not stop the new account");
        with_a2app(|state| assert!(state.closing_room_sessions.is_empty()));

        let captured_account = begin(&mut fixture.cx, &room).unwrap();
        super::super::super::information_flow::TEST_ACCOUNT.with(|value| { value.replace(None); });
        finish(&mut fixture.cx, &WidgetRef::empty(), &room, Some(&captured_account));
        with_a2app(|state| assert!(state.closing_room_sessions.is_empty(), "sign-out must not leave a permanent closing marker"));
    }

    #[test]
    fn late_matrix_errors_and_successes_cannot_enter_a_reopened_callback() {
        let mut fixture = Fixture::new("late-matrix-close-result");
        let prompt = fixture.prompt(0);
        let ParkedRequest::Bridge(Some(request)) = &prompt.parked[0] else { panic!("expected native request") };
        let old_reply = Reply { heap_key: request.heap_key, req_id: request.req_id };
        let context = fixture.context(0);
        let authorization = matrix::policy::MatrixAuthorization::new(&fixture.manifest.id, "on_room_message", Some(ORIGIN),
            &PermissionStore::default()).with_flow(context.clone());
        let result = |reply, authorization, response| A2AppMatrixResult { reply, authorization: Some(authorization),
            result: response, target: Some((ORIGIN.into(), RoomAccess::Read)), reads_rooms: false };
        on_room_closed(&mut fixture.cx, &ORIGIN.try_into().unwrap());
        deliver_matrix_result(&mut fixture.cx, result(old_reply, authorization.clone(), Err("old worker failed".into())));
        fixture.cx.with_vm_and_async(|_| {});
        assert_eq!(fixture.host(0).widget(&fixture.cx, ids!(status)).text(), "waiting");
        apply_op(&mut fixture.cx, &WidgetRef::empty(), A2AppOp::RoomClosed {
            room_id: ORIGIN.try_into().unwrap(), account: Some(fixture.account.clone()),
        });
        let host = instances::ensure(&mut fixture.cx, &fixture.keys[0], &fixture.manifest, &[]).unwrap();
        let owner = fixture.cx.widget_tree().root_uid();
        instances::adopt(&mut fixture.cx, &fixture.keys[0], owner, instances::Surface::Tab).unwrap();
        host.widget(&fixture.cx, ids!(status));
        let replacement = fixture.prompt(0);
        let ParkedRequest::Bridge(Some(request)) = &replacement.parked[0] else { panic!("expected replacement request") };
        let reply = Reply { heap_key: request.heap_key, req_id: request.req_id };
        // Simulate a reused heap and request ID: context identity is equal, but
        // the authorization retains the retired activation epoch.
        for response in [Err("old upload failed".into()), Ok("{}".into())] {
            deliver_matrix_result(&mut fixture.cx, result(reply, authorization.clone(), response));
            fixture.cx.with_vm_and_async(|_| {});
            assert_eq!(host.widget(&fixture.cx, ids!(status)).text(), "waiting");
        }
        let mut consent = PermissionStore::default();
        consent.set(&fixture.manifest.id, Permission::MatrixRoomWatch, GrantState::Granted);
        let current = matrix::policy::MatrixAuthorization::new(&fixture.manifest.id, "on_room_message", Some(ORIGIN), &consent)
            .with_flow(fixture.context(0));
        with_a2app(|state| state.permissions.set(&fixture.manifest.id, Permission::MatrixRoomWatch, GrantState::Denied));
        deliver_matrix_result(&mut fixture.cx, result(reply, current, Ok("{}".into())));
        fixture.cx.with_vm_and_async(|_| {});
        assert_eq!(host.widget(&fixture.cx, ids!(status)).text(), "callback", "a revocation error still reaches its own live callback");
    }

    #[test]
    fn native_composer_completion_rejects_retired_or_reused_callback_identity() {
        let mut fixture = Fixture::new("stale-composer-completion");
        let prompt = fixture.prompt(0);
        let ParkedRequest::Bridge(Some(request)) = &prompt.parked[0] else { panic!("expected native request") };
        let mut completion = AppComposerCompletion::new(Reply { heap_key: request.heap_key, req_id: request.req_id });
        on_room_closed(&mut fixture.cx, &ORIGIN.try_into().unwrap());
        finish_app_composer(&mut fixture.cx, completion.clone(), Err("room closed".into()));
        fixture.cx.with_vm_and_async(|_| {});
        assert_eq!(fixture.host(0).widget(&fixture.cx, ids!(status)).text(), "waiting");
        apply_op(&mut fixture.cx, &WidgetRef::empty(), A2AppOp::RoomClosed {
            room_id: ORIGIN.try_into().unwrap(), account: Some(fixture.account.clone()),
        });
        let host = instances::ensure(&mut fixture.cx, &fixture.keys[0], &fixture.manifest, &[]).unwrap();
        let owner = fixture.cx.widget_tree().root_uid();
        instances::adopt(&mut fixture.cx, &fixture.keys[0], owner, instances::Surface::Tab).unwrap();
        host.widget(&fixture.cx, ids!(status));
        let prompt = fixture.prompt(0);
        let ParkedRequest::Bridge(Some(request)) = &prompt.parked[0] else { panic!("expected replacement request") };
        completion.reply = Reply { heap_key: request.heap_key, req_id: request.req_id };
        for response in [Ok("attached".into()), Err("old preview closed".into())] {
            finish_app_composer(&mut fixture.cx, completion.clone(), response);
            fixture.cx.with_vm_and_async(|_| {});
            assert_eq!(host.widget(&fixture.cx, ids!(status)).text(), "waiting");
        }
        let current = AppComposerCompletion::new(completion.reply);
        finish_app_composer(&mut fixture.cx, current, Err("current preview cancelled".into()));
        fixture.cx.with_vm_and_async(|_| {});
        assert_eq!(host.widget(&fixture.cx, ids!(status)).text(), "callback", "live native cancellation keeps its documented callback");
    }

    #[test]
    fn closing_one_origin_requeues_unrelated_group_member_with_a_new_identity() {
        let mut fixture = Fixture::new("room-group-survivor");
        let affected = fixture.prompt(0);
        let survivor = fixture.prompt(1);
        let group_id = affected.id;
        let old_survivor_id = survivor.id;
        with_a2app(|state| { state.active_prompt = Some(affected); state.active_prompt_batch.push(survivor); });
        on_room_closed(&mut fixture.cx, &ORIGIN.try_into().unwrap());
        let response = PermissionPromptGroupResponse { group_id, responses: [group_id, old_survivor_id].into_iter().map(|prompt_id|
            PermissionPromptResponse { prompt_id, answer: PermissionPromptAction::AllowScoped {
                scope: RoomScope::AllRooms, duration: GrantDuration::RoomSession, network: None,
            } }).collect() };
        permission_batch::answer_group(&mut fixture.cx, &WidgetRef::empty(), response);
        with_a2app(|state| {
            assert!(state.permissions.scoped_grants(&fixture.manifest.id).is_empty());
            assert!(state.active_prompt_batch.is_empty());
            let survivor = state.prompts.front().unwrap();
            assert_ne!(survivor.id, old_survivor_id);
            assert!(prompt_originates_in(survivor, &fixture.account, OTHER));
        });
        apply_op(&mut fixture.cx, &WidgetRef::empty(), A2AppOp::RoomClosed { room_id: ORIGIN.try_into().unwrap(), account: Some(fixture.account.clone()) });
        with_a2app(|state| assert!(state.active_prompt.as_ref().is_some_and(|prompt|
            prompt_originates_in(prompt, &fixture.account, OTHER) && prompt.id != old_survivor_id)));
    }
}
