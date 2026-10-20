// Shared by the stock mini-app test crates through `include!`: the real Splash
// VM, host bridge and broker, plus the fixtures their permission audits use.

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
use makepad_widgets::{makepad_script::ScriptRunBudget, widget_async::{SplashVmId, with_isolate}};

const ACCOUNT: &str = "@owner:test";
const ROOM: &str = "!private:test";
const SITE: &str = "https://example.test/";
static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);
static CALLBACK_LOG_TAP: std::sync::Once = std::sync::Once::new();

thread_local! {
    static CALLBACK_ERRORS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

fn capture_callback_error(message: &str, _: LogLevel) {
    // splash_host_respond drains synchronous errors into the logger before
    // the caller can inspect its VM. Observe that real path without replacing it.
    if message.starts_with("splash host callback error:") || message.starts_with("splash: ") {
        CALLBACK_ERRORS.with(|errors| errors.borrow_mut().push(message.into()));
    }
}

fn headless_splash_entry<R>(cx: &mut Cx, vm_id: SplashVmId, run: impl FnOnce(&mut Cx) -> R) -> R {
    // The UI's 64ms wall deadline includes time a parallel test is descheduled.
    // Keep Splash's own 200,000-instruction guards, with the longer wall budget
    // used by Makepad's headless script benchmark instead of testing CPU load.
    with_isolate(cx, vm_id, |cx| {
        let previous = cx.with_vm(|vm| vm.bx.run_budget.replace(ScriptRunBudget::from_durations(
            std::time::Duration::from_secs(10), std::time::Duration::from_secs(10), 512,
        )));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(cx)));
        cx.with_vm(|vm| vm.bx.run_budget = previous);
        match result {
            Ok(result) => result,
            Err(error) => std::panic::resume_unwind(error),
        }
    })
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

