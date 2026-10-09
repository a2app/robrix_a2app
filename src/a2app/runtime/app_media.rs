//! A mini-app media post keeps its send receipt while upload is reviewed.
use super::*;
use a2app_core::information_flow::ContextId;
use matrix::MatrixAuthorization;

pub struct AppMediaPost {
    request: A2AppMatrixRequest,
    send: MatrixAuthorization,
    reply: Reply,
    may_prompt: bool,
}

impl AppMediaPost {
    pub(super) fn activation(&self) -> Option<(usize, u64, ContextId, u64)> {
        Some((self.reply.heap_key, self.reply.req_id, self.send.flow_context.clone()?, self.send.flow_epoch?))
    }

    pub(super) fn can_prompt(&self) -> bool {
        self.may_prompt && self.is_live() && self.send.check_current_permission().is_ok()
            && self.send.flow_context.as_ref().zip(self.send.flow_epoch)
                .is_some_and(|(context, epoch)| instances::context_can_prompt(context, epoch))
    }

    fn is_live(&self) -> bool {
        self.send.check_context().is_ok() && self.send.flow_context.as_ref()
            .is_some_and(|context| super::super::information_flow::context_for_heap(self.reply.heap_key).as_ref() == Ok(context))
    }

    pub(super) fn rooms(&self) -> (Option<String>, Option<String>) {
        (self.send.origin_room.clone(), self.send.target_room.clone())
    }

    pub(super) fn capability() -> &'static a2app_core::capabilities::Capability {
        a2app_core::capabilities::by_id("matrix.media.upload").unwrap()
    }

    pub(super) fn message_preview(&self) -> Option<PermissionMessagePreview> {
        permission_message::from_flow_payload("matrix.media.send", &self.request.media_post_payload()?, false)
    }

    pub(super) fn permits_send(&self, store: &PermissionStore) -> bool {
        self.send.permits_request(store)
    }

    pub(super) fn refuse(self, cx: &mut Cx, reason: &str) {
        // A replaced isolate must never receive an old call's completion.
        if self.is_live() { services::respond(cx, self.reply, Err(reason)); }
    }

    pub(super) fn resume(self, cx: &mut Cx, ui: &WidgetRef) {
        if !self.is_live() { return; }
        let gate = with_a2app(|state| {
            let manifest = state.registry.get(&self.send.subject).ok_or("This mini-app is no longer installed.")?;
            let capability = Self::capability();
            let context = PermissionContext { origin_room: self.send.origin_room.as_deref(), target_room: self.send.target_room.as_deref() };
            if !self.permits_send(&state.permissions)
                || !manifest.declares_capability(a2app_core::capabilities::by_id("matrix.media.send").unwrap())
            { return Err("Media sending is no longer allowed for this call."); }
            let effective = state.permissions.effective_capability_in_context(manifest, capability, context);
            let upload = (effective == Effective::Granted).then(|| {
                let context = self.send.flow_context.clone().unwrap();
                let authorization = MatrixAuthorization::new(&self.send.subject, capability.id,
                    self.send.origin_room.as_deref(), &state.permissions).with_flow(context)
                    .with_payload(self.send.flow_payload.clone().unwrap());
                state.permissions.record_access(&self.send.subject, Permission::MatrixMedia, versions::now_unix());
                state.perms_dirty = true;
                authorization
            });
            Ok((effective, upload))
        }).unwrap_or(Err("Mini Apps are unavailable."));
        match gate {
            Ok((Effective::Granted, Some(upload))) => {
                let reply = self.reply;
                match self.request.authorized_media(self.send, upload) {
                    Ok(request) => submit_async_request(MatrixRequest::A2App(request)),
                    Err(error) => services::respond(cx, reply, Err(&error)),
                }
            }
            Ok((Effective::NeedsPrompt, _)) if self.can_prompt() => {
                let subject = self.send.subject.clone();
                queue_permission_prompt(cx, ui, subject, Permission::MatrixMedia, ParkedRequest::AppMedia(Box::new(self)), None);
            }
            Ok((Effective::Undeclared, _)) => self.refuse(cx, "Declare matrix.media.upload separately before sending media."),
            Ok(_) => self.refuse(cx, "Media upload is not allowed. Draft the attachment for review or request upload permission separately."),
            Err(error) => self.refuse(cx, error),
        }
    }
}

pub(super) fn start(cx: &mut Cx, ui: &WidgetRef, request: A2AppMatrixRequest, subject: String,
    origin: Option<String>, consent: Box<PermissionStore>, context: ContextId, reply: Reply, may_prompt: bool)
{
    let Some(payload) = request.media_post_payload() else {
        services::respond(cx, reply, Err("Missing prepared media."));
        return;
    };
    if context.app() != Some(subject.as_str()) || context.room() != origin.as_deref()
        || payload["room_id"].as_str() != origin.as_deref()
    {
        services::respond(cx, reply, Err("The media post does not belong to this mini-app and room."));
        return;
    }
    let send = MatrixAuthorization::new(&subject, "matrix.media.send", origin.as_deref(), &consent)
        .with_flow(context).with_payload(payload);
    AppMediaPost { request, send, reply, may_prompt }.resume(cx, ui);
}

