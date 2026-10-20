//! Room Gallery stock mini-app: real Splash VM, host bridge and broker, with room media answered by fixtures.
#![allow(dead_code, unused_imports)] // the shared stock harness serves several test crates
include!("support/stock_harness.rs");

use serde_json::json;

const APP: &str = "room-gallery";
/// A real 2x2 PNG, so the decoded bytes reach Makepad's image loader.
const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAIAAAD91JpzAAAAEElEQVR4nGM4IRcFRAwQCgAkxgUBBHQ3qwAAAABJRU5ErkJggg==";
const TS: u64 = 1_720_000_000_000;

/// How the fixture user answers the gallery's prompts and reviews.
#[derive(Clone, Copy)]
struct Answers {
    is_read_allowed: bool,
    is_watch_allowed: bool,
    is_media_allowed: bool,
    is_nav_allowed: bool,
    is_composer_allowed: bool,
    is_sharing_allowed: bool,
}

impl Default for Answers {
    fn default() -> Self {
        Self { is_read_allowed: true, is_watch_allowed: true, is_media_allowed: true, is_nav_allowed: true,
            is_composer_allowed: true, is_sharing_allowed: true }
    }
}

/// Everything the gallery asked the host for.
#[derive(Default)]
struct Trace {
    audit: StockAudit,
    batches: Vec<Vec<Permission>>,
    reviews: Vec<String>,
    reads: Vec<u32>,
    older: Vec<(Option<String>, u32)>,
    events: Vec<String>,
    previews: Vec<(String, u32)>,
    downloads: Vec<String>,
    navigations: Vec<String>,
    attachments: Vec<(String, String, String, Option<String>)>,
    unsubscribed: Vec<String>,
    /// Setup batches Robrix answered without a prompt, everything already being allowed.
    silent_batches: usize,
    refusals: usize,
    /// The exact arguments of every Matrix request, as the app sent them.
    args: Vec<(String, serde_json::Value)>,
}

/// One gallery instance on the real broker. Media downloads are parked so each test answers them.
struct Rig {
    host: Harness,
    app: WidgetRef,
    context: ContextId,
    trace: Trace,
    answers: Answers,
    parked: VecDeque<(Reply, String, Option<u32>)>,
    /// When set, latest-message reads wait in `parked_reads` too.
    is_parking_reads: bool,
    parked_reads: VecDeque<Reply>,
    /// The `device.info` answer, or None to fail it.
    device: Option<serde_json::Value>,
    /// A hook whose subscription is refused, with the refusal's text.
    refused_hook: Option<(&'static str, &'static str)>,
}

/// A US Pacific desktop, the default device answer.
fn pacific_device() -> serde_json::Value {
    json!({"platform": "macos", "locale": "en_US.UTF-8", "time_zone": "America/Los_Angeles",
        "utc_offset_minutes": -420, "desktop_view": true, "ui_zoom": 1})
}

/// `launch_stock_permissions` for any room, including none.
fn launch_widget(host: &mut Harness, room: Option<&str>) -> (WidgetRef, ContextId) {
    host.review_effects = true;
    let manifest = a2app_core::builtin::stock(APP).unwrap();
    let context = ContextId::App { account: ACCOUNT.into(), app: APP.into(), room: room.map(str::to_string) };
    host.flow.borrow_mut().register_context(&context).unwrap();
    host.apps.insert(manifest.clone());
    host.permissions.set_matrix_write(true);
    let widget = host.cx.with_vm(|vm| {
        let value = vm.eval(script! { use mod.widgets.* Splash{} });
        assert!(!value.is_err(), "host widget startup errors: {:?}", vm.take_errors());
        WidgetRef::script_from_value(vm, value)
    });
    makepad_widgets::widget_tree::set_ui_root(&mut host.cx, &widget);
    let jail = host.flow.borrow().context_storage_path(&context).unwrap();
    std::fs::create_dir_all(&jail).unwrap();
    let heap = {
        let mut splash = widget.borrow_mut::<Splash>().unwrap();
        splash.set_host_io_only(true);
        splash.set_allow_net(false);
        splash.set_host_tag(&mut host.cx, Some(instance_tag(APP, context.room())));
        splash.set_host_caps(&mut host.cx, host.permissions.granted_caps(&manifest));
        splash.set_sandbox_dir(&mut host.cx, Some(jail));
        host.load_splash(&mut splash, APP, &manifest.source)
    };
    host.contexts.insert(heap, context.clone());
    host.panes.insert(heap, PaneState { surface: "dock", side: None, foreground: true, width: 460.0, height: 700.0 });
    host.stock_hosts.push(widget.clone());
    for line in manifest.source.lines() {
        if let Some((prefix, _)) = line.split_once(":=") {
            if let Some(name) = prefix.split_whitespace().last() {
                widget.widget(&host.cx, &[LiveId::from_str(name)]);
            }
        }
    }
    let source = widget.borrow::<Splash>().unwrap().view.source.clone();
    let vm_id = host.cx.script_ref_vm_id(&source).unwrap();
    host.cx.with_script_vm_id(vm_id, |vm| vm.bx.captured_errors = Some(Vec::new()));
    (widget, context)
}

impl Rig {
    fn launch() -> Self {
        let mut host = Harness::new();
        let (app, context) = host.launch_stock_permissions(APP, false, false);
        Self::with(host, app, context, Answers::default())
    }

    fn with(host: Harness, app: WidgetRef, context: ContextId, answers: Answers) -> Self {
        Self { host, app, context, trace: Trace::default(), answers, parked: VecDeque::new(),
            is_parking_reads: false, parked_reads: VecDeque::new(), device: Some(pacific_device()), refused_hook: None }
    }

    fn launch_detached() -> Self {
        let mut host = Harness::new();
        let (app, context) = launch_widget(&mut host, None);
        Self::with(host, app, context, Answers::default())
    }

    /// Closes this instance and opens the stock app again in the same room and storage jail.
    fn relaunch(self) -> Self {
        let Rig { mut host, app, context, answers, .. } = self;
        host.retire_stock(&app, &context);
        let (app, reopened) = host.launch_stock_permissions(APP, false, false);
        assert_eq!(reopened, context);
        Self::with(host, app, context, answers)
    }

    fn boot(&mut self) {
        self.host.boot();
        self.resize(460.0, 700.0);
        self.settle();
    }

    fn resize(&mut self, w: f64, h: f64) {
        assert!(self.host.call(&self.app, "on_app_resize", &[w.into(), h.into()]));
    }

    /// Fires pending timers. The pause between automatic requests is what refills the host's request budget.
    fn tick(&mut self) {
        self.host.broker.forget_app(APP);
        self.host.boot();
        self.settle();
    }

    /// A person's next action comes after the host's request budget has refilled.
    fn act(&mut self, name: &str, args: &[ScriptValue]) {
        self.host.broker.forget_app(APP);
        assert!(self.host.call(&self.app, name, args), "{name} exists");
        self.settle();
    }

    fn act_str(&mut self, name: &str, args: &[&str]) {
        self.host.broker.forget_app(APP);
        assert!(self.host.call_strings(&self.app, name, args), "{name} exists");
        self.settle();
    }

    fn open(&mut self, tile: u32) { self.act("open_tile", &[tile.into()]); }

    fn click(&mut self, text: &str) {
        self.host.broker.forget_app(APP);
        self.host.widget_click(&self.app, text, &[]);
        self.settle();
    }

    fn text(&self, path: &[&str]) -> String {
        let ids: Vec<LiveId> = path.iter().map(|name| LiveId::from_str(name)).collect();
        self.app.widget(&self.host.cx, &ids).text()
    }

    fn is_visible(&self, path: &[&str]) -> bool {
        let ids: Vec<LiveId> = path.iter().map(|name| LiveId::from_str(name)).collect();
        self.app.widget(&self.host.cx, &ids).visible()
    }

    fn tile(&self, k: usize) -> String { self.text(&[&format!("t{k}"), "name"]) }

    /// The filter whose filled chip shows; each chip has a plain and a filled copy.
    fn active_chip(&self) -> Vec<&'static str> {
        ["all", "photo", "video", "audio", "file"].into_iter().filter(|key| self.is_visible(&[&format!("f_{key}_on_wrap")])).collect()
    }

    /// Every text the gallery shows, in order.
    fn seen(&self) -> Vec<String> {
        fn walk(widget: &WidgetRef, out: &mut Vec<String>) {
            if !widget.visible() { return; }
            let text = widget.text();
            if !text.is_empty() { out.push(text); }
            let mut children = Vec::new();
            widget.children(&mut |_, child| children.push(child));
            for child in children { walk(&child, out); }
        }
        let mut out = Vec::new();
        walk(&self.app, &mut out);
        out
    }

    fn tile_meta(&self, k: usize) -> String { self.text(&[&format!("t{k}"), "meta"]) }

    fn is_tile_shown(&self, k: usize) -> bool { self.is_visible(&[&format!("t{k}")]) }

    fn is_tile_picture(&self, k: usize) -> bool { self.is_visible(&[&format!("t{k}"), "img_wrap"]) }

    fn shown_tiles(&self) -> Vec<String> {
        (0..24).filter(|k| self.is_tile_shown(*k)).map(|k| self.tile(k)).collect()
    }

    fn jail_file(&self, file: &str) -> std::path::PathBuf {
        self.host.flow.borrow().context_storage_path(&self.context).unwrap().join(file.trim_start_matches('/'))
    }

    fn stored(&self, file: &str) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(self.jail_file(file)).unwrap()).unwrap()
    }

    fn queue_read(&mut self, list: &[serde_json::Value]) {
        self.host.queue_matrix_reply("matrix.room.messages.read", json!({ "messages": list, "unread_count": 0 }));
    }

    fn queue_older(&mut self, list: &[serde_json::Value], has_more: bool) {
        self.host.queue_matrix_reply("matrix.room.messages.paginate", json!({ "messages": list, "has_more": has_more }));
    }

    fn queue_event(&mut self, row: serde_json::Value) {
        self.host.queue_matrix_reply("matrix.room.event.read", row);
    }

    fn deliver(&mut self, list: &[serde_json::Value]) {
        let batch = serde_json::Value::Array(list.to_vec()).to_string();
        assert!(self.host.call_strings(&self.app, "on_room_message", &[&batch]));
        self.settle();
    }

    fn change(&mut self, payload: serde_json::Value) {
        assert!(self.host.call_strings(&self.app, "on_room_message_changed", &[&payload.to_string()]));
        self.settle();
    }

    fn set_grant(&mut self, permission: Permission, state: GrantState) {
        self.host.permissions.set(APP, permission, state);
        self.host.publish_grants(&self.app, APP);
        self.settle();
    }

    fn vm_id(&mut self) -> SplashVmId {
        let source = self.app.borrow::<Splash>().unwrap().view.source.clone();
        self.host.cx.script_ref_vm_id(&source).unwrap()
    }

    /// `Harness::reply`, run inside the harness's `headless_splash_entry`: the longer wall budget it already
    /// gives guest calls, so a descheduled parallel test is not mistaken for slow script. Instruction caps still apply.
    fn respond(&mut self, reply: Reply, result: Result<&str, &str>) {
        if let (Some(context), Ok(data)) = (self.host.contexts.get(&reply.heap_key), result) {
            let mut sources = Vec::new();
            if let Ok(value) = serde_json::from_str(data) { response_room_sources(&value, &mut sources); }
            let influences = sources.iter().filter_map(|source| match source {
                Source::Room { account, room } => Some(Influence::RoomContent { account: account.clone(), room: room.clone() }),
                _ => None,
            }).collect::<Vec<_>>();
            self.host.flow.borrow_mut().add_sources(context, sources).unwrap();
            self.host.flow.borrow_mut().add_influences(context, influences).unwrap();
        }
        let vm_id = self.vm_id();
        headless_splash_entry(&mut self.host.cx, vm_id, |cx| { services::respond(cx, reply, result); });
        self.host.assert_callback_errors(&format!("reply {}", reply.req_id));
    }

    fn reply(&mut self, reply: Reply, data: &str) { self.respond(reply, Ok(data)); }

    /// Answers the oldest parked download and settles what follows.
    fn answer_download(&mut self, data: serde_json::Value) -> (String, Option<u32>) {
        let (reply, event_id, preview) = self.parked.pop_front().expect("a parked media download");
        self.reply(reply, &data.to_string());
        self.host.cx.with_vm_and_async(|_| {});
        self.settle();
        (event_id, preview)
    }

    fn answer_preview(&mut self) -> (String, Option<u32>) {
        let (_, event_id, preview) = self.parked.front().cloned().expect("a parked preview");
        self.answer_download(preview_reply(&event_id, preview.unwrap_or(256)))
    }

    fn fail_download(&mut self, error: &str) -> (String, Option<u32>) {
        let (reply, event_id, preview) = self.parked.pop_front().expect("a parked media download");
        self.refuse(reply, error);
        self.settle();
        (event_id, preview)
    }

    fn refuse(&mut self, reply: Reply, error: &str) {
        self.respond(reply, Err(error));
        self.host.cx.with_vm_and_async(|_| {});
        self.host.assert_callback_errors(APP);
    }

    /// `Harness::process`, counting the setup batches it answers without a prompt.
    fn process(&mut self) -> Vec<BrokerAsk> {
        let mut pending = Vec::new();
        for _ in 0..100 {
            let mut is_answered = false;
            for ask in self.host.dispatch(None) {
                match ask {
                    BrokerAsk::PermissionBatch { app_id, request, perms }
                        if perms.iter().all(|permission| self.host.batch_status(&app_id, &request, *permission) == Effective::Granted) =>
                    {
                        self.trace.silent_batches += 1;
                        let answers: serde_json::Map<String, serde_json::Value> = perms.into_iter()
                            .map(|permission| (permission.as_str().into(), true.into())).collect();
                        self.host.reply(Reply { heap_key: request.heap_key, req_id: request.req_id },
                            &json!({"granted": true, "permissions": answers}).to_string());
                        is_answered = true;
                    }
                    other => pending.push(other),
                }
            }
            if !is_answered { return pending; }
        }
        panic!("an already approved setup did not finish");
    }

    /// Serves every host request like `settle_stock`, but parks media downloads and answers per `self.answers`.
    fn settle(&mut self) {
        let mut pending = VecDeque::from(self.process());
        let mut idle = 0;
        for _ in 0..200 {
            while let Some(ask) = pending.pop_front() {
                match ask {
                    BrokerAsk::PermissionBatch { app_id, request, perms } => {
                        assert_eq!(app_id, APP);
                        // Robrix answers a group with a blocked ability "no" without asking.
                        let status = |rig: &Self, permission: Permission| rig.host.batch_status(APP, &request, permission);
                        if perms.iter().all(|permission| matches!(status(self, *permission), Effective::Denied | Effective::Granted)) {
                            self.trace.silent_batches += 1;
                            let answers: serde_json::Map<String, serde_json::Value> = perms.iter()
                                .map(|permission| (permission.as_str().into(), (status(self, *permission) == Effective::Granted).into())).collect();
                            self.reply(Reply { heap_key: request.heap_key, req_id: request.req_id },
                                &json!({"granted": false, "permissions": answers}).to_string());
                            continue;
                        }
                        self.trace.batches.push(perms.clone());
                        let answers = self.answers;
                        let selected: Vec<Permission> = perms.iter().copied().filter(|perm| match perm {
                            Permission::MatrixRoomRead => answers.is_read_allowed,
                            Permission::MatrixRoomWatch => answers.is_watch_allowed,
                            _ => true,
                        }).collect();
                        self.host.answer_stock_batch(&self.app, APP, request, &perms, &selected, &mut self.trace.audit);
                    }
                    BrokerAsk::Prompt { app_id, perm, request: Some(request), .. } => {
                        assert_eq!(app_id, APP);
                        self.trace.audit.prompts.push(perm);
                        self.trace.audit.concrete_prompts += 1;
                        let is_allowed = match perm {
                            Permission::MatrixMedia => self.answers.is_media_allowed,
                            Permission::RobrixNavigation => self.answers.is_nav_allowed,
                            Permission::RobrixComposer => self.answers.is_composer_allowed,
                            other => panic!("unexpected concrete prompt for {other:?}"),
                        };
                        if !is_allowed {
                            self.trace.refusals += 1;
                            let cap = a2app_core::capabilities::for_service(&request.service).unwrap();
                            self.refuse(Reply { heap_key: request.heap_key, req_id: request.req_id },
                                &format!("The request to use \"{}\" was not approved.", cap.title));
                            continue;
                        }
                        self.host.grant_stock_request(APP, perm, &request);
                        self.host.publish_grants(&self.app, APP);
                        pending.extend(self.host.dispatch(Some(request)));
                    }
                    BrokerAsk::FlowReview { request, review } => {
                        assert!(!review.allowed);
                        if !self.answers.is_sharing_allowed {
                            self.trace.reviews.push(request.service.clone());
                            self.trace.audit.reviews += 1;
                            self.trace.refusals += 1;
                            self.refuse(Reply { heap_key: request.heap_key, req_id: request.req_id }, "This request was not approved.");
                            continue;
                        }
                        // Robrix re-checks a queued review before showing it, so one approved
                        // a moment earlier can cover it; it then resumes without a dialog.
                        if self.host.flow.borrow_mut().approve_effect_session(&review, SharingDuration::RobrixSession).is_ok() {
                            self.trace.reviews.push(request.service.clone());
                            self.trace.audit.reviews += 1;
                        }
                        pending.extend(self.host.dispatch(Some(request)));
                    }
                    BrokerAsk::Matrix { reply, capability, call, args, .. } => {
                        self.trace.audit.services.push(capability.into());
                        self.trace.args.push((capability.into(), args));
                        match &call {
                            services::MatrixServiceCall::ReadMessages { limit } => self.trace.reads.push(*limit),
                            services::MatrixServiceCall::OlderMessages { before, limit } => self.trace.older.push((before.clone(), *limit)),
                            services::MatrixServiceCall::Event { event_id } => self.trace.events.push(event_id.clone()),
                            _ => {}
                        }
                        if self.is_parking_reads && matches!(call, services::MatrixServiceCall::ReadMessages { .. }) {
                            self.parked_reads.push_back(reply);
                            continue;
                        }
                        if let services::MatrixServiceCall::DownloadMedia { event_id, preview } = call {
                            match preview {
                                Some(edge) => self.trace.previews.push((event_id.clone(), edge)),
                                None => self.trace.downloads.push(event_id.clone()),
                            }
                            self.parked.push_back((reply, event_id, preview));
                            continue;
                        }
                        let data = self.host.matrix_replies.get_mut(capability).and_then(VecDeque::pop_front)
                            .unwrap_or_else(|| matrix_fixture(&call));
                        match data["$error"].as_str() {
                            Some(error) => self.refuse(reply, error),
                            None => self.reply(reply, &data.to_string()),
                        }
                    }
                    BrokerAsk::Subscribe { reply, hook, .. } => {
                        if let Some((refused, error)) = self.refused_hook.filter(|(refused, _)| *refused == hook) {
                            assert_eq!(refused, hook);
                            self.refuse(reply, error);
                            continue;
                        }
                        self.trace.audit.subscriptions.push(hook.into());
                        self.reply(reply, "{\"subscribed\":true}");
                    }
                    BrokerAsk::Unsubscribe { reply, hook, .. } => {
                        self.trace.unsubscribed.push(hook.unwrap_or("*").into());
                        self.reply(reply, "{}");
                    }
                    BrokerAsk::HostQuery { reply, query: services::HostQuery::DeviceInfo } => {
                        self.trace.audit.services.push("device.info".into());
                        match self.device.clone() {
                            Some(device) => self.reply(reply, &device.to_string()),
                            None => self.refuse(reply, "Device details are unavailable."),
                        }
                    }
                    BrokerAsk::HostAction { reply, action, .. } => {
                        match action {
                            HostAction::JumpToEvent { room, event_id } => {
                                assert_eq!(room.as_deref(), Some(ROOM));
                                self.trace.audit.actions.push("event".into());
                                self.trace.navigations.push(event_id);
                            }
                            HostAction::ComposerAttach { room, data_base64, filename, mime_type, caption } => {
                                assert_eq!(room.as_deref(), Some(ROOM));
                                self.trace.audit.actions.push("attach".into());
                                self.trace.attachments.push((data_base64, filename, mime_type, caption));
                            }
                            _ => panic!("the gallery only jumps to messages and stages attachments"),
                        }
                        self.reply(reply, "{}");
                    }
                    BrokerAsk::Used { .. } => {}
                    BrokerAsk::Prompt { .. } => panic!("gallery prompts are always tied to a request"),
                    BrokerAsk::Restrict { reason, .. } => panic!("gallery was restricted: {reason}"),
                    _ => panic!("unexpected gallery host request"),
                }
                self.host.cx.with_vm_and_async(|_| {});
                self.host.assert_callback_errors(APP);
            }
            pending.extend(self.process());
            if pending.is_empty() {
                idle += 1;
                if idle < 2 || self.host.stock_callbacks_paused() { continue; }
                self.host.assert_callback_errors(APP);
                return;
            }
            idle = 0;
        }
        panic!("gallery did not settle");
    }
}