/// The visible widget showing `text`: a hidden control can't be clicked.
fn visible_control(root: &WidgetRef, text: &str) -> Option<WidgetRef> {
    if !root.visible() { return None; }
    if root.text() == text { return Some(root.clone()); }
    let mut children = Vec::new();
    root.children(&mut |_, child| children.push(child));
    children.into_iter().find_map(|child| visible_control(&child, text))
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

    /// Runs a host reply or timer for the guest owning `heap_key` under the same headless
    /// wall budget as `call_guest`. Splash's instruction cap still applies.
    fn guest_work<R>(&mut self, heap_key: usize, run: impl FnOnce(&mut Cx) -> R) -> R {
        let source = self.stock_hosts.iter().map(|host| host.borrow::<Splash>().unwrap().view.source.clone())
            .chain(self.splashes.iter().map(|splash| splash.view.source.clone()))
            .find(|source| source.heap_key() == heap_key);
        match source.and_then(|source| self.cx.script_ref_vm_id(&source)) {
            Some(vm_id) => headless_splash_entry(&mut self.cx, vm_id, run),
            None => run(&mut self.cx),
        }
    }

    fn load_splash(&mut self, splash: &mut Splash, app: &str, source: &str) -> usize {
        splash.set_debug_name(app);
        // Allocate Splash's private VM with an empty view before running the
        // actual guest source. This lets the test install its entry budget
        // without replacing the real Splash evaluator or its instruction cap.
        splash.set_text(&mut self.cx, " ");
        let vm_id = self.cx.script_ref_vm_id(&splash.view.source).expect("empty Splash owns an isolate");
        headless_splash_entry(&mut self.cx, vm_id, |cx| {
            cx.with_script_vm_id_trusted(vm_id, |vm| vm.bx.captured_errors = Some(Vec::new()));
            splash.set_text(cx, source);
        });
        let logged = CALLBACK_ERRORS.with(|errors| std::mem::take(&mut *errors.borrow_mut()));
        assert!(logged.is_empty(), "{app} source startup errors: {logged:?}");
        self.cx.with_script_vm_id_trusted(vm_id, |vm| {
            let errors = vm.take_errors();
            assert!(errors.is_empty(), "{app} source startup errors: {errors:?}");
            vm.bx.captured_errors = Some(Vec::new());
        });
        splash.isolate_heap_key(&mut self.cx).expect("script isolate was created")
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
            assert!(!value.is_err(), "{app} host widget startup errors: {:?}", vm.take_errors());
            Splash::script_from_value(vm, value)
        });
        splash.set_host_io_only(true);
        splash.set_allow_net(false);
        splash.set_host_tag(&mut self.cx, Some(instance_tag(app, context.room())));
        let jail = self.flow.borrow().context_storage_path(&context).unwrap();
        std::fs::create_dir_all(&jail).unwrap();
        splash.set_sandbox_dir(&mut self.cx, Some(jail));
        let heap = self.load_splash(&mut splash, app, source);
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
            assert!(!value.is_err(), "{app} host widget startup errors: {:?}", vm.take_errors());
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
            self.load_splash(&mut splash, app, &manifest.source)
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
                        let reply = Reply { heap_key: request.heap_key, req_id: request.req_id };
                        self.guest_work(reply.heap_key, |cx| services::respond(cx, reply, Err("Permission denied.")));
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

    fn call_guest(&mut self, app: &WidgetRef, name: &str, call: impl FnOnce(&mut Splash, &mut Cx) -> bool) -> bool {
        let source = app.borrow::<Splash>().unwrap().view.source.clone();
        let vm_id = self.cx.script_ref_vm_id(&source).expect("guest call belongs to its live isolate");
        let called = headless_splash_entry(&mut self.cx, vm_id, |cx|
            call(&mut app.borrow_mut::<Splash>().unwrap(), cx));
        // Restore MAIN before pumping continuations: widget async dispatch
        // owns cross-VM routing and must not run with this isolate installed.
        self.cx.with_vm_and_async(|_| {});
        self.assert_callback_errors(name);
        called
    }

    fn call(&mut self, app: &WidgetRef, name: &str, args: &[ScriptValue]) -> bool {
        self.call_guest(app, name, |splash, cx| splash.call_script_fn(cx, LiveId::from_str(name), args))
    }

    fn call_strings(&mut self, app: &WidgetRef, name: &str, args: &[&str]) -> bool {
        self.call_guest(app, name, |splash, cx| splash.call_script_fn_with_strings(cx, LiveId::from_str(name), args))
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

    fn call_index_text(&mut self, app: &WidgetRef, name: &str, index: u32, text: &str) -> bool {
        let source = app.borrow::<Splash>().unwrap().view.source.clone();
        let vm_id = self.cx.script_ref_vm_id(&source).unwrap();
        let text = self.cx.with_script_vm_id(vm_id, |vm| vm.new_string_with(|_, out| out.push_str(text)));
        self.call(app, name, &[index.into(), text])
    }

    fn call_object(&mut self, app: &WidgetRef, name: &str, json: &str) -> bool {
        let source = app.borrow::<Splash>().unwrap().view.source.clone();
        let vm_id = self.cx.script_ref_vm_id(&source).unwrap();
        let argument = self.cx.with_script_vm_id(vm_id, |vm|
            makepad_widgets::makepad_script::json::JsonParserThread::default().read_json(json, &mut vm.bx.heap));
        self.call(app, name, &[argument])
    }

    fn widget_click(&mut self, app: &WidgetRef, text: &str, args: &[ScriptValue]) {
        // Names the hidden ancestor when the control exists but isn't visible.
        fn hidden_by(root: &WidgetRef, text: &str, path: &mut Vec<String>) -> bool {
            path.push(format!("{}{}", if root.visible() { "" } else { "HIDDEN " }, root.text().chars().take(24).collect::<String>()));
            if root.text() == text { return true; }
            let mut children = Vec::new();
            root.children(&mut |id, child| children.push((id, child)));
            for (id, child) in children {
                path.push(format!("{id}"));
                if hidden_by(&child, text, path) { return true; }
                path.pop();
            }
            path.pop();
            false
        }
        let widget = visible_control(app, text).unwrap_or_else(|| {
            let mut path = Vec::new();
            let exists = hidden_by(app, text, &mut path);
            panic!("the visible stock control {text:?} exists (exists: {exists}; path: {path:?})")
        });
        self.widget_callback(app, &widget, id!(on_click), args);
    }

    /// Retries from whichever of `texts` is on screen, as a user would.
    fn widget_click_any(&mut self, app: &WidgetRef, texts: &[&str], args: &[ScriptValue]) {
        let text = texts.iter().copied().find(|text| visible_control(app, text).is_some())
            .unwrap_or_else(|| panic!("one of the stock controls {texts:?} is visible"));
        self.widget_click(app, text, args);
    }

    fn widget_callback(&mut self, app: &WidgetRef, widget: &WidgetRef, method: LiveId, args: &[ScriptValue]) {
        let source = app.borrow::<Splash>().unwrap().view.source.clone();
        let vm_id = self.cx.script_ref_vm_id(&source).unwrap();
        let (source, callback) = self.cx.with_script_vm_id(vm_id, |vm| {
            let source = widget.script_source();
            let value = vm.bx.heap.value(source, method.into(), NoTrap);
            assert!(!value.is_nil(), "the actual stock control has its guest callback");
            let callback = ScriptFnRef::script_from_value(vm, value);
            let source = vm.bx.heap.new_object_ref(source);
            (source, callback)
        });
        self.cx.widget_to_script_call(widget.widget_uid(), NIL, source, callback, args);
        self.cx.with_vm_and_async(|_| {});
        self.assert_callback_errors("stock widget callback");
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
                    BrokerAsk::Unsubscribe { reply, hook, .. } => {
                        audit.unsubscriptions.push(hook.unwrap_or("*").into());
                        self.reply(reply, "{}");
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
                    BrokerAsk::McpToolResult { reply, ok, text, .. } => {
                        audit.tool_results.push((ok, text));
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
            let is_fired = self.guest_work(timer.callback.heap_key(), |cx| hooks.into_iter().any(|hook| hook(cx, &timer, NIL)));
            assert!(is_fired, "stock boot timer belongs to its isolate");
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
        self.guest_work(reply.heap_key, |cx| services::respond(cx, reply, Ok(data)));
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
    unsubscriptions: Vec<String>,
    actions: Vec<String>,
    notifications: Vec<String>,
    completions: Vec<(u64, bool)>,
    write_setups: usize,
    matrix_args: Vec<String>,
    tool_results: Vec<(bool, String)>,
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