#[cfg(test)]
mod tests {
    use super::*;
    use a2app_core::information_flow as flow;

    const ROOM: &str = "!native-media:test";

    struct Fixture {
        cx: Cx,
        manifest: MiniAppManifest,
        root: WidgetRef,
        host: WidgetRef,
        key: instances::InstanceKey,
        heap: usize,
        context: ContextId,
    }

    fn with_fixture(name: &str, run: impl FnOnce(&mut Fixture)) {
        let _review_lock = super::super::super::effect_review::TEST_LOCK.lock().unwrap();
        let previous_account = super::super::super::information_flow::TEST_ACCOUNT.with(|account|
            account.replace(Some(format!("@{name}:test"))));
        let previous_state = A2APP.with(|state| state.replace(None));
        let previous_permissions = previous_state.as_ref().map(|state| state.permissions.clone()).unwrap_or_default();
        let previous_snapshot = previous_state.as_ref().map(|state| state.permissions.snapshot(&state.registry))
            .unwrap_or_else(|| previous_permissions.snapshot(&AppRegistry::new(Vec::new())));
        let mut manifest = builtin::stock("room-peek").unwrap();
        manifest.id = name.into();
        manifest.permissions = vec![Permission::MatrixMedia.as_str().into()];
        manifest.capabilities = vec!["matrix.media.send".into(), "matrix.media.upload".into()];
        manifest.source = r#"
fn post(){
    ui.result.set_text("waiting")
    host.request("matrix.send_media", {
        data_base64:"AAH/", filename:"fixture.bin", mime_type:"application/octet-stream", caption:"native fixture"
    }, fn(r){
        if r.is_ok { ui.result.set_text("sent") }
        else { ui.result.set_text("refused: " + r.error) }
    })
}
View{result := Label{text:"untouched"}}
"#.into();
        initialize_background_test(manifest.clone());
        with_a2app(|state| state.permissions.set_matrix_write(true));
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
        let key = (manifest.id.clone(), Some(ROOM.try_into().unwrap()));
        let host = instances::ensure(&mut cx, &key, &manifest, &[]).unwrap();
        instances::adopt(&mut cx, &key, root.widget_uid(), instances::Surface::Modal).unwrap();
        host.widget(&cx, ids!(result));
        let heap = instances::heap_of(&key).unwrap();
        let context = instances::context_of_key(&key).unwrap();
        publish_grants(&mut cx);
        let mut fixture = Fixture { cx, manifest, root, host, key, heap, context };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&mut fixture)));
        instances::quit_app(&mut fixture.cx, &fixture.manifest.id);
        instances::clear_host_template();
        A2APP.with(|state| { state.replace(previous_state); });
        super::super::super::information_flow::TEST_ACCOUNT.with(|account| { account.replace(previous_account); });
        publish_grants(&mut fixture.cx);
        matrix::publish_permission_policy(&previous_permissions);
        a2app_core::permissions::publish_snapshot(previous_snapshot);
        makepad_widgets::splash_host::take_splash_host_requests();
        drop(_review_lock);
        if let Err(error) = result { std::panic::resume_unwind(error); }
    }

    impl Fixture {
        fn fire(&mut self) {
            assert!(self.host.widget(&self.cx, ids!(splash)).borrow_mut::<Splash>().unwrap()
                .call_script_fn(&mut self.cx, id!(post), &[]));
            self.cx.with_vm_and_async(|_| {});
        }

        fn text(&self) -> String { self.host.widget(&self.cx, ids!(result)).text() }

        fn answer(&mut self, id: u64, answer: PermissionPromptAction) {
            answer_permission_prompt(&mut self.cx, &WidgetRef::empty(), PermissionPromptResponse { prompt_id: id, answer });
            self.cx.with_vm_and_async(|_| {});
        }

        fn pending_upload(&mut self) -> u64 {
            self.fire();
            process_broker(&mut self.cx, &WidgetRef::empty());
            let send_id = with_a2app(|state| {
                let prompt = state.active_prompt.as_ref().expect("primary send prompt");
                assert_eq!(prompt.parked.len(), 1);
                assert_eq!(parked_capability(&prompt.parked[0]).unwrap().id, "matrix.media.send");
                assert!(prompt.flow.is_none(), "the native fixture performs no SDK metadata lookup");
                prompt.id
            }).unwrap();
            assert_eq!(self.text(), "waiting");
            self.answer(send_id, PermissionPromptAction::AllowOnce);
            assert_eq!(self.text(), "waiting", "primary consent must keep the original callback pending");
            with_a2app(|state| {
                assert!(state.permissions.scoped_grants(&self.manifest.id).is_empty(), "temporary send grant was removed after replay");
                let prompt = state.active_prompt.as_ref().expect("secondary upload prompt");
                assert_ne!(prompt.id, send_id);
                let [ParkedRequest::AppMedia(post)] = prompt.parked.as_slice() else { panic!("missing native media continuation") };
                assert_eq!(post.reply.heap_key, self.heap);
                assert_eq!(post.send.flow_context.as_ref(), Some(&self.context));
                assert!(post.permits_send(&state.permissions), "the captured primary Once receipt survives the second popup");
                assert!(post.can_prompt());
                assert_eq!(post.send.flow_payload.as_ref().unwrap()["media"]["filename"], "fixture.bin");
                assert_eq!(post.send.flow_payload.as_ref().unwrap()["media"]["size"], 3);
                assert_eq!(post.send.flow_payload.as_ref().unwrap()["media"]["caption"], "native fixture");
                assert_eq!(parked_capability(&prompt.parked[0]).unwrap().id, "matrix.media.upload");
                prompt.id
            }).unwrap()
        }

        fn assert_no_authority(&self) {
            with_a2app(|state| {
                assert!(state.permissions.scoped_grants(&self.manifest.id).is_empty());
                assert!(state.active_prompt.is_none());
                assert!(state.prompts.is_empty());
            });
            assert!(flow::effect_authorities().unwrap().iter().all(|grant| grant.context != self.context), "failed or stale answers cannot create an effect grant");
        }
    }

    #[test]
    fn send_once_survives_the_upload_prompt_and_upload_denial_answers_the_callback() {
        with_fixture("native-media-upload-denial", |fixture| {
            let upload_id = fixture.pending_upload();
            fixture.answer(upload_id, PermissionPromptAction::Deny);
            assert!(fixture.text().starts_with("refused: Media upload was not approved."));
            fixture.assert_no_authority();
        });
    }

    #[test]
    fn revoking_send_before_upload_approval_refuses_without_granting_or_hanging() {
        for blocker in ["capability", "room", "switch"] {
            with_fixture(&format!("native-media-revoked-{blocker}"), |fixture| {
                let upload_id = fixture.pending_upload();
                with_a2app(|state| match blocker {
                    "capability" => state.permissions.set_capability(&fixture.manifest.id, "matrix.media.send", GrantState::Denied),
                    "room" => state.permissions.set_room_policy(ROOM, RoomAccess::Write, PolicyDecision::Deny),
                    _ => state.permissions.set_matrix_write(false),
                });
                publish_grants(&mut fixture.cx);
                fixture.answer(upload_id, PermissionPromptAction::AllowOnce);
                assert!(fixture.text().starts_with("refused: The media request is no longer allowed."), "{blocker}: {}", fixture.text());
                fixture.assert_no_authority();
            });
        }
    }

    #[test]
    fn an_upload_answer_cannot_approve_or_answer_a_reopened_activation() {
        with_fixture("native-media-reopened", |fixture| {
            let upload_id = fixture.pending_upload();
            let old_epoch = flow::context_epoch(&fixture.context).unwrap();
            let old_heap = fixture.heap;
            instances::quit_app(&mut fixture.cx, &fixture.manifest.id);
            fixture.host = instances::ensure(&mut fixture.cx, &fixture.key, &fixture.manifest, &[]).unwrap();
            instances::adopt(&mut fixture.cx, &fixture.key, fixture.root.widget_uid(), instances::Surface::Modal).unwrap();
            fixture.host.widget(&fixture.cx, ids!(result));
            fixture.heap = instances::heap_of(&fixture.key).unwrap();
            assert_ne!(fixture.heap, old_heap);
            assert_ne!(flow::context_epoch(&fixture.context).unwrap(), old_epoch);
            fixture.fire();
            assert_eq!(fixture.text(), "waiting");
            fixture.answer(upload_id, PermissionPromptAction::AllowOnce);
            assert_eq!(fixture.text(), "waiting", "an old completion cannot enter the replacement callback");
            fixture.assert_no_authority();
        });
    }

    #[test]
    fn a_nonprompting_media_continuation_refuses_upload_without_another_popup() {
        with_fixture("native-media-no-prompt", |fixture| {
            fixture.pending_upload();
            let mut post = with_a2app(|state| {
                let mut prompt = state.active_prompt.take().unwrap();
                let ParkedRequest::AppMedia(post) = prompt.parked.pop().unwrap() else { panic!("missing media continuation") };
                post
            }).unwrap();
            post.may_prompt = false;
            post.resume(&mut fixture.cx, &WidgetRef::empty());
            fixture.cx.with_vm_and_async(|_| {});
            assert!(fixture.text().starts_with("refused: Media upload is not allowed."));
            fixture.assert_no_authority();
        });
    }
}