fn row(event: &str, msgtype: &str, body: &str, ts: u64, media: serde_json::Value) -> serde_json::Value {
    json!({"event_id": event, "room_id": ROOM, "sender": "alice", "sender_id": "@alice:test", "sender_name": "Alice", "body": body,
        "ts": ts, "msgtype": msgtype, "media": media, "unread": false})
}

fn media(filename: &str, mime: &str, size: u64) -> serde_json::Value {
    json!({"uri": "mxc://media.test/never-used", "encrypted": false, "filename": filename, "mime_type": mime,
        "size": size, "width": null, "height": null, "duration_ms": null})
}

fn photo(event: &str, name: &str, ts: u64) -> serde_json::Value {
    let mut m = media(name, "image/jpeg", 2_516_582);
    m["width"] = 4032.into();
    m["height"] = 3024.into();
    row(event, "m.image", name, ts, m)
}

fn video(event: &str, name: &str, ts: u64) -> serde_json::Value {
    let mut m = media(name, "video/mp4", 12_582_912);
    m["width"] = 1920.into();
    m["height"] = 1080.into();
    m["duration_ms"] = 42_000.into();
    row(event, "m.video", name, ts, m)
}

fn voice(event: &str, ts: u64) -> serde_json::Value {
    let mut m = media("Voice message.ogg", "audio/ogg", 24_576);
    m["duration_ms"] = 12_400.into();
    row(event, "m.audio", "Voice message", ts, m)
}

fn song(event: &str, name: &str, ts: u64) -> serde_json::Value {
    let mut m = media(name, "audio/mpeg", 5_452_595);
    m["duration_ms"] = 201_000.into();
    row(event, "m.audio", name, ts, m)
}

fn file(event: &str, name: &str, mime: &str, size: u64, ts: u64) -> serde_json::Value {
    row(event, "m.file", name, ts, media(name, mime, size))
}

fn text(event: &str, body: &str, ts: u64) -> serde_json::Value {
    json!({"event_id": event, "room_id": ROOM, "sender": "bob", "sender_id": "@bob:test", "body": body,
        "ts": ts, "msgtype": "m.text", "media": null, "unread": false})
}

fn preview_reply(event: &str, edge: u32) -> serde_json::Value {
    json!({"room_id": ROOM, "event_id": event, "data_base64": PNG, "mime_type": "image/png", "width": 2, "height": 2,
        "original_width": 4032, "original_height": 3024, "filename": "fixture.png",
        "size": 73, "requested_edge": edge})
}

/// Oldest first, like `matrix.read_messages`: a mixed room with text, every media kind and malformed rows.
fn mixed_room() -> Vec<serde_json::Value> {
    vec![
        text("$t1:test", "Hello everyone", TS),
        photo("$p1:test", "beach.jpg", TS + 1_000),
        file("$f1:test", "Quarterly report.pdf", "application/pdf", 2_516_582, TS + 2_000),
        voice("$v1:test", TS + 3_000),
        json!({"event_id": "$bad:test", "msgtype": "m.image", "body": "no media field", "ts": TS + 3_500}),
        video("$m1:test", "launch.mp4", TS + 4_000),
        text("$t2:test", "Look at these", TS + 5_000),
        song("$s1:test", "theme.mp3", TS + 6_000),
        file("$f2:test", "sources.zip", "application/zip", 31_457_280, TS + 7_000),
        json!({"msgtype": "m.file", "body": "no event id", "media": {"filename": "x.bin"}}),
        photo("$p2:test", "sunset.jpg", TS + 8_000),
    ]
}

impl Rig {
    fn grant(&mut self, permission: Permission) { self.set_grant(permission, GrantState::Granted); }

    /// Answers every parked preview, firing the pacing timer between them, until none is left.
    fn answer_all_previews(&mut self) {
        for _ in 0..60 {
            while !self.parked.is_empty() { self.answer_preview(); }
            self.tick();
            if self.parked.is_empty() { return; }
        }
        panic!("previews kept coming");
    }

    /// Makepad's image cache entries created from in-memory script bytes.
    fn script_images(&mut self) -> Vec<(usize, usize)> {
        use makepad_widgets::makepad_draw::image_cache::{ImageCache, ImageCacheEntry};
        if !self.host.cx.has_global::<ImageCache>() { return Vec::new(); }
        let cache = self.host.cx.get_global::<ImageCache>();
        let mut out: Vec<(usize, usize)> = cache.map.iter()
            .filter(|(path, _)| path.display().to_string().starts_with("script_image_data://"))
            .map(|(_, entry)| match entry { ImageCacheEntry::Loading(w, h) => (*w, *h), ImageCacheEntry::Loaded(_) => (0, 0) })
            .collect();
        out.sort();
        out
    }

    fn is_browsing(&self) -> bool { self.is_visible(&["grid_scroll"]) && !self.is_visible(&["detail_wrap"]) }

    /// The next answer for `capability` is this error.
    fn queue_error(&mut self, capability: &str, error: &str) {
        self.host.queue_matrix_reply(capability, json!({ "$error": error }));
    }

    fn pictures(&self) -> usize { (0..24).filter(|k| self.is_tile_shown(*k) && self.is_tile_picture(*k)).count() }

    /// Answers the oldest parked media request: a preview with a picture, a whole file with small bytes.
    fn answer_front(&mut self) {
        let (_, event_id, preview) = self.parked.front().cloned().expect("a parked media request");
        let data = match preview {
            Some(edge) => preview_reply(&event_id, edge),
            None => whole_file(&event_id, "bWVkaWE=", "photo.jpg", "image/jpeg"),
        };
        self.answer_download(data);
    }

    /// Fires only the timers pending at least `age`: a real-time run must never fire one early,
    /// or the app's clock would run ahead of the real request budget.
    fn fire_timers_older_than(&mut self, age: std::time::Duration, seen: &mut HashMap<LiveId, std::time::Instant>) -> usize {
        let now = std::time::Instant::now();
        let timers = std::mem::take(&mut self.host.cx.script_data.timers.timers);
        let mut fired = 0;
        for timer in timers {
            let first = *seen.entry(timer.id).or_insert(now);
            if timer.repeat || now.duration_since(first) < age {
                self.host.cx.script_data.timers.timers.push(timer);
                continue;
            }
            let hooks = self.host.cx.script_data.timers.dispatch_hooks.clone();
            assert!(self.host.guest_work(timer.callback.heap_key(), |cx| hooks.into_iter().any(|hook| hook(cx, &timer, NIL))));
            fired += 1;
        }
        self.host.cx.with_vm_and_async(|_| {});
        self.host.assert_callback_errors("timer");
        fired
    }
}

/// Milliseconds since 1970, like the timestamps Robrix sends.
fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64
}

fn mixed_with(extra: &[serde_json::Value]) -> Vec<serde_json::Value> {
    let mut rows = mixed_room();
    rows.extend(extra.iter().cloned());
    rows
}

fn photos(count: u64, from: u64) -> Vec<serde_json::Value> {
    (from..from + count).map(|n| photo(&format!("$p{n}:test"), &format!("photo {n}.jpg"), TS + n * 1_000)).collect()
}

fn booted(rows: &[serde_json::Value]) -> Rig {
    let mut rig = Rig::launch();
    rig.queue_read(rows);
    rig.boot();
    rig
}

/// Boots with matrix-media already allowed, so previews start on their own.
fn booted_with_media(rows: &[serde_json::Value]) -> Rig {
    let mut rig = Rig::launch();
    rig.host.permissions.set(APP, Permission::MatrixMedia, GrantState::Granted);
    rig.host.publish_grants(&rig.app, APP);
    rig.queue_read(rows);
    rig.boot();
    rig
}

const NEWEST_FIRST: [&str; 7] = ["sunset.jpg", "sources.zip", "theme.mp3", "launch.mp4", "Voice message.ogg", "Quarterly report.pdf", "beach.jpg"];

// ---- Startup and the grid

#[test]
fn boot_reads_latest_messages_after_one_grouped_setup_and_shows_every_attachment() {
    let rig = booted(&mixed_room());
    assert_eq!(rig.trace.batches, [vec![Permission::MatrixRoomRead, Permission::MatrixRoomWatch]], "one grouped setup at startup");
    assert_eq!(rig.trace.audit.prompts, [Permission::MatrixRoomRead, Permission::MatrixRoomWatch]);
    assert_eq!(rig.trace.reads, [30]);
    assert_eq!(rig.trace.audit.services, ["matrix.room.messages.read", "device.info"], "device facts are read once the room answers");
    assert_eq!(rig.trace.audit.subscriptions, ["on_room_message", "on_room_message_changed"]);
    assert!(rig.trace.previews.is_empty() && rig.trace.downloads.is_empty(), "media waits for the user's OK");
    assert!(rig.trace.events.is_empty() && rig.trace.navigations.is_empty() && rig.trace.attachments.is_empty());
    assert_eq!(rig.shown_tiles(), NEWEST_FIRST, "every attachment, newest first; text and malformed rows are left out");
    assert_eq!((0..7).map(|k| rig.tile_meta(k)).collect::<Vec<_>>(),
        ["2.4 MB", "Archive · 30 MB", "3:21 · 5.2 MB", "0:42 · 12 MB", "0:12 · 24 KB", "PDF · 2.4 MB", "2.4 MB"]);
    assert_eq!((0..7).map(|k| rig.text(&[&format!("t{k}"), "ph_icon"])).collect::<Vec<_>>(),
        ["🖼️", "📦", "🎵", "🎬", "🎤", "📕", "🖼️"]);
    assert!((0..7).all(|k| !rig.is_tile_picture(k)), "no picture without a preview");
    assert_eq!(rig.text(&["stats_line"]), "7 attachments · 54 MB", "the chips count each kind below");
    assert_eq!([rig.text(&["f_all"]), rig.text(&["f_photo"]), rig.text(&["f_video"]), rig.text(&["f_audio"]), rig.text(&["f_file"])],
        ["All 7", "Photos 2", "Videos 1", "Audio 2", "Files 2"]);
    assert_eq!(rig.active_chip(), ["all"]);
    assert_eq!(rig.text(&["room_line"]), "Everything shared in Private room");
    assert_eq!(rig.text(&["status"]), "Tap a tile for details.");
    assert!(rig.is_visible(&["preview_wrap"]));
    assert_eq!(rig.text(&["preview_note"]), "Photos and videos show as icons until you allow previews from your homeserver.");
    assert!(!rig.is_visible(&["pager_wrap"]), "one page needs no pager");
    assert!(rig.is_visible(&["grid"]) && !rig.is_visible(&["empty_wrap"]));
    assert!(rig.is_visible(&["older_btn_wrap"]) && !rig.is_visible(&["stop_wrap"]));
    assert_eq!(rig.text(&["older_line"]), "Checked the latest 11 messages.");
    assert!(rig.is_visible(&["title_wrap"]) && rig.is_visible(&["hint_wrap"]) && !rig.is_visible(&["no_room_wrap"]));
    assert!(!rig.seen().iter().any(|text| text == "Room Gallery"), "the pane header names the app");
    assert!(rig.is_browsing());
}

#[test]
fn without_a_room_the_gallery_explains_itself_and_asks_for_nothing() {
    let mut rig = Rig::launch_detached();
    rig.boot();
    assert!(rig.trace.batches.is_empty() && rig.trace.audit.prompts.is_empty());
    assert!(rig.trace.audit.services.is_empty() && rig.trace.audit.subscriptions.is_empty());
    assert!(rig.is_visible(&["no_room_wrap"]));
    assert_eq!(rig.text(&["room_line"]), "Not attached to a room", "the subtitle doesn't claim a room");
    assert_eq!(rig.text(&["no_room_title"]), "No room attached");
    assert_eq!(rig.text(&["no_room_line"]), "Open Room Gallery from a room to see what people shared.");
    assert!(!rig.is_visible(&["env_retry_wrap"]));
    for section in ["filter_wrap", "status_wrap", "grid_scroll", "older_wrap", "stats_wrap", "preview_wrap", "detail_wrap"] {
        assert!(!rig.is_visible(&[section]), "{section} stays hidden without a room");
    }
    // Live hooks, taps and permission changes stay inert without a room.
    rig.deliver(&[json!({"event_id": "$x:test", "msgtype": "m.image", "body": "x.jpg", "sender_name": "Alice"})]);
    rig.act("open_tile", &[0_u32.into()]);
    rig.act("load_older", &[]);
    rig.host.publish_grants(&rig.app, APP);
    rig.settle();
    assert!(rig.trace.audit.services.is_empty() && rig.trace.batches.is_empty() && rig.trace.events.is_empty());
    assert!(!rig.is_visible(&["detail_wrap"]));
}

