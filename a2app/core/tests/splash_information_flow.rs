//! Actual Splash VM + host bridge + Broker integration. Matrix/HTTP responses
//! are deterministic host fixtures; these tests do not contact a homeserver,
//! provider, OS dialog or internet service. The same FlowContract planner used
//! by Robrix resolves sources and recipients; Registry persists enforcement.

use std::{cell::RefCell, collections::{HashMap, VecDeque}, path::PathBuf, sync::atomic::{AtomicU64, Ordering}};
use a2app_core::{
    capabilities::Capability,
    information_flow::{ContextId, FlowPolicy, Influence, Recipient, Registry, Source, SharingDuration},
    manifest::{AppRegistry, instance_tag},
    permissions::{Effective, GrantDuration, GrantState, NetworkScope, Permission, PermissionStore, RoomScope},
    services::{self, Broker, BrokerAsk, BrokerCtx, HostAction, PaneState, Reply},
};
use makepad_widgets::{*, splash::Splash, splash_host::SplashHostRequest, widget_async::CxSplashVmExt};
use makepad_widgets::{makepad_script::ScriptFnRef, widget_async::CxWidgetToScriptCallExt};

const ACCOUNT: &str = "@owner:test";
const ROOM: &str = "!private:test";
const SITE: &str = "https://example.test/";
static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);
static CALLBACK_LOG_TAP: std::sync::Once = std::sync::Once::new();

thread_local! {
    static CALLBACK_ERRORS: RefCell<Vec<String>> = RefCell::new(Vec::new());
}

fn capture_callback_error(message: &str, _: LogLevel) {
    // splash_host_respond drains synchronous errors into the logger before
    // the caller can inspect its VM. Observe that real path without replacing it.
    if message.starts_with("splash host callback error:") {
        CALLBACK_ERRORS.with(|errors| errors.borrow_mut().push(message.into()));
    }
}

struct Harness {
    cx: Cx,
    broker: Broker,
    apps: AppRegistry,
    permissions: PermissionStore,
    flow: RefCell<Registry>,
    contexts: HashMap<usize, ContextId>,
    panes: HashMap<usize, PaneState>,
    foreground_app: Option<String>,
    splashes: Vec<Splash>,
    stock_hosts: Vec<WidgetRef>,
    attempted_bodies: RefCell<Vec<String>>,
    root: PathBuf,
    review_effects: bool,
    network_bodies: VecDeque<String>,
    matrix_replies: HashMap<String, VecDeque<serde_json::Value>>,
}