#[test]
fn filters_show_one_kind_with_counts_and_an_empty_state() {
    let mut rig = booted(&mixed_room());
    rig.act_str("set_filter", &["photo"]);
    assert_eq!(rig.shown_tiles(), ["sunset.jpg", "beach.jpg"]);
    assert_eq!([rig.text(&["f_all"]), rig.text(&["f_photo"])], ["All 7", "Photos 2"]);
    assert_eq!(rig.active_chip(), ["photo"]);
    rig.act_str("set_filter", &["audio"]);
    assert_eq!(rig.shown_tiles(), ["theme.mp3", "Voice message.ogg"], "voice messages count as audio");
    rig.act_str("set_filter", &["video"]);
    assert_eq!(rig.shown_tiles(), ["launch.mp4"]);
    rig.act_str("set_filter", &["file"]);
    assert_eq!(rig.shown_tiles(), ["sources.zip", "Quarterly report.pdf"]);
    assert_eq!(rig.text(&["f_file"]), "Files 2");
    assert_eq!(rig.active_chip(), ["file"]);
    assert_eq!(rig.text(&["stats_line"]), "7 attachments · 54 MB", "stats always cover everything");
    rig.act_str("set_filter", &["all"]);
    assert_eq!(rig.shown_tiles(), NEWEST_FIRST);

    let mut rig = booted(&photos(3, 1));
    assert!(rig.is_visible(&["f_photo_wrap"]));
    for chip in ["f_video_wrap", "f_audio_wrap", "f_file_wrap"] { assert!(!rig.is_visible(&[chip]), "{chip}: empty kinds get no chip"); }
    rig.act_str("set_filter", &["video"]);
    assert!(rig.is_visible(&["f_video_wrap"]), "the active filter keeps its chip");
    assert!(rig.shown_tiles().is_empty());
    assert!(rig.is_visible(&["empty_wrap"]) && !rig.is_visible(&["grid"]));
    assert_eq!([rig.text(&["empty_title"]), rig.text(&["empty_line"])], ["No videos here yet", "Try another filter, or tap Load older."]);
    assert_eq!(rig.text(&["stats_line"]), "3 attachments · 7.2 MB");
    assert_eq!(rig.text(&["f_video"]), "Videos 0");
    assert_eq!(rig.active_chip(), ["video"]);
    rig.act_str("set_filter", &["all"]);
    assert!(!rig.is_visible(&["f_video_wrap"]));
}

#[test]
fn an_empty_room_says_so_and_offers_older_history() {
    let rig = booted(&[text("$t1:test", "No attachments here", TS)]);
    assert!(rig.shown_tiles().is_empty());
    assert!(rig.is_visible(&["empty_wrap"]));
    assert_eq!(rig.text(&["empty_title"]), "Nothing shared lately");
    assert_eq!(rig.text(&["empty_line"]), "No photos, videos or files in the latest messages. Tap Load older to look further back.");
    assert_eq!(rig.text(&["status"]), "Nothing shared in the latest messages yet.");
    assert_eq!(rig.text(&["stats_line"]), "No attachments found yet");
    assert!(!rig.is_visible(&["preview_wrap"]), "no photos means no preview note");
    assert!(rig.is_visible(&["older_btn_wrap"]));
    assert!(!rig.is_visible(&["grid_scroll"]), "the card sits right above Load older, with no empty grid between");
}

#[test]
fn the_tile_pool_pages_through_more_attachments_than_it_holds() {
    let mut rig = booted(&photos(30, 1));
    let page_one: Vec<String> = (0..24).map(|n| format!("photo {}.jpg", 30 - n)).collect();
    assert_eq!(rig.shown_tiles(), page_one, "24 tiles, newest first");
    assert!(rig.is_visible(&["pager_wrap"]) && rig.is_visible(&["next_wrap"]) && !rig.is_visible(&["prev_wrap"]));
    assert_eq!(rig.text(&["page_line"]), "1–24 of 30");
    rig.act("go_page", &[1_f64.into()]);
    assert_eq!(rig.shown_tiles(), ["photo 6.jpg", "photo 5.jpg", "photo 4.jpg", "photo 3.jpg", "photo 2.jpg", "photo 1.jpg"]);
    assert!((6..24).all(|k| !rig.is_tile_shown(k)), "unused tiles are hidden");
    assert_eq!(rig.text(&["page_line"]), "25–30 of 30");
    assert!(rig.is_visible(&["prev_wrap"]) && !rig.is_visible(&["next_wrap"]));
    assert!(!rig.is_visible(&["grid"]), "the grid hides for a frame so its scroll returns to the top");
    rig.tick();
    assert!(rig.is_visible(&["grid"]));
    rig.act("go_page", &[1_f64.into()]);
    assert_eq!(rig.text(&["page_line"]), "25–30 of 30", "there is no third page");
    rig.click("‹ Previous");
    assert_eq!(rig.shown_tiles(), page_one);
    rig.act("open_tile", &[23_u32.into()]);
    assert_eq!(rig.text(&["d_name"]), "photo 7.jpg", "a tile opens the item it shows");
    rig.click("‹ Back to gallery");
    rig.click("Next ›");
    rig.act("open_tile", &[5_u32.into()]);
    assert_eq!(rig.text(&["d_name"]), "photo 1.jpg");
    rig.act("back_to_grid", &[]);
    rig.act("open_tile", &[6_u32.into()]);
    assert!(rig.is_browsing(), "a hidden tile opens nothing");
}

// ---- Previews

#[test]
fn previews_start_from_a_tap_then_load_paced_two_at_a_time_into_the_tiles() {
    let mut rig = booted(&mixed_room());
    assert!(rig.trace.previews.is_empty());
    rig.click("Show previews");
    assert_eq!(rig.trace.audit.prompts, [Permission::MatrixRoomRead, Permission::MatrixRoomWatch, Permission::MatrixMedia],
        "the tap asks for the media group once");
    assert_eq!(rig.trace.reviews, ["matrix.media_preview"], "the homeserver-sharing check runs for the first preview");
    assert_eq!(rig.trace.previews, [("$p2:test".to_string(), 256)], "the first visible photo, as a small preview");
    assert_eq!(rig.tile_meta(0), "Loading preview…", "a tile waiting for its picture says so");
    // Until one preview gets through, they go one at a time, so a first review shows one request.
    rig.tick();
    assert_eq!(rig.trace.previews.len(), 1);
    assert_eq!(rig.answer_preview(), ("$p2:test".to_string(), Some(256)));
    assert!(rig.is_tile_picture(0), "the decoded picture replaces the icon");
    assert!(rig.is_visible(&["t0", "img_wrap"]), "the opaque picture layer covers the icon and caption");
    // Then two at a time, never more.
    assert_eq!(rig.trace.previews[1..], [("$m1:test".to_string(), 256), ("$p1:test".to_string(), 256)], "a video gets its poster");
    rig.tick();
    rig.tick();
    assert_eq!(rig.parked.len(), 2, "never more than two previews in flight");
    rig.answer_all_previews();
    assert!(rig.is_tile_picture(0) && rig.is_tile_picture(3) && rig.is_tile_picture(6));
    assert!(rig.is_visible(&["t3", "badge_wrap"]) && rig.text(&["t3", "badge"]) == "▶️ 0:42", "videos keep a play badge with their length");
    assert!(!rig.is_tile_picture(1) && !rig.is_tile_picture(2), "audio and files have no picture to fetch");
    assert_eq!([rig.tile_meta(1), rig.tile_meta(2)], ["Archive · 30 MB", "3:21 · 5.2 MB"]);
    assert_eq!(rig.trace.previews.len(), 3, "only photos and videos are previewed");
    // The harness reviews each exact request; Robrix keys previews to one reviewed operation.
    assert!(rig.trace.reviews.iter().all(|service| service == "matrix.media_preview"));
    assert!(!rig.is_visible(&["preview_wrap"]), "the preview note goes away once previews are on");
    // The PNG really reached Makepad's image loader through decode_base64.
    assert_eq!(rig.script_images(), [(2, 2), (2, 2), (2, 2)]);
    // Each preview is kept in the jail for the next visit.
    let index = rig.stored("/thumbs.json");
    assert_eq!(&index["ids"].as_array().unwrap()[..3], [json!("$p2:test"), json!("$m1:test"), json!("$p1:test")]);
    for slot in 0..3 { assert_eq!(std::fs::read_to_string(rig.jail_file(&format!("/thumbs/{slot}.txt"))).unwrap(), PNG); }
    // Nothing repeats on its own: not on a tick, not on an unrelated permission change.
    rig.tick();
    rig.host.publish_grants(&rig.app, APP);
    rig.settle();
    assert_eq!(rig.trace.previews.len(), 3);
}

#[test]
fn previews_load_lazily_for_the_visible_page_only() {
    let mut rig = booted_with_media(&photos(30, 1));
    assert_eq!(rig.trace.previews.len(), 1, "an earlier grant starts previews without a prompt");
    assert_eq!(rig.trace.audit.prompts, [Permission::MatrixRoomRead, Permission::MatrixRoomWatch]);
    rig.answer_all_previews();
    let page_one: Vec<String> = (0..24).map(|n| format!("$p{}:test", 30 - n)).collect();
    assert_eq!(rig.trace.previews.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>(), page_one, "first page, in order");
    assert!(rig.trace.previews.iter().all(|(_, edge)| *edge == 256));
    rig.act("go_page", &[1_f64.into()]);
    rig.answer_all_previews();
    assert_eq!(rig.trace.previews.len(), 30, "the second page is fetched once it is shown");
    rig.act("go_page", &[(-1_f64).into()]);
    rig.answer_all_previews();
    assert_eq!(rig.trace.previews.len(), 30, "cached previews are never fetched again");
    assert!((0..24).all(|k| rig.is_tile_picture(k)));
    // While a detail is open, the hidden grid fetches nothing.
    let mut rig = booted_with_media(&photos(3, 1));
    rig.open(2);
    let parked = |rig: &Rig| rig.parked.iter().map(|(_, id, edge)| (id.clone(), *edge)).collect::<Vec<_>>();
    assert_eq!(parked(&rig), [("$p3:test".to_string(), Some(256))], "until one preview gets through, the large one waits its turn");
    rig.answer_preview();
    assert_eq!(parked(&rig), [("$p1:test".to_string(), Some(768))], "then only the open item's large preview joins");
    rig.answer_all_previews();
    assert_eq!(rig.trace.previews.len(), 2);
    rig.act("back_to_grid", &[]);
    rig.answer_all_previews();
    assert_eq!(rig.trace.previews.iter().filter(|(_, edge)| *edge == 256).count(), 3);
}

#[test]
fn cached_previews_come_back_on_reopen_without_downloading() {
    let mut rig = booted_with_media(&mixed_room());
    rig.answer_all_previews();
    assert_eq!(rig.trace.previews.len(), 3);
    let mut rig = rig.relaunch();
    rig.host.boot();
    // Saved items and their pictures show before the room answers anything.
    assert_eq!(rig.shown_tiles(), NEWEST_FIRST);
    assert!(rig.is_tile_picture(0) && rig.is_tile_picture(3) && rig.is_tile_picture(6));
    assert_eq!(rig.text(&["status"]), "Starting…");
    rig.queue_read(&mixed_room());
    rig.resize(460.0, 700.0);
    rig.settle();
    rig.tick();
    assert!(rig.trace.previews.is_empty(), "reopening downloads no cached preview");
    assert_eq!(rig.trace.reads, [30]);
    assert_eq!(rig.shown_tiles(), NEWEST_FIRST);
}

#[test]
fn failed_previews_are_not_requested_again_until_the_user_asks() {
    let mut rig = booted_with_media(&mixed_room());
    assert_eq!(rig.fail_download("No picture preview for this attachment."), ("$p2:test".to_string(), Some(256)));
    assert_eq!(rig.fail_download("The homeserver refused the media download (HTTP 404)."), ("$m1:test".to_string(), Some(256)));
    rig.answer_all_previews();
    assert_eq!(rig.trace.previews.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(), ["$p2:test", "$m1:test", "$p1:test"]);
    assert!(!rig.is_tile_picture(0) && !rig.is_tile_picture(3) && rig.is_tile_picture(6));
    assert_eq!(rig.text(&["t0", "ph_icon"]), "🖼️", "a photo without a preview keeps its icon");
    assert_eq!([rig.tile_meta(0), rig.tile_meta(3)], ["2.4 MB", "0:42 · 12 MB"], "and its size, not a loading line");
    assert!(!rig.is_visible(&["preview_wrap"]), "a missing picture is not a refusal");
    for _ in 0..3 { rig.tick(); }
    assert_eq!(rig.trace.previews.len(), 3, "failures are remembered");
    // A failure that wasn't a refusal gets one more try when its page is shown again.
    rig.act_str("set_filter", &["photo"]);
    rig.act_str("set_filter", &["all"]);
    rig.tick();
    assert_eq!(rig.fail_download("The homeserver refused the media download (HTTP 404)."), ("$m1:test".to_string(), Some(256)));
    rig.act_str("set_filter", &["video"]);
    rig.act_str("set_filter", &["all"]);
    rig.answer_all_previews();
    assert_eq!(rig.trace.previews.len(), 4, "a second failure is final until the user asks");
    let saved = rig.stored("/gallery.json");
    let sunset = saved["items"].as_array().unwrap().iter().find(|it| it["id"] == "$p2:test").unwrap().clone();
    assert_eq!(sunset["has_no_preview"], true, "\"no picture\" is kept with the item");
    // Refresh retries what failed, but never a picture that does not exist.
    rig.queue_read(&mixed_room());
    rig.click("Refresh");
    rig.answer_all_previews();
    assert_eq!(rig.trace.previews.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(), ["$p2:test", "$m1:test", "$p1:test", "$m1:test", "$m1:test"]);
    assert!(rig.is_tile_picture(3));
    let mut rig = rig.relaunch();
    rig.queue_read(&mixed_room());
    rig.boot();
    rig.answer_all_previews();
    assert!(rig.trace.previews.is_empty(), "a relaunch keeps the no-picture verdict and the cache");
}

#[test]
fn malformed_preview_data_falls_back_to_the_icon_without_script_errors() {
    let mut rig = booted_with_media(&photos(2, 1));
    let (reply, id, _) = rig.parked.pop_front().unwrap();
    assert_eq!(id, "$p2:test");
    rig.reply(reply, &json!({"room_id": ROOM, "event_id": id, "data_base64": "Zg=", "mime_type": "image/png"}).to_string());
    rig.host.cx.with_vm_and_async(|_| {});
    rig.settle();
    rig.tick();
    let (reply, id, _) = rig.parked.pop_front().unwrap();
    assert_eq!(id, "$p1:test");
    rig.reply(reply, &json!({"room_id": ROOM, "event_id": id, "mime_type": "image/png"}).to_string());
    rig.host.cx.with_vm_and_async(|_| {});
    rig.settle();
    rig.answer_all_previews();
    assert!(!rig.is_tile_picture(0) && !rig.is_tile_picture(1));
    assert!(rig.script_images().is_empty(), "invalid base64 never reaches the image loader");
    assert!(!rig.jail_file("/thumbs/0.txt").exists(), "nothing invalid is cached");
    assert_eq!(rig.trace.previews.len(), 2, "and nothing is fetched again");
}

// ---- The detail panel

#[test]
fn opening_a_photo_shows_its_details_and_asks_for_a_large_preview() {
    let mut rig = booted_with_media(&mixed_with(&[{
        let mut row = photo("$c1:test", "harbour.jpg", TS + 9_000);
        row["body"] = "Morning light over the harbour".into();
        row
    }]));
    rig.answer_all_previews();
    let before = rig.trace.previews.len();
    rig.open(0);
    assert!(rig.is_visible(&["detail_wrap"]) && !rig.is_browsing());
    for section in ["filter_wrap", "status_wrap", "grid_scroll", "older_wrap", "stats_wrap", "pager_wrap", "preview_wrap"] {
        assert!(!rig.is_visible(&[section]), "{section} hides behind the detail");
    }
    assert_eq!(rig.trace.previews[before..], [("$c1:test".to_string(), 768)], "the tap asks for a large preview right away");
    assert_eq!(rig.text(&["d_name"]), "harbour.jpg");
    assert_eq!(rig.text(&["d_kind"]), "PHOTO · 2.4 MB");
    assert_eq!(rig.text(&["d_info"]), "4032 × 3024 pixels · image/jpeg");
    assert_eq!([rig.text(&["d_who"]), rig.text(&["d_when"])], ["Shared by Alice", "on Jul 3, 2024"], "an older year shows a local date");
    assert!(rig.is_visible(&["d_caption_wrap"]));
    assert_eq!(rig.text(&["d_caption"]), "Morning light over the harbour");
    assert!(rig.is_visible(&["d_pic_box"]) && !rig.is_visible(&["d_file_box"]));
    assert!(rig.is_visible(&["d_img_wrap"]), "the cached small preview shows at once");
    assert!(!rig.is_visible(&["d_preview_wrap"]) && !rig.is_visible(&["d_note_wrap"]));
    let images = rig.script_images().len();
    assert_eq!(rig.answer_preview(), ("$c1:test".to_string(), Some(768)));
    assert_eq!(rig.script_images().len(), images + 1, "the large preview is decoded into the detail image");
    rig.tick();
    assert!(rig.parked.is_empty(), "one large preview per opening");
    rig.click("‹ Back to gallery");
    assert!(rig.is_browsing());
    assert_eq!(rig.shown_tiles()[0], "harbour.jpg");
    // The large preview is kept for a while, so opening it again costs nothing.
    rig.open(0);
    assert!(rig.is_visible(&["d_img_wrap"]));
    assert_eq!(rig.script_images().len(), images + 2, "the kept large preview shows at once");
    rig.tick();
    assert_eq!(rig.trace.previews.iter().filter(|(_, edge)| *edge == 768).count(), 1, "reopening asks for nothing");
}

#[test]
fn opening_files_audio_and_videos_shows_what_each_one_is() {
    let mut rig = booted(&mixed_room());
    rig.open(5);
    assert_eq!(rig.text(&["d_name"]), "Quarterly report.pdf");
    assert_eq!(rig.text(&["d_kind"]), "PDF · 2.4 MB");
    assert!(!rig.is_visible(&["d_info_wrap"]), "a known file type needs no MIME line");
    assert!(rig.is_visible(&["d_file_box"]) && !rig.is_visible(&["d_pic_box"]));
    assert_eq!([rig.text(&["d_file_icon"]), rig.text(&["d_file_label"])], ["📕", "PDF"]);
    assert!(!rig.is_visible(&["d_caption_wrap"]) && !rig.is_visible(&["d_preview_wrap"]));
    rig.act("back_to_grid", &[]);
    rig.open(4);
    assert_eq!([rig.text(&["d_kind"]), rig.text(&["d_info"])], ["VOICE MESSAGE · 24 KB", "Length 0:12 · audio/ogg"]);
    assert_eq!(rig.text(&["d_file_icon"]), "🎤");
    rig.act("back_to_grid", &[]);
    rig.open(1);
    assert_eq!(rig.text(&["d_kind"]), "ARCHIVE · 30 MB");
    rig.act("back_to_grid", &[]);
    rig.open(3);
    assert_eq!([rig.text(&["d_kind"]), rig.text(&["d_info"])], ["VIDEO · 12 MB", "1920 × 1080 pixels · Length 0:42 · video/mp4"]);
    assert!(rig.is_visible(&["d_pic_box"]) && !rig.is_visible(&["d_img_wrap"]));
    assert!(rig.is_visible(&["d_preview_wrap"]), "without media permission the detail offers a preview button");
    assert_eq!(rig.text(&["d_icon_label"]), "VIDEO");
    assert!(rig.trace.previews.is_empty(), "opening never asks for media permission by itself");
    rig.click("Show preview");
    assert_eq!(rig.trace.previews, [("$m1:test".to_string(), 768)]);
    assert_eq!(rig.trace.audit.prompts.last(), Some(&Permission::MatrixMedia));
    assert_eq!(rig.text(&["d_icon_label"]), "VIDEO · LOADING PREVIEW…");
    assert!(!rig.is_visible(&["d_preview_wrap"]), "the button hides while its preview loads");
    rig.answer_preview();
    assert!(rig.is_visible(&["d_img_wrap"]) && !rig.is_visible(&["d_icon_wrap"]) && !rig.is_visible(&["d_preview_wrap"]));
    // A missing poster is final for that video.
    let mut rig = booted_with_media(&[video("$m9:test", "no poster.mp4", TS)]);
    rig.fail_download("No picture preview for this attachment.");
    rig.open(0);
    assert_eq!(rig.trace.previews.len(), 1, "no large preview is requested for a video without a picture");
    assert_eq!(rig.text(&["d_icon_label"]), "VIDEO · NO PREVIEW");
    assert!(!rig.is_visible(&["d_preview_wrap"]));
}

#[test]
fn show_in_chat_jumps_to_the_message_after_one_prompt() {
    let mut rig = booted(&mixed_room());
    rig.open(5);
    rig.click("Show in chat");
    assert_eq!(rig.trace.navigations, ["$f1:test"]);
    assert_eq!(rig.trace.audit.prompts.last(), Some(&Permission::RobrixNavigation));
    assert_eq!(rig.text(&["d_note"]), "Shown in the chat.");
    assert!(rig.is_visible(&["d_note_wrap"]));
    let prompts = rig.trace.audit.prompts.len();
    let reviews = rig.trace.reviews.len();
    // A second tap right away waits for Robrix's request budget instead of being refused.
    rig.click("Show in chat");
    assert_eq!(rig.trace.navigations, ["$f1:test"]);
    assert_eq!(rig.text(&["d_note"]), "One moment…");
    rig.tick();
    assert_eq!(rig.trace.navigations, ["$f1:test", "$f1:test"]);
    assert_eq!(rig.text(&["d_note"]), "Shown in the chat.");
    assert_eq!((rig.trace.audit.prompts.len(), rig.trace.reviews.len()), (prompts, reviews), "no second prompt or review");
    // A refusal is shown, and nothing else happens.
    let mut rig = booted(&mixed_room());
    rig.answers.is_nav_allowed = false;
    rig.open(0);
    rig.click("Show in chat");
    assert!(rig.trace.navigations.is_empty());
    assert_eq!(rig.text(&["d_note"]), "The request to use \"Jump to a message\" was not approved.");
    assert!(rig.trace.downloads.is_empty() && rig.trace.attachments.is_empty());
}

fn whole_file(event: &str, data: &str, filename: &str, mime: &str) -> serde_json::Value {
    json!({"room_id": ROOM, "event_id": event, "data_base64": data, "filename": filename, "mime_type": mime,
        "size": data.len() * 3 / 4, "caption": null, "sha256": "0".repeat(64)})
}

#[test]
fn share_again_downloads_the_file_then_stages_exactly_those_bytes() {
    let notes = file("$n1:test", "notes.txt", "text/plain", 5, TS + 9_000);
    let mut rig = booted(&mixed_with(&[notes]));
    rig.open(0);
    rig.click("Share again");
    assert_eq!(rig.trace.downloads, ["$n1:test"], "the whole file, never a preview");
    assert_eq!(rig.trace.audit.prompts.last(), Some(&Permission::MatrixMedia));
    assert_eq!(rig.text(&["d_note"]), "Downloading notes.txt…");
    rig.click("Share again");
    assert_eq!(rig.trace.downloads.len(), 1, "a second tap waits for the first");
    rig.answer_download(whole_file("$n1:test", "bWVkaWE=", "notes.txt", "text/plain"));
    assert_eq!(rig.trace.attachments, [("bWVkaWE=".to_string(), "notes.txt".to_string(), "text/plain".to_string(), None)],
        "composer.attach gets the downloaded bytes, name and type");
    assert_eq!(rig.trace.audit.prompts.last(), Some(&Permission::RobrixComposer));
    assert_eq!(rig.text(&["d_note"]), "Ready in the message box. Review it there and press Send.");
    assert!(!rig.trace.audit.services.iter().any(|service| service.contains("send") || service.contains("upload")), "nothing is posted");
    // Robrix's names win over the event's, which an app could only guess. Taps this close
    // together wait for the request budget, and each step goes out once it allows.
    rig.click("Share again");
    assert_eq!(rig.text(&["d_note"]), "One moment…");
    rig.tick();
    rig.answer_download(whole_file("$n1:test", "bWVkaWE=", "notes (1).txt", "text/markdown"));
    assert_eq!(rig.text(&["d_note"]), "One moment…");
    rig.tick();
    assert_eq!(rig.trace.attachments[1], ("bWVkaWE=".to_string(), "notes (1).txt".to_string(), "text/markdown".to_string(), None));
    assert_eq!(rig.text(&["d_note"]), "Ready in the message box. Review it there and press Send.");
}

#[test]
fn share_again_explains_files_over_the_attachment_limit() {
    let unknown = row("$u1:test", "m.file", "mystery.bin", TS + 9_000, json!({"filename": "mystery.bin", "mime_type": "application/octet-stream"}));
    let mut rig = booted(&mixed_with(&[unknown]));
    // A declared size over 3 MiB never downloads.
    rig.open(2);
    assert_eq!(rig.text(&["d_name"]), "sources.zip");
    rig.click("Share again");
    assert!(rig.trace.downloads.is_empty());
    assert_eq!(rig.text(&["d_note"]), "This file is 30 MB, larger than the 3 MB Share again can attach. Use Show in chat to find it in the conversation instead.");
    rig.act("back_to_grid", &[]);
    // Without a declared size, the downloaded bytes are measured before attaching.
    rig.open(0);
    assert_eq!(rig.text(&["d_name"]), "mystery.bin");
    rig.click("Share again");
    assert_eq!(rig.trace.downloads, ["$u1:test"]);
    let big = "A".repeat(4 * 1024 * 1024 + 4);
    rig.answer_download(whole_file("$u1:test", &big, "mystery.bin", "application/octet-stream"));
    assert!(rig.trace.attachments.is_empty(), "nothing over 4 MiB of base64 reaches composer.attach");
    assert_eq!(rig.text(&["d_note"]), "This file is larger than the 3 MB Share again can attach. Use Show in chat to find it in the conversation instead.");
    // Exactly 4 MiB of base64 is still allowed. A small draft first approves the composer
    // action, as Robrix's review covers attachment metadata rather than the harness's 64 KiB payload.
    rig.click("Share again");
    rig.tick();
    rig.answer_download(whole_file("$u1:test", "bWVkaWE=", "mystery.bin", "application/octet-stream"));
    rig.tick();
    assert_eq!(rig.trace.attachments.len(), 1);
    rig.click("Share again");
    rig.tick();
    let limit = "A".repeat(4 * 1024 * 1024);
    rig.answer_download(whole_file("$u1:test", &limit, "mystery.bin", "application/octet-stream"));
    rig.tick();
    assert_eq!(rig.trace.attachments.len(), 2);
    assert_eq!(rig.trace.attachments[1].0.len(), 4 * 1024 * 1024);
    assert_eq!(rig.text(&["d_note"]), "Ready in the message box. Review it there and press Send.");
}

#[test]
fn share_again_refusals_are_shown_and_never_fall_back_to_sending() {
    let notes = file("$n1:test", "notes.txt", "text/plain", 5, TS + 9_000);
    let mut rig = booted(&mixed_with(std::slice::from_ref(&notes)));
    rig.answers.is_media_allowed = false;
    rig.open(0);
    rig.click("Share again");
    assert!(rig.trace.downloads.is_empty() && rig.trace.attachments.is_empty());
    assert_eq!(rig.text(&["d_note"]), "Couldn't download it: The request to use \"Read an attachment\" was not approved.");
    // Downloading works, but the composer is refused.
    rig.answers.is_media_allowed = true;
    rig.answers.is_composer_allowed = false;
    rig.click("Share again");
    rig.answer_download(whole_file("$n1:test", "bWVkaWE=", "notes.txt", "text/plain"));
    rig.tick();
    assert!(rig.trace.attachments.is_empty());
    assert_eq!(rig.text(&["d_note"]), "The request to use \"Attach media for you\" was not approved.");
    assert!(!rig.trace.audit.services.iter().any(|service|
        !["matrix.room.messages.read", "device.info", "matrix.media.download"].contains(&service.as_str())),
        "a refused draft never turns into an upload or a send");
    // Robrix's request limit reads as plain words, not the limiter's own.
    rig.answers.is_composer_allowed = true;
    rig.click("Share again");
    rig.tick();
    rig.fail_download("too many requests, slow down");
    assert_eq!(rig.text(&["d_note"]), "Couldn't download it: Robrix asked the gallery to slow down.");
    // The homeserver-sharing check can refuse the download too.
    let mut rig = booted(&mixed_with(&[notes]));
    rig.answers.is_sharing_allowed = false;
    rig.open(0);
    rig.click("Share again");
    assert_eq!(rig.text(&["d_note"]), "Couldn't download it: This request was not approved.");
    assert!(rig.trace.attachments.is_empty());
}

#[test]
fn leaving_the_detail_abandons_its_pending_actions() {
    let notes = file("$n1:test", "notes.txt", "text/plain", 5, TS + 9_000);
    let mut rig = booted(&mixed_with(&[notes]));
    rig.grant(Permission::MatrixMedia);
    rig.open(0);
    rig.click("Share again");
    assert_eq!(rig.trace.downloads, ["$n1:test"]);
    rig.act("back_to_grid", &[]);
    rig.open(6);
    assert_eq!(rig.text(&["d_name"]), "Quarterly report.pdf");
    rig.answer_download(whole_file("$n1:test", "bWVkaWE=", "notes.txt", "text/plain"));
    assert!(rig.trace.attachments.is_empty(), "a download for an earlier selection is not staged");
    assert!(!rig.is_visible(&["d_note_wrap"]), "and it does not touch the current detail");
    rig.click("Share again");
    assert_eq!(rig.trace.downloads, ["$n1:test", "$f1:test"], "the new selection can share at once");
}

// ---- Live updates

fn live(event: &str, msgtype: &str, body: &str, ts: u64) -> serde_json::Value {
    json!({"room_id": ROOM, "event_id": event, "sender": "carol", "sender_id": "@carol:test", "sender_name": "Carol",
        "body": body, "ts": ts, "msgtype": msgtype, "is_own": false})
}

/// The same row, sent by Carol, as the live fixtures are.
fn by_carol(row: serde_json::Value) -> serde_json::Value {
    let mut row = row;
    row["sender"] = "carol".into();
    row["sender_id"] = "@carol:test".into();
    row["sender_name"] = "Carol".into();
    row
}

/// `matrix.event` answers a message row plus its edit and reaction state.
fn event_reply(row: serde_json::Value) -> serde_json::Value {
    let mut row = row;
    row["edited"] = false.into();
    row["reactions"] = json!([]);
    row["thread_root"] = serde_json::Value::Null;
    row
}