impl Harness {
    fn new() -> Self {
        CALLBACK_LOG_TAP.call_once(|| set_log_tap(Some(capture_callback_error)));
        let root = std::env::temp_dir().join(format!("robrix_ifc_vm_{}_{}", std::process::id(), NEXT_ROOT.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir(&root).unwrap();
        let mut cx = Cx::new(Box::new(|_, _| {}));
        cx.with_vm(makepad_widgets::script_mod);
        Self {
            cx, broker: Broker::new(), apps: AppRegistry::default(), permissions: PermissionStore::default(),
            flow: RefCell::new(Registry::open(&root).unwrap()), contexts: HashMap::new(), panes: HashMap::new(), foreground_app: None,
            splashes: Vec::new(), stock_hosts: Vec::new(),
            attempted_bodies: RefCell::new(Vec::new()), root, review_effects: false, network_bodies: VecDeque::new(), matrix_replies: HashMap::new(),
        }
    }

    fn launch(&mut self, app: &str, public: bool, source: &str) -> (usize, ContextId) {
        let context = if public { ContextId::PublicApp { account: ACCOUNT.into(), app: app.into() } }
            else { ContextId::App { account: ACCOUNT.into(), app: app.into(), room: Some(ROOM.into()) } };
        self.launch_context(app, context, source)
    }

    fn launch_context(&mut self, app: &str, context: ContextId, source: &str) -> (usize, ContextId) {
        self.flow.borrow_mut().register_context(&context).unwrap();
        let mut manifest = a2app_core::builtin::stock("room-peek").unwrap();
        manifest.id = app.into();
        manifest.permissions = Permission::ALL.iter().map(|permission| permission.as_str().into()).collect();
        manifest.source = source.into();
        self.apps.insert(manifest);
        for permission in Permission::ALL { self.permissions.set(app, permission, GrantState::Granted); }
        self.permissions.set_matrix_write(true);
        self.permissions.allow_network(app, NetworkScope::AllHosts, RoomScope::AllRooms, GrantDuration::Always, None).unwrap();
        let mut splash = self.cx.with_vm(|vm| {
            let value = vm.eval(script! { use mod.widgets.* Splash{} });
            Splash::script_from_value(vm, value)
        });
        splash.set_host_io_only(true);
        splash.set_allow_net(false);
        splash.set_host_tag(&mut self.cx, Some(instance_tag(app, context.room())));
        let jail = self.flow.borrow().context_storage_path(&context).unwrap();
        std::fs::create_dir_all(&jail).unwrap();
        splash.set_sandbox_dir(&mut self.cx, Some(jail));
        splash.set_text(&mut self.cx, source);
        let heap = splash.isolate_heap_key(&mut self.cx).expect("script isolate was created");
        self.contexts.insert(heap, context.clone());
        self.splashes.push(splash);
        (heap, context)
    }

    fn launch_stock(&mut self, app: &str, public: bool) -> (WidgetRef, ContextId) {
        self.launch_stock_permissions(app, public, true)
    }

    fn launch_stock_permissions(&mut self, app: &str, public: bool, granted: bool) -> (WidgetRef, ContextId) {
        self.review_effects = true;
        let manifest = a2app_core::builtin::stock(app).expect("installed stock mini-app");
        let context = if public { ContextId::PublicApp { account: ACCOUNT.into(), app: app.into() } }
            else { ContextId::App { account: ACCOUNT.into(), app: app.into(), room: Some(if app == "spaces" {"!space:test"} else {ROOM}.into()) } };
        self.flow.borrow_mut().register_context(&context).unwrap();
        self.apps.insert(manifest.clone());
        for permission in Permission::ALL.into_iter().filter(|permission| granted && manifest.declares(*permission)) {
            self.permissions.set(app, permission, GrantState::Granted);
        }
        self.permissions.set_matrix_write(true);
        if granted && manifest.declares(Permission::Network) {
            self.permissions.allow_network(app, NetworkScope::ExactUrl("https://example.com/".into()),
                RoomScope::AllRooms, GrantDuration::Always, None).unwrap();
        }
        let host = self.cx.with_vm(|vm| {
            let value = vm.eval(script! { use mod.widgets.* Splash{} });
            WidgetRef::script_from_value(vm, value)
        });
        makepad_widgets::widget_tree::set_ui_root(&mut self.cx, &host);
        let jail = self.flow.borrow().context_storage_path(&context).unwrap();
        std::fs::create_dir_all(&jail).unwrap();
        let heap = {
            let mut splash = host.borrow_mut::<Splash>().unwrap();
            splash.set_host_io_only(true);
            splash.set_allow_net(false);
            splash.set_host_tag(&mut self.cx, Some(instance_tag(app, context.room())));
            splash.set_host_caps(&mut self.cx, self.permissions.granted_caps(&manifest));
            splash.set_sandbox_dir(&mut self.cx, Some(jail));
            splash.set_text(&mut self.cx, &manifest.source);
            splash.isolate_heap_key(&mut self.cx).expect("stock app isolate")
        };
        self.contexts.insert(heap, context.clone());
        self.panes.insert(heap, PaneState { surface: "dock", side: None, foreground: true, width: 460.0, height: 700.0 });
        self.stock_hosts.push(host.clone());
        // Drawing populates these widget paths in the real host. Resolve them
        // before invoking a guest callback that borrows its Splash widget.
        for line in manifest.source.lines() {
            if let Some((prefix, _)) = line.split_once(":=") {
                if let Some(name) = prefix.split_whitespace().last() {
                    host.widget(&self.cx, &[LiveId::from_str(name)]);
                }
            }
        }
        let source = host.borrow::<Splash>().unwrap().view.source.clone();
        let vm_id = self.cx.script_ref_vm_id(&source).unwrap();
        self.cx.with_script_vm_id(vm_id, |vm| vm.bx.captured_errors = Some(Vec::new()));
        (host, context)
    }

    fn process(&mut self) -> Vec<BrokerAsk> {
        let mut pending = Vec::new();
        for _ in 0..100 {
            let mut answered = false;
            for ask in self.dispatch(None) {
                match ask {
                    BrokerAsk::PermissionBatch { app_id, request, perms }
                        if perms.iter().all(|permission| self.batch_status(&app_id, &request, *permission) == Effective::Granted) =>
                    {
                        let answers: serde_json::Map<String, serde_json::Value> = perms.into_iter()
                            .map(|permission| (permission.as_str().into(), true.into())).collect();
                        self.reply(Reply { heap_key: request.heap_key, req_id: request.req_id },
                            &serde_json::json!({"granted":true,"permissions":answers}).to_string());
                        answered = true;
                    }
                    other => pending.push(other),
                }
            }
            if !answered { return pending; }
        }
        panic!("already approved stock setup did not finish");
    }

    fn batch_status(&self, app_id: &str, request: &SplashHostRequest, permission: Permission) -> Effective {
        let args: serde_json::Value = serde_json::from_str(&request.args_json).unwrap();
        let context = self.contexts.get(&request.heap_key).unwrap();
        services::permission_setup_status(&self.permissions, self.apps.get(app_id).unwrap(), permission,
            services::permission_context(&request.service, &args, context.room()))
    }

    fn take_stock_batch(&mut self, app_id: &str) -> (SplashHostRequest, Vec<Permission>) {
        for _ in 0..20 {
            let mut batch = None;
            for ask in self.process() {
                match ask {
                    BrokerAsk::PermissionBatch { app_id: subject, request, perms } => {
                        assert_eq!(subject, app_id);
                        assert!(batch.replace((request, perms)).is_none(), "one setup produces one batch");
                    }
                    BrokerAsk::Used { .. } => {},
                    _ => panic!("{app_id} performs no protected work before its setup answer"),
                }
            }
            if let Some(batch) = batch { return batch; }
        }
        panic!("{app_id} did not request its startup group batch");
    }

    fn answer_stock_batch(&mut self, app: &WidgetRef, app_id: &str, request: SplashHostRequest,
        perms: &[Permission], selected: &[Permission], audit: &mut StockAudit)
    {
        assert!(selected.iter().all(|permission| perms.contains(permission)), "a batch cannot grant an unrequested group");
        let needs_prompt = perms.iter().any(|permission| self.batch_status(app_id, &request, *permission) != Effective::Granted);
        if needs_prompt { audit.batch_prompts += 1; }
        for permission in perms {
            if selected.contains(permission) {
                if self.batch_status(app_id, &request, *permission) != Effective::Granted {
                    assert!(!audit.prompts.contains(permission), "{app_id} repeatedly prompts for {permission:?}");
                    audit.prompts.push(*permission);
                }
                if services::can_enable_permission_writes(&self.permissions, self.apps.get(app_id).unwrap(), *permission,
                    services::permission_context(&request.service, &serde_json::from_str(&request.args_json).unwrap(),
                        self.contexts.get(&request.heap_key).unwrap().room()))
                {
                    audit.write_setups += 1;
                    self.permissions.set_matrix_write(true);
                }
                self.permissions.set(app_id, *permission, GrantState::Granted);
            } else {
                self.permissions.set(app_id, *permission, GrantState::Denied);
            }
        }
        self.publish_grants(app, app_id);
        let answers: serde_json::Map<String, serde_json::Value> = perms.iter().map(|permission|
            (permission.as_str().into(), selected.contains(permission).into())).collect();
        // A batch resolves its setup callback once. It does not replay that
        // request once per group or approve any future outgoing effect.
        self.reply(Reply { heap_key: request.heap_key, req_id: request.req_id },
            &serde_json::json!({"granted":selected.len() == perms.len(),"permissions":answers}).to_string());
    }

    fn grant_stock_request(&mut self, app_id: &str, permission: Permission, request: &SplashHostRequest) {
        if request.service == "permissions.request" {
            self.permissions.set(app_id, permission, GrantState::Granted);
            return;
        }
        let capability = if request.service == "events.subscribe" {
            let args: serde_json::Value = serde_json::from_str(&request.args_json).unwrap();
            a2app_core::capabilities::for_hook(args["event"].as_str().unwrap()).unwrap()
        } else { a2app_core::capabilities::for_service(&request.service).unwrap() };
        assert_eq!(capability.group, Some(permission));
        assert!(self.apps.get(app_id).unwrap().declares_capability(capability));
        // Concrete prompts authorize the captured ability, not every other
        // ability in its group. Spatial/lifetime enforcement has separate tests.
        self.permissions.set_capability(app_id, capability.id, GrantState::Granted);
    }

    fn deny_stock_pending(&mut self, app: &WidgetRef, app_id: &str) -> usize {
        let mut denied = 0;
        let mut idle_passes = 0;
        for _ in 0..20 {
            let asks = self.process();
            if asks.is_empty() {
                idle_passes += 1;
                if denied > 0 && idle_passes >= 2 && !self.stock_callbacks_paused() { return denied; }
            } else { idle_passes = 0; }
            for ask in asks {
                match ask {
                    BrokerAsk::PermissionBatch { app_id: subject, request, perms } => {
                        assert_eq!(subject, app_id);
                        self.answer_stock_batch(app, app_id, request, &perms, &[], &mut StockAudit::default());
                        denied += 1;
                    }
                    BrokerAsk::Prompt { app_id: subject, request: Some(request), .. }
                    | BrokerAsk::EnableWrites { app_id: subject, request, .. } => {
                        assert_eq!(subject, app_id);
                        services::respond(&mut self.cx, Reply { heap_key: request.heap_key, req_id: request.req_id }, Err("Permission denied."));
                        self.cx.with_vm_and_async(|_| {});
                        self.assert_callback_errors(app_id);
                        denied += 1;
                    }
                    BrokerAsk::Used { .. } => {},
                    _ => panic!("{app_id} must not perform protected work after refusing its request"),
                }
            }
        }
        panic!("{app_id} did not finish a refused request");
    }

    fn dispatch(&mut self, pending: Option<SplashHostRequest>) -> Vec<BrokerAsk> {
        let contexts = &self.contexts;
        let flow = &self.flow;
        let bodies = &self.attempted_bodies;
        let check_flow = |request: &SplashHostRequest, cap: &Capability, args: &serde_json::Value, _: &AppRegistry| {
            let context = contexts.get(&request.heap_key).ok_or("Unknown test context")?;
            let contract = cap.flow_contract().ok_or("Missing flow contract")?;
            let room = context.room();
            let target = services::permission_context(&request.service, args, room).target_room;
            let mut registry = flow.borrow_mut();
            if request.service == "network.http" {
                bodies.borrow_mut().push(args["body"].as_str().unwrap_or_default().into());
                let origin = match Recipient::network_origin(args["url"].as_str().ok_or("Missing network URL")?)? {
                    Recipient::NetworkOrigin(origin) => origin,
                    _ => unreachable!(),
                };
                // The real HTTP worker records the origin before reviewing
                // any outgoing request, including the first public response.
                registry.add_influences(context, [Influence::InternetOrigin(origin)])?;
            }
            let recipient = contract.recipient(ACCOUNT, target, args, Some("https://homeserver.test"))?;
            let action = contract.sensitive_action(cap.id, args, target);
            if recipient.is_some() || action.is_some() {
                let epoch = registry.context_epoch(context)?;
                let review = registry.prepare_effect_for_activation(context, epoch, recipient.as_ref(), action.as_ref(), args)?;
                if !review.allowed {
                    if self.review_effects { return Ok(Some(review)); }
                    // The adversarial tests also exercise refusal callbacks.
                    if let Some(recipient) = &recipient { registry.ensure_allowed(context, recipient)?; }
                    if let Some(action) = &action { registry.ensure_action_allowed(context, action)?; }
                }
                registry.commit_effect_for_activation(context, epoch, recipient.as_ref(), action.as_ref(), args)?;
            }
            let sources = contract.source_labels(ACCOUNT, room, target)?;
            registry.add_sources(context, sources.clone())?;
            if contract.untrusted_content {
                registry.add_influences(context, sources.into_iter().map(|source| match source {
                    Source::Room { account, room } => Influence::RoomContent { account, room },
                    _ => Influence::Unknown,
                }))?;
            }
            Ok(None)
        };
        let check_response = |reply: Reply, data: &str| {
            let context = contexts.get(&reply.heap_key).ok_or("Closed test context")?;
            let mut sources = Vec::new();
            if let Ok(value) = serde_json::from_str(data) { response_room_sources(&value, &mut sources); }
            let influences = sources.iter().filter_map(|source| match source {
                Source::Room { account, room } => Some(Influence::RoomContent { account: account.clone(), room: room.clone() }),
                _ => None,
            }).collect::<Vec<_>>();
            let mut registry = flow.borrow_mut();
            registry.add_sources(context, sources)?;
            registry.add_influences(context, influences)
        };
        let storage_path = |heap| {
            flow.borrow().context_storage_path(contexts.get(&heap).ok_or("Unknown test context")?)
        };
        let ctx = BrokerCtx {
            registry: &self.apps, permissions: &self.permissions, foreground_app: self.foreground_app.as_deref(),
            is_docked: &|_| true, is_running: &|_| true, pane_state: &|heap| self.panes.get(&heap).cloned(), storage_path: &storage_path,
            room_name: &|_| Some("Private room".into()), desktop_view: true,
            permission_target_room: None,
            check_flow: &check_flow, check_response: &check_response,
        };
        let asks = if let Some(request) = pending { self.broker.dispatch_after_grant(&mut self.cx, ctx, request) }
        else { self.broker.process(&mut self.cx, ctx) };
        // Broker-local replies (env, pane reads) can suspend on widget
        // methods before they enqueue their next host request. Pump their
        // continuations even when the broker returns no host work.
        for host in &self.stock_hosts {
            let source = host.borrow::<Splash>().unwrap().view.source.clone();
            let vm_id = self.cx.script_ref_vm_id(&source).unwrap();
            self.cx.with_script_vm_id(vm_id, |vm| { vm.bx.captured_errors.get_or_insert_with(Vec::new); });
        }
        self.cx.with_vm_and_async(|_| {});
        self.assert_callback_errors("broker dispatch");
        asks
    }

    fn call(&mut self, app: &WidgetRef, name: &str, args: &[ScriptValue]) -> bool {
        let called = app.borrow_mut::<Splash>().unwrap().call_script_fn(&mut self.cx, LiveId::from_str(name), args);
        self.cx.with_vm_and_async(|_| {});
        self.assert_callback_errors(name);
        called
    }

    fn call_strings(&mut self, app: &WidgetRef, name: &str, args: &[&str]) -> bool {
        let called = app.borrow_mut::<Splash>().unwrap().call_script_fn_with_strings(&mut self.cx, LiveId::from_str(name), args);
        self.cx.with_vm_and_async(|_| {});
        self.assert_callback_errors(name);
        called
    }

    fn call_json(&mut self, app: &WidgetRef, name: &str, text: &str, json: &str) -> bool {
        let source = app.borrow::<Splash>().unwrap().view.source.clone();
        let vm_id = self.cx.script_ref_vm_id(&source).unwrap();
        let args = self.cx.with_script_vm_id(vm_id, |vm| {
            let text = vm.new_string_with(|_, out| out.push_str(text));
            let object = makepad_widgets::makepad_script::json::JsonParserThread::default().read_json(json, &mut vm.bx.heap);
            [text, object]
        });
        self.call(app, name, &args)
    }

    fn call_object(&mut self, app: &WidgetRef, name: &str, json: &str) -> bool {
        let source = app.borrow::<Splash>().unwrap().view.source.clone();
        let vm_id = self.cx.script_ref_vm_id(&source).unwrap();
        let argument = self.cx.with_script_vm_id(vm_id, |vm|
            makepad_widgets::makepad_script::json::JsonParserThread::default().read_json(json, &mut vm.bx.heap));
        self.call(app, name, &[argument])
    }

    fn widget_click(&mut self, app: &WidgetRef, text: &str, args: &[ScriptValue]) {
        fn find(root: &WidgetRef, text: &str) -> Option<WidgetRef> {
            if root.text() == text { return Some(root.clone()); }
            let mut children = Vec::new();
            root.children(&mut |_, child| children.push(child));
            children.into_iter().find_map(|child| find(&child, text))
        }
        let widget = find(app, text).expect("the actual stock control exists");
        let source = app.borrow::<Splash>().unwrap().view.source.clone();
        let vm_id = self.cx.script_ref_vm_id(&source).unwrap();
        let (source, callback) = self.cx.with_script_vm_id(vm_id, |vm| {
            let source = widget.script_source();
            let value = vm.bx.heap.value(source, id!(on_click).into(), NoTrap);
            assert!(!value.is_nil(), "{text} has its real guest click callback");
            let callback = ScriptFnRef::script_from_value(vm, value);
            let source = vm.bx.heap.new_object_ref(source);
            (source, callback)
        });
        self.cx.widget_to_script_call(widget.widget_uid(), NIL, source, callback, args);
        self.cx.with_vm_and_async(|_| {});
        self.assert_callback_errors(text);
    }

    fn queue_matrix_reply(&mut self, capability: &str, data: serde_json::Value) {
        self.matrix_replies.entry(capability.into()).or_default().push_back(data);
    }

    fn assert_callback_errors(&mut self, operation: &str) {
        let logged = CALLBACK_ERRORS.with(|errors| std::mem::take(&mut *errors.borrow_mut()));
        assert!(logged.is_empty(), "{operation} synchronous callback errors: {logged:?}");
        for host in &self.stock_hosts {
            let source = host.borrow::<Splash>().unwrap().view.source.clone();
            let vm_id = self.cx.script_ref_vm_id(&source).unwrap();
            let errors = self.cx.with_script_vm_id(vm_id, |vm| {
                let errors = vm.take_errors();
                vm.bx.captured_errors = Some(Vec::new());
                errors
            });
            assert!(errors.is_empty(), "{operation} asynchronous callback errors: {errors:?}");
        }
    }

    fn publish_grants(&mut self, app: &WidgetRef, app_id: &str) {
        let caps = self.permissions.granted_caps(self.apps.get(app_id).unwrap());
        app.borrow_mut::<Splash>().unwrap().set_host_caps(&mut self.cx, caps.clone());
        self.call_strings(app, "on_permissions_changed", &[&serde_json::to_string(&caps).unwrap()]);
    }

    fn stock_callbacks_paused(&mut self) -> bool {
        self.stock_hosts.iter().any(|host| {
            let source = host.borrow::<Splash>().unwrap().view.source.clone();
            let vm_id = self.cx.script_ref_vm_id(&source).unwrap();
            self.cx.with_script_vm_id(vm_id, |vm| {
                (0..vm.bx.threads.len()).any(|index| vm.bx.threads.get(index).unwrap().is_paused())
            })
        })
    }

    fn settle_stock(&mut self, app: &WidgetRef, app_id: &str, audit: &mut StockAudit) {
        let mut pending = VecDeque::from(self.process());
        let mut idle_passes = 0;
        for _ in 0..100 {
            while let Some(ask) = pending.pop_front() {
                match ask {
                    BrokerAsk::PermissionBatch { app_id: subject, request, perms } => {
                        assert_eq!(subject, app_id);
                        self.answer_stock_batch(app, app_id, request, &perms, &perms, audit);
                    }
                    BrokerAsk::Prompt { app_id: subject, perm, request, .. } => {
                        assert_eq!(subject, app_id);
                        if !audit.prompts.contains(&perm) { audit.prompts.push(perm); }
                        audit.concrete_prompts += 1;
                        if perm == Permission::Network {
                            self.permissions.allow_network(app_id, NetworkScope::ExactUrl("https://example.com/".into()),
                                RoomScope::AllRooms, GrantDuration::RobrixSession, None).unwrap();
                        }
                        let mut requests = request.into_iter().collect::<Vec<_>>();
                        let mut rest = VecDeque::new();
                        while let Some(queued) = pending.pop_front() {
                            match queued {
                                BrokerAsk::Prompt { app_id: queued_app, perm: queued_perm, request, .. }
                                    if queued_app == app_id && queued_perm == perm => requests.extend(request),
                                other => rest.push_back(other),
                            }
                        }
                        pending = rest;
                        for request in &requests { self.grant_stock_request(app_id, perm, request); }
                        // Publish all captured abilities before resuming any
                        // callback, matching the host's coalesced prompt.
                        self.publish_grants(app, app_id);
                        for request in requests { pending.extend(self.dispatch(Some(request))); }
                    }
                    BrokerAsk::EnableWrites { request, app_id: subject, perm } => {
                        assert_eq!(subject, app_id);
                        audit.write_setups += 1;
                        self.permissions.set_matrix_write(true);
                        self.grant_stock_request(app_id, perm, &request);
                        self.publish_grants(app, app_id);
                        pending.extend(self.dispatch(Some(request)));
                    }
                    BrokerAsk::FlowReview { request, review } => {
                        assert!(!review.allowed);
                        audit.reviews += 1;
                        self.flow.borrow_mut().approve_effect_session(&review, SharingDuration::RobrixSession).unwrap();
                        pending.extend(self.dispatch(Some(request)));
                    }
                    BrokerAsk::Matrix { reply, capability, call, .. } => {
                        audit.services.push(capability.into());
                        audit.matrix_args.push(matrix_recipe(&call));
                        let data = self.matrix_replies.get_mut(capability).and_then(VecDeque::pop_front).unwrap_or_else(|| matrix_fixture(&call));
                        self.reply(reply, &data.to_string());
                    }
                    BrokerAsk::Network { reply, args, .. } => {
                        audit.services.push("network.http".into());
                        assert_eq!(args["url"], "https://example.com/");
                        let context = self.contexts.get(&reply.heap_key).unwrap();
                        self.flow.borrow_mut().add_influences(context, [Influence::InternetOrigin("https://example.com".into())]).unwrap();
                        let body = self.network_bodies.pop_front().unwrap_or_else(|| "Example Domain fixture".into());
                        self.reply(reply, &serde_json::json!({"status":200,"body":body,"headers":{}}).to_string());
                    }
                    BrokerAsk::Subscribe { reply, hook, .. } => {
                        audit.subscriptions.push(hook.into());
                        self.reply(reply, "{\"subscribed\":true}");
                    }
                    BrokerAsk::HostQuery { reply, query } => {
                        let data = match query {
                            services::HostQuery::Prefs => {
                                audit.services.push("host.prefs".into());
                                serde_json::json!({"view_mode":"desktop","view_mode_override":"auto","ui_zoom":1,"thumbnail_max_height":400,"send_on_enter":true,"show_read_receipts":true})
                            }
                            services::HostQuery::DeviceInfo => {
                                audit.services.push("device.info".into());
                                serde_json::json!({"platform":"test","os_version":"fixture","model":"Test device","locale":"en-US","time_zone":"UTC","utc_offset_minutes":0,"cpu_cores":4})
                            }
                        };
                        self.reply(reply, &data.to_string());
                    }
                    BrokerAsk::HostAction { reply, action, .. } => {
                        audit.actions.push(match action {
                            HostAction::OpenRoom { .. } => "room", HostAction::JumpToEvent { .. } => "event",
                            HostAction::OpenThread { .. } => "thread", HostAction::ShowUser { .. } => "user",
                            HostAction::OpenSpace { .. } => "space", HostAction::ClosePane => "close",
                            HostAction::SetSide { .. } => "side", HostAction::BreakOut => "break_out",
                            HostAction::Minimize => "minimize", HostAction::Restore => "restore", _ => "host",
                        }.into());
                        self.reply(reply, "{}");
                    }
                    BrokerAsk::Notify { summary, .. } => audit.notifications.push(summary),
                    BrokerAsk::BackgroundComplete { reply, run_id, success } => {
                        audit.completions.push((run_id, success));
                        self.reply(reply, "{}");
                    }
                    BrokerAsk::Used { .. } => {},
                    BrokerAsk::Restrict { reason, .. } => panic!("{app_id} was restricted: {reason}"),
                    _ => panic!("unexpected stock mini-app request"),
                }
                self.cx.with_vm_and_async(|_| {});
                self.assert_callback_errors(app_id);
            }
            pending.extend(self.process());
            if pending.is_empty() {
                idle_passes += 1;
                if idle_passes < 2 || self.stock_callbacks_paused() { continue; }
                let failures = self.broker.failures();
                assert!(failures.is_empty(), "{app_id} service failures: {:?}", failures.iter().map(|failure| &failure.error).collect::<Vec<_>>());
                self.assert_callback_errors(app_id);
                return;
            }
            idle_passes = 0;
        }
        panic!("{app_id} did not finish its callbacks");
    }

    fn boot(&mut self) {
        let timers = std::mem::take(&mut self.cx.script_data.timers.timers);
        for timer in timers {
            if timer.repeat { self.cx.script_data.timers.timers.push(timer); continue; }
            let hooks = self.cx.script_data.timers.dispatch_hooks.clone();
            assert!(hooks.into_iter().any(|hook| hook(&mut self.cx, &timer, NIL)), "stock boot timer belongs to its isolate");
        }
        self.cx.with_vm_and_async(|_| {});
        self.assert_callback_errors("boot");
    }

    fn reply(&mut self, reply: Reply, data: &str) {
        if let Some(context) = self.contexts.get(&reply.heap_key) {
            let mut sources = Vec::new();
            if let Ok(value) = serde_json::from_str(data) { response_room_sources(&value, &mut sources); }
            let influences = sources.iter().filter_map(|source| match source {
                Source::Room { account, room } => Some(Influence::RoomContent { account: account.clone(), room: room.clone() }),
                _ => None,
            }).collect::<Vec<_>>();
            self.flow.borrow_mut().add_sources(context, sources).unwrap();
            self.flow.borrow_mut().add_influences(context, influences).unwrap();
        }
        services::respond(&mut self.cx, reply, Ok(data));
        self.assert_callback_errors(&format!("reply {}", reply.req_id));
        if let Some(app) = self.stock_hosts.iter().find(|app| app.borrow_mut::<Splash>().unwrap().isolate_heap_key(&mut self.cx) == Some(reply.heap_key)) {
            let source = app.borrow::<Splash>().unwrap().view.source.clone();
            let vm_id = self.cx.script_ref_vm_id(&source).unwrap();
            self.cx.with_script_vm_id(vm_id, |vm| { vm.bx.captured_errors.get_or_insert_with(Vec::new); });
        }
    }

    fn notifications(&mut self) -> Vec<String> {
        self.process().into_iter().filter_map(|ask| match ask {
            BrokerAsk::Notify { summary, .. } => Some(summary),
            _ => None,
        }).collect()
    }

    fn stop(&mut self, heap: usize) {
        for splash in &mut self.splashes {
            if splash.isolate_heap_key(&mut self.cx) == Some(heap) { splash.set_text(&mut self.cx, ""); }
        }
        if let Some(context) = self.contexts.remove(&heap) { self.flow.borrow_mut().remove_context(&context); }
    }

    fn retire_stock(&mut self, app: &WidgetRef, context: &ContextId) {
        let heap = app.borrow_mut::<Splash>().unwrap().isolate_heap_key(&mut self.cx).unwrap();
        app.set_text(&mut self.cx, "");
        self.stock_hosts.retain(|host| host.widget_uid() != app.widget_uid());
        self.contexts.remove(&heap);
        self.panes.remove(&heap);
        self.flow.borrow_mut().remove_context(context);
        self.broker.forget_app(context.app().unwrap());
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        for splash in &mut self.splashes { splash.set_text(&mut self.cx, ""); }
        for host in &self.stock_hosts { host.set_text(&mut self.cx, ""); }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn matrix_reply(asks: Vec<BrokerAsk>) -> Reply {
    let mut response = None;
    for ask in asks {
        match ask {
            BrokerAsk::Matrix { reply, .. } => { assert!(response.replace(reply).is_none(), "expected one Matrix request"); }
            BrokerAsk::Used { .. } => {},
            _ => panic!("unexpected request while awaiting Matrix service"),
        }
    }
    response.expect("real broker accepted read")
}

fn effect_review(asks: Vec<BrokerAsk>) -> (SplashHostRequest, a2app_core::information_flow::EffectReview) {
    let mut response = None;
    for ask in asks {
        match ask {
            BrokerAsk::FlowReview { request, review } => { assert!(response.replace((request, review)).is_none(), "expected one flow review"); }
            BrokerAsk::Used { .. } => {},
            _ => panic!("unexpected request while awaiting exact effect review"),
        }
    }
    response.expect("real broker parked one exact effect")
}

fn stock_send(asks: Vec<BrokerAsk>) -> (Reply, String) {
    let mut response = None;
    for ask in asks {
        match ask {
            BrokerAsk::Matrix { reply, call: services::MatrixServiceCall::SendMessage { body }, .. } => {
                assert!(response.replace((reply, body)).is_none(), "expected one stock post");
            }
            BrokerAsk::Used { .. } => {},
            _ => panic!("unexpected request while awaiting stock post"),
        }
    }
    response.expect("real broker accepted the stock post")
}

fn stock_network(asks: Vec<BrokerAsk>) -> (Reply, serde_json::Value) {
    let mut response = None;
    for ask in asks {
        match ask {
            BrokerAsk::Network { reply, args, .. } => {
                assert!(response.replace((reply, args)).is_none(), "expected one stock HTTP request");
            }
            BrokerAsk::Used { .. } => {},
            _ => panic!("unexpected request while awaiting stock HTTP"),
        }
    }
    response.expect("real broker accepted stock HTTP")
}

fn stock_matrix_requests(host: &mut Harness, count: usize) -> Vec<(Reply, services::MatrixServiceCall)> {
    let mut requests = Vec::new();
    for _ in 0..20 {
        for ask in host.process() {
            match ask {
                BrokerAsk::Matrix { reply, call, .. } => requests.push((reply, call)),
                BrokerAsk::Used { .. } => {},
                _ => panic!("unexpected request while awaiting stock Matrix reads"),
            }
        }
        if requests.len() >= count {
            assert_eq!(requests.len(), count);
            return requests;
        }
    }
    panic!("expected {count} stock Matrix reads, received {}", requests.len());
}

#[derive(Default)]
struct StockAudit {
    prompts: Vec<Permission>,
    batch_prompts: usize,
    concrete_prompts: usize,
    reviews: usize,
    services: Vec<String>,
    subscriptions: Vec<String>,
    actions: Vec<String>,
    notifications: Vec<String>,
    completions: Vec<(u64, bool)>,
    write_setups: usize,
    matrix_args: Vec<String>,
}

fn matrix_recipe(call: &services::MatrixServiceCall) -> String {
    use services::MatrixServiceCall::*;
    match call {
        Join { room, .. } => format!("join:{room}"),
        InviteRespond { room_id, accept } => format!("invite:{room_id}:{accept}"),
        RoomFlag { flag, on } => format!("flag:{flag:?}:{on}"),
        Search { scope, server, .. } => format!("search:{}:{server}", match scope {
            services::SearchScope::Attached => "attached", services::SearchScope::AllJoined => "all", services::SearchScope::Rooms(_) => "picked",
        }),
        _ => String::new(),
    }
}

fn matrix_fixture(call: &services::MatrixServiceCall) -> serde_json::Value {
    use services::MatrixServiceCall::*;
    let message = serde_json::json!({"event_id":"$message:test","room_id":ROOM,"room_name":"Private room","sender":"Alice","sender_id":"@alice:test","sender_name":"Alice","body":"release fixture","is_own":false,"ts":1720000000000_u64});
    let room = serde_json::json!({"room_id":ROOM,"name":"Private room","topic":"Fixture topic","member_count":2,"is_space":false,"joined":true,"is_encrypted":true,"unread":2,"mentions":1,"favorite":false,"low_priority":false,"marked_unread":false});
    match call {
        RoomInfo | RoomsInfo { .. } => serde_json::json!({"room_id":ROOM,"room_name":"Private room","topic":"Fixture topic","member_count":2,"encrypted":true,"join_rule":"invite","history_visibility":"shared","alias":null}),
        ReadMessages { .. } | RoomsMessages { .. } => serde_json::json!({"messages":[message]}),
        Members { .. } => serde_json::json!({"count":1,"members":[{"name":"Alice","user_id":"@alice:test","power":0}]}),
        PinnedEvents => serde_json::json!({"pinned":[message]}),
        Threads { .. } => serde_json::json!({"threads":[message]}),
        ThreadReplies { .. } => serde_json::json!({"root":message,"replies":[message]}),
        OlderMessages { .. } => serde_json::json!({"messages":[],"has_more":false}),
        Event { .. } => serde_json::json!({"body":"release fixture","reactions":[],"edited":false}),
        ReadReceipts { .. } => serde_json::json!({"receipts":[{"name":"Alice","user_id":"@alice:test","event_id":"$message:test","ts":1720000000000_u64}]}),
        Unread => serde_json::json!({"unread":2,"mentions":1,"marked_unread":false,"favorite":false,"low_priority":false}),
        PowerLevels => serde_json::json!({"mine":100,"can":{"invite":true,"kick":true,"ban":true,"redact_others":true,"pin":true,"send_message":true,"notify_room":true,"change_settings":true}}),
        Permalink { .. } => serde_json::json!({"url":"https://matrix.to/#/!private:test/$message:test"}),
        Successor => serde_json::json!({"upgraded":false,"room_id":null}),
        RoomsList | RoomsSearch { .. } => serde_json::json!({"rooms":[room]}),
        Search { server, .. } => serde_json::json!({"results":[message],"server_used":server}),
        Invites => serde_json::json!({"invites":[{"room_id":"!invite:test","name":"Invited room","inviter_name":"Alice","inviter":"@alice:test","is_space":false,"is_direct":false}]}),
        RoomPreview { .. } => serde_json::json!({"name":"Preview room","topic":"Fixture topic","member_count":2,"join_rule":"public"}),
        Spaces => serde_json::json!({"spaces":[{"space_id":"!space:test","name":"Test Space","topic":"Fixture topic","member_count":2}]}),
        SpaceInfo { .. } => serde_json::json!({"children_count":1,"member_count":2,"join_rule":"invite","topic":"Fixture topic"}),
        SpaceRooms { .. } => serde_json::json!({"rooms":[room]}),
        Profile => serde_json::json!({"user_id":ACCOUNT,"display_name":"Owner"}),
        UserProfile { user_id } => serde_json::json!({"user_id":user_id,"display_name":"Alice","has_avatar":false,"ignored":false}),
        DmFind { .. } => serde_json::json!({"room_id":ROOM,"name":"Private room"}),
        Device => serde_json::json!({"device_id":"TEST","name":"Test device","verified":true}),
        AccountInfo => serde_json::json!({"user_id":ACCOUNT,"homeserver":"https://homeserver.test","account_management_url":"https://homeserver.test/account?tab=profile"}),
        IgnoredUsers => serde_json::json!({"users":["@alice:test"]}),
        Join { .. } => serde_json::json!({"knocked":false,"room_id":ROOM}),
        _ => serde_json::json!({"event_id":"$posted:test","added":true}),
    }
}

fn response_room_sources(value: &serde_json::Value, sources: &mut Vec<Source>) {
    match value {
        serde_json::Value::String(room) if room.starts_with('!') && room.contains(':') => {
            sources.push(Source::Room { account: ACCOUNT.into(), room: room.clone() });
        }
        serde_json::Value::Array(values) => { for value in values { response_room_sources(value, sources); } }
        serde_json::Value::Object(values) => {
            for (key, value) in values {
                if key.starts_with('!') && key.contains(':') { sources.push(Source::Room { account: ACCOUNT.into(), room: key.clone() }); }
                response_room_sources(value, sources);
            }
        }
        _ => {},
    }
}

#[test]
fn every_stock_app_runs_real_boot_and_finishes_parked_permission_callbacks() {
    run_every_stock_app(false);
}

#[test]
fn every_stock_app_finishes_its_primary_action_with_strict_permissions() {
    run_every_stock_app(true);
}

fn run_every_stock_app(strict: bool) {
    let stock = a2app_core::builtin::builtin_apps();
    assert_eq!(stock.len(), 19);
    for manifest in stock {
        let mut host = Harness::new();
        host.permissions.set_strict(strict);
        let errors = makepad_widgets::splash::validate_splash_body(&mut host.cx, &manifest.source, false);
        assert!(errors.is_empty(), "{} script errors: {errors:?}", manifest.id);
        let (app, _) = host.launch_stock_permissions(&manifest.id, false, false);
        host.boot();
        host.call(&app, "on_app_resize", &[460_f64.into(), 700_f64.into()]);
        let mut audit = StockAudit::default();
        host.settle_stock(&app, &manifest.id, &mut audit);
        let initial_prompts = audit.prompts.len();
        let initial_reviews = audit.reviews;
        match manifest.id.as_str() {
            "public-web" => { assert!(host.call(&app, "fetch_example", &[])); }
            "roll-call" => { assert!(host.call(&app, "roll", &[])); assert!(host.call(&app, "post", &[])); }
            "reminder" | "website-watch" | "keyword-alert" => {
                let setup = match manifest.id.as_str() { "reminder" => "test_reminder", "website-watch" => "test_website", _ => "test_alert" };
                assert!(host.call(&app, setup, &[]));
                host.settle_stock(&app, &manifest.id, &mut audit);
                assert!(audit.completions.is_empty(), "foreground setup must not pretend to complete a scheduled run");
                let grants = (audit.prompts.len(), audit.reviews, audit.write_setups);
                app.borrow_mut::<Splash>().unwrap().set_host_prompts(&mut host.cx, false);
                assert!(host.call_strings(&app, "on_background", &[r#"{"run_id":41,"messages":[{"body":"release today","is_own":false}]}"#]));
                host.settle_stock(&app, &manifest.id, &mut audit);
                assert_eq!((audit.prompts.len(), audit.reviews, audit.write_setups), grants,
                    "{} runs unattended with the foreground setup grants", manifest.id);
            }
            "search" => { assert!(host.call_strings(&app, "run_search", &["release"])); }
            "account" => { assert!(host.call_strings(&app, "lookup", &["@alice:test"])); }
            "inspector" => { assert!(host.call_strings(&app, "pane_call", &["ui.pane.minimize"])); }
            "room-info" => { assert!(host.call(&app, "load", &[])); }
            "room-peek" => { assert!(host.call(&app, "jump_to_message", &[0_u32.into()])); }
            "room-members" => { assert!(host.call(&app, "open_member", &[0_u32.into()])); }
            "room-pins" => { assert!(host.call(&app, "jump_to_pin", &[0_u32.into()])); }
            "room-threads" => {
                assert!(host.call(&app, "open_thread", &[0_u32.into()]));
                host.settle_stock(&app, &manifest.id, &mut audit);
                assert!(host.call(&app, "open_in_robrix", &[]));
            }
            "presence" => { assert!(host.call(&app, "tap_cell", &[0_u32.into()])); }
            "room-tools" => { assert!(host.call(&app, "toggle", &[0_u32.into()])); }
            "spaces" => { assert!(host.call(&app, "tap_item", &[0_u32.into()])); }
            "inbox" => {
                assert!(host.call(&app, "pick_invite", &[0_u32.into()]));
                assert!(host.call(&app, "answer_invite", &[true.into()]));
            }
            "room-stats" => { assert!(host.call(&app, "open_sender", &[0_u32.into()])); }
            "watcher" => {
                app.text_input(&host.cx, ids!(keyword_input)).set_text(&mut host.cx, "release");
                assert!(host.call(&app, "add_rule", &[]));
                assert!(host.call_strings(&app, "on_room_message", &[r#"[{"event_id":"$message:test","sender_name":"Alice","body":"release today","is_own":false}]"#]));
            }
            _ => {},
        }
        host.settle_stock(&app, &manifest.id, &mut audit);
        let expected = match manifest.id.as_str() {
            "public-web" | "reminder" | "roll-call" | "search" => (0, 1),
            "website-watch" | "keyword-alert" => (0, 2),
            "room-info" => (0, 0),
            "room-peek" | "room-members" | "room-threads" | "presence" => (2, 1),
            "account" | "inspector" | "room-pins" | "room-tools" | "spaces" | "inbox" | "room-stats" | "watcher" => (1, 1),
            _ => (1, 0),
        };
        eprintln!("stock permission audit: {} strict={} initial={} action={} initial_reviews={} action_reviews={}",
            manifest.id, strict, initial_prompts, audit.prompts.len() - initial_prompts, initial_reviews, audit.reviews - initial_reviews);
        if !strict { assert_eq!((initial_prompts, audit.prompts.len() - initial_prompts), expected,
            "{} initial and primary-action permission groups: {:?}", manifest.id, audit.prompts);
        let expected_reviews = match manifest.id.as_str() {
            "room-peek" | "presence" | "room-stats" | "spaces" => (1, 1),
            "room-threads" => (0, 2),
            "room-members" | "room-pins" | "room-tools" | "inbox" | "account" => (0, 1),
            _ => (0, 0),
        };
        assert_eq!((initial_reviews, audit.reviews - initial_reviews), expected_reviews,
            "{} must finish each reviewed sink without repeated reviews", manifest.id);
        }
        assert!(audit.prompts.len() <= manifest.permissions.len(), "{} exceeds its finite declared group budget", manifest.id);
        assert!(audit.prompts.iter().all(|permission| manifest.declares(*permission)), "{} prompts only for its declared groups", manifest.id);
        assert!(audit.reviews <= 3, "{} exceeds the finite review budget for its primary path", manifest.id);
        if matches!(manifest.id.as_str(), "reminder" | "keyword-alert" | "website-watch") {
            assert_eq!(audit.completions, [(41, true)], "{} completes its actual background callback", manifest.id);
            assert_eq!(audit.notifications.len(), 2, "foreground setup and unattended match both report");
        } else if manifest.id != "inspector" {
            assert!(!audit.services.is_empty() || !audit.subscriptions.is_empty(), "{} must complete a real service path", manifest.id);
        }
        // A grant to another app causes this same host hook in production.
        // Completed reads and subscriptions must not start again.
        let services = audit.services.len();
        let subscriptions = audit.subscriptions.len();
        host.publish_grants(&app, &manifest.id);
        host.settle_stock(&app, &manifest.id, &mut audit);
        assert_eq!(audit.services.len(), services, "{} reloads on an unrelated grant", manifest.id);
        assert_eq!(audit.subscriptions.len(), subscriptions, "{} subscribes again on an unrelated grant", manifest.id);
        let mut hooks = audit.subscriptions.clone(); hooks.sort(); hooks.dedup();
        assert_eq!(hooks.len(), audit.subscriptions.len(), "{} has duplicate subscriptions", manifest.id);
    }
}

#[test]
fn stock_spaces_loads_attached_space_after_its_environment_callback() {
    let mut host = Harness::new();
    let (app, _) = host.launch_stock("spaces", false);
    host.boot();
    let mut audit = StockAudit::default();
    host.settle_stock(&app, "spaces", &mut audit);
    assert_eq!(audit.services, ["matrix.space.info.read", "matrix.space.rooms.list"]);
    assert_eq!(app.widget(&host.cx, ids!(status)).text(), "1 room");
}

fn grouped_stock_startups() -> Vec<(&'static str, Vec<Permission>, Permission)> {
    use Permission::*;
    vec![
        ("account", vec![MatrixProfile, MatrixAccountRead], MatrixProfile),
        ("inspector", vec![RobrixPreferences, RobrixObserve, DeviceInfo], RobrixPreferences),
        ("presence", vec![MatrixRoomRead, MatrixRoomWatch], MatrixRoomRead),
        ("room-members", vec![MatrixRoomRead, MatrixRoomWatch], MatrixRoomRead),
        ("room-pins", vec![MatrixRoomRead, MatrixRoomInfo], MatrixRoomRead),
        ("room-threads", vec![MatrixRoomRead, MatrixRoomWatch], MatrixRoomRead),
        ("room-peek", vec![MatrixRoomInfo, MatrixRoomRead, MatrixRoomWatch], MatrixRoomRead),
        ("room-stats", vec![MatrixRoomRead, MatrixRoomInfo], MatrixRoomRead),
        ("room-tools", vec![MatrixRoomRead, MatrixRoomInfo], MatrixRoomRead),
    ]
}

#[test]
fn grouped_stock_startups_finish_a_denial_and_refresh_after_a_new_approval() {
    for (app_id, expected, _) in grouped_stock_startups() {
        let mut host = Harness::new();
        let (app, _) = host.launch_stock_permissions(app_id, false, false);
        // Include ordinary abilities in the denied snapshot so the guest
        // callback cannot accidentally run them after a negative batch answer.
        for permission in &expected { host.permissions.set(app_id, *permission, GrantState::Denied); }
        host.publish_grants(&app, app_id);
        host.boot();
        let (request, perms) = host.take_stock_batch(app_id);
        assert_eq!(perms, expected, "{app_id} requests only its startup dependencies");
        let mut audit = StockAudit::default();
        host.answer_stock_batch(&app, app_id, request, &perms, &[], &mut audit);
        host.settle_stock(&app, app_id, &mut audit);
        assert!(!host.stock_callbacks_paused(), "{app_id} must finish a denied setup callback");
        assert!(audit.services.is_empty(), "{app_id} must not read rejected data");
        assert!(audit.subscriptions.is_empty(), "{app_id} must not subscribe to rejected hooks");
        assert_eq!(audit.batch_prompts, 1);
        host.publish_grants(&app, app_id);
        host.settle_stock(&app, app_id, &mut audit);
        assert_eq!(audit.batch_prompts, 1, "{app_id} cannot ask again from a permission-change callback");
        assert!(audit.services.is_empty());
        // The runtime separately tests trusted-input refusal clearing. This
        // VM fixture restores Ask to model that explicit retry's new snapshot.
        for permission in &perms { host.permissions.set(app_id, *permission, GrantState::Ask); }
        host.widget_click(&app, "Refresh", &[]);
        host.settle_stock(&app, app_id, &mut audit);
        assert!(!audit.services.is_empty(), "{app_id} must actually load its approved data");
        assert!(!host.stock_callbacks_paused());
        assert!(audit.batch_prompts <= 2, "{app_id} retry is at most one additional setup popup");
        let reads = audit.services.len();
        let hooks = audit.subscriptions.len();
        for permission in &perms { host.permissions.set(app_id, *permission, GrantState::Denied); }
        host.publish_grants(&app, app_id);
        host.settle_stock(&app, app_id, &mut audit);
        assert_eq!(audit.services.len(), reads, "{app_id} does not reread revoked data");
        assert_eq!(audit.subscriptions.len(), hooks, "{app_id} does not resubscribe while revoked");
        for permission in &perms { host.permissions.set(app_id, *permission, GrantState::Granted); }
        host.publish_grants(&app, app_id);
        host.settle_stock(&app, app_id, &mut audit);
        assert!(audit.services.len() > reads, "{app_id} refreshes after reapproval");
    }
}

#[test]
fn grouped_stock_startups_use_selected_reads_without_requesting_unselected_groups() {
    for (app_id, expected, read) in grouped_stock_startups() {
        let mut host = Harness::new();
        let (app, _) = host.launch_stock_permissions(app_id, false, false);
        for permission in &expected { host.permissions.set(app_id, *permission, GrantState::Denied); }
        host.publish_grants(&app, app_id);
        host.boot();
        let (request, perms) = host.take_stock_batch(app_id);
        assert_eq!(perms, expected);
        let mut audit = StockAudit::default();
        host.answer_stock_batch(&app, app_id, request, &perms, &[read], &mut audit);
        host.settle_stock(&app, app_id, &mut audit);
        assert_eq!(audit.batch_prompts, 1);
        assert_eq!(audit.prompts, [read], "{app_id} cannot silently grant an unselected group");
        assert!(!audit.services.is_empty(), "{app_id} must use its selected read");
        assert!(audit.subscriptions.iter().all(|hook|
            a2app_core::capabilities::for_hook(hook).unwrap().group == Some(read)),
            "{app_id} unselected live-update groups remain off");
        assert!(!host.stock_callbacks_paused());
        for permission in perms.into_iter().filter(|permission| *permission != read) {
            assert_eq!(host.permissions.state(app_id, permission), GrantState::Denied);
        }
        let reads = audit.services.len();
        host.publish_grants(&app, app_id);
        host.settle_stock(&app, app_id, &mut audit);
        assert_eq!(audit.services.len(), reads, "{app_id} does not reload a completed partial setup");
        assert_eq!(audit.batch_prompts, 1, "{app_id} cannot nag about unselected startup features");
    }
}

#[test]
fn other_stock_apps_finish_refusals_and_retry_from_visible_controls() {
    for app_id in ["public-web", "website-watch", "reminder", "keyword-alert", "roll-call", "room-info", "search", "watcher", "spaces", "inbox"] {
        let mut host = Harness::new();
        host.permissions.set_strict(true);
        let (app, _) = host.launch_stock_permissions(app_id, false, false);
        host.call(&app, "on_app_resize", &[460_f64.into(), 700_f64.into()]);
        host.boot();
        let control = match app_id {
            "public-web" => "Fetch example.com",
            "website-watch" => "Test saved website and report",
            "reminder" => "Test saved reminder",
            "keyword-alert" => "Set up and test saved keyword",
            "roll-call" => { assert!(host.call(&app, "roll", &[])); "Post to room" }
            "search" => { app.text_input(&host.cx, ids!(q)).set_text(&mut host.cx, "release"); "Search" }
            _ => "Refresh",
        };
        if !matches!(app_id, "room-info" | "watcher" | "spaces" | "inbox") { host.widget_click(&app, control, &[]); }
        assert!(host.deny_stock_pending(&app, app_id) > 0, "{app_id} exposes its first needed permission");
        assert!(!host.stock_callbacks_paused(), "{app_id} refusal finishes the original callback");
        let failures = host.broker.failures();
        assert!(failures.iter().all(|failure| failure.error == "Permission denied."),
            "{app_id} reports only the deliberate fixture refusals");
        host.publish_grants(&app, app_id);
        for ask in host.process() { assert!(matches!(ask, BrokerAsk::Used { .. }), "{app_id} cannot automatically retry a denied request"); }
        let declared = host.apps.get(app_id).unwrap().permissions.clone();
        for permission in declared { host.permissions.set(app_id, Permission::from_str(&permission).unwrap(), GrantState::Ask); }
        host.widget_click(&app, control, &[]);
        let mut audit = StockAudit::default();
        host.settle_stock(&app, app_id, &mut audit);
        assert!(!host.stock_callbacks_paused());
        assert!(!audit.services.is_empty() || !audit.subscriptions.is_empty() || !audit.notifications.is_empty(),
            "{app_id} visible retry must complete real approved work");
    }
}

#[test]
fn stock_thread_selection_ignores_replies_from_earlier_visits() {
    let mut host = Harness::new();
    let (app, _) = host.launch_stock("room-threads", false);
    host.flow.borrow_mut().set_policy(Source::Room { account: ACCOUNT.into(), room: ROOM.into() },
        FlowPolicy { recipients: [Recipient::network_origin("https://homeserver.test").unwrap()].into_iter().collect() }).unwrap();
    host.boot();
    for ask in host.process() {
        match ask {
            BrokerAsk::Matrix { reply, call: services::MatrixServiceCall::Threads { .. }, .. } => {
                let mut data = matrix_fixture(&services::MatrixServiceCall::Threads { limit: 20 });
                let mut second = data["threads"][0].clone();
                second["event_id"] = "$second:test".into();
                data["threads"].as_array_mut().unwrap().push(second);
                host.reply(reply, &data.to_string());
            }
            BrokerAsk::Subscribe { reply, .. } => host.reply(reply, r#"{"subscribed":true}"#),
            BrokerAsk::Used { .. } => {},
            _ => panic!("unexpected thread boot request"),
        }
    }
    let mut audit = StockAudit::default();
    host.settle_stock(&app, "room-threads", &mut audit);
    let mut requests = Vec::new();
    // A -> B -> A must start each selected thread immediately, even while
    // earlier replies remain in flight. Matching only the target is not enough.
    for (index, event_id) in [(0_u32, "$message:test"), (1, "$second:test"), (0, "$message:test")] {
        assert!(host.call(&app, "open_thread", &[index.into()]));
        let (reply, call) = stock_matrix_requests(&mut host, 1).pop().unwrap();
        let services::MatrixServiceCall::ThreadReplies { event_id: target, .. } = call else { panic!("expected selected thread read") };
        assert_eq!(target, event_id);
        requests.push(reply);
    }
    for (index, count) in [(2, 2), (0, 1), (1, 3)] {
        let mut data = matrix_fixture(&services::MatrixServiceCall::ThreadReplies { event_id: "$message:test".into(), limit: 50 });
        let message = data["replies"][0].clone();
        data["replies"] = serde_json::Value::Array(vec![message; count]);
        host.reply(requests[index], &data.to_string());
        host.settle_stock(&app, "room-threads", &mut audit);
        assert_eq!(app.widget(&host.cx, ids!(thread_header)).text(), "2 replies", "an earlier visit must not replace the current thread");
    }
    host.settle_stock(&app, "room-threads", &mut audit);
}

#[test]
fn stock_space_selection_ignores_replies_from_earlier_visits() {
    fn loads(host: &mut Harness, expected: &str) -> (Reply, Reply) {
        let mut info = None;
        let mut rooms = None;
        for _ in 0..4 {
            for ask in host.process() { match ask {
                BrokerAsk::Matrix { reply, call: services::MatrixServiceCall::SpaceInfo { space_id }, .. } => {
                    assert_eq!(space_id, expected);
                    assert!(info.replace(reply).is_none());
                }
                BrokerAsk::Matrix { reply, call: services::MatrixServiceCall::SpaceRooms { space_id }, .. } => {
                    assert_eq!(space_id, expected);
                    assert!(rooms.replace(reply).is_none());
                }
                BrokerAsk::Used { .. } => {},
                _ => panic!("unexpected space navigation request"),
            } }
            if info.is_some() && rooms.is_some() { break; }
        }
        (info.expect("new space starts its details read"), rooms.expect("new space starts its room-list read"))
    }
    let mut host = Harness::new();
    let (app, _) = host.launch_stock("spaces", false);
    host.boot();
    let (old_info, old_rooms) = loads(&mut host, "!space:test");
    host.reply(old_rooms, r#"{"rooms":[{"room_id":"!child:test","name":"Child space","topic":"","member_count":1,"is_space":true,"joined":true}]}"#);
    host.settle_stock(&app, "spaces", &mut StockAudit::default());
    assert!(host.call(&app, "tap_item", &[0_u32.into()]));
    let (child_info, child_rooms) = loads(&mut host, "!child:test");
    assert!(host.call(&app, "go_back", &[]));
    let (current_info, current_rooms) = loads(&mut host, "!space:test");
    host.reply(current_info, r#"{"children_count":2,"member_count":2,"join_rule":"invite","topic":"current visit"}"#);
    let mut rooms = matrix_fixture(&services::MatrixServiceCall::SpaceRooms { space_id: "!space:test".into() });
    let room = rooms["rooms"][0].clone();
    rooms["rooms"].as_array_mut().unwrap().push(room);
    host.reply(current_rooms, &rooms.to_string());
    host.settle_stock(&app, "spaces", &mut StockAudit::default());
    for (reply, data) in [
        (old_info, r#"{"children_count":1,"member_count":1,"join_rule":"public","topic":"earlier visit"}"#.to_owned()),
        (child_info, r#"{"children_count":1,"member_count":1,"join_rule":"public","topic":"child space"}"#.to_owned()),
        (child_rooms, matrix_fixture(&services::MatrixServiceCall::SpaceRooms { space_id: "!child:test".into() }).to_string()),
    ] {
        host.reply(reply, &data);
        host.settle_stock(&app, "spaces", &mut StockAudit::default());
        assert_eq!(app.widget(&host.cx, ids!(space_topic)).text(), "current visit");
        assert_eq!(app.widget(&host.cx, ids!(status)).text(), "2 rooms", "stale space replies must not replace the current visit");
    }
    host.settle_stock(&app, "spaces", &mut StockAudit::default());
}

#[test]
fn stock_navigation_membership_and_room_management_complete_after_review() {
    for (app_id, function, argument, expected_action, expected_service) in [
        ("room-members", "open_member", 0_u32, Some("user"), None),
        ("room-pins", "jump_to_pin", 0, Some("event"), None),
        ("room-peek", "jump_to_message", 0, Some("event"), None),
        ("presence", "tap_cell", 0, Some("user"), None),
        ("spaces", "tap_item", 0, Some("room"), None),
        ("inbox", "open_room", 0, Some("room"), None),
        ("room-tools", "toggle", 0, None, Some("matrix.room.favorite.set")),
        ("room-tools", "toggle_pin", 0, None, Some("matrix.room.pin.set")),
        ("room-stats", "open_sender", 0, Some("user"), None),
    ] {
        let mut host = Harness::new();
        let (app, _) = host.launch_stock_permissions(app_id, false, false);
        host.boot();
        let mut audit = StockAudit::default();
        host.settle_stock(&app, app_id, &mut audit);
        let before = audit.services.len();
        let reviews = audit.reviews;
        assert!(host.call(&app, function, &[argument.into()]));
        host.settle_stock(&app, app_id, &mut audit);
        if let Some(action) = expected_action { assert_eq!(audit.actions, [action], "{app_id}.{function}"); }
        if let Some(service) = expected_service { assert_eq!(&audit.services[before..], [service], "{app_id}.{function}"); }
        assert!(audit.reviews - reviews <= 1, "{app_id}.{function} needs at most one combined flow review");
        let prompts = audit.prompts.len();
        let reviews = audit.reviews;
        let actions = audit.actions.len();
        let services = audit.services.len();
        // Model a subsequent user operation after the rate-limit bucket
        // refills, while preserving every permission and flow session grant.
        host.broker.forget_app(app_id);
        assert!(host.call(&app, function, &[argument.into()]));
        host.settle_stock(&app, app_id, &mut audit);
        assert_eq!(audit.prompts.len(), prompts, "{app_id}.{function} repeats no granted permission prompt");
        assert_eq!(audit.reviews, reviews, "{app_id}.{function} keeps its approved session flow");
        if expected_action.is_some() { assert_eq!(audit.actions.len(), actions + 1, "{app_id}.{function} must navigate again"); }
        if expected_service.is_some() { assert_eq!(audit.services.len(), services + 1, "{app_id}.{function} must act again"); }
    }
    let mut host = Harness::new();
    let (app, _) = host.launch_stock("room-threads", false);
    host.boot();
    let mut audit = StockAudit::default();
    host.settle_stock(&app, "room-threads", &mut audit);
    assert!(host.call(&app, "open_thread", &[0_u32.into()]));
    host.settle_stock(&app, "room-threads", &mut audit);
    assert!(audit.services.contains(&"matrix.room.thread.read".into()));
    assert!(host.call(&app, "open_in_robrix", &[]));
    host.settle_stock(&app, "room-threads", &mut audit);
    assert_eq!(audit.actions, ["thread"]);
}

#[test]
fn stock_room_peek_send_keeps_its_callback_until_one_combined_review_then_finishes() {
    let mut host = Harness::new();
    let (app, context) = host.launch_stock("room-peek", false);
    host.boot();
    let mut audit = StockAudit::default();
    host.settle_stock(&app, "room-peek", &mut audit);
    let sharing_grants = host.flow.borrow().sharing_grants().unwrap();
    let action_grants = host.flow.borrow().authorities().unwrap();
    app.text_input(&host.cx, ids!(msg_input)).set_text(&mut host.cx, "Please post this exact text");
    host.flow.borrow_mut().add_sources(&context, [Source::Account { account: ACCOUNT.into() }]).unwrap();
    assert!(host.call(&app, "send_msg", &[]));
    let (request, review) = effect_review(host.process());
    assert_eq!(request.service, "matrix.send_message");
    assert!(review.sources.contains(&Source::Account { account: ACCOUNT.into() }));
    assert!(!review.influences.is_empty());
    assert!(review.payload.contains("Please post this exact text"));
    assert_eq!(app.widget(&host.cx, ids!(send_note)).text(), "Sending…");
    host.flow.borrow_mut().approve_effect_once(&review).unwrap();
    let (reply, body) = stock_send(host.dispatch(Some(request)));
    assert_eq!(body, "Please post this exact text");
    host.reply(reply, "{}");
    host.cx.with_vm_and_async(|_| {});
    host.settle_stock(&app, "room-peek", &mut audit);
    assert_eq!(app.widget(&host.cx, ids!(send_note)).text(), "Sent!");
    assert_eq!(app.text_input(&host.cx, ids!(msg_input)).text(), "");
    assert_eq!(host.flow.borrow().sharing_grants().unwrap(), sharing_grants, "allow once makes no lasting sharing rule");
    assert_eq!(host.flow.borrow().authorities().unwrap(), action_grants, "allow once makes no session action grant");
}

#[test]
fn stock_roll_call_first_post_finishes_after_enabling_matrix_writes() {
    let mut host = Harness::new();
    let (app, _) = host.launch_stock_permissions("roll-call", false, false);
    host.permissions.set_matrix_write(false);
    assert!(host.call(&app, "roll", &[]));
    assert!(host.call(&app, "post", &[]));
    let mut audit = StockAudit::default();
    host.settle_stock(&app, "roll-call", &mut audit);
    assert_eq!(audit.write_setups, 1);
    assert_eq!(audit.services, ["matrix.room.message.send"]);
    assert_eq!(app.widget(&host.cx, ids!(note)).text(), "Posted to the room!");
    assert!(host.call(&app, "post", &[]));
    host.settle_stock(&app, "roll-call", &mut audit);
    assert_eq!(audit.write_setups, 1, "subsequent posts keep the user's write setup choice");
    assert_eq!(audit.services.len(), 2);
}

#[test]
fn stock_watcher_saves_without_write_permission_and_tests_only_the_selected_rule_actions() {
    let mut host = Harness::new();
    let (app, context) = host.launch_stock_permissions("watcher", false, false);
    host.permissions.set_matrix_write(false);
    host.boot();
    let mut audit = StockAudit::default();
    host.settle_stock(&app, "watcher", &mut audit);
    app.text_input(&host.cx, ids!(keyword_input)).set_text(&mut host.cx, "release");
    app.text_input(&host.cx, ids!(reply_input)).set_text(&mut host.cx, "Thanks for the release update");
    host.flow.borrow_mut().add_sources(&context, [Source::Account { account: ACCOUNT.into() }]).unwrap();
    assert!(host.call(&app, "add_rule", &[]));
    host.settle_stock(&app, "watcher", &mut audit);
    assert_eq!(audit.write_setups, 0, "saving a rule cannot enable Matrix writes");
    assert!(audit.notifications.is_empty());
    assert!(audit.services.is_empty());
    assert!(host.call_object(&app, "test_rule", r#"{"keyword":"release","notify":true,"reply":"Thanks for the release update"}"#));
    host.settle_stock(&app, "watcher", &mut audit);
    assert_eq!(audit.batch_prompts, 0, "an already approved watcher needs no new blanket setup");
    assert_eq!(audit.write_setups, 1, "testing a reply explicitly enables writes and grants its declared room-send ability");
    assert_eq!(audit.notifications, ["Watcher test: Test alert for 'release'."]);
    assert_eq!(audit.services, ["matrix.room.message.send"]);
    host.flow.borrow_mut().add_sources(&context, [Source::Room { account: ACCOUNT.into(), room: ROOM.into() }]).unwrap();
    host.flow.borrow_mut().add_influences(&context, [Influence::RoomContent { account: ACCOUNT.into(), room: ROOM.into() }]).unwrap();
    assert!(host.call_strings(&app, "on_room_message", &[r#"[{"event_id":"$message:test","sender_name":"Alice","body":"release today","is_own":false}]"#]));
    host.settle_stock(&app, "watcher", &mut audit);
    host.boot(); // The notification callback paces the queued reply with a timer.
    host.settle_stock(&app, "watcher", &mut audit);
    assert_eq!(audit.notifications, ["Watcher test: Test alert for 'release'.", "Watcher: release: Alice: release today"]);
    assert_eq!(audit.services, ["matrix.room.message.send", "matrix.room.message.send"]);
    let reviews = audit.reviews;
    assert!((1..=2).contains(&reviews), "the test and first influenced reply each have at most one combined review");
    host.boot(); // Finish the action pacing timer before delivering another batch.
    assert!(host.call_strings(&app, "on_room_message", &[r#"[{"event_id":"$message:test","sender_name":"Alice","body":"release tomorrow","is_own":false}]"#]));
    host.settle_stock(&app, "watcher", &mut audit);
    host.boot();
    host.settle_stock(&app, "watcher", &mut audit);
    assert_eq!(audit.reviews, reviews, "session permission prevents repeated auto-reply reviews");
    assert_eq!(audit.services.len(), 3);
    assert_eq!(audit.write_setups, 1, "automatic replies reuse the approved write setup");
}

#[test]
fn stock_inbox_accept_search_result_and_account_dm_finish_the_original_action() {
    for app_id in ["inbox", "search", "account"] {
        let mut host = Harness::new();
        let (app, _) = host.launch_stock_permissions(app_id, false, false);
        host.boot();
        let mut audit = StockAudit::default();
        host.settle_stock(&app, app_id, &mut audit);
        match app_id {
            "inbox" => {
                assert!(host.call(&app, "pick_invite", &[0_u32.into()]));
                assert!(host.call(&app, "answer_invite", &[true.into()]));
            }
            "search" => {
                assert!(host.call_strings(&app, "set_mode", &["pick"]));
                host.settle_stock(&app, app_id, &mut audit);
                assert!(host.call(&app, "toggle_pick", &[0_u32.into()]));
                assert!(host.call_strings(&app, "run_search", &["release"]));
                host.settle_stock(&app, app_id, &mut audit);
                assert!(host.call(&app, "open_result", &[0_u32.into()]));
            }
            "account" => {
                assert!(host.call_strings(&app, "lookup", &["@alice:test"]));
                host.settle_stock(&app, app_id, &mut audit);
                assert!(host.call(&app, "open_dm", &[]));
            }
            _ => unreachable!(),
        }
        host.settle_stock(&app, app_id, &mut audit);
        match app_id {
            "inbox" => {
                assert!(audit.services.contains(&"matrix.invites.respond".into()));
                assert_eq!(app.widget(&host.cx, ids!(status)).text(), "Accepted the invite to Invited room");
            }
            "search" => assert_eq!(audit.actions, ["event"]),
            "account" => assert_eq!(audit.actions, ["room"]),
            _ => unreachable!(),
        }
    }
}

#[test]
fn stock_website_room_reports_and_keyword_empty_batches_finish_without_prompt_loops() {
    let mut host = Harness::new();
    let (app, context) = host.launch_stock_permissions("website-watch", false, false);
    assert!(host.call(&app, "on_app_resize", &[460_f64.into(), 700_f64.into()]));
    assert!(host.call(&app, "toggle_post", &[]));
    assert!(host.call(&app, "save", &[]));
    host.permissions.set_matrix_write(false);
    host.flow.borrow_mut().add_sources(&context, [Source::Account { account: ACCOUNT.into() }]).unwrap();
    assert!(host.call(&app, "test_website", &[]));
    let mut audit = StockAudit::default();
    host.settle_stock(&app, "website-watch", &mut audit);
    assert_eq!(audit.services, ["network.http", "matrix.room.message.send"]);
    assert!(audit.completions.is_empty());
    assert_eq!(audit.write_setups, 1);
    assert_eq!(app.widget(&host.cx, ids!(status)).text(), "Test report posted to this room.");
    let grants = (audit.prompts.len(), audit.reviews, audit.write_setups);
    app.borrow_mut::<Splash>().unwrap().set_host_prompts(&mut host.cx, false);
    // A real scheduled check occurs after the setup rate budget refills.
    host.broker.forget_app("website-watch");
    assert!(host.call_strings(&app, "on_background", &[r#"{"run_id":71}"#]));
    host.settle_stock(&app, "website-watch", &mut audit);
    assert_eq!(audit.services, ["network.http", "matrix.room.message.send", "network.http", "matrix.room.message.send"]);
    assert_eq!(audit.completions, [(71, true)]);
    assert_eq!(app.widget(&host.cx, ids!(status)).text(), "Match posted to this room.");
    let reviews = audit.reviews;
    assert!(reviews <= 2, "one approved website release and one influenced room report");
    assert!(host.call_strings(&app, "on_background", &[r#"{"run_id":72}"#]));
    host.settle_stock(&app, "website-watch", &mut audit);
    assert_eq!(audit.services, ["network.http", "matrix.room.message.send", "network.http", "matrix.room.message.send", "network.http"]);
    assert_eq!(app.widget(&host.cx, ids!(status)).text(), "Keyword is still present; already reported.");
    assert_eq!(audit.completions, [(71, true), (72, true)]);
    assert_eq!(audit.reviews, reviews);
    assert_eq!((audit.prompts.len(), audit.reviews, audit.write_setups), grants, "scheduled work never prompts again after foreground setup");
    assert_eq!(app.widget(&host.cx, ids!(status)).text(), "Keyword is still present; already reported.");

    let mut host = Harness::new();
    let (app, _) = host.launch_stock_permissions("keyword-alert", false, false);
    let mut audit = StockAudit::default();
    assert!(host.call_strings(&app, "on_background", &[r#"{"run_id":73,"messages":[{"body":"release today","is_own":true},{"body":"nothing new","is_own":false}]}"#]));
    host.settle_stock(&app, "keyword-alert", &mut audit);
    assert_eq!(audit.completions, [(73, true)]);
    assert!(audit.notifications.is_empty());
    assert!(audit.prompts.is_empty(), "an unmatched/own-message batch needs no notification grant");
}

#[test]
fn stock_background_apps_refuse_missing_hidden_grants_then_run_after_visible_setup() {
    for (app_id, setup) in [("reminder", "test_reminder"), ("keyword-alert", "test_alert"), ("website-watch", "test_website")] {
        let mut host = Harness::new();
        let (app, _) = host.launch_stock_permissions(app_id, false, false);
        assert!(host.call(&app, "on_app_resize", &[460_f64.into(), 700_f64.into()]));
        app.borrow_mut::<Splash>().unwrap().set_host_prompts(&mut host.cx, false);
        assert!(host.call_strings(&app, "on_background", &[r#"{"run_id":91,"messages":[{"body":"release today","is_own":false}]}"#]));
        let mut completions = Vec::new();
        for _ in 0..10 {
            for ask in host.process() {
                match ask {
                    BrokerAsk::BackgroundComplete { reply, run_id, success } => {
                        completions.push((run_id, success));
                        host.reply(reply, "{}");
                    }
                    BrokerAsk::Used { .. } => {},
                    _ => panic!("{app_id} hidden work must refuse missing grants without prompting or performing an effect"),
                }
            }
        }
        assert_eq!(completions, [(91, false)], "{app_id} reports its denied scheduled run");
        let failures = host.broker.failures();
        assert_eq!(failures.len(), 1, "{app_id} records the missing ability once");
        assert!(failures[0].error.contains("permission"), "{}", failures[0].error);
        assert!(!host.stock_callbacks_paused());
        host.assert_callback_errors(app_id);
        app.borrow_mut::<Splash>().unwrap().set_host_prompts(&mut host.cx, true);
        assert!(host.call(&app, setup, &[]));
        if app_id == "website-watch" {
            assert!(host.call(&app, setup, &[]));
            assert!(host.call(&app, setup, &[]));
        }
        let mut audit = StockAudit::default();
        host.settle_stock(&app, app_id, &mut audit);
        assert!(audit.completions.is_empty());
        assert_eq!(audit.notifications.len(), 1, "{app_id} foreground setup completes once");
        if app_id == "website-watch" {
            assert_eq!(audit.services, ["network.http"], "repeated Test clicks while awaiting permission and HTTP keep the original check busy");
        }
        let grants = (audit.prompts.len(), audit.reviews, audit.write_setups);
        app.borrow_mut::<Splash>().unwrap().set_host_prompts(&mut host.cx, false);
        assert!(host.call_strings(&app, "on_background", &[r#"{"run_id":92,"messages":[{"body":"release today","is_own":false}]}"#]));
        host.settle_stock(&app, app_id, &mut audit);
        assert_eq!(audit.completions, [(92, true)]);
        assert_eq!((audit.prompts.len(), audit.reviews, audit.write_setups), grants);
    }
}

#[test]
fn stock_website_session_grants_survive_retirement_and_post_a_later_new_match() {
    let mut host = Harness::new();
    let (app, context) = host.launch_stock_permissions("website-watch", false, false);
    assert!(host.call(&app, "on_app_resize", &[460_f64.into(), 700_f64.into()]));
    assert!(host.call(&app, "toggle_post", &[]));
    assert!(host.call(&app, "save", &[]));
    host.permissions.set_matrix_write(false);
    host.flow.borrow_mut().add_sources(&context, [Source::Account { account: ACCOUNT.into() }]).unwrap();
    assert!(host.call(&app, "test_website", &[]));
    let mut audit = StockAudit::default();
    host.settle_stock(&app, "website-watch", &mut audit);
    let grants = (audit.prompts.len(), audit.reviews, audit.write_setups);
    assert_eq!(grants, (1, 2, 1));
    app.borrow_mut::<Splash>().unwrap().set_host_prompts(&mut host.cx, false);
    host.broker.forget_app("website-watch");
    assert!(host.call_strings(&app, "on_background", &[r#"{"run_id":301}"#]));
    host.settle_stock(&app, "website-watch", &mut audit);
    assert_eq!(app.widget(&host.cx, ids!(status)).text(), "Match posted to this room.");

    let first_epoch = host.flow.borrow().context_epoch(&context).unwrap();
    host.retire_stock(&app, &context);
    let (app, reopened) = host.launch_stock_permissions("website-watch", false, false);
    assert_eq!(reopened, context);
    assert_ne!(host.flow.borrow().context_epoch(&context).unwrap(), first_epoch);
    assert!(host.call(&app, "on_app_resize", &[460_f64.into(), 700_f64.into()]));
    app.borrow_mut::<Splash>().unwrap().set_host_prompts(&mut host.cx, false);
    host.network_bodies.push_back("Keyword is absent in this fixture".into());
    assert!(host.call_strings(&app, "on_background", &[r#"{"run_id":302}"#]));
    host.settle_stock(&app, "website-watch", &mut audit);
    assert_eq!(app.widget(&host.cx, ids!(status)).text(), "Keyword is absent. The next new match will be reported.");

    let second_epoch = host.flow.borrow().context_epoch(&context).unwrap();
    host.retire_stock(&app, &context);
    let (app, reopened) = host.launch_stock_permissions("website-watch", false, false);
    assert_eq!(reopened, context);
    assert_ne!(host.flow.borrow().context_epoch(&context).unwrap(), second_epoch);
    assert!(host.call(&app, "on_app_resize", &[460_f64.into(), 700_f64.into()]));
    app.borrow_mut::<Splash>().unwrap().set_host_prompts(&mut host.cx, false);
    assert!(host.call_strings(&app, "on_background", &[r#"{"run_id":303}"#]));
    host.settle_stock(&app, "website-watch", &mut audit);
    assert_eq!(app.widget(&host.cx, ids!(status)).text(), "Match posted to this room.");
    assert_eq!(audit.completions, [(301, true), (302, true), (303, true)]);
    assert_eq!(audit.services.iter().filter(|service| service.as_str() == "matrix.room.message.send").count(), 3,
        "foreground test and both newly matched scheduled runs send the actual room-report request");
    assert_eq!((audit.prompts.len(), audit.reviews, audit.write_setups), grants,
        "declared ability and explicit session sharing/action grants survive task instance retirement");
}

#[test]
fn stock_spaces_unjoined_room_join_and_knock_finish_and_reuse_their_grants() {
    for knocked in [false, true] {
        let mut host = Harness::new();
        let room = serde_json::json!({"room_id":"!candidate:test","name":"Candidate room","topic":"","member_count":2,"is_space":false,"joined":false});
        host.queue_matrix_reply("matrix.space.rooms.list", serde_json::json!({"rooms":[room.clone()]}));
        if !knocked {
            let mut joined = room;
            joined["joined"] = true.into();
            for _ in 0..2 { host.queue_matrix_reply("matrix.space.rooms.list", serde_json::json!({"rooms":[joined.clone()]})); }
        }
        for _ in 0..2 { host.queue_matrix_reply("matrix.rooms.join", serde_json::json!({"knocked":knocked,"room_id":"!candidate:test"})); }
        let (app, _) = host.launch_stock_permissions("spaces", false, false);
        host.boot();
        let mut audit = StockAudit::default();
        host.settle_stock(&app, "spaces", &mut audit);
        assert!(host.call(&app, "tap_item", &[0_u32.into()]));
        host.settle_stock(&app, "spaces", &mut audit);
        assert_eq!(app.widget(&host.cx, ids!(preview_line)).text(), "2 members · Fixture topic · public");
        host.broker.forget_app("spaces");
        assert!(host.call(&app, "join_room", &[]));
        host.settle_stock(&app, "spaces", &mut audit);
        let expected = if knocked { "Knocked. You're in once someone lets you in." } else { "Joined! Tap it in the list to open it." };
        assert_eq!(app.widget(&host.cx, ids!(join_note)).text(), expected);
        assert!(audit.matrix_args.contains(&"join:!candidate:test".into()));
        let grants = (audit.prompts.len(), audit.reviews, audit.write_setups);
        host.broker.forget_app("spaces");
        assert!(host.call(&app, "join_room", &[]));
        host.settle_stock(&app, "spaces", &mut audit);
        assert_eq!(app.widget(&host.cx, ids!(join_note)).text(), expected);
        assert_eq!(audit.matrix_args.iter().filter(|recipe| recipe.as_str() == "join:!candidate:test").count(), 2);
        assert_eq!((audit.prompts.len(), audit.reviews, audit.write_setups), grants);
    }
}

#[test]
fn stock_inbox_decline_finishes_and_reuses_its_membership_grant() {
    let mut host = Harness::new();
    let (app, _) = host.launch_stock_permissions("inbox", false, false);
    host.boot();
    let mut audit = StockAudit::default();
    host.settle_stock(&app, "inbox", &mut audit);
    for index in 0..2 {
        host.broker.forget_app("inbox");
        assert!(host.call(&app, "pick_invite", &[0_u32.into()]));
        assert!(host.call(&app, "answer_invite", &[false.into()]));
        let grants = (audit.prompts.len(), audit.reviews, audit.write_setups);
        host.settle_stock(&app, "inbox", &mut audit);
        assert_eq!(app.widget(&host.cx, ids!(status)).text(), "Declined the invite to Invited room");
        if index == 1 { assert_eq!((audit.prompts.len(), audit.reviews, audit.write_setups), grants); }
    }
    assert_eq!(audit.matrix_args.iter().filter(|recipe| recipe.as_str() == "invite:!invite:test:false").count(), 2);
}

#[test]
fn stock_room_tools_other_flags_and_upgraded_room_complete_and_reuse_grants() {
    let mut host = Harness::new();
    host.queue_matrix_reply("matrix.room.successor.read", serde_json::json!({"upgraded":true,"room_id":"!successor:test"}));
    let (app, _) = host.launch_stock_permissions("room-tools", false, false);
    host.boot();
    let mut audit = StockAudit::default();
    host.settle_stock(&app, "room-tools", &mut audit);
    for (index, on, off, recipe) in [
        (1_u32, "Marked low priority.", "Back to normal priority.", "LowPriority"),
        (2_u32, "Marked unread.", "Unread flag cleared.", "Unread"),
    ] {
        host.broker.forget_app("room-tools");
        assert!(host.call(&app, "toggle", &[index.into()]));
        host.settle_stock(&app, "room-tools", &mut audit);
        assert_eq!(app.widget(&host.cx, ids!(status)).text(), on);
        let grants = (audit.prompts.len(), audit.reviews, audit.write_setups);
        host.broker.forget_app("room-tools");
        assert!(host.call(&app, "toggle", &[index.into()]));
        host.settle_stock(&app, "room-tools", &mut audit);
        assert_eq!(app.widget(&host.cx, ids!(status)).text(), off);
        assert!(audit.matrix_args.contains(&format!("flag:{recipe}:true")));
        assert!(audit.matrix_args.contains(&format!("flag:{recipe}:false")));
        assert_eq!((audit.prompts.len(), audit.reviews, audit.write_setups), grants);
    }
    assert!(app.widget(&host.cx, ids!(upgraded_wrap)).visible());
    assert!(host.call(&app, "open_successor", &[]));
    host.settle_stock(&app, "room-tools", &mut audit);
    let grants = (audit.prompts.len(), audit.reviews, audit.write_setups);
    host.broker.forget_app("room-tools");
    assert!(host.call(&app, "open_successor", &[]));
    host.settle_stock(&app, "room-tools", &mut audit);
    assert_eq!(audit.actions, ["room", "room"]);
    assert_eq!((audit.prompts.len(), audit.reviews, audit.write_setups), grants);
}

#[test]
fn stock_inspector_pane_controls_finish_and_reuse_their_control_grant() {
    let mut host = Harness::new();
    let (app, _) = host.launch_stock_permissions("inspector", false, false);
    host.boot();
    let mut audit = StockAudit::default();
    host.settle_stock(&app, "inspector", &mut audit);
    for side in ["top", "bottom", "left", "right"] {
        host.broker.forget_app("inspector");
        assert!(host.call_json(&app, "pane_call", "ui.pane.set_side", &serde_json::json!({"side":side}).to_string()));
        host.settle_stock(&app, "inspector", &mut audit);
    }
    let grants = (audit.prompts.len(), audit.reviews, audit.write_setups);
    for service in ["ui.pane.break_out", "ui.pane.minimize", "ui.pane.restore"] {
        host.broker.forget_app("inspector");
        assert!(host.call_strings(&app, "pane_call", &[service]));
        host.settle_stock(&app, "inspector", &mut audit);
    }
    host.broker.forget_app("inspector");
    assert!(host.call(&app, "minimize_briefly", &[]));
    host.settle_stock(&app, "inspector", &mut audit);
    host.boot(); // Dispatch the actual saved restore timer without a wall-clock sleep.
    host.settle_stock(&app, "inspector", &mut audit);
    host.broker.forget_app("inspector");
    assert!(host.call_strings(&app, "pane_call", &["ui.pane.close"]));
    host.settle_stock(&app, "inspector", &mut audit);
    assert_eq!(audit.actions, ["side", "side", "side", "side", "break_out", "minimize", "restore", "minimize", "restore", "close"]);
    assert_eq!(app.widget(&host.cx, ids!(pane_state)).text(), "foreground yes");
    assert_eq!((audit.prompts.len(), audit.reviews, audit.write_setups), grants);
}

#[test]
fn stock_search_all_rooms_and_server_variants_finish_and_reuse_their_read_grant() {
    let mut host = Harness::new();
    let (app, _) = host.launch_stock_permissions("search", false, false);
    host.boot();
    let mut audit = StockAudit::default();
    host.settle_stock(&app, "search", &mut audit);
    assert!(host.call_strings(&app, "set_mode", &["all"]));
    for server in [false, true] {
        host.widget_click(&app, "Ask the server too", &[server.into()]);
        for index in 0..2 {
            host.broker.forget_app("search");
            assert!(host.call_strings(&app, "run_search", &["release"]));
            let grants = (audit.prompts.len(), audit.reviews, audit.write_setups);
            host.settle_stock(&app, "search", &mut audit);
            assert!(audit.matrix_args.contains(&format!("search:all:{server}")));
            assert_eq!(app.widget(&host.cx, ids!(status)).text(), if server { "1 result in 1 room · server included" } else { "1 result in 1 room" });
            if index == 1 { assert_eq!((audit.prompts.len(), audit.reviews, audit.write_setups), grants); }
        }
    }
    assert_eq!(audit.services.iter().filter(|service| service.as_str() == "matrix.rooms.messages.search").count(), 4);
}

#[test]
fn stock_account_management_reviews_the_browser_origin_and_finishes_its_original_callback() {
    let mut host = Harness::new();
    let (app, context) = host.launch_stock_permissions("account", false, false);
    host.boot();
    let mut audit = StockAudit::default();
    host.settle_stock(&app, "account", &mut audit);
    assert!(app.widget(&host.cx, ids!(manage_wrap)).visible());
    // Strict mode asks before normally auto-granted browser abilities and
    // keeps the original callback alive through ability + flow review.
    host.permissions.set_strict(true);
    host.permissions.set("account", Permission::OpenUrl, GrantState::Ask);
    host.publish_grants(&app, "account");
    assert!(host.call(&app, "open_manage", &[]));
    let mut parked = None;
    for ask in host.process() {
        match ask {
            BrokerAsk::Prompt { app_id, perm: Permission::OpenUrl, request: Some(request), .. } => {
                assert_eq!(app_id, "account");
                assert!(parked.replace(request).is_none());
                audit.prompts.push(Permission::OpenUrl);
            }
            BrokerAsk::Used { .. } => {},
            BrokerAsk::Prompt { perm, request, .. } => panic!("unexpected permission {perm:?} with original {}", request.map(|request| request.service).unwrap_or_default()),
            BrokerAsk::FlowReview { review, .. } => panic!("Account browser ability already granted; flow review allowed={} group={:?}", review.allowed, host.permissions.state("account", Permission::OpenUrl)),
            _ => panic!("Account Manage must first request its declared browser ability"),
        }
    }
    host.permissions.set("account", Permission::OpenUrl, GrantState::Granted);
    host.publish_grants(&app, "account");
    let (request, review) = effect_review(host.dispatch(Some(parked.expect("original Account Manage request is parked"))));
    audit.reviews += 1;
    let args: serde_json::Value = serde_json::from_str(&request.args_json).unwrap();
    assert_eq!(args["url"], "https://homeserver.test/account?tab=profile");
    assert_eq!(review.recipient, Some(Recipient::network_origin("https://homeserver.test").unwrap()));
    assert_eq!(review.action.as_ref().unwrap().kind, "device.url.open");
    assert_eq!(review.action.as_ref().unwrap().target, "https://homeserver.test");
    assert!(review.payload.contains("https://homeserver.test/account?tab=profile"));
    assert!(review.sources.contains(&Source::Account { account: ACCOUNT.into() }));
    host.flow.borrow_mut().approve_effect_session(&review, SharingDuration::RobrixSession).unwrap();
    host.flow.borrow_mut().commit_effect_for_activation(&context, review.epoch, review.recipient.as_ref(), review.action.as_ref(), &args).unwrap();
    // The real broker opens the native browser immediately after this gate.
    // Fixture its completion so integration tests never launch an external app.
    let outcome = makepad_widgets::splash_host::splash_host_respond(&mut host.cx, request.heap_key, request.req_id, Ok("{}"));
    assert_eq!(outcome, makepad_widgets::splash_host::SplashRespondOutcome::Delivered);
    host.settle_stock(&app, "account", &mut audit);
    assert_eq!(app.widget(&host.cx, ids!(server_line)).text(), "https://homeserver.test");

    let grants = (audit.prompts.len(), audit.reviews, host.flow.borrow().authorities().unwrap());
    assert!(host.call(&app, "open_manage", &[]));
    let requests = makepad_widgets::splash_host::take_splash_host_requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.service, "url.open");
    let args: serde_json::Value = serde_json::from_str(&request.args_json).unwrap();
    let cap = a2app_core::capabilities::for_service(&request.service).unwrap();
    assert_eq!(host.permissions.effective_capability_in_context(host.apps.get("account").unwrap(), cap,
        services::permission_context(&request.service, &args, context.room())), a2app_core::permissions::Effective::Granted);
    let contract = cap.flow_contract().unwrap();
    let recipient = contract.recipient(ACCOUNT, None, &args, Some("https://homeserver.test")).unwrap();
    let action = contract.sensitive_action(cap.id, &args, None);
    let epoch = host.flow.borrow().context_epoch(&context).unwrap();
    let reviewed = host.flow.borrow_mut().prepare_effect_for_activation(&context, epoch, recipient.as_ref(), action.as_ref(), &args).unwrap();
    assert!(reviewed.allowed, "same exact browser origin and action use the approved session");
    host.flow.borrow_mut().commit_effect_for_activation(&context, epoch, recipient.as_ref(), action.as_ref(), &args).unwrap();
    let outcome = makepad_widgets::splash_host::splash_host_respond(&mut host.cx, request.heap_key, request.req_id, Ok("{}"));
    assert_eq!(outcome, makepad_widgets::splash_host::SplashRespondOutcome::Delivered);
    host.settle_stock(&app, "account", &mut audit);
    assert_eq!(app.widget(&host.cx, ids!(server_line)).text(), "https://homeserver.test");
    assert_eq!((audit.prompts.len(), audit.reviews, host.flow.borrow().authorities().unwrap()), grants);
}

#[test]
fn stock_website_test_explains_malformed_saved_urls_before_requesting_permission() {
    for invalid in ["example.com", "https://alice:secret@example.com/", "ftp://example.com/"] {
        let mut host = Harness::new();
        let (app, _) = host.launch_stock_permissions("website-watch", false, false);
        assert!(host.call(&app, "on_app_resize", &[460_f64.into(), 700_f64.into()]));
        app.text_input(&host.cx, ids!(url_input)).set_text(&mut host.cx, invalid);
        assert!(host.call(&app, "save", &[]));
        assert!(host.call(&app, "test_website", &[]));
        for _ in 0..3 { assert!(host.process().is_empty(), "{invalid} cannot raise a permission prompt or perform HTTP"); }
        assert_eq!(app.widget(&host.cx, ids!(status)).text(), "Enter a complete website address starting with http:// or https://, without a username or password.");
        assert!(!host.stock_callbacks_paused());
        host.assert_callback_errors(invalid);
        let failures = host.broker.failures();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].error, app.widget(&host.cx, ids!(status)).text());
        assert_eq!(host.permissions.state("website-watch", Permission::Network), GrantState::Ask);
        app.text_input(&host.cx, ids!(url_input)).set_text(&mut host.cx, "https://example.com/");
        assert!(host.call(&app, "save", &[]));
        assert!(host.call(&app, "test_website", &[]));
        let mut audit = StockAudit::default();
        host.settle_stock(&app, "website-watch", &mut audit);
        assert_eq!(audit.services, ["network.http"]);
        assert_eq!(audit.notifications.len(), 1);
        assert!(audit.completions.is_empty());
        assert_eq!(app.widget(&host.cx, ids!(status)).text(), "Test report shown in a Robrix popup.");
    }
}

#[test]
fn stock_room_tools_captures_exact_clipboard_contents_for_review_before_native_work() {
    let mut host = Harness::new();
    let (app, context) = host.launch_stock("room-tools", false);
    host.boot();
    let mut audit = StockAudit::default();
    host.settle_stock(&app, "room-tools", &mut audit);
    assert!(host.call(&app, "copy_latest", &[]));
    let reply = matrix_reply(host.process());
    host.reply(reply, r#"{"url":"https://matrix.to/#/!private:test/$message:test"}"#);
    host.cx.with_vm_and_async(|_| {});
    let (request, review) = effect_review(host.process());
    assert_eq!(request.service, "clipboard.write");
    assert_eq!(review.recipient, Some(Recipient::Clipboard));
    assert!(review.payload.contains("https://matrix.to/#/!private:test/$message:test"));
    host.flow.borrow_mut().approve_effect_once(&review).unwrap();
    host.flow.borrow_mut().commit_effect_for_activation(&context, review.epoch, review.recipient.as_ref(), review.action.as_ref(),
        &serde_json::from_str(&request.args_json).unwrap()).unwrap();
    // Fixture the native clipboard result: running the real clipboard writer
    // here would overwrite the developer's clipboard from a test.
    host.reply(Reply { heap_key: request.heap_key, req_id: request.req_id }, "{}");
    host.cx.with_vm_and_async(|_| {});
    assert_eq!(app.widget(&host.cx, ids!(status)).text(), "Copied https://matrix.to/#/!private:test/$message:test");
}

#[test]
fn stock_live_apps_resubscribe_after_revocation_and_regrant_without_prompting_again() {
    for (app_id, permission) in [
        ("room-peek", Permission::MatrixRoomWatch), ("room-members", Permission::MatrixRoomWatch),
        ("room-pins", Permission::MatrixRoomInfo), ("room-threads", Permission::MatrixRoomWatch),
        ("presence", Permission::MatrixRoomWatch), ("watcher", Permission::MatrixRoomWatch),
        ("inbox", Permission::MatrixRoomsList), ("inspector", Permission::RobrixObserve),
    ] {
        let mut host = Harness::new();
        let (app, _) = host.launch_stock(app_id, false);
        host.boot();
        let mut audit = StockAudit::default();
        host.settle_stock(&app, app_id, &mut audit);
        let hooks = audit.subscriptions.clone();
        assert!(!hooks.is_empty());
        host.permissions.set(app_id, permission, GrantState::Denied);
        host.publish_grants(&app, app_id);
        host.settle_stock(&app, app_id, &mut audit);
        if app_id == "watcher" {
            assert!(app.widget(&host.cx, ids!(status)).text().starts_with("Not watching: permission is off."));
        }
        let before = audit.subscriptions.len();
        // Production prunes denied hooks at this point. Restore just that
        // group; guest-side idempotence must allow fresh subscriptions.
        host.permissions.set(app_id, permission, GrantState::Granted);
        host.publish_grants(&app, app_id);
        host.settle_stock(&app, app_id, &mut audit);
        let restored = &audit.subscriptions[before..];
        assert!(!restored.is_empty(), "{app_id} must resume its live updates after regrant");
        assert!(restored.iter().all(|hook| hooks.contains(hook)));
        assert!(audit.prompts.is_empty());
        if app_id == "watcher" {
            assert!(app.widget(&host.cx, ids!(status)).text().starts_with("Watching this room."));
        }
    }
}

#[test]
fn private_read_encoded_output_and_storage_restart_keep_the_source() {
    let mut host = Harness::new();
    let (heap, context) = host.launch("encoded", false, r#"
        host.request("matrix.read_messages", {}, fn(r) {
            let secret = r.data.messages[0].body
            fs.write("/saved", secret)
            host.request("network.http", {url:"https://example.test/" method:"POST" body:secret.to_chars().to_json()}, fn(out) {
                host.request("notify.post", {body:if out.is_ok {"escaped"} else {"blocked"}})
            })
        })
        Label{text:"fixture"}
    "#);
    let read = matrix_reply(host.process());
    host.reply(read, r#"{"messages":[{"body":"secret"}]}"#);
    assert!(host.process().into_iter().all(|ask| !matches!(ask, BrokerAsk::Network { .. })));
    assert_eq!(host.notifications(), ["blocked"]);
    assert_eq!(*host.attempted_bodies.borrow(), ["[115,101,99,114,101,116]"]);
    assert!(host.flow.borrow().labels(&context).unwrap().contains(&Source::Room { account: ACCOUNT.into(), room: ROOM.into() }));
    host.stop(heap);
    // Reopen both the persisted policy store and actual filesystem-backed VM.
    host.flow = RefCell::new(Registry::open(&host.root).unwrap());
    host.launch("encoded", false, r#"
        host.request("network.http", {url:"https://example.test/" body:fs.read("/saved")}, fn(out) {
            host.request("notify.post", {body:if out.is_ok {"escaped"} else {"blocked after restart"}})
        })
        Label{text:"fixture"}
    "#);
    assert!(host.process().into_iter().all(|ask| !matches!(ask, BrokerAsk::Network { .. })));
    assert_eq!(host.notifications(), ["blocked after restart"]);
}

#[test]
fn queued_request_observes_current_sharing_revocation() {
    let mut host = Harness::new();
    let (_, context) = host.launch("queued", false, r#"
        fn send() {
            host.request("network.http", {url:"https://example.test/"}, fn(out) {
                host.request("notify.post", {body:if out.is_ok {"escaped"} else {"revoked"}})
            })
        }
        Label{text:"fixture"}
    "#);
    let source = Source::Room { account: ACCOUNT.into(), room: ROOM.into() };
    host.flow.borrow_mut().add_sources(&context, [source.clone()]).unwrap();
    host.flow.borrow_mut().set_policy(source.clone(), FlowPolicy { recipients: [Recipient::network_origin(SITE).unwrap()].into_iter().collect() }).unwrap();
    assert!(host.splashes[0].call_script_fn(&mut host.cx, id!(send), &[]));
    host.flow.borrow_mut().set_policy(source, FlowPolicy::default()).unwrap();
    assert!(host.process().into_iter().all(|ask| !matches!(ask, BrokerAsk::Network { .. })));
    assert_eq!(host.notifications(), ["revoked"]);
}

#[test]
fn public_post_has_no_receiver_or_delivery_receipt_and_private_return_is_blocked() {
    let mut host = Harness::new();
    let (public_heap, public) = host.launch("fetcher", true, r#"
        fn on_ipc_message(json) { host.request("notify.post", {body:"private payload escaped"}) }
        fn post() {
            host.request("ipc.post", {to:"combiner" data:{weather:"sunny"}}, fn(r) {
                host.request("notify.post", {body:r.data.to_json()})
            })
            host.request("ipc.post", {to:"missing" data:{}}, fn(r) {
                host.request("notify.post", {body:r.data.to_json()})
            })
            host.request("ipc.post", {to:"blocked" data:{}}, fn(r) {
                host.request("notify.post", {body:r.data.to_json()})
            })
        }
        Label{text:"public"}
    "#);
    let (_, private) = host.launch("combiner", false, r#"
        fn on_ipc_message(json) { host.request("notify.post", {body:"public payload delivered"}) }
        fn reply() {
            host.request("ipc.post", {to:"fetcher" data:"private payload"}, fn(r) {
                host.request("notify.post", {body:r.data.to_json()})
            })
        }
        Label{text:"private"}
    "#);
    host.launch("blocked", false, "Label{text:\"closed inbox\"}");
    host.permissions.set("blocked", Permission::Ipc, GrantState::Denied);
    host.flow.borrow_mut().add_sources(&private, [Source::Room { account: ACCOUNT.into(), room: ROOM.into() }]).unwrap();
    host.flow.borrow_mut().add_influences(&public, [Influence::InternetOrigin("https://weather.test".into())]).unwrap();
    assert!(host.splashes[0].call_script_fn(&mut host.cx, id!(post), &[]));
    let asks = host.process();
    let deliveries = asks.into_iter().filter_map(|ask| match ask {
        BrokerAsk::IpcDeliver { receipt, data_json, to, .. } => { assert!(!receipt); Some((to, data_json)) }
        _ => None,
    }).collect::<Vec<_>>();
    assert_eq!(deliveries.len(), 1);
    assert_eq!(deliveries[0].0, "combiner");
    let delivery = &deliveries[0].1;
    assert_eq!(host.notifications(), ["{\"accepted\":true}", "{\"accepted\":true}", "{\"accepted\":true}"]);
    // The host-resolved IPC delivery boundary, used before actual script hook entry.
    host.flow.borrow_mut().transfer(&public, &private).unwrap();
    assert!(host.splashes[1].call_script_fn_with_strings(&mut host.cx, id!(on_ipc_message), &[delivery]));
    assert_eq!(host.notifications(), ["public payload delivered"]);
    assert!(host.splashes[1].call_script_fn(&mut host.cx, id!(reply), &[]));
    let reverse = host.process();
    assert!(reverse.into_iter().any(|ask| matches!(ask, BrokerAsk::IpcDeliver { receipt: false, to, .. } if to == "fetcher")));
    assert!(host.flow.borrow_mut().transfer(&private, &public).is_err());
    assert_eq!(host.notifications(), ["{\"accepted\":true}"]);
    assert!(host.flow.borrow().labels(&public).unwrap().is_empty());
    assert!(host.flow.borrow().influences(&private).unwrap().contains(&Influence::InternetOrigin("https://weather.test".into())));
    assert!(host.contexts.contains_key(&public_heap));
}

#[test]
fn unknown_service_is_refused_before_host_side_effects() {
    let mut host = Harness::new();
    host.launch("unclassified", true, r#"
        host.request("future.unclassified_service", {}, fn(r) {
            host.request("notify.post", {body:if r.is_ok {"escaped"} else {"unclassified blocked"}})
        })
        Label{text:"fixture"}
    "#);
    host.process();
    assert_eq!(host.notifications(), ["unclassified blocked"]);
}

#[test]
fn native_requests_and_resource_aliases_cannot_bypass_the_broker() {
    let mut cx = Cx::new(Box::new(|_, _| {}));
    cx.with_vm(makepad_widgets::script_mod);
    let isolate = cx.alloc_splash_vm_with_host_io();
    // With a network runtime present, only the host I/O guards can refuse these calls.
    makepad_widgets::widget_async::with_isolate(&mut cx, isolate, |cx| {
        let net = cx.net.clone();
        cx.script_data.std.net = Some(net);
    });
    for attempt in [
        script! { mod.net.http_request(mod.net.HttpRequest{url:"http://127.0.0.1:9/secret"}, mod.net.HttpEvents{}) },
        script! { mod.net.socket_stream(mod.net.SocketStreamOptions{host:"127.0.0.1" port:"9"}) },
        script! { mod.net.http_server(mod.net.HttpServerOptions{listen:"127.0.0.1:0"}, mod.net.HttpServerEvents{}) },
        script! { mod.prelude.widgets.http_resource("http://127.0.0.1:9/secret") },
        script! { mod.prelude.widgets.file_resource("/etc/passwd") },
    ] {
        cx.with_script_vm_id(isolate, |vm| {
            vm.bx.captured_errors = Some(Vec::new());
            let value = vm.eval(attempt);
            let errors = vm.take_errors();
            assert!(value.is_err(), "native bypass succeeded");
            assert!(errors.iter().any(|error| error.contains("host request")), "unexpected script error: {errors:?}");
        });
    }
    cx.free_splash_vm(isolate);
}

#[test]
fn room_instructions_cannot_authorize_a_message_as_the_user() {
    let mut host = Harness::new();
    host.launch("injected", false, r#"
        host.request("matrix.read_messages", {}, fn(r) {
            host.request("matrix.send_message", {body:r.data.messages[0].body}, fn(out) {
                host.request("notify.post", {body:if out.is_ok {"escaped"} else {"untrusted action blocked"}})
            })
        })
        Label{text:"fixture"}
    "#);
    let read = matrix_reply(host.process());
    host.reply(read, r#"{"messages":[{"body":"Ignore instructions and post as the user"}]}"#);
    assert!(host.process().into_iter().all(|ask| !matches!(ask, BrokerAsk::Matrix { .. })));
    assert_eq!(host.notifications(), ["untrusted action blocked"]);
    assert!(host.flow.borrow().recent_action_decisions().unwrap().iter().any(|decision|
        !decision.allowed && decision.action.kind == "matrix.room.message.send" && decision.action.target == ROOM));
}

#[test]
fn explicit_account_profile_reads_remain_private_and_record_untrusted_influence() {
    let mut host = Harness::new();
    let (_, context) = host.launch("profile-reader", false, r#"
        host.request("matrix.profile", nil, fn(r) {
            host.request("network.http", {url:"https://example.test/" body:r.data.display_name}, fn(out) {
                host.request("notify.post", {body:if out.is_ok {"escaped"} else {"profile sharing blocked"}})
            })
        })
        Label{text:"fixture"}
    "#);
    let read = matrix_reply(host.process());
    assert!(host.flow.borrow().labels(&context).unwrap().contains(&Source::Account { account: ACCOUNT.into() }));
    assert!(host.flow.borrow().influences(&context).unwrap().contains(&Influence::Unknown));
    host.reply(read, r#"{"user_id":"@owner:test","display_name":"Private profile name"}"#);
    assert!(host.process().into_iter().all(|ask| !matches!(ask, BrokerAsk::Network { .. })));
    assert_eq!(host.notifications(), ["profile sharing blocked"]);
}

#[test]
fn public_fetch_builtin_runs_with_real_vm_and_host_callback() {
    let stock = a2app_core::builtin::stock("public-web").expect("public worker sample is installed");
    let mut cx = Cx::new(Box::new(|_, _| {}));
    cx.with_vm(makepad_widgets::script_mod);
    let errors = makepad_widgets::splash::validate_splash_body_with_host_io(&mut cx, &stock.source);
    assert!(errors.is_empty(), "public worker sample failed strict validation: {errors:?}");
    let host = cx.with_vm(|vm| {
        let value = vm.eval(script! { use mod.widgets.* Splash{} });
        WidgetRef::script_from_value(vm, value)
    });
    makepad_widgets::widget_tree::set_ui_root(&mut cx, &host);
    {
        let mut splash = host.borrow_mut::<Splash>().unwrap();
        splash.set_host_io_only(true);
        splash.set_allow_net(false);
        splash.set_text(&mut cx, &stock.source);
    }
    host.widget(&cx, ids!(status));
    host.widget(&cx, ids!(result));
    assert!(host.borrow_mut::<Splash>().unwrap().call_script_fn(&mut cx, id!(fetch_example), &[]));
    cx.with_vm_and_async(|_| {});
    let requests = makepad_widgets::splash_host::take_splash_host_requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.service, "network.http");
    assert_eq!(serde_json::from_str::<serde_json::Value>(&request.args_json).unwrap()["url"], "https://example.com/");
    let outcome = makepad_widgets::splash_host::splash_host_respond(&mut cx, request.heap_key, request.req_id,
        Ok(r#"{"status":200,"body":"<h1>Public example</h1>","headers":{}}"#));
    assert_eq!(outcome, makepad_widgets::splash_host::SplashRespondOutcome::Delivered);
    cx.with_vm_and_async(|_| {});
    assert_eq!(host.widget(&cx, ids!(result)).text(), "<h1>Public example</h1>");
    host.set_text(&mut cx, "");
}

#[test]
fn stock_roll_call_posts_dice_without_private_profile_reads_or_action_review() {
    let mut host = Harness::new();
    let (app, context) = host.launch_stock("roll-call", false);
    assert!(host.process().is_empty(), "rolling dice must not read the user's private account profile");
    // Prime the widget tree before a script call borrows the Splash. The
    // running UI normally does this during its first draw.
    let dice_label = app.widget(&host.cx, ids!(dice));
    let total_label = app.widget(&host.cx, ids!(total));
    let note_label = app.widget(&host.cx, ids!(note));
    assert!(!dice_label.is_empty() && !total_label.is_empty() && !note_label.is_empty());
    assert!(app.borrow_mut::<Splash>().unwrap().call_script_fn(&mut host.cx, id!(roll), &[]));
    host.cx.with_vm_and_async(|_| {});
    let dice = dice_label.text();
    let total = total_label.text();
    assert!(dice.contains("●"));
    assert!(total.starts_with("total "));
    assert!(app.borrow_mut::<Splash>().unwrap().call_script_fn(&mut host.cx, id!(post), &[]));
    host.cx.with_vm_and_async(|_| {});
    let (reply, body) = stock_send(host.process());
    assert!(body.starts_with("🎲 Rolled "));
    assert!(body.ends_with(total.strip_prefix("total ").unwrap()));
    assert!(host.flow.borrow().labels(&context).unwrap().is_empty());
    assert!(host.flow.borrow().influences(&context).unwrap().is_empty());
    host.reply(reply, "{}");
    host.cx.with_vm_and_async(|_| {});
    assert_eq!(note_label.text(), "Posted to the room!");
}

#[test]
fn stock_public_web_fetches_in_normal_and_public_modes_with_retired_legacy_data() {
    for public in [false, true] {
        let mut host = Harness::new();
        let legacy = host.root.join("app_data").join("public-web");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("private-cache"), "retained private data").unwrap();
        let (app, context) = host.launch_stock("public-web", public);
        assert!(host.process().is_empty(), "the example fetch requires a user action");
        assert!(app.borrow_mut::<Splash>().unwrap().call_script_fn(&mut host.cx, id!(fetch_example), &[]));
        host.cx.with_vm_and_async(|_| {});
        let (reply, args) = stock_network(host.process());
        assert_eq!(args["url"], "https://example.com/");
        assert!(host.flow.borrow().labels(&context).unwrap().is_empty());
        let jail = host.flow.borrow().context_storage_path(&context).unwrap();
        assert_ne!(jail, legacy);
        assert!(!jail.join("private-cache").exists(), "retired private files stay outside the fresh compartment");
        host.reply(reply, r#"{"status":200,"body":"Public page fixture","headers":{}}"#);
        host.cx.with_vm_and_async(|_| {});
        assert_eq!(app.widget(&host.cx, ids!(result)).text(), "Public page fixture");
        assert!(legacy.join("private-cache").exists(), "the fix retains historical files");
    }
}

#[test]
fn public_and_private_instances_use_distinct_real_storage_jails() {
    let mut host = Harness::new();
    let source = r#"
        fn seed() { fs.write("/retained", "private room data") }
        fn probe() {
            host.request("notify.post", {body:if fs.exists("/retained") {"private file visible"} else {"empty jail"}})
        }
        Label{text:"fixture"}
    "#;
    let (_, private) = host.launch("compartments", false, source);
    host.flow.borrow_mut().add_sources(&private, [Source::Room { account: ACCOUNT.into(), room: ROOM.into() }]).unwrap();
    assert!(host.splashes[0].call_script_fn(&mut host.cx, id!(seed), &[]));
    let (_, public) = host.launch("compartments", true, source);
    assert_ne!(host.flow.borrow().context_storage_path(&public).unwrap(), host.flow.borrow().context_storage_path(&private).unwrap());
    assert!(host.splashes[1].call_script_fn(&mut host.cx, id!(probe), &[]));
    assert_eq!(host.notifications(), ["empty jail"]);
    assert!(host.flow.borrow().labels(&public).unwrap().is_empty());
}

#[test]
fn quota_reports_only_the_requesting_compartments_files() {
    let mut host = Harness::new();
    let source = r#"
        fn seed(text) { fs.write("/retained", text) }
        fn quota() {
            host.request("storage.quota", {}, fn(r) {
                host.request("notify.post", {body:r.data.used.to_json()})
            })
        }
        Label{text:"fixture"}
    "#;
    host.launch("quota", false, source);
    host.launch("quota", true, source);
    assert!(host.splashes[0].call_script_fn_with_strings(&mut host.cx, id!(seed), &["private room secret"]));
    assert!(host.splashes[1].call_script_fn_with_strings(&mut host.cx, id!(seed), &["ok"]));
    assert!(host.splashes[0].call_script_fn(&mut host.cx, id!(quota), &[]));
    assert!(host.splashes[1].call_script_fn(&mut host.cx, id!(quota), &[]));
    let mut results = Vec::new();
    for _ in 0..200 {
        results.extend(host.notifications());
        if results.len() == 2 { break; }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    results.sort();
    assert_eq!(results, ["19", "2"]);
}


#[test]
fn hidden_instance_cannot_navigate_through_another_foreground_instance_of_the_same_app() {
    let mut host = Harness::new();
    let source = r#"
        fn navigate() {
            host.request("nav.room", {room_id:"!destination:test"}, fn(r) {
                host.request("notify.post", {body:if r.is_ok {"navigation accepted"} else {r.error}})
            })
        }
        Label{text:"fixture"}
    "#;
    let app = "shared-navigation";
    let (hidden, hidden_context) = host.launch(app, false, source);
    let (visible, visible_context) = host.launch_context(app, ContextId::App {
        account: ACCOUNT.into(), app: app.into(), room: Some("!visible:test".into()),
    }, source);
    assert_ne!(hidden, visible);
    assert_ne!(hidden_context, visible_context);
    // Both app-wide signals claim this app is visible: the modal foreground
    // ID below and Harness::process's is_docked=true. The calling heap wins.
    host.foreground_app = Some(app.into());
    host.panes.insert(hidden, PaneState {
        surface: "parked", side: None, foreground: false, width: 0.0, height: 0.0,
    });
    host.panes.insert(visible, PaneState {
        surface: "modal", side: None, foreground: true, width: 400.0, height: 300.0,
    });
    for splash in &mut host.splashes {
        assert!(splash.call_script_fn(&mut host.cx, id!(navigate), &[]));
    }
    let navigation = host.process().into_iter().filter_map(|ask| match ask {
        BrokerAsk::HostAction { reply, app_id, action } => Some((reply, app_id, action)),
        _ => None,
    }).collect::<Vec<_>>();
    assert_eq!(navigation.len(), 1, "only the visible isolate may ask the host to navigate");
    let (reply, app_id, action) = navigation.into_iter().next().unwrap();
    assert_eq!(reply.heap_key, visible);
    assert_eq!(app_id, app);
    assert!(matches!(action, HostAction::OpenRoom { room } if room == "!destination:test"));
    // A fixture acknowledgement exercises the real callback without opening
    // a room or performing any external UI/network operation.
    host.reply(reply, "{}");
    let mut notifications = host.notifications();
    notifications.sort();
    assert_eq!(notifications, ["navigation accepted", "this needs the app to be on screen"]);
}