#[test]
fn new_attachments_arrive_live_with_one_lookup_each() {
    let mut rig = booted(&mixed_room());
    rig.queue_event(event_reply(by_carol(photo("$new:test", "fresh.jpg", TS + 20_000))));
    rig.deliver(&[live("$chat:test", "m.text", "just text", TS + 19_000), live("$new:test", "m.image", "fresh.jpg", TS + 20_000)]);
    assert_eq!(rig.trace.events, ["$new:test"], "only attachments are looked up, for their media details");
    assert_eq!(rig.tile(0), "fresh.jpg", "the newest attachment comes first");
    assert_eq!(rig.text(&["f_photo"]), "Photos 3");
    assert_eq!(rig.text(&["stats_line"]), "8 attachments · 57 MB");
    rig.open(0);
    assert_eq!(rig.text(&["d_who"]), "Shared by Carol", "named by display name, like every older attachment");
    rig.act("back_to_grid", &[]);
    let saved = rig.stored("/gallery.json");
    assert_eq!(saved["items"][0]["id"], "$new:test", "live additions are saved");
    // Known attachments, repeats and malformed batches cost nothing.
    rig.deliver(&[live("$new:test", "m.image", "fresh.jpg", TS + 20_000), live("$p1:test", "m.image", "beach.jpg", TS + 1_000)]);
    assert!(rig.host.call_strings(&rig.app, "on_room_message", &["not json"]));
    assert!(rig.host.call_strings(&rig.app, "on_room_message", &[r#"[7, {"msgtype": "m.image"}, {"event_id": "$x:test"}]"#]));
    rig.settle();
    assert_eq!(rig.trace.events, ["$new:test"]);
    // Several at once are looked up one at a time, paced like every automatic request.
    rig.tick();
    rig.queue_event(event_reply(video("$v2:test", "clip.mp4", TS + 30_000)));
    rig.queue_event(event_reply(file("$f9:test", "slides.pptx", "application/vnd.ms-powerpoint", 900_000, TS + 31_000)));
    rig.deliver(&[live("$v2:test", "m.video", "clip.mp4", TS + 30_000), live("$f9:test", "m.file", "slides.pptx", TS + 31_000)]);
    rig.tick();
    assert_eq!(rig.trace.events, ["$new:test", "$v2:test"]);
    rig.tick();
    assert_eq!(rig.trace.events, ["$new:test", "$v2:test", "$f9:test"]);
    assert_eq!(rig.shown_tiles()[..3], ["slides.pptx", "clip.mp4", "fresh.jpg"]);
    assert_eq!(rig.tile_meta(0), "Slides · 879 KB");
}

#[test]
fn live_lookups_are_bounded_and_a_refusal_waits_for_refresh() {
    let burst: Vec<serde_json::Value> = (0..25).map(|n| live(&format!("$b{n}:test"), "m.image", "burst.jpg", TS + 40_000 + n)).collect();
    let mut rig = booted(&mixed_room());
    rig.deliver(&burst);
    assert_eq!(rig.trace.events.len(), 1, "one lookup at a time");
    for _ in 0..30 { rig.tick(); }
    assert_eq!(rig.trace.events.len(), 20, "a burst queues at most twenty lookups");
    assert_eq!(rig.text(&["status"]), "Tap a tile for details. Too many new attachments at once. Tap Refresh to catch up.");
    // A refused lookup stops live lookups.
    let mut rig = booted(&mixed_room());
    rig.answers.is_sharing_allowed = false;
    rig.deliver(&burst);
    assert!(rig.trace.events.is_empty());
    assert_eq!(rig.trace.reviews, ["matrix.event"], "the homeserver-sharing check refused the first lookup");
    assert!(rig.text(&["status"]).ends_with("Couldn't add new attachments: This request was not approved. Tap Refresh to catch up."),
        "{}", rig.text(&["status"]));
    for _ in 0..3 { rig.tick(); }
    rig.deliver(&[live("$later:test", "m.image", "later.jpg", TS + 50_000)]);
    rig.tick();
    assert_eq!(rig.trace.reviews.len(), 1, "a refusal is not retried, not even for later messages");
    assert!(rig.trace.events.is_empty());
    // Refresh asks again and catches up from the latest messages.
    rig.answers.is_sharing_allowed = true;
    rig.queue_read(&mixed_with(&[photo("$later:test", "later.jpg", TS + 50_000)]));
    rig.click("Refresh");
    assert_eq!(rig.trace.reads.len(), 2);
    assert_eq!(rig.tile(0), "later.jpg");
    assert_eq!(rig.text(&["status"]), "Tap a tile for details.");
    rig.queue_event(event_reply(photo("$after:test", "after.jpg", TS + 60_000)));
    rig.deliver(&[live("$after:test", "m.image", "after.jpg", TS + 60_000)]);
    rig.tick();
    assert_eq!(rig.trace.events.last().map(String::as_str), Some("$after:test"), "live lookups resume after Refresh");
}

#[test]
fn redactions_remove_attachments_and_their_cached_previews() {
    let mut rig = booted_with_media(&mixed_room());
    rig.answer_all_previews();
    assert!(rig.jail_file("/thumbs/0.txt").exists());
    rig.change(json!({"room_id": ROOM, "event_id": "$p2:test", "edited": false, "redacted": true, "body": ""}));
    assert_eq!(rig.shown_tiles(), NEWEST_FIRST[1..]);
    assert!(!rig.jail_file("/thumbs/0.txt").exists(), "a deleted photo's preview leaves the jail");
    assert_eq!(rig.text(&["f_photo"]), "Photos 1");
    assert!(!rig.stored("/gallery.json")["items"].as_array().unwrap().iter().any(|it| it["id"] == "$p2:test"));
    // A deletion of the open attachment closes it with a note.
    rig.open(0);
    assert_eq!(rig.text(&["d_name"]), "sources.zip");
    rig.change(json!({"room_id": ROOM, "event_id": "$f2:test", "edited": false, "redacted": false, "body": ""}));
    assert!(rig.is_browsing());
    assert_eq!(rig.text(&["status"]), "That attachment was deleted from the room.");
    // A deletion that races its own lookup never resurrects the attachment.
    rig.queue_event(event_reply(photo("$gone:test", "gone.jpg", TS + 70_000)));
    rig.host.broker.forget_app(APP);
    assert!(rig.host.call_strings(&rig.app, "on_room_message", &[&json!([live("$gone:test", "m.image", "gone.jpg", TS + 70_000)]).to_string()]));
    assert!(rig.host.call_strings(&rig.app, "on_room_message_changed",
        &[&json!({"room_id": ROOM, "event_id": "$gone:test", "edited": false, "redacted": true, "body": ""}).to_string()]));
    rig.settle();
    assert!(!rig.shown_tiles().contains(&"gone.jpg".to_string()));
    // Unknown events and edits of unknown events are ignored.
    rig.change(json!({"room_id": ROOM, "event_id": "$unknown:test", "edited": true, "redacted": false, "body": "edited"}));
    assert_eq!(rig.shown_tiles().len(), 5);
}

#[test]
fn changes_during_restore_reach_items_that_are_not_back_yet() {
    let rig = Rig::launch();
    let items: Vec<serde_json::Value> = (0..150_u64).map(|n| json!({"id": format!("$s{n}:test"), "kind": "photo", "name": format!("saved {n}.jpg"),
        "mime": "image/jpeg", "size": 1000, "w": 0, "h": 0, "dur": 0, "sender": "alice", "ts": TS - n * 1000, "caption": "", "has_no_preview": false})).collect();
    std::fs::write(rig.jail_file("/gallery.json"), json!({"v": 1, "items": items, "before": "$s149:test", "newest_ts": TS,
        "scanned": 150, "is_at_start": false, "is_capped": false, "gap": null}).to_string()).unwrap();
    std::fs::create_dir_all(rig.jail_file("/thumbs")).unwrap();
    std::fs::write(rig.jail_file("/thumbs/0.txt"), "AAAA").unwrap();
    std::fs::write(rig.jail_file("/thumbs.json"), json!({"ids": ["$s140:test"], "next": 1}).to_string()).unwrap();
    let mut rig = rig.relaunch();
    rig.queue_read(&[]);
    rig.boot();
    assert_eq!(rig.text(&["f_photo"]), "Photos 100", "the newest slice comes back first");
    rig.change(json!({"room_id": ROOM, "event_id": "$s140:test", "edited": false, "redacted": true, "body": ""}));
    assert!(!rig.jail_file("/thumbs/0.txt").exists(), "the preview of a deleted photo leaves the jail before it is restored");
    rig.change(json!({"room_id": ROOM, "event_id": "$s120:test", "edited": true, "redacted": false, "body": "Edited caption"}));
    rig.change(json!({"room_id": ROOM, "event_id": "$s121:test", "edited": true, "redacted": false, "body": "saved 121.jpg"}));
    for _ in 0..3 { rig.tick(); }
    assert_eq!(rig.text(&["f_photo"]), "Photos 149");
    let saved = rig.stored("/gallery.json")["items"].as_array().unwrap().clone();
    assert_eq!(saved.len(), 149);
    assert!(!saved.iter().any(|it| it["id"] == "$s140:test"));
    let caption_of = |id: &str| saved.iter().find(|it| it["id"] == id).unwrap()["caption"].as_str().unwrap().to_string();
    assert_eq!([caption_of("$s120:test"), caption_of("$s121:test")], ["Edited caption", ""]);
}

#[test]
fn caption_edits_update_the_saved_item_and_the_open_detail() {
    let mut rig = booted(&mixed_room());
    rig.open(0);
    assert!(!rig.is_visible(&["d_caption_wrap"]));
    rig.change(json!({"room_id": ROOM, "event_id": "$p2:test", "edited": true, "redacted": false, "body": "Sunset from the pier"}));
    assert!(rig.is_visible(&["d_caption_wrap"]));
    assert_eq!(rig.text(&["d_caption"]), "Sunset from the pier");
    let saved = rig.stored("/gallery.json");
    assert_eq!(saved["items"][0]["caption"], "Sunset from the pier");
    rig.change(json!({"room_id": ROOM, "event_id": "$p2:test", "edited": true, "redacted": false, "body": "sunset.jpg"}));
    assert!(!rig.is_visible(&["d_caption_wrap"]), "a body equal to the file name is no caption");
}

// ---- Older history

#[test]
fn load_older_pages_back_a_few_pages_per_tap_with_progress_stop_and_an_end() {
    let mut rig = booted(&mixed_room());
    let page = |from: u64, count: u64| -> Vec<serde_json::Value> {
        (from..from + count).map(|n| if n % 5 == 0 { photo(&format!("$o{n}:test"), &format!("old {n}.jpg"), TS - 1_000_000 + n * 1_000) }
            else { text(&format!("$x{n}:test"), "chatter", TS - 1_000_000 + n * 1_000) }).collect()
    };
    let first = page(100, 50);
    let second = page(50, 50);
    let third = page(0, 50);
    rig.queue_older(&first, true);
    rig.queue_older(&second, true);
    rig.queue_older(&third, true);
    rig.click("Load older");
    assert_eq!(rig.trace.older, [(Some("$t1:test".to_string()), 50)], "the first page answers the tap, before the latest window");
    assert_eq!(rig.text(&["older_line"]), "Looking further back… 50 messages checked, 10 attachments found.");
    assert!(rig.is_visible(&["stop_wrap"]) && !rig.is_visible(&["older_btn_wrap"]));
    rig.tick();
    assert_eq!(rig.trace.older[1], (Some("$o100:test".to_string()), 50), "each page continues from the oldest message so far");
    rig.tick();
    assert_eq!(rig.trace.older.len(), 3);
    rig.tick();
    assert_eq!(rig.trace.older.len(), 3, "three pages per tap");
    assert!(!rig.is_visible(&["stop_wrap"]) && rig.is_visible(&["older_btn_wrap"]));
    assert_eq!(rig.text(&["older_line"]), "Checked the latest 161 messages.");
    assert_eq!(rig.text(&["f_photo"]), "Photos 32");
    assert_eq!(rig.text(&["page_line"]), "1–24 of 37");
    // Stop ends a run between pages.
    let fourth: Vec<serde_json::Value> = (0..50).map(|n| text(&format!("$y{n}:test"), "older chatter", TS - 2_000_000 + n * 1_000)).collect();
    rig.queue_older(&fourth, true);
    rig.click("Load older");
    assert_eq!(rig.trace.older.len(), 4);
    rig.click("Stop");
    rig.tick();
    assert_eq!(rig.trace.older.len(), 4, "Stop prevents the next page");
    assert_eq!(rig.text(&["older_line"]), "Stopped after 50 messages.");
    assert!(rig.is_visible(&["older_btn_wrap"]));
    // The start of the room ends paging for good.
    rig.queue_older(&[photo("$first:test", "first ever.jpg", TS - 3_000_000)], false);
    rig.click("Load older");
    assert_eq!(rig.trace.older[4], (Some("$y0:test".to_string()), 50));
    assert_eq!(rig.text(&["older_line"]), "That's everything, back to the start of this room.");
    assert!(!rig.is_visible(&["older_btn_wrap"]) && !rig.is_visible(&["stop_wrap"]));
    let saved = rig.stored("/gallery.json");
    assert_eq!((saved["before"].as_str(), saved["is_at_start"].as_bool()), (Some("$first:test"), Some(true)));
    rig.act("load_older", &[]);
    assert_eq!(rig.trace.older.len(), 5, "nothing more to load");
}

#[test]
fn a_failed_page_stops_and_waits_for_the_next_tap() {
    let mut rig = booted(&mixed_room());
    rig.set_grant(Permission::MatrixRoomRead, GrantState::Granted);
    rig.host.permissions.set_capability(APP, "matrix.room.messages.paginate", GrantState::Denied);
    rig.host.publish_grants(&rig.app, APP);
    rig.settle();
    rig.click("Load older");
    assert!(rig.trace.older.is_empty());
    assert_eq!(rig.text(&["older_line"]), "Loading older messages is turned off for Room Gallery in Mini Apps settings.",
        "Refresh can't undo a blocked ability, so it isn't offered");
    rig.host.permissions.set_capability(APP, "matrix.room.messages.paginate", GrantState::Granted);
    rig.host.publish_grants(&rig.app, APP);
    rig.settle();
    assert!(rig.trace.older.is_empty(), "a grant alone does not page");
    // A failed page ends the run and waits for the next tap.
    rig.queue_error("matrix.room.messages.paginate", "couldn't load older messages: timed out");
    rig.click("Load older");
    for _ in 0..3 { rig.tick(); }
    assert_eq!(rig.trace.older.len(), 1);
    assert_eq!(rig.text(&["older_line"]), "Couldn't load older messages: timed out.");
    assert!(rig.is_visible(&["older_btn_wrap"]) && !rig.is_visible(&["stop_wrap"]));
    // An empty page that claims more history would repeat the same request forever, so it ends here.
    rig.queue_older(&[], true);
    rig.click("Load older");
    for _ in 0..3 { rig.tick(); }
    assert_eq!(rig.trace.older.len(), 2);
    assert_eq!(rig.text(&["older_line"]), "Robrix couldn't load anything older than this. It may not be readable on this device.");
    assert!(!rig.is_visible(&["older_btn_wrap"]));
    rig.act("load_older", &[]);
    assert_eq!(rig.trace.older.len(), 2, "the stuck cursor is not asked again");
    // Refresh is the way to try again.
    rig.queue_read(&mixed_room());
    rig.click("Refresh");
    assert!(rig.is_visible(&["older_btn_wrap"]));
}

// ---- Permissions: the shared stock audits, for this app

#[test]
fn grouped_startup_finishes_a_denial_and_refreshes_after_a_new_approval() {
    let expected = vec![Permission::MatrixRoomRead, Permission::MatrixRoomWatch];
    let mut host = Harness::new();
    let (app, _) = host.launch_stock_permissions(APP, false, false);
    for permission in &expected { host.permissions.set(APP, *permission, GrantState::Denied); }
    host.publish_grants(&app, APP);
    host.boot();
    let (request, perms) = host.take_stock_batch(APP);
    assert_eq!(perms, expected, "the startup batch is exactly read and watch");
    let mut audit = StockAudit::default();
    host.answer_stock_batch(&app, APP, request, &perms, &[], &mut audit);
    host.settle_stock(&app, APP, &mut audit);
    assert!(!host.stock_callbacks_paused());
    assert!(audit.services.is_empty() && audit.subscriptions.is_empty(), "nothing rejected is read or watched");
    assert_eq!(audit.batch_prompts, 1);
    assert_eq!(app.widget(&host.cx, ids!(status)).text(), "Reading this room isn't allowed. Tap Refresh to ask again.");
    assert_eq!(app.widget(&host.cx, ids!(empty_line)).text(), "Allow Room Gallery to read what's shared in this room.");
    assert!(app.widget(&host.cx, ids!(empty_allow_wrap)).visible() && !app.widget(&host.cx, ids!(empty_retry_wrap)).visible(),
        "a refusal offers the blue Ask again, not a retry");
    host.publish_grants(&app, APP);
    host.settle_stock(&app, APP, &mut audit);
    assert_eq!(audit.batch_prompts, 1, "a permission change never asks again");
    assert!(audit.services.is_empty());
    for permission in &perms { host.permissions.set(APP, *permission, GrantState::Ask); }
    host.widget_click(&app, "Refresh", &[]);
    host.settle_stock(&app, APP, &mut audit);
    assert_eq!(audit.services, ["matrix.room.messages.read", "device.info"], "the visible retry reads the approved room");
    assert_eq!(audit.subscriptions, ["on_room_message", "on_room_message_changed"]);
    assert!(audit.batch_prompts <= 2);
    let reads = audit.services.len();
    let hooks = audit.subscriptions.len();
    for permission in &perms { host.permissions.set(APP, *permission, GrantState::Denied); }
    host.publish_grants(&app, APP);
    host.settle_stock(&app, APP, &mut audit);
    assert_eq!((audit.services.len(), audit.subscriptions.len()), (reads, hooks), "revoked reads and hooks stop");
    for permission in &perms { host.permissions.set(APP, *permission, GrantState::Granted); }
    host.publish_grants(&app, APP);
    host.settle_stock(&app, APP, &mut audit);
    assert_eq!(audit.services.len(), reads + 1, "reapproval reads again");
    assert_eq!(audit.subscriptions.len(), hooks + 2, "and resubscribes both hooks");
    assert_eq!(audit.batch_prompts, 2);
}

#[test]
fn grouped_startup_uses_a_selected_read_without_watching() {
    let mut host = Harness::new();
    let (app, _) = host.launch_stock_permissions(APP, false, false);
    for permission in [Permission::MatrixRoomRead, Permission::MatrixRoomWatch] { host.permissions.set(APP, permission, GrantState::Denied); }
    host.publish_grants(&app, APP);
    host.boot();
    let (request, perms) = host.take_stock_batch(APP);
    let mut audit = StockAudit::default();
    host.answer_stock_batch(&app, APP, request, &perms, &[Permission::MatrixRoomRead], &mut audit);
    host.settle_stock(&app, APP, &mut audit);
    assert_eq!(audit.prompts, [Permission::MatrixRoomRead]);
    assert_eq!(audit.services, ["matrix.room.messages.read", "device.info"]);
    assert!(audit.subscriptions.is_empty(), "declined live updates stay off");
    assert_eq!(host.permissions.state(APP, Permission::MatrixRoomWatch), GrantState::Denied);
    assert_eq!(app.widget(&host.cx, ids!(status)).text(), "Nothing shared in the latest messages yet. Live updates are off.");
    host.publish_grants(&app, APP);
    host.settle_stock(&app, APP, &mut audit);
    assert_eq!(audit.services.len(), 2, "a completed partial setup does not reload");
    assert_eq!(audit.batch_prompts, 1);
}

#[test]
fn a_strict_refusal_finishes_and_retries_only_from_refresh() {
    let mut host = Harness::new();
    host.permissions.set_strict(true);
    let (app, _) = host.launch_stock_permissions(APP, false, false);
    host.call(&app, "on_app_resize", &[460_f64.into(), 700_f64.into()]);
    host.boot();
    assert!(host.deny_stock_pending(&app, APP) > 0, "the startup batch is the first ask");
    assert!(!host.stock_callbacks_paused());
    assert!(host.broker.failures().iter().all(|failure| failure.error == "Permission denied."));
    host.publish_grants(&app, APP);
    for ask in host.process() { assert!(matches!(ask, BrokerAsk::Used { .. }), "no automatic retry"); }
    let declared = host.apps.get(APP).unwrap().permissions.clone();
    for permission in declared { host.permissions.set(APP, Permission::from_str(&permission).unwrap(), GrantState::Ask); }
    host.widget_click(&app, "Refresh", &[]);
    let mut audit = StockAudit::default();
    host.settle_stock(&app, APP, &mut audit);
    assert!(!host.stock_callbacks_paused());
    assert!(!audit.services.is_empty() && !audit.subscriptions.is_empty(), "the visible retry completes real work");
}

#[test]
fn live_updates_resubscribe_after_revocation_and_regrant_without_prompting() {
    let mut host = Harness::new();
    let (app, _) = host.launch_stock(APP, false);
    host.boot();
    let mut audit = StockAudit::default();
    host.settle_stock(&app, APP, &mut audit);
    let hooks = audit.subscriptions.clone();
    assert_eq!(hooks, ["on_room_message", "on_room_message_changed"]);
    host.permissions.set(APP, Permission::MatrixRoomWatch, GrantState::Denied);
    host.publish_grants(&app, APP);
    host.settle_stock(&app, APP, &mut audit);
    let before = audit.subscriptions.len();
    assert_eq!(before, 2, "nothing resubscribes while revoked");
    host.permissions.set(APP, Permission::MatrixRoomWatch, GrantState::Granted);
    host.publish_grants(&app, APP);
    host.settle_stock(&app, APP, &mut audit);
    let restored = &audit.subscriptions[before..];
    assert_eq!(restored.len(), 2, "both hooks come back");
    assert!(restored.iter().all(|hook| hooks.contains(hook)));
    assert!(audit.prompts.is_empty());
}

// ---- Media refusals and revocation

#[test]
fn a_refused_media_prompt_shows_icons_and_asks_again_only_from_show_previews() {
    let mut rig = booted(&mixed_room());
    rig.answers.is_media_allowed = false;
    rig.click("Show previews");
    assert_eq!(rig.trace.refusals, 1);
    assert!(rig.trace.previews.is_empty());
    assert!(rig.is_visible(&["preview_wrap"]));
    assert_eq!(rig.text(&["preview_note"]), "Previews weren't allowed, so photos and videos show as icons.");
    assert!((0..7).all(|k| !rig.is_tile_picture(k)) && rig.text(&["t0", "ph_icon"]) == "🖼️");
    rig.host.publish_grants(&rig.app, APP);
    rig.settle();
    for _ in 0..3 { rig.tick(); }
    rig.act("open_tile", &[0_u32.into()]);
    assert!(rig.is_visible(&["d_preview_wrap"]), "the detail offers its own retry");
    rig.act("back_to_grid", &[]);
    assert_eq!((rig.trace.refusals, rig.trace.audit.concrete_prompts), (1, 1), "nothing asks again on its own");
    rig.answers.is_media_allowed = true;
    rig.click("Show previews");
    assert_eq!(rig.trace.audit.concrete_prompts, 2, "the visible control asks again");
    assert_eq!(rig.trace.previews, [("$p2:test".to_string(), 256)]);
    rig.answer_all_previews();
    assert!(rig.is_tile_picture(0) && !rig.is_visible(&["preview_wrap"]));
}

#[test]
fn a_refused_homeserver_share_pauses_previews_until_show_previews() {
    let mut rig = Rig::launch();
    rig.host.permissions.set(APP, Permission::MatrixMedia, GrantState::Granted);
    rig.host.publish_grants(&rig.app, APP);
    rig.answers.is_sharing_allowed = false;
    rig.queue_read(&mixed_room());
    rig.boot();
    assert_eq!(rig.trace.reviews, ["matrix.media_preview"]);
    assert!(rig.trace.previews.is_empty(), "the refused download never ran");
    assert_eq!(rig.text(&["preview_note"]), "Previews were blocked: This request was not approved.");
    assert!(rig.is_visible(&["preview_wrap"]));
    for _ in 0..3 { rig.tick(); }
    rig.host.publish_grants(&rig.app, APP);
    rig.settle();
    rig.open(0);
    assert_eq!(rig.trace.reviews.len(), 1, "no timer, grant or opening repeats a refused share");
    assert!(rig.is_visible(&["d_preview_wrap"]));
    rig.act("back_to_grid", &[]);
    rig.answers.is_sharing_allowed = true;
    rig.click("Show previews");
    assert_eq!(rig.trace.previews, [("$p2:test".to_string(), 256)]);
    rig.answer_all_previews();
    assert_eq!(rig.trace.previews.len(), 3);
    assert!(!rig.is_visible(&["preview_wrap"]));
}

#[test]
fn a_rate_limited_preview_pauses_previews_until_show_previews() {
    let mut rig = booted_with_media(&photos(6, 1));
    // One refusal is waited out quietly: nothing goes out during Robrix's cooldown, then previews resume.
    assert_eq!(rig.fail_download("too many requests, slow down"), ("$p6:test".to_string(), Some(256)));
    assert!(rig.parked.is_empty(), "nothing more is asked during the cooldown");
    assert!(!rig.is_visible(&["preview_wrap"]), "the button stays hidden during the cooldown");
    // A tap that landed just before it hid still sends nothing.
    rig.host.broker.forget_app(APP);
    assert!(rig.host.call(&rig.app, "show_previews", &[]));
    rig.settle();
    assert!(rig.parked.is_empty(), "not even from a tap");
    rig.tick();
    assert_eq!(rig.trace.previews[1], ("$p6:test".to_string(), 256), "the refused preview is asked again after the cooldown");
    // A second refusal inside Robrix's two-minute strike window pauses previews until the user asks.
    rig.fail_download("too many requests, slow down");
    assert_eq!(rig.text(&["preview_note"]), "Previews are paused so Robrix's request limit can recover.");
    assert!(rig.is_visible(&["preview_wrap"]));
    for _ in 0..3 { rig.tick(); }
    assert_eq!(rig.trace.previews.len(), 2, "a paused budget is never hammered");
    rig.click("Show previews");
    rig.tick();
    assert_eq!(rig.trace.previews.len(), 3);
    assert_eq!(rig.trace.previews[2], ("$p6:test".to_string(), 256), "the paused photo is fetched again only now");
}

#[test]
fn revoking_media_stops_previews_and_a_regrant_resumes_without_a_prompt() {
    let mut rig = booted_with_media(&photos(6, 1));
    rig.answer_preview();
    assert_eq!(rig.parked.len(), 2);
    rig.set_grant(Permission::MatrixMedia, GrantState::Denied);
    rig.answer_preview();
    rig.answer_preview();
    for _ in 0..3 { rig.tick(); }
    assert_eq!(rig.trace.previews.len(), 3, "no new preview while media is revoked");
    assert!(rig.is_tile_picture(0) && rig.is_tile_picture(1) && rig.is_tile_picture(2), "previews already granted still land");
    assert!(rig.is_visible(&["preview_wrap"]));
    let prompts = rig.trace.audit.prompts.len();
    rig.set_grant(Permission::MatrixMedia, GrantState::Granted);
    rig.answer_all_previews();
    assert_eq!(rig.trace.previews.len(), 6, "the rest load after the regrant");
    assert_eq!(rig.trace.audit.prompts.len(), prompts, "without another prompt");
    // An unrelated permission change repeats nothing.
    let services = rig.trace.audit.services.len();
    rig.host.publish_grants(&rig.app, APP);
    rig.settle();
    rig.tick();
    assert_eq!(rig.trace.audit.services.len(), services);
}

#[test]
fn a_hidden_pane_pauses_previews_until_it_is_shown_again() {
    let mut rig = Rig::launch();
    rig.host.permissions.set(APP, Permission::MatrixMedia, GrantState::Granted);
    rig.host.publish_grants(&rig.app, APP);
    rig.queue_read(&photos(4, 1));
    rig.host.boot();
    assert!(rig.host.call_strings(&rig.app, "on_focus_changed", &[r#"{"foreground": false}"#]));
    rig.resize(460.0, 700.0);
    rig.settle();
    for _ in 0..3 { rig.tick(); }
    assert!(rig.trace.previews.is_empty(), "nothing downloads for a pane nobody sees");
    assert_eq!(rig.trace.reads, [30], "the room is still read");
    assert!(rig.host.call_strings(&rig.app, "on_focus_changed", &[r#"{"foreground": true}"#]));
    rig.settle();
    assert_eq!(rig.trace.previews.len(), 1);
    rig.answer_all_previews();
    assert_eq!(rig.trace.previews.len(), 4);
}
// ---- Stale replies

#[test]
fn a_read_from_before_refresh_is_ignored() {
    let mut rig = Rig::launch();
    rig.is_parking_reads = true;
    rig.boot();
    let early = rig.parked_reads.pop_front().expect("the first read");
    rig.click("Refresh");
    let current = rig.parked_reads.pop_front().expect("the refreshed read");
    assert_eq!(rig.trace.reads, [30, 30]);
    rig.reply(current, &json!({"messages": [photo("$now:test", "now.jpg", TS + 2_000)], "unread_count": 0}).to_string());
    rig.settle();
    rig.reply(early, &json!({"messages": [photo("$then:test", "then.jpg", TS + 1_000)], "unread_count": 0}).to_string());
    rig.settle();
    assert_eq!(rig.shown_tiles(), ["now.jpg"], "the earlier refresh's answer is dropped");
    assert_eq!(rig.text(&["status"]), "Tap a tile for details.");
}

// ---- Saving and reopening

#[test]
fn reopening_bridges_a_gap_left_while_the_gallery_was_closed() {
    let rig = booted(&[photo("$a:test", "a.jpg", TS + 1_000), photo("$b:test", "b.jpg", TS + 2_000), photo("$c:test", "c.jpg", TS + 3_000)]);
    let saved = rig.stored("/gallery.json");
    assert_eq!((saved["before"].as_str(), saved["newest_ts"].as_u64(), saved["scanned"].as_u64()), (Some("$a:test"), Some(TS + 3_000), Some(3)));
    // A week later the latest window no longer reaches the last visit.
    let mut rig = rig.relaunch();
    rig.queue_read(&[photo("$x:test", "x.jpg", TS + 100_000), photo("$y:test", "y.jpg", TS + 101_000)]);
    rig.boot();
    assert_eq!(rig.shown_tiles(), ["y.jpg", "x.jpg", "c.jpg", "b.jpg", "a.jpg"]);
    assert_eq!(rig.text(&["older_line"]), "Load older first fills in what was shared while the gallery was closed.");
    rig.queue_older(&[photo("$g1:test", "g1.jpg", TS + 50_000), text("$g2:test", "chat", TS + 60_000)], true);
    rig.queue_older(&[photo("$c:test", "c.jpg", TS + 3_000), photo("$g0:test", "g0.jpg", TS + 4_000)], true);
    rig.queue_older(&[photo("$z:test", "z.jpg", TS)], false);
    rig.click("Load older");
    rig.tick();
    rig.tick();
    assert_eq!(rig.trace.older, [(Some("$x:test".to_string()), 50), (Some("$g1:test".to_string()), 50), (Some("$a:test".to_string()), 50)],
        "the gap fills first, then paging continues from the saved cursor");
    assert_eq!(rig.shown_tiles(), ["y.jpg", "x.jpg", "g1.jpg", "g0.jpg", "c.jpg", "b.jpg", "a.jpg", "z.jpg"]);
    assert_eq!(rig.text(&["older_line"]), "That's everything, back to the start of this room.");
    let saved = rig.stored("/gallery.json");
    assert!(saved["gap"].is_null());
    assert_eq!((saved["before"].as_str(), saved["is_at_start"].as_bool()), (Some("$z:test"), Some(true)));
}

#[test]
fn an_overlapping_reopen_keeps_the_cursor_and_counts_only_new_messages() {
    let rows = vec![photo("$a:test", "a.jpg", TS + 1_000), text("$t:test", "hi", TS + 2_000)];
    let rig = booted(&rows);
    let mut rig = rig.relaunch();
    let mut newer = rows.clone();
    newer.push(photo("$n:test", "n.jpg", TS + 3_000));
    rig.queue_read(&newer);
    rig.boot();
    assert_eq!(rig.shown_tiles(), ["n.jpg", "a.jpg"]);
    assert_eq!(rig.text(&["older_line"]), "Checked the latest 3 messages.");
    let saved = rig.stored("/gallery.json");
    assert_eq!((saved["before"].as_str(), saved["gap"].is_null()), (Some("$a:test"), true));
}

#[test]
fn corrupt_saved_files_fall_back_to_a_fresh_gallery() {
    for (gallery, thumbs) in [
        ("not json", "not json"),
        (r#"{"v": 2, "items": [{"id": "$a:test", "kind": "photo"}]}"#, r#"{"ids": "nope"}"#),
        (r#"{"v": 1, "items": [{"id": "$a:test", "kind": "hologram"}, {"kind": "photo"}, 7, {"id": "$b:test", "kind": "file", "size": "big"}], "gap": {"before": 5}}"#,
            r#"{"ids": [7, "$x:test"], "next": 900}"#),
    ] {
        let rig = Rig::launch();
        std::fs::write(rig.jail_file("/gallery.json"), gallery).unwrap();
        std::fs::write(rig.jail_file("/thumbs.json"), thumbs).unwrap();
        let mut rig = rig.relaunch();
        rig.queue_read(&mixed_room());
        rig.boot();
        let mut expected: Vec<String> = NEWEST_FIRST.iter().map(|name| name.to_string()).collect();
        // The one usable saved row is kept with safe defaults; the rest are dropped.
        if gallery.contains("hologram") { expected.push("File".into()); }
        assert_eq!(rig.shown_tiles(), expected, "{gallery}");
        assert_eq!(rig.text(&["status"]), "Tap a tile for details.");
    }

    // Overlong saved text is clipped like fresh rows.
    let rig = Rig::launch();
    let long = "x".repeat(5_000);
    std::fs::write(rig.jail_file("/gallery.json"), json!({"v": 1, "items": [{"id": "$long:test", "kind": "photo", "name": long, "caption": long, "ts": TS}]}).to_string()).unwrap();
    let mut rig = rig.relaunch();
    rig.queue_read(&[]);
    rig.boot();
    assert_eq!(rig.tile(0), format!("{}…", "x".repeat(120)));
    rig.open(0);
    assert_eq!(rig.text(&["d_caption"]), format!("{}…", "x".repeat(300)));
}

#[test]
fn the_gallery_keeps_at_most_five_hundred_attachments() {
    let rig = Rig::launch();
    let items: Vec<serde_json::Value> = (0..520).map(|n| json!({"id": format!("$s{n}:test"), "kind": "photo", "name": format!("saved {n}.jpg"),
        "mime": "image/jpeg", "size": 1000, "w": 0, "h": 0, "dur": 0, "sender": "alice", "ts": TS - n * 1000, "caption": "", "has_no_preview": false})).collect();
    std::fs::write(rig.jail_file("/gallery.json"), json!({"v": 1, "items": items, "before": "$s519:test", "newest_ts": TS,
        "scanned": 900, "is_at_start": false, "is_capped": false, "gap": null}).to_string()).unwrap();
    let mut rig = rig.relaunch();
    rig.queue_read(&[photo("$new:test", "new.jpg", TS + 1_000)]);
    rig.boot();
    assert_eq!(rig.tile(0), "new.jpg", "the newest page shows before the rest comes back");
    for _ in 0..6 { rig.tick(); }
    assert_eq!(rig.text(&["f_photo"]), "Photos 500");
    assert_eq!(rig.tile(0), "new.jpg");
    assert_eq!(rig.text(&["older_line"]), "Showing the newest 500 attachments.");
    assert!(!rig.is_visible(&["older_btn_wrap"]), "a full gallery stops loading older history");
    assert_eq!(rig.stored("/gallery.json")["items"].as_array().unwrap().len(), 500);
    assert_eq!(rig.text(&["page_line"]), "1–24 of 500");
    for _ in 0..20 { rig.act("go_page", &[1_f64.into()]); }
    assert_eq!(rig.text(&["page_line"]), "481–500 of 500");
    assert_eq!(rig.shown_tiles().len(), 20);

    // A burst of new attachments merges into the full list within one reply's budget.
    let fresh: Vec<serde_json::Value> = (0..30_u64).map(|n| photo(&format!("$f{n}:test"), &format!("fresh {n}.jpg"), TS + 10_000 + n * 1000)).collect();
    rig.queue_read(&fresh);
    rig.click("Refresh");
    assert_eq!(rig.text(&["page_line"]), "481–500 of 500", "the open page stays put");
    for _ in 0..20 { rig.act("go_page", &[(-1_f64).into()]); }
    assert_eq!(rig.shown_tiles()[..3], ["fresh 29.jpg", "fresh 28.jpg", "fresh 27.jpg"]);
    let saved = rig.stored("/gallery.json")["items"].as_array().unwrap().clone();
    assert_eq!(saved.len(), 500);
    assert_eq!([saved[29]["name"].as_str(), saved[30]["name"].as_str(), saved[499]["name"].as_str()], [Some("fresh 0.jpg"), Some("new.jpg"), Some("saved 468.jpg")]);
}

// ---- Size tiers

#[test]
fn narrow_short_and_wide_panes_get_their_own_tiers() {
    let mut rig = booted(&photos(30, 1));
    rig.resize(214.0, 640.0);
    assert_eq!([rig.text(&["f_all"]), rig.text(&["f_photo"]), rig.text(&["f_video"])], ["All 30", "🖼️ 30", "🎬 0"]);
    assert_eq!(rig.active_chip(), ["all"]);
    assert_eq!([rig.text(&["prev_btn"]), rig.text(&["next_btn"])], ["‹", "›"]);
    assert_eq!(rig.text(&["stats_line"]), "30 attachments · 72 MB");
    assert!(rig.is_visible(&["title_wrap"]) && !rig.is_visible(&["hint_wrap"]));
    assert!(!rig.is_visible(&["refresh_wide"]) && rig.is_visible(&["refresh_narrow"]), "Refresh leaves the summary its width");
    assert!(!rig.seen().iter().any(|text| text == "Room Gallery"), "the pane header names the app");
    rig.resize(460.0, 400.0);
    assert!(rig.is_visible(&["refresh_wide"]) && !rig.is_visible(&["refresh_narrow"]));
    assert!(!rig.is_visible(&["title_wrap"]), "a short pane drops the room line");
    assert!(rig.is_visible(&["stats_wrap"]) && !rig.is_visible(&["hint_wrap"]), "the summary stays beside Refresh");
    assert_eq!(rig.text(&["f_photo"]), "Photos 30");
    rig.resize(1600.0, 900.0);
    assert!(rig.is_visible(&["title_wrap"]) && rig.is_visible(&["stats_wrap"]) && rig.is_visible(&["hint_wrap"]));
    // The download note stays folded under its disclosure until asked for.
    assert!(!rig.is_visible(&["how_body"]));
    rig.click("How it works ▼");
    assert!(rig.is_visible(&["how_body"]));
    rig.click("How it works ▲");
    assert!(!rig.is_visible(&["how_body"]));
    assert!(rig.is_visible(&["head_wrap"]));
    assert_eq!([rig.text(&["prev_btn"]), rig.text(&["next_btn"])], ["‹ Previous", "Next ›"]);
    assert_eq!(rig.text(&["stats_line"]), "30 photos · 72 MB");
    // Resizing changes no data.
    assert_eq!(rig.trace.audit.services, ["matrix.room.messages.read", "device.info"]);
}

// ---- The shared stock audit for this app

/// The body of `run_every_stock_app` in splash_information_flow.rs, with this app's primary action.
fn audit(strict: bool) {
    let manifest = a2app_core::builtin::stock(APP).unwrap();
    let mut host = Harness::new();
    host.permissions.set_strict(strict);
    let (app, _) = host.launch_stock_permissions(&manifest.id, false, false);
    host.boot();
    host.call(&app, "on_app_resize", &[460_f64.into(), 700_f64.into()]);
    let mut audit = StockAudit::default();
    host.settle_stock(&app, &manifest.id, &mut audit);
    let initial_prompts = audit.prompts.len();
    let initial_reviews = audit.reviews;
    // The primary action: a photo arrives live, the user opens it and shows it in the chat.
    host.queue_matrix_reply("matrix.room.event.read", serde_json::json!({"event_id":"$photo:test","room_id":ROOM,"sender":"alice","sender_id":"@alice:test","body":"cat.jpg","ts":1720000000000_u64,"msgtype":"m.image","media":{"uri":"mxc://media.test/cat","encrypted":false,"filename":"cat.jpg","mime_type":"image/jpeg","size":2048,"width":640,"height":480,"duration_ms":null},"edited":false,"reactions":[],"thread_root":null}));
    assert!(host.call_strings(&app, "on_room_message", &[r#"[{"room_id":"!private:test","event_id":"$photo:test","sender":"alice","sender_id":"@alice:test","sender_name":"Alice","body":"cat.jpg","ts":1720000000000,"msgtype":"m.image","is_own":false}]"#]));
    host.settle_stock(&app, &manifest.id, &mut audit);
    assert!(host.call(&app, "open_tile", &[0_u32.into()]));
    host.settle_stock(&app, &manifest.id, &mut audit);
    assert!(host.call(&app, "show_in_chat", &[]));
    host.settle_stock(&app, &manifest.id, &mut audit);
    let expected = (2, 1);
    eprintln!("stock permission audit: {} strict={} initial={} action={} initial_reviews={} action_reviews={}",
        manifest.id, strict, initial_prompts, audit.prompts.len() - initial_prompts, initial_reviews, audit.reviews - initial_reviews);
    if !strict {
        assert_eq!((initial_prompts, audit.prompts.len() - initial_prompts), expected,
            "{} initial and primary-action permission groups: {:?}", manifest.id, audit.prompts);
        assert_eq!((initial_reviews, audit.reviews - initial_reviews), (0, 2),
            "{} must finish each reviewed sink without repeated reviews", manifest.id);
    }
    assert_eq!(audit.actions, ["event"], "the primary action jumps to the message");
    assert!(audit.prompts.len() <= manifest.permissions.len(), "{} exceeds its finite declared group budget", manifest.id);
    assert!(audit.prompts.iter().all(|permission| manifest.declares(*permission)), "{} prompts only for its declared groups", manifest.id);
    assert!(audit.reviews <= 3, "{} exceeds the finite review budget for its primary path", manifest.id);
    assert!(!audit.services.is_empty() || !audit.subscriptions.is_empty(), "{} must complete a real service path", manifest.id);
    let services = audit.services.len();
    let subscriptions = audit.subscriptions.len();
    host.publish_grants(&app, &manifest.id);
    host.settle_stock(&app, &manifest.id, &mut audit);
    assert_eq!(audit.services.len(), services, "{} reloads on an unrelated grant", manifest.id);
    assert_eq!(audit.subscriptions.len(), subscriptions, "{} subscribes again on an unrelated grant", manifest.id);
    let mut hooks = audit.subscriptions.clone(); hooks.sort(); hooks.dedup();
    assert_eq!(hooks.len(), audit.subscriptions.len(), "{} has duplicate subscriptions", manifest.id);
    // A later tap reuses its grants: no new prompt or review, one more jump.
    let (prompts, reviews) = (audit.prompts.len(), audit.reviews);
    host.broker.forget_app(APP);
    assert!(host.call(&app, "show_in_chat", &[]));
    // A tap right after the first waits for the request budget; its timer sends it.
    host.boot();
    host.settle_stock(&app, APP, &mut audit);
    assert_eq!((audit.prompts.len(), audit.reviews, audit.actions.len()), (prompts, reviews, 2));
}

#[test]
fn the_stock_audit_passes_with_ordinary_permissions() { audit(false); }

#[test]
fn the_stock_audit_passes_with_strict_permissions() { audit(true); }

#[test]
fn every_request_carries_exactly_its_documented_arguments() {
    let notes = file("$n1:test", "notes.txt", "text/plain", 5, TS + 9_000);
    let mut rig = booted_with_media(&mixed_with(&[notes]));
    rig.answer_all_previews();
    rig.queue_older(&[photo("$o1:test", "old.jpg", TS - 5_000)], false);
    rig.click("Load older");
    rig.queue_event(event_reply(photo("$new:test", "fresh.jpg", TS + 20_000)));
    rig.tick();
    rig.deliver(&[live("$new:test", "m.image", "fresh.jpg", TS + 20_000)]);
    rig.tick();
    rig.answer_all_previews();
    rig.open(1);
    assert_eq!(rig.text(&["d_name"]), "notes.txt");
    rig.click("Share again");
    rig.tick();
    rig.answer_download(whole_file("$n1:test", "bWVkaWE=", "notes.txt", "text/plain"));
    rig.tick();
    rig.act("back_to_grid", &[]);
    rig.open(0);
    rig.answer_all_previews();
    let args: Vec<(String, String)> = rig.trace.args.iter().map(|(cap, args)| (cap.clone(), args.to_string())).collect();
    let expect = |cap: &str, json: &str| (cap.to_string(), json.to_string());
    assert_eq!(args, [
        expect("matrix.room.messages.read", r#"{"limit":30}"#),
        expect("matrix.media.download", r#"{"event_id":"$p2:test","size":256}"#),
        expect("matrix.media.download", r#"{"event_id":"$m1:test","size":256}"#),
        expect("matrix.media.download", r#"{"event_id":"$p1:test","size":256}"#),
        expect("matrix.room.messages.paginate", r#"{"before":"$t1:test","limit":50}"#),
        expect("matrix.media.download", r#"{"event_id":"$o1:test","size":256}"#),
        expect("matrix.room.event.read", r#"{"event_id":"$new:test"}"#),
        expect("matrix.media.download", r#"{"event_id":"$new:test","size":256}"#),
        expect("matrix.media.download", r#"{"event_id":"$n1:test"}"#),
        expect("matrix.media.download", r#"{"event_id":"$new:test","size":768}"#),
    ]);
    assert_eq!(rig.trace.attachments, [("bWVkaWE=".to_string(), "notes.txt".to_string(), "text/plain".to_string(), None)]);
    // No request names a room, a URI or a file name: the host resolves media by event id alone.
    for (_, args) in &rig.trace.args {
        for key in ["room_id", "uri", "url", "filename", "source_room"] { assert!(args.get(key).is_none(), "{args}"); }
    }
}

// ---- The request budget

/// Launches with every declared group already allowed, media included.
fn booted_with_everything(rows: &[serde_json::Value]) -> Rig {
    let mut host = Harness::new();
    let (app, context) = host.launch_stock(APP, false);
    let mut rig = Rig::with(host, app, context, Answers::default());
    rig.queue_read(rows);
    rig.boot();
    rig
}

#[test]
fn previews_burst_then_pace_under_the_budget() {
    let mut rig = booted_with_everything(&photos(30, 1));
    assert!(rig.trace.batches.is_empty() && rig.trace.silent_batches == 0, "with everything allowed, startup asks nothing");
    assert_eq!(rig.trace.previews.len(), 1, "the first preview goes alone");
    // Answered at once, previews keep coming without a timer while the budget stays above the taps' reserve.
    let mut max_in_flight = 0;
    let mut answer_parked = |rig: &mut Rig| while !rig.parked.is_empty() {
        max_in_flight = max_in_flight.max(rig.parked.len());
        rig.answer_preview();
    };
    answer_parked(&mut rig);
    let burst = rig.trace.previews.len();
    assert!((6..=8).contains(&burst), "a burst bounded by the reserve kept for taps: {burst}");
    // Then each pace step lets one more through.
    let mut steps = 0;
    while rig.pictures() < 24 {
        rig.tick();
        steps += 1;
        assert!(steps <= 24, "the page kept waiting");
        answer_parked(&mut rig);
    }
    assert_eq!(max_in_flight, 2, "never more than two at a time");
    assert_eq!(rig.trace.previews.len(), 24, "only the visible page");
    assert!(steps >= 24 - burst - 2, "the rest is paced: {steps} steps after a burst of {burst}");
}

/// Runs the gallery in real time on the real request budget, which nothing resets. Media requests
/// are answered `latency` after they go out, and a timer fires only once it has been pending
/// `timer_age`, longer than any wait the app arms in that run. Returns the time taken and the most
/// previews in flight; fails on any refusal for going over budget.
fn run_real_time(rig: &mut Rig, latency: std::time::Duration, timer_age: std::time::Duration, limit: std::time::Duration,
    mut each: impl FnMut(&mut Rig, std::time::Duration), is_done: impl Fn(&Rig) -> bool) -> (std::time::Duration, usize)
{
    let start = std::time::Instant::now();
    let mut timers = HashMap::new();
    let mut asked: HashMap<(String, Option<u32>), std::time::Instant> = HashMap::new();
    let mut max_in_flight = 0;
    loop {
        rig.settle();
        let now = std::time::Instant::now();
        for (_, id, edge) in &rig.parked { asked.entry((id.clone(), *edge)).or_insert(now); }
        max_in_flight = max_in_flight.max(rig.parked.iter().filter(|(_, _, edge)| edge.is_some()).count());
        while let Some((_, id, edge)) = rig.parked.front().cloned() {
            let at = *asked.entry((id.clone(), edge)).or_insert(now);
            if now.duration_since(at) < latency { break; }
            asked.remove(&(id, edge));
            rig.answer_front();
        }
        rig.fire_timers_older_than(timer_age, &mut timers);
        rig.settle();
        assert_eq!(rig.host.broker.refusal_count(APP), 0, "Robrix refused a request for going over budget");
        each(rig, start.elapsed());
        if is_done(rig) { return (start.elapsed(), max_in_flight); }
        assert!(start.elapsed() < limit, "still running after {limit:?}");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[test]
fn a_first_page_fills_in_seconds_on_the_real_budget() {
    let mut rig = booted_with_everything(&photos(30, 1));
    // No pace wait in a plain fill is longer than 0.42 s; firing later only slows the run down.
    let (took, in_flight) = run_real_time(&mut rig, std::time::Duration::from_millis(100), std::time::Duration::from_millis(420),
        std::time::Duration::from_secs(25), |_, _| {}, |rig| rig.pictures() == 24);
    eprintln!("room gallery: first page of 24 previews in {took:?} on the real budget");
    assert!(took < std::time::Duration::from_secs(15), "{took:?}");
    assert_eq!(in_flight, 2);
    assert_eq!(rig.trace.previews.len(), 24);
}

/// One tap of a scripted session: the guest function (with an optional tile index), and
/// what the person waits to see before tapping.
type Tap = (&'static str, fn(&Rig) -> bool);

#[test]
fn brisk_taps_during_a_fill_never_trip_the_real_budget() {
    let mut rows = photos(30, 1);
    rows.push(file("$n1:test", "notes.txt", "text/plain", 5, TS + 100_000));
    let mut rig = booted_with_everything(&rows);
    // A tap about every 0.7 s while the first page fills; each waits for what the person would see.
    let steps: [Tap; 8] = [
        ("open:1", |_| true),
        ("show_in_chat", |_| true),
        ("back_to_grid", |rig| rig.trace.navigations.len() == 1),
        ("open:0", |_| true),
        ("share_again", |_| true),
        ("back_to_grid", |rig| rig.trace.attachments.len() == 1),
        ("open:2", |_| true),
        ("show_in_chat", |_| true),
    ];
    let mut next = 0;
    let mut last = std::time::Duration::ZERO;
    // Taps can leave the budget near zero, and no wait is then longer than 1.85 s.
    let (took, in_flight) = run_real_time(&mut rig, std::time::Duration::from_millis(100), std::time::Duration::from_millis(1900),
        std::time::Duration::from_secs(40), |rig, elapsed| {
            if next == steps.len() || elapsed < last + std::time::Duration::from_millis(700) || !(steps[next].1)(rig) { return; }
            let (name, arg) = steps[next].0.split_once(':').unwrap_or((steps[next].0, ""));
            let args: Vec<ScriptValue> = arg.parse::<u32>().into_iter().map(Into::into).collect();
            let name = if name == "open" { "open_tile" } else { name };
            assert!(rig.host.call(&rig.app, name, &args));
            rig.settle();
            next += 1;
            last = elapsed;
        }, |rig| rig.trace.navigations.len() == 2);
    eprintln!("room gallery: brisk taps finished in {took:?} with no refusal");
    assert_eq!(next, steps.len());
    assert_eq!(rig.trace.attachments.len(), 1);
    assert!(in_flight <= 2);
}

// ---- Errors, deletions and live updates

#[test]
fn an_unknown_preview_error_fails_only_that_item() {
    let mut rig = booted_with_media(&photos(6, 1));
    rig.answer_preview();
    // A dropped connection or a bad encrypted file is this attachment's trouble, not a refusal.
    assert_eq!(rig.fail_download("The Matrix media download failed."), ("$p5:test".to_string(), Some(256)));
    assert_eq!(rig.fail_download("The attachment could not be decrypted or its hash did not match."), ("$p4:test".to_string(), Some(256)));
    rig.answer_all_previews();
    assert_eq!(rig.trace.previews.len(), 6, "the other previews keep loading");
    assert!(!rig.is_visible(&["preview_wrap"]), "nothing is paused or blamed on a refusal");
    assert_eq!(rig.text(&["status"]), "Tap a tile for details.");
    assert_eq!(rig.pictures(), 4);
    assert_eq!([rig.tile_meta(1), rig.tile_meta(2)], ["2.4 MB", "2.4 MB"]);
    // The detail's large preview fails the same way, and offers its own retry.
    rig.open(3);
    rig.fail_download("The picture preview could not be prepared.");
    assert!(rig.is_visible(&["d_preview_wrap"]) && !rig.is_visible(&["d_note_wrap"]));
    // Back on the page, each failed tile gets one more try.
    rig.act("back_to_grid", &[]);
    rig.tick();
    rig.answer_all_previews();
    assert_eq!(rig.pictures(), 6);
}

#[test]
fn attachments_deleted_while_closed_are_checked_before_removal() {
    let rows: Vec<serde_json::Value> = ["a", "b", "c", "d", "e"].iter().enumerate()
        .map(|(n, name)| photo(&format!("${name}:test"), &format!("{name}.jpg"), TS + n as u64 * 1_000)).collect();
    let mut rig = booted_with_media(&rows);
    rig.answer_all_previews();
    let b_slot = rig.stored("/thumbs.json")["ids"].as_array().unwrap().iter().position(|id| id == "$b:test").unwrap();
    let b_thumb = format!("/thumbs/{b_slot}.txt");
    assert!(rig.jail_file(&b_thumb).exists());
    // While the gallery was closed b was deleted; d is only missing from a gap in Robrix's cache.
    let mut rig = rig.relaunch();
    rig.queue_read(&[rows[0].clone(), rows[2].clone(), rows[4].clone()]);
    rig.queue_event(event_reply(rows[3].clone()));
    rig.queue_error("matrix.room.event.read", "that event isn't a message");
    rig.boot();
    for _ in 0..3 { rig.tick(); }
    assert_eq!(rig.trace.events, ["$d:test", "$b:test"], "the saved attachments the room left out are looked up");
    assert_eq!(rig.shown_tiles(), ["e.jpg", "d.jpg", "c.jpg", "a.jpg"], "only the confirmed deletion goes");
    assert!(!rig.jail_file(&b_thumb).exists(), "with its cached preview");
    assert!(!rig.stored("/gallery.json")["items"].as_array().unwrap().iter().any(|it| it["id"] == "$b:test"));
    assert!(rig.trace.previews.is_empty(), "the rest come from the cache");
    // An attachment found deleted when its picture is fetched goes too.
    let mut rig = booted_with_media(&[photo("$x:test", "x.jpg", TS), photo("$y:test", "y.jpg", TS + 1_000)]);
    assert_eq!(rig.fail_download("The selected event is not an available media message in the source room."), ("$y:test".to_string(), Some(256)));
    assert_eq!(rig.shown_tiles(), ["x.jpg"]);
    assert!(!rig.stored("/gallery.json")["items"].as_array().unwrap().iter().any(|it| it["id"] == "$y:test"));
}

#[test]
fn a_deletion_restarts_preview_loading() {
    let mut rig = booted_with_media(&photos(25, 1));
    rig.answer_all_previews();
    assert_eq!(rig.trace.previews.len(), 24);
    rig.change(json!({"room_id": ROOM, "event_id": "$p25:test", "edited": false, "redacted": true, "body": ""}));
    assert_eq!(rig.tile(23), "photo 1.jpg", "an older photo moves onto the page");
    rig.tick();
    assert_eq!(rig.trace.previews.last(), Some(&("$p1:test".to_string(), 256)), "and its preview is asked for without a tap");
    rig.answer_all_previews();
    assert!(rig.is_tile_picture(23));
    // Deleting the open photo brings back the grid previews its detail was holding.
    let mut rig = booted_with_media(&photos(30, 1));
    rig.answer_preview();
    rig.open(5);
    let tiles = |rig: &Rig| rig.trace.previews.iter().filter(|(_, edge)| *edge == 256).count();
    for _ in 0..3 {
        while !rig.parked.is_empty() { rig.answer_preview(); }
        rig.tick();
    }
    let before = tiles(&rig);
    assert_eq!(rig.trace.previews.last(), Some(&("$p25:test".to_string(), 768)), "only the open photo's large preview went out");
    rig.tick();
    assert_eq!(tiles(&rig), before, "the grid waits behind the open detail");
    rig.change(json!({"room_id": ROOM, "event_id": "$p25:test", "edited": false, "redacted": true, "body": ""}));
    assert!(rig.is_browsing());
    assert_eq!(rig.text(&["status"]), "That attachment was deleted from the room.");
    rig.tick();
    assert!(tiles(&rig) > before, "the grid's previews resume on their own");
}

#[test]
fn a_message_delivered_twice_during_its_lookup_is_looked_up_once() {
    let mut rig = booted(&mixed_room());
    rig.queue_event(event_reply(photo("$new:test", "fresh.jpg", TS + 20_000)));
    let batch = json!([live("$new:test", "m.image", "fresh.jpg", TS + 20_000)]).to_string();
    // Robrix can deliver a message again after a sync gap, here while its lookup is still out.
    assert!(rig.host.call_strings(&rig.app, "on_room_message", &[&batch]));
    assert!(rig.host.call_strings(&rig.app, "on_room_message", &[&batch]));
    rig.settle();
    for _ in 0..3 { rig.tick(); }
    assert_eq!(rig.trace.events, ["$new:test"]);
    assert_eq!(rig.tile(0), "fresh.jpg");
}

#[test]
fn the_preview_note_shows_only_while_a_picture_is_missing() {
    let mut rig = booted_with_media(&photos(3, 1));
    rig.answer_all_previews();
    // A session grant ended: the cached pictures still show, so there is nothing to explain.
    rig.host.permissions.set(APP, Permission::MatrixMedia, GrantState::Ask);
    let mut rig = rig.relaunch();
    rig.queue_read(&photos(3, 1));
    rig.boot();
    assert_eq!(rig.pictures(), 3);
    assert!(!rig.is_visible(&["preview_wrap"]), "cached pictures are not icons");
    // A new photo that has no picture yet brings the note back.
    rig.queue_event(event_reply(photo("$p4:test", "photo 4.jpg", TS + 4_000)));
    rig.deliver(&[live("$p4:test", "m.image", "photo 4.jpg", TS + 4_000)]);
    rig.tick();
    assert_eq!(rig.tile(0), "photo 4.jpg");
    assert!(rig.is_visible(&["preview_wrap"]));
    assert_eq!(rig.text(&["preview_note"]), "Photos and videos show as icons until you allow previews from your homeserver.");
}

#[test]
fn a_rate_limited_lookup_keeps_its_queue_and_resumes() {
    let mut rig = booted(&mixed_room());
    rig.queue_error("matrix.room.event.read", "too many requests, slow down");
    rig.queue_event(event_reply(photo("$n2:test", "two.jpg", TS + 21_000)));
    rig.queue_event(event_reply(photo("$n1:test", "one.jpg", TS + 20_000)));
    rig.deliver(&[live("$n1:test", "m.image", "one.jpg", TS + 20_000), live("$n2:test", "m.image", "two.jpg", TS + 21_000)]);
    assert_eq!(rig.trace.events, ["$n1:test"]);
    assert_eq!(rig.text(&["status"]), "Tap a tile for details.", "a slow-down is waited out, not reported as a refusal");
    for _ in 0..3 { rig.tick(); }
    assert_eq!(rig.trace.events, ["$n1:test", "$n2:test", "$n1:test"], "the queue is kept and resumes after Robrix's cooldown");
    assert_eq!(rig.shown_tiles()[..2], ["two.jpg", "one.jpg"]);
    // Live updates stayed on.
    rig.queue_event(event_reply(photo("$n3:test", "three.jpg", TS + 22_000)));
    rig.deliver(&[live("$n3:test", "m.image", "three.jpg", TS + 22_000)]);
    rig.tick();
    assert_eq!(rig.tile(0), "three.jpg");
}

#[test]
fn lookups_wait_while_the_pane_is_hidden() {
    let mut rig = booted(&mixed_room());
    assert!(rig.host.call_strings(&rig.app, "on_focus_changed", &[r#"{"foreground": false}"#]));
    rig.queue_event(event_reply(photo("$new:test", "fresh.jpg", TS + 20_000)));
    rig.deliver(&[live("$new:test", "m.image", "fresh.jpg", TS + 20_000)]);
    for _ in 0..3 { rig.tick(); }
    assert!(rig.trace.events.is_empty() && rig.trace.reviews.is_empty(), "no homeserver lookup, and no review, while nobody sees the pane");
    assert!(rig.host.call_strings(&rig.app, "on_focus_changed", &[r#"{"foreground": true}"#]));
    rig.settle();
    rig.tick();
    assert_eq!(rig.trace.events, ["$new:test"]);
    assert_eq!(rig.tile(0), "fresh.jpg");
}

#[test]
fn a_refused_hook_waits_for_refresh() {
    let mut rig = Rig::launch();
    rig.refused_hook = Some(("on_room_message_changed", "too many requests, slow down"));
    rig.queue_read(&mixed_room());
    rig.boot();
    assert_eq!(rig.trace.audit.subscriptions, ["on_room_message"]);
    assert_eq!(rig.text(&["status"]),
        "Tap a tile for details. Live updates are off: Robrix asked the gallery to slow down. Tap Refresh to turn them on.");
    // An unrelated grant does not subscribe again behind the user's back.
    rig.grant(Permission::RobrixNavigation);
    rig.tick();
    assert_eq!(rig.trace.audit.subscriptions, ["on_room_message"]);
    // Refresh does.
    rig.refused_hook = None;
    rig.queue_read(&mixed_room());
    rig.click("Refresh");
    rig.tick();
    assert_eq!(rig.trace.audit.subscriptions, ["on_room_message", "on_room_message_changed"]);
    assert_eq!(rig.text(&["status"]), "Tap a tile for details.");
}

#[test]
fn a_blocked_read_points_to_settings_instead_of_refresh() {
    let mut rig = booted(&mixed_room());
    rig.host.permissions.set_capability(APP, "matrix.room.messages.read", GrantState::Denied);
    rig.host.publish_grants(&rig.app, APP);
    rig.settle();
    let blocked = "Reading this room's messages is turned off for Room Gallery in Mini Apps settings.";
    assert_eq!(rig.text(&["status"]), blocked);
    let (reads, prompts) = (rig.trace.reads.len(), rig.trace.audit.prompts.len());
    rig.click("Refresh");
    rig.tick();
    assert_eq!((rig.trace.reads.len(), rig.trace.audit.prompts.len()), (reads, prompts), "asking again can't undo a blocked ability");
    assert_eq!(rig.text(&["status"]), blocked);
}

#[test]
fn refresh_skips_the_setup_batch_when_everything_is_allowed() {
    let mut rig = booted(&mixed_room());
    assert_eq!(rig.trace.batches.len(), 1);
    rig.queue_read(&mixed_room());
    rig.click("Refresh");
    assert_eq!(rig.trace.reads, [30, 30], "Refresh reads again");
    assert_eq!((rig.trace.batches.len(), rig.trace.silent_batches), (1, 0), "but asks for nothing it already has");
}

// ---- Dates and wording

/// The detail's date as the app should write it after "Shared by …", from an independent implementation.
fn expected_when(ts: u64, now: u64, offset_minutes: Option<i64>, is_24h: bool) -> String {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    const DAYS: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
    fn civil(days: i64) -> (i64, usize, i64) {
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let month = if mp < 10 { mp + 3 } else { mp - 9 };
        (yoe + era * 400 + i64::from(month <= 2), month as usize, doy - (153 * mp + 2) / 5 + 1)
    }
    let plural = |n: i64, one: &str, many: &str| if n == 1 { format!("1 {one}") } else { format!("{n} {many}") };
    let (ts, now) = (ts as i64, now as i64);
    let age = now - ts;
    if age > -300_000 && age < 60_000 { return "just now".into(); }
    if (60_000..3_600_000).contains(&age) { return format!("{} ago", plural(age / 60_000, "min", "mins")); }
    let Some(offset) = offset_minutes else {
        if age > 0 && age < 86_400_000 { return format!("{} ago", plural(age / 3_600_000, "hour", "hours")); }
        if age > 0 && age < 604_800_000 { return format!("{} ago", plural(age / 86_400_000, "day", "days")); }
        let (y, m, d) = civil(ts.div_euclid(86_400_000));
        if y == civil(now.div_euclid(86_400_000)).0 { return format!("on {} {d}", MONTHS[m - 1]); }
        return format!("on {} {d}, {y}", MONTHS[m - 1]);
    };
    let shift = offset * 60_000;
    let day = (ts + shift).div_euclid(86_400_000);
    let today = (now + shift).div_euclid(86_400_000);
    let ms = (ts + shift).rem_euclid(86_400_000);
    let (hour, minute) = (ms / 3_600_000, ms % 3_600_000 / 60_000);
    let clock = if is_24h { format!("{hour:02}:{minute:02}") }
        else { format!("{}:{minute:02}{}", if hour % 12 == 0 { 12 } else { hour % 12 }, if hour >= 12 { "pm" } else { "am" }) };
    let (y, m, d) = civil(day);
    if day == today { format!("today at {clock}") }
    else if day == today - 1 { format!("yesterday at {clock}") }
    else if day < today && day > today - 7 { format!("on {} at {clock}", DAYS[(day + 4).rem_euclid(7) as usize]) }
    else if y == civil(today).0 { format!("on {} {d} at {clock}", MONTHS[m - 1]) }
    else { format!("on {} {d}, {y}", MONTHS[m - 1]) }
}

#[test]
fn dates_are_local_and_relative_once_device_info_answers() {
    let ages: [u64; 7] = [20_000, 330_000, 3 * 3_600_000 + 60_000, 26 * 3_600_000, 4 * 86_400_000 + 7_200_000, 40 * 86_400_000, 400 * 86_400_000];
    let device = |locale: &str, offset: i64| json!({"platform": "macos", "locale": locale, "time_zone": null,
        "utc_offset_minutes": offset, "desktop_view": true, "ui_zoom": 1});
    let cases = [
        (Some(device("en_IN.UTF-8", 330)), Some(330), false),
        (Some(device("de_DE.UTF-8", 120)), Some(120), true),
        (Some(device("fr_CA.UTF-8", -240)), Some(-240), true),
        (None, None, false),
    ];
    for (answer, offset, is_24h) in cases {
        let now = now_ms();
        let rows: Vec<serde_json::Value> = ages.iter().enumerate()
            .map(|(n, age)| photo(&format!("$d{n}:test"), &format!("d{n}.jpg"), now - age)).collect();
        let mut rig = Rig::launch();
        rig.device = answer;
        rig.queue_read(&rows);
        rig.boot();
        for (n, age) in ages.iter().enumerate() {
            rig.open(n as u32);
            assert_eq!(rig.text(&["d_when"]), expected_when(now - age, now_ms(), offset, is_24h), "{offset:?} {age}");
            rig.act("back_to_grid", &[]);
        }
    }
    // Without the device-info permission nothing is asked, and dates fall back the same way.
    let mut rig = Rig::launch();
    rig.host.permissions.set(APP, Permission::DeviceInfo, GrantState::Denied);
    rig.host.publish_grants(&rig.app, APP);
    // An older host's row has no display name either, so it falls back to the user name.
    let mut old = photo("$old:test", "old.jpg", TS);
    old.as_object_mut().unwrap().remove("sender_name");
    rig.queue_read(&[old]);
    rig.boot();
    assert!(!rig.trace.audit.services.iter().any(|service| service == "device.info"));
    rig.open(0);
    assert_eq!([rig.text(&["d_who"]), rig.text(&["d_when"])], ["Shared by alice", "on Jul 3, 2024"], "a date, never a UTC clock time");
}

#[test]
fn read_failures_read_as_sentences() {
    let mut rig = Rig::launch();
    rig.queue_error("matrix.room.messages.read", "room not joined");
    rig.boot();
    assert_eq!(rig.text(&["status"]), "Couldn't read this room: room not joined. Tap Refresh to try again.");
    assert_eq!([rig.text(&["empty_title"]), rig.text(&["empty_line"]), rig.text(&["empty_ask"])],
        ["Couldn't read this room", "Room not joined.", "Try again"], "not a request to allow reading");
    assert!(!rig.is_visible(&["stats_wrap"]) && !rig.is_visible(&["filter_wrap"]), "no counts for a room that wasn't read");
    // The host's own prefix isn't repeated.
    rig.queue_error("matrix.room.messages.read", "couldn't read messages: timed out");
    rig.click("Refresh");
    rig.tick();
    assert_eq!(rig.text(&["status"]), "Couldn't read this room: timed out. Tap Refresh to try again.");
    rig.queue_read(&mixed_room());
    rig.click("Refresh");
    rig.tick();
    assert_eq!(rig.text(&["status"]), "Tap a tile for details.");
    assert!(rig.is_visible(&["stats_wrap"]) && rig.is_visible(&["filter_wrap"]));
}

#[test]
fn counts_and_filters_wait_for_the_first_answer() {
    let mut rig = Rig::launch();
    rig.is_parking_reads = true;
    rig.boot();
    assert_eq!(rig.text(&["status"]), "Looking through recent messages…");
    assert_eq!(rig.text(&["empty_title"]), "Looking through this room…");
    assert!(!rig.is_visible(&["stats_wrap"]) && !rig.is_visible(&["filter_wrap"]), "no 'nothing found' before the room answers");
    let read = rig.parked_reads.pop_front().unwrap();
    rig.reply(read, &json!({"messages": mixed_room(), "unread_count": 0}).to_string());
    rig.settle();
    assert!(rig.is_visible(&["stats_wrap"]) && rig.is_visible(&["filter_wrap"]));
    assert_eq!(rig.text(&["f_all"]), "All 7");
    assert_eq!(rig.active_chip(), ["all"]);
}
