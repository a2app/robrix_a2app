//! Translator stock mini-app: real Splash VM, host bridge and broker, with translation HTTP answered by fixtures.
#![allow(dead_code, unused_imports)] // the shared stock harness serves several test crates
include!("support/stock_harness.rs");

use serde_json::json;

const APP: &str = "translator";
const MYMEMORY: &str = "https://api.mymemory.translated.net/get";

// Language chip positions: 0 is Automatic, then the app's language list in order.
const EN: u32 = 1;
const ZH_CN: u32 = 2;
const ZH_TW: u32 = 3;
const ES: u32 = 6;
const FR: u32 = 7;
const DE: u32 = 8;

/// How the fixture user answers the translator's prompts and reviews.
#[derive(Clone, Copy)]
struct Answers {
    is_batch_allowed: bool,
    is_watch_allowed: bool,
    is_device_allowed: bool,
    is_network_allowed: bool,
    is_sharing_allowed: bool,
    are_actions_allowed: bool,
}

impl Default for Answers {
    fn default() -> Self {
        Self { is_batch_allowed: true, is_watch_allowed: true, is_device_allowed: true,
            is_network_allowed: true, is_sharing_allowed: true, are_actions_allowed: true }
    }
}

/// Everything the translator asked the host for.
#[derive(Default)]
struct Trace {
    audit: StockAudit,
    batches: Vec<Vec<Permission>>,
    reviews: Vec<String>,
    reads: Vec<u32>,
    unsubscribed: Vec<String>,
    inserted: Vec<String>,
    copied: Vec<String>,
}

/// One translator instance on the real broker. HTTP requests are parked so each test answers them.
struct Rig {
    host: Harness,
    app: WidgetRef,
    context: ContextId,
    trace: Trace,
    answers: Answers,
    locale: serde_json::Value,
    http: VecDeque<(Reply, serde_json::Value)>,
}

impl Rig {
    fn launch(is_granted: bool) -> Self {
        Self::launch_in(Harness::new(), is_granted, false)
    }

    fn launch_in(mut host: Harness, is_granted: bool, is_public: bool) -> Self {
        let (app, context) = host.launch_stock_permissions(APP, is_public, is_granted);
        Self { host, app, context, trace: Trace::default(), answers: Answers::default(), locale: "en-US".into(), http: VecDeque::new() }
    }

    /// Closes this instance and opens the stock app again in the same room and storage jail.
    fn relaunch(self) -> Self {
        let Rig { mut host, app, context, answers, locale, .. } = self;
        host.retire_stock(&app, &context);
        let (app, reopened) = host.launch_stock_permissions(APP, false, false);
        assert_eq!(reopened, context);
        Self { host, app, context, trace: Trace::default(), answers, locale, http: VecDeque::new() }
    }

    fn boot(&mut self) {
        self.fire_timers();
        self.resize(460.0, 700.0);
        self.settle();
    }

    fn resize(&mut self, w: f64, h: f64) {
        assert!(self.host.call(&self.app, "on_app_resize", &[w.into(), h.into()]));
    }

    /// Fires pending timers, such as the pause between queued requests. The pause is what refills
    /// the host's request budget, so the fixture refills it too.
    fn tick(&mut self) {
        self.host.broker.forget_app(APP);
        self.fire_timers();
        self.settle();
    }

    fn vm_id(&mut self) -> SplashVmId {
        let source = self.app.borrow::<Splash>().unwrap().view.source.clone();
        self.host.cx.script_ref_vm_id(&source).unwrap()
    }

    /// `Harness::boot` for pending timers, under the wall budget the harness gives guest calls:
    /// a busy machine must not fail a callback. Splash's instruction guard still applies.
    fn fire_timers(&mut self) {
        let vm_id = self.vm_id();
        let timers = std::mem::take(&mut self.host.cx.script_data.timers.timers);
        for timer in timers {
            if timer.repeat { self.host.cx.script_data.timers.timers.push(timer); continue; }
            let hooks = self.host.cx.script_data.timers.dispatch_hooks.clone();
            let is_fired = headless_splash_entry(&mut self.host.cx, vm_id, |cx| hooks.into_iter().any(|hook| hook(cx, &timer, NIL)));
            assert!(is_fired, "the timer belongs to the translator");
        }
        self.host.cx.with_vm_and_async(|_| {});
        self.host.assert_callback_errors("timers");
    }

    /// `Harness::reply`, under the same budget as `fire_timers`.
    fn reply(&mut self, reply: Reply, result: Result<&str, &str>) {
        if let (Ok(data), Some(context)) = (result, self.host.contexts.get(&reply.heap_key)) {
            let mut sources = Vec::new();
            if let Ok(value) = serde_json::from_str(data) { response_room_sources(&value, &mut sources); }
            let influences: Vec<_> = sources.iter().filter_map(|source| match source {
                Source::Room { account, room } => Some(Influence::RoomContent { account: account.clone(), room: room.clone() }),
                _ => None,
            }).collect();
            self.host.flow.borrow_mut().add_sources(context, sources).unwrap();
            self.host.flow.borrow_mut().add_influences(context, influences).unwrap();
        }
        let vm_id = self.vm_id();
        headless_splash_entry(&mut self.host.cx, vm_id, |cx| services::respond(cx, reply, result));
        self.host.assert_callback_errors(&format!("reply {}", reply.req_id));
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

    fn widget(&self, name: &str) -> WidgetRef {
        self.app.widget(&self.host.cx, &[LiveId::from_str(name)])
    }

    /// Runs a named widget's own guest callback, as a click or tap on it would.
    fn invoke(&mut self, name: &str, method: LiveId, args: &[ScriptValue]) {
        self.host.broker.forget_app(APP);
        let widget = self.widget(name);
        self.host.widget_callback(&self.app, &widget, method, args);
        self.settle();
    }

    fn press(&mut self, button: &str) { self.invoke(button, id!(on_click), &[]); }

    /// Taps the row at `index` from the top of the list, through the list's own on_item_tap.
    fn tap_shown(&mut self, index: u32) { self.invoke("msg_list", id!(on_item_tap), &[index.into()]); }

    /// Taps the shown message whose text is `body`.
    fn tap_on(&mut self, body: &str) {
        let index = self.shown_index(body).unwrap_or_else(|| panic!("{body:?} is not shown in {:?}", self.seen("msg_list")));
        self.tap_shown(index);
    }

    /// The list position of the card showing `body`, counted from the top.
    fn shown_index(&self, body: &str) -> Option<u32> {
        let mut cards = Vec::new();
        self.widget("msg_list").children(&mut |_, child| cards.push(child));
        cards.iter().position(|card| Self::texts(card).iter().any(|text| text == body)).map(|index| index as u32)
    }

    /// Chooses a language chip from a picker's menu, opening the menu first like its button does.
    fn pick(&mut self, kind: &str, index: u32) {
        if !self.is_visible(&format!("{kind}_menu_wrap")) { self.press(&format!("{kind}_btn")); }
        assert!(self.is_visible(&format!("{kind}_menu_wrap")));
        self.invoke(&format!("{kind}_menu"), id!(on_item_tap), &[index.into()]);
    }

    fn click(&mut self, text: &str) {
        self.host.broker.forget_app(APP);
        self.host.widget_click(&self.app, text, &[]);
        self.settle();
    }

    /// Taps the chip at `index` in a chip row, through the row's own on_item_tap.
    fn tap_chip(&mut self, row: &str, index: u32) { self.invoke(row, id!(on_item_tap), &[index.into()]); }

    /// The labels of a chip row's filled chips: the chosen ones (unchosen chips are white).
    fn chosen(&mut self, row: &str) -> Vec<String> {
        let vm_id = self.vm_id();
        let mut chips = Vec::new();
        self.widget(row).children(&mut |_, child| chips.push(child));
        chips.into_iter().filter(|chip| self.host.cx.with_script_vm_id(vm_id, |vm| {
            let bg = vm.bx.heap.value(chip.script_source(), id!(draw_bg).into(), NoTrap);
            vm.bx.heap.value(bg.as_object().unwrap(), id!(color).into(), NoTrap).as_color().unwrap() != 0xFFFF_FFFF
        })).map(|chip| Self::texts(&chip).join(" ")).collect()
    }

    fn type_into(&mut self, input: &str, text: &str) {
        self.app.text_input(&self.host.cx, &[LiveId::from_str(input)]).set_text(&mut self.host.cx, text);
    }

    fn input(&self, input: &str) -> String {
        self.app.text_input(&self.host.cx, &[LiveId::from_str(input)]).text()
    }

    fn text(&self, name: &str) -> String { self.widget(name).text() }

    fn is_visible(&self, name: &str) -> bool { self.widget(name).visible() }

    /// The texts a person can see under one widget, in order.
    fn texts(widget: &WidgetRef) -> Vec<String> {
        fn walk(widget: &WidgetRef, out: &mut Vec<String>) {
            if !widget.visible() { return; }
            let text = widget.text();
            if !text.is_empty() && out.last() != Some(&text) { out.push(text); }
            let mut children = Vec::new();
            widget.children(&mut |_, child| children.push(child));
            for child in children { walk(&child, out); }
        }
        let mut out = Vec::new();
        walk(widget, &mut out);
        out
    }

    fn seen(&self, name: &str) -> Vec<String> { Self::texts(&self.widget(name)) }

    fn is_showing(&self, name: &str, text: &str) -> bool {
        self.seen(name).iter().any(|seen| seen == text)
    }

    fn jail(&self) -> PathBuf {
        self.host.flow.borrow().context_storage_path(&self.context).unwrap()
    }

    fn stored(&self, file: &str) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(self.jail().join(file)).unwrap()).unwrap()
    }

    fn queue_messages(&mut self, list: &[serde_json::Value]) {
        self.host.queue_matrix_reply("matrix.room.messages.read", json!({ "messages": list }));
    }

    fn deliver(&mut self, list: &[serde_json::Value]) {
        let batch = serde_json::Value::Array(list.to_vec()).to_string();
        assert!(self.host.call_strings(&self.app, "on_room_message", &[&batch]));
        self.settle();
    }

    fn deny(&mut self, permission: Permission) { self.set_grant(permission, GrantState::Denied); }

    fn set_grant(&mut self, permission: Permission, state: GrantState) {
        self.host.permissions.set(APP, permission, state);
        self.host.publish_grants(&self.app, APP);
        self.settle();
    }

    /// Answers the oldest parked translation request and settles what follows.
    fn answer(&mut self, status: u16, body: &str) -> serde_json::Value {
        let (reply, args) = self.http.pop_front().expect("a parked translation request");
        self.reply(reply, Ok(&json!({ "status": status, "body": body, "headers": {} }).to_string()));
        self.host.cx.with_vm_and_async(|_| {});
        self.settle();
        args
    }

    fn translate(&mut self, text: &str, detected: &str) -> serde_json::Value {
        self.answer(200, &mymemory(text, detected))
    }

    /// Fails the oldest parked translation request with a host error.
    fn fail(&mut self, error: &str) -> serde_json::Value {
        let (reply, args) = self.http.pop_front().expect("a parked translation request");
        self.reply(reply, Err(error));
        self.host.cx.with_vm_and_async(|_| {});
        self.settle();
        args
    }

    fn refuse(&mut self, request: SplashHostRequest, error: &str) {
        self.reply(Reply { heap_key: request.heap_key, req_id: request.req_id }, Err(error));
        self.host.cx.with_vm_and_async(|_| {});
        self.host.assert_callback_errors(APP);
    }

    /// Serves every host request like `settle_stock`, but parks HTTP and answers per `self.answers`.
    fn settle(&mut self) {
        let mut pending = VecDeque::from(self.host.process());
        let mut idle = 0;
        for _ in 0..200 {
            while let Some(ask) = pending.pop_front() {
                match ask {
                    BrokerAsk::PermissionBatch { app_id, request, perms } => {
                        assert_eq!(app_id, APP);
                        self.trace.batches.push(perms.clone());
                        let answers = self.answers;
                        let selected: Vec<Permission> = perms.iter().copied().filter(|perm| answers.is_batch_allowed
                            && (answers.is_watch_allowed || *perm != Permission::MatrixRoomWatch)
                            && (answers.is_device_allowed || *perm != Permission::DeviceInfo)).collect();
                        self.host.answer_stock_batch(&self.app, APP, request, &perms, &selected, &mut self.trace.audit);
                    }
                    BrokerAsk::Prompt { app_id, perm, request: Some(request), .. } => {
                        assert_eq!(app_id, APP);
                        self.trace.audit.prompts.push(perm);
                        self.trace.audit.concrete_prompts += 1;
                        let is_allowed = if perm == Permission::Network { self.answers.is_network_allowed } else { self.answers.are_actions_allowed };
                        if !is_allowed {
                            let cap = a2app_core::capabilities::for_service(&request.service).unwrap();
                            self.refuse(request, &format!("The request to use \"{}\" was not approved.", cap.title));
                            continue;
                        }
                        if perm == Permission::Network {
                            let args: serde_json::Value = serde_json::from_str(&request.args_json).unwrap();
                            let site = url::Url::parse(args["url"].as_str().unwrap()).unwrap().host_str().unwrap().to_string();
                            self.host.permissions.allow_network(APP, NetworkScope::Host(site), RoomScope::AllRooms,
                                GrantDuration::RobrixSession, None).unwrap();
                        } else {
                            self.host.grant_stock_request(APP, perm, &request);
                        }
                        self.host.publish_grants(&self.app, APP);
                        pending.extend(self.host.dispatch(Some(request)));
                    }
                    BrokerAsk::FlowReview { request, review } => {
                        assert!(!review.allowed);
                        self.trace.reviews.push(request.service.clone());
                        self.trace.audit.reviews += 1;
                        if !self.answers.is_sharing_allowed {
                            self.refuse(request, "This request was not approved.");
                            continue;
                        }
                        if request.service == "clipboard.write" {
                            // Fixture the native clipboard: a test must never overwrite the developer's clipboard.
                            let args: serde_json::Value = serde_json::from_str(&request.args_json).unwrap();
                            assert_eq!(review.recipient, Some(Recipient::Clipboard));
                            assert!(review.payload.contains(args["text"].as_str().unwrap()));
                            self.host.flow.borrow_mut().approve_effect_once(&review).unwrap();
                            self.host.flow.borrow_mut().commit_effect_for_activation(&self.context, review.epoch,
                                review.recipient.as_ref(), review.action.as_ref(), &args).unwrap();
                            self.trace.copied.push(args["text"].as_str().unwrap().into());
                            self.reply(Reply { heap_key: request.heap_key, req_id: request.req_id }, Ok("{}"));
                            continue;
                        }
                        self.host.flow.borrow_mut().approve_effect_session(&review, SharingDuration::RobrixSession).unwrap();
                        pending.extend(self.host.dispatch(Some(request)));
                    }
                    BrokerAsk::Matrix { reply, capability, call, .. } => {
                        self.trace.audit.services.push(capability.into());
                        if let services::MatrixServiceCall::ReadMessages { limit } = &call { self.trace.reads.push(*limit); }
                        let data = self.host.matrix_replies.get_mut(capability).and_then(VecDeque::pop_front)
                            .unwrap_or_else(|| matrix_fixture(&call));
                        self.reply(reply, Ok(&data.to_string()));
                    }
                    BrokerAsk::Network { reply, args, .. } => {
                        self.trace.audit.services.push("network.http".into());
                        self.http.push_back((reply, args));
                    }
                    BrokerAsk::Subscribe { reply, hook, .. } => {
                        self.trace.audit.subscriptions.push(hook.into());
                        self.reply(reply, Ok("{\"subscribed\":true}"));
                    }
                    BrokerAsk::Unsubscribe { reply, hook, .. } => {
                        self.trace.unsubscribed.push(hook.unwrap_or("*").into());
                        self.reply(reply, Ok("{}"));
                    }
                    BrokerAsk::HostQuery { reply, query: services::HostQuery::DeviceInfo } => {
                        self.trace.audit.services.push("device.info".into());
                        let info = json!({"platform":"test","locale":self.locale,"time_zone":"UTC",
                            "utc_offset_minutes":0,"desktop_view":true,"ui_zoom":1}).to_string();
                        self.reply(reply, Ok(&info));
                    }
                    BrokerAsk::HostAction { reply, action: HostAction::ComposerInsert { room, text }, .. } => {
                        assert_eq!(room.as_deref(), Some(ROOM));
                        self.trace.inserted.push(text);
                        self.reply(reply, Ok("{\"status\":\"drafted\"}"));
                    }
                    BrokerAsk::Used { .. } => {}
                    BrokerAsk::Prompt { .. } => panic!("translator prompts are always tied to a request"),
                    BrokerAsk::Restrict { reason, .. } => panic!("translator was restricted: {reason}"),
                    _ => panic!("unexpected translator host request"),
                }
                self.host.cx.with_vm_and_async(|_| {});
                self.host.assert_callback_errors(APP);
            }
            pending.extend(self.host.process());
            if pending.is_empty() {
                idle += 1;
                if idle < 2 || self.host.stock_callbacks_paused() { continue; }
                self.host.assert_callback_errors(APP);
                return;
            }
            idle = 0;
        }
        panic!("translator did not settle");
    }
}

/// A history row as Robrix sends it: `sender` is the short name and `sender_name` the display name.
fn message(event: &str, sender: &str, body: &str) -> serde_json::Value {
    json!({"event_id": event, "room_id": ROOM, "sender": sender.to_lowercase(), "sender_id": format!("@{}:test", sender.to_lowercase()),
        "sender_name": sender, "body": body, "ts": 1720000000000_u64, "msgtype": "m.text"})
}

/// A live row as Robrix sends it: `sender` is the short name and `sender_name` the display name.
fn live(event: &str, sender: &str, body: &str, is_own: bool) -> serde_json::Value {
    json!({"room_id": ROOM, "event_id": event, "sender": sender.to_lowercase(), "sender_id": format!("@{}:test", sender.to_lowercase()),
        "sender_name": sender, "body": body, "ts": 1720000000000_u64, "msgtype": "m.text", "is_own": is_own})
}

/// JSON the way MyMemory's PHP backend writes it: every slash and every non-ASCII character escaped.
fn php_json(value: &serde_json::Value) -> String {
    let mut out = String::new();
    for c in value.to_string().chars() {
        match c {
            '/' => out.push_str("\\/"),
            c if c.is_ascii() => out.push(c),
            c => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) { out.push_str(&format!("\\u{unit:04x}")); }
            }
        }
    }
    out
}

/// A successful MyMemory reply, in its real shape and escaping.
fn mymemory(text: &str, detected: &str) -> String {
    php_json(&json!({"responseData": {"translatedText": text, "match": 0.85, "detectedLanguage": detected},
        "quotaFinished": false, "mtLangSupported": null, "responseDetails": "", "responseStatus": 200, "responderId": null,
        "exception_code": null, "matches": [{"id": "0", "segment": "", "translation": text, "source": "es-ES", "target": "en-GB",
            "quality": "74", "reference": null, "usage-count": 2, "subject": "All", "created-by": "MateCat",
            "last-updated-by": "MateCat", "create-date": "2021/07/05 13:08:23", "last-update-date": "2021/07/05 13:08:23", "match": 0.85}]}))
}

/// MyMemory's answer when the text is already in the target language.
fn mymemory_same() -> String {
    php_json(&json!({"responseData": {"translatedText": "PLEASE SELECT TWO DISTINCT LANGUAGES", "match": 0},
        "quotaFinished": false, "mtLangSupported": null, "responseDetails": "PLEASE SELECT TWO DISTINCT LANGUAGES",
        "responseStatus": "403", "responderId": null, "exception_code": null, "matches": ""}))
}

/// The decoded `q` and `langpair` of a MyMemory request, after checking its exact URL shape.
fn mymemory_query(args: &serde_json::Value) -> (String, String) {
    let url = args["url"].as_str().unwrap();
    assert!(url.starts_with(&format!("{MYMEMORY}?q=")), "{url}");
    assert!(args.get("method").is_none(), "MyMemory requests are plain GETs");
    assert!(args.get("body").is_none() && args.get("headers").is_none());
    let parsed = url::Url::parse(url).unwrap();
    let pairs: Vec<(String, String)> = parsed.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
    assert_eq!(pairs.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(), ["q", "langpair"], "{url}");
    (pairs[0].1.clone(), pairs[1].1.clone())
}

fn spanish_room() -> Vec<serde_json::Value> {
    vec![
        message("$m1:test", "Ana", "Hola amigos"),
        message("$m2:test", "Luis", "¿Vamos al cine mañana?"),
        message("$m3:test", "Ana", "Sí, a las ocho."),
    ]
}

/// Boots with three Spanish messages in the room, approving each prompt as it comes.
fn booted() -> Rig {
    let mut rig = Rig::launch(false);
    rig.queue_messages(&spanish_room());
    rig.boot();
    rig
}

/// Translates the shown message `body`, approving the network prompt and sharing review the first time.
fn translate_row(rig: &mut Rig, body: &str, text: &str, detected: &str) -> serde_json::Value {
    rig.tap_on(body);
    assert_eq!(rig.http.len(), 1, "one request for the tapped message");
    rig.translate(text, detected)
}

const PRIVACY: &str = "🔒 Text is sent to api.mymemory.translated.net only when you translate it.";

// ---- Startup

#[test]
fn boot_reads_recent_messages_after_one_grouped_setup_and_sends_nothing() {
    let mut rig = Rig::launch(false);
    rig.queue_messages(&spanish_room());
    rig.boot();
    assert_eq!(rig.trace.batches, [vec![Permission::MatrixRoomRead, Permission::DeviceInfo]], "one grouped setup at startup");
    assert_eq!(rig.trace.audit.prompts, [Permission::MatrixRoomRead], "device facts never prompt outside strict mode");
    assert_eq!(rig.trace.reads, [30]);
    assert_eq!(rig.trace.audit.services, ["device.info", "matrix.room.messages.read"]);
    assert!(rig.http.is_empty(), "opening the app must not send room text anywhere");
    assert!(rig.trace.audit.subscriptions.is_empty(), "live translation is off until turned on");
    assert_eq!(rig.seen("msg_list"), ["A", "Ana", "Jul 3, 2024", "Sí, a las ocho.", "L", "Luis", "Jul 3, 2024", "¿Vamos al cine mañana?",
        "A", "Ana", "Jul 3, 2024", "Hola amigos"], "the newest message comes first, each beside its sender's avatar");
    assert!(rig.is_visible("list_status_wrap"));
    assert_eq!(rig.text("list_status"), "3 recent messages, newest first · tap one to translate it");
    assert!(rig.is_visible("all_wrap"));
    assert_eq!(rig.text("privacy_note"), PRIVACY);
    let everything = rig.seen("col");
    assert!(!everything.iter().any(|text| text.contains("Choose websites")), "Robrix's prompt has no such control");
    assert_eq!(rig.text("target_btn"), "English ▼");
    assert_eq!(rig.text("target_hint"), "Automatic · your device language");
    assert_eq!(rig.text("reply_btn"), "English ▼");
    assert_eq!(rig.text("reply_hint"), "Automatic · until this room's language is detected");
    assert_eq!(rig.text("live_btn"), "○ Translate new messages");
    assert!(!rig.is_visible("service_body"), "settings stay folded until asked for");
    assert_eq!(rig.seen("service_body"), Vec::<String>::new());
    assert!(!rig.is_visible("reply_status_wrap") && !rig.is_visible("service_status_wrap"), "empty notes take no room");
    assert!(!everything.iter().any(|text| text == "Translator"), "the pane header names the app");
}

#[test]
fn media_and_empty_messages_are_left_out_of_the_list() {
    let mut rig = Rig::launch(false);
    rig.queue_messages(&[
        message("$m1:test", "Ana", "Hola"),
        json!({"event_id": "$img:test", "sender": "Ana", "body": "photo.jpg", "msgtype": "m.image", "ts": 1}),
        json!({"event_id": "$blank:test", "sender": "Ana", "body": "   ", "msgtype": "m.text"}),
        json!({"event_id": "$note:test", "sender": "bot", "body": "Despliegue listo", "msgtype": "m.notice"}),
        json!({"sender": "Ana", "body": "no event id"}),
    ]);
    rig.boot();
    let shown = rig.seen("msg_list");
    assert!(shown.iter().any(|seen| seen == "Hola") && shown.iter().any(|seen| seen == "Despliegue listo"), "{shown:?}");
    assert!(!shown.iter().any(|seen| seen == "photo.jpg" || seen == "no event id"), "{shown:?}");
    assert_eq!(rig.text("list_status"), "2 recent messages, newest first · tap one to translate it");
}

#[test]
fn device_locale_picks_the_default_target_and_unknown_locales_fall_back_to_english() {
    for (locale, expected) in [
        (json!("es-MX"), "Español ▼"), (json!("zh-Hant-TW"), "繁體中文 ▼"), (json!("zh_CN"), "简体中文 ▼"),
        (json!("PT-br"), "Português ▼"), (json!("nb-NO"), "English ▼"), (json!(null), "English ▼"),
    ] {
        let mut rig = Rig::launch(false);
        rig.locale = locale.clone();
        rig.queue_messages(&spanish_room());
        rig.boot();
        assert_eq!(rig.text("target_btn"), expected, "{locale}");
    }
}

#[test]
fn strict_mode_asks_for_device_facts_only_inside_the_one_startup_batch() {
    let mut rig = Rig::launch(false);
    rig.host.permissions.set_strict(true);
    rig.host.publish_grants(&rig.app, APP);
    rig.locale = json!("de-DE");
    rig.queue_messages(&spanish_room());
    rig.boot();
    assert_eq!(rig.trace.audit.batch_prompts, 1);
    assert_eq!(rig.trace.audit.prompts, [Permission::MatrixRoomRead, Permission::DeviceInfo]);
    assert_eq!(rig.text("target_btn"), "Deutsch ▼");
    // Declining device facts keeps English, and allowing them later needs no second prompt.
    let mut rig = Rig::launch(false);
    rig.host.permissions.set_strict(true);
    rig.host.publish_grants(&rig.app, APP);
    rig.locale = json!("de-DE");
    rig.answers.is_device_allowed = false;
    rig.queue_messages(&spanish_room());
    rig.boot();
    assert!(!rig.trace.audit.services.contains(&"device.info".to_string()), "declined device facts are not read");
    assert_eq!(rig.text("target_btn"), "English ▼");
    assert!(rig.is_showing("msg_list", "Hola amigos"));
    rig.set_grant(Permission::DeviceInfo, GrantState::Granted);
    assert_eq!(rig.text("target_btn"), "Deutsch ▼");
    assert_eq!(rig.trace.audit.batch_prompts, 1);
}

#[test]
fn without_a_room_the_app_skips_room_setup_and_still_translates_typed_text() {
    let mut rig = Rig::launch_in(Harness::new(), false, true);
    rig.boot();
    assert!(rig.trace.batches.is_empty(), "no room, no room permissions");
    assert!(rig.seen("msg_list").is_empty());
    assert_eq!(rig.seen("empty_wrap"), ["🧭", "No room attached", "Open Translator from a room to read its messages. You can still translate your own reply below."]);
    assert!(!rig.is_visible("list_status_wrap") && !rig.is_visible("all_wrap"), "the empty list explains itself once");
    assert!(!rig.is_visible("live_wrap"), "without a room there is nothing to watch");
    rig.act("set_live", &[true.into()]);
    assert_eq!(rig.text("live_note"), "Open Translator from a room to translate its new messages.");
    assert_eq!(rig.text("live_btn"), "○ Translate new messages");
    rig.click("Refresh");
    assert!(rig.trace.batches.is_empty());
    rig.tap_shown(0);
    assert!(rig.http.is_empty(), "the empty-state line is not a message");
    rig.pick("reply", ES);
    rig.type_into("reply_input", "Good morning");
    rig.act("translate_reply", &[]);
    let args = rig.translate("Buenos días", "en");
    assert_eq!(mymemory_query(&args), ("Good morning".into(), "autodetect|es".into()));
    assert!(rig.trace.reviews.is_empty(), "typed text in a room-less instance carries no room data");
    assert_eq!(rig.text("result_text"), "Buenos días");
    rig.act("insert_reply", &[]);
    assert!(rig.trace.inserted.is_empty());
    assert_eq!(rig.text("reply_status"), "Open Translator from a room to use its message box. Copy still works.");
}

// ---- Translating messages

#[test]
fn tapping_a_message_translates_it_with_mymemory() {
    let mut rig = booted();
    rig.tap_on("Hola amigos");
    assert_eq!(rig.trace.audit.prompts, [Permission::MatrixRoomRead, Permission::Network]);
    assert_eq!(rig.trace.reviews, ["network.http"], "room text needs a sharing review for the new site");
    assert_eq!(rig.http.len(), 1);
    assert!(rig.is_showing("msg_list", "Translating…"));
    let args = rig.translate("Hello friends", "es");
    assert_eq!(args["url"], "https://api.mymemory.translated.net/get?q=Hola%20amigos&langpair=autodetect%7Cen");
    assert_eq!(mymemory_query(&args), ("Hola amigos".into(), "autodetect|en".into()));
    assert!(rig.is_showing("msg_list", "Hello friends"));
    assert!(rig.is_showing("msg_list", "from Spanish"));
    assert_eq!(rig.text("privacy_note"), PRIVACY, "the note naming the site stays, so nothing moves under your finger");
    assert!(rig.http.is_empty());
    // The approved site carries the next message without another permission prompt.
    let args = translate_row(&mut rig, "¿Vamos al cine mañana?", "Shall we go to the cinema tomorrow?", "es");
    assert_eq!(mymemory_query(&args), ("¿Vamos al cine mañana?".into(), "autodetect|en".into()));
    assert_eq!(args["url"], "https://api.mymemory.translated.net/get?q=%C2%BFVamos%20al%20cine%20ma%C3%B1ana%3F&langpair=autodetect%7Cen");
    assert_eq!(rig.trace.audit.prompts.len(), 2);
    assert!(rig.is_showing("msg_list", "Shall we go to the cinema tomorrow?"));
    assert!(rig.is_showing("msg_list", "Hello friends"), "earlier translations stay");
}

#[test]
fn the_list_shows_the_newest_message_first_and_a_tap_reaches_that_message() {
    let mut rig = booted();
    assert_eq!(rig.shown_index("Sí, a las ocho."), Some(0));
    assert_eq!(rig.shown_index("Hola amigos"), Some(2));
    rig.tap_shown(0);
    assert_eq!(mymemory_query(&rig.translate("Yes, at eight.", "es")).0, "Sí, a las ocho.");
    // A new live message goes on top, and the top row is what a tap there translates.
    rig.act("set_live", &[true.into()]);
    rig.deliver(&[live("$own:test", "Owner", "Me voy", true)]);
    assert_eq!(rig.shown_index("Me voy"), Some(0));
    rig.tap_shown(0);
    assert_eq!(mymemory_query(&rig.translate("I'm leaving", "es")).0, "Me voy");
    rig.tap_shown(9);
    assert!(rig.http.is_empty(), "a tap past the last row does nothing");
}

#[test]
fn langpairs_follow_each_direction_and_service_codes() {
    let mut rig = booted();
    rig.pick("target", ZH_TW);
    assert_eq!(rig.text("target_btn"), "繁體中文 ▼");
    assert!(!rig.is_visible("target_hint_wrap"), "a chosen language needs no hint");
    let args = translate_row(&mut rig, "Hola amigos", "朋友們好", "es");
    assert_eq!(mymemory_query(&args).1, "autodetect|zh-TW");
    rig.pick("target", ZH_CN);
    let args = translate_row(&mut rig, "Hola amigos", "朋友们好", "es");
    assert_eq!(mymemory_query(&args).1, "autodetect|zh-CN");
    assert!(rig.is_showing("msg_list", "朋友们好"), "escaped CJK comes through: {:?}", rig.seen("msg_list"));
    rig.pick("reply", DE);
    rig.type_into("reply_input", "See you soon");
    rig.act("translate_reply", &[]);
    let args = rig.translate("Bis bald", "en");
    assert_eq!(mymemory_query(&args), ("See you soon".into(), "autodetect|de".into()));
    assert_eq!(rig.text("result_text"), "Bis bald");
    assert_eq!(rig.text("result_note"), "German · from English");
}

#[test]
fn same_language_messages_are_marked_and_later_skipped_without_a_request() {
    let mut rig = booted();
    let args = translate_row(&mut rig, "Hola amigos", "Hello friends", "es");
    assert_eq!(mymemory_query(&args).1, "autodetect|en");
    // An English message needs nothing: it is marked, not shown twice.
    rig.queue_messages(&[message("$m1:test", "Ana", "Hola amigos"), message("$en:test", "Bo", "See you there")]);
    rig.click("Refresh");
    rig.tap_on("See you there");
    rig.answer(200, &mymemory_same());
    assert!(rig.is_showing("msg_list", "Already in English"));
    assert!(!rig.seen("msg_list").iter().any(|seen| seen == "from English" || seen.contains("DISTINCT")));
    // Spanish is now known for $m1, so choosing Spanish marks it without sending it again.
    rig.pick("target", ES);
    rig.tap_on("Hola amigos");
    assert!(rig.http.is_empty(), "a known source language is skipped without a request");
    assert!(rig.is_showing("msg_list", "Already in Spanish"));
    // A long message stops after its first part when that part is already in the target language.
    let long = "Esta es una frase bastante larga para el mensaje de prueba. ".repeat(12);
    rig.queue_messages(&[message("$long:test", "Ana", long.trim())]);
    rig.click("Refresh");
    rig.tap_on(long.trim());
    rig.answer(200, &mymemory_same());
    rig.tick();
    assert!(rig.http.is_empty(), "no further parts once the language matches");
    assert!(rig.is_showing("msg_list", "Already in Spanish"));
}

#[test]
fn long_messages_are_split_into_ordered_parts_under_the_byte_limit() {
    let latin = (1..=24).map(|n| format!("Esta es la frase número {n} del mensaje largo.")).collect::<Vec<_>>().join(" ");
    let japanese = "これは長い日本語の文章の一部です。".repeat(20);
    let unbroken = "漢".repeat(400);
    assert!(latin.len() > 900 && japanese.len() > 900 && unbroken.len() == 1200);
    for (body, parts, ends) in [(latin.as_str(), 3, Some(".")), (japanese.as_str(), 3, Some("。")), (unbroken.as_str(), 3, None)] {
        let mut rig = Rig::launch(false);
        rig.queue_messages(&[message("$long:test", "Ana", body)]);
        rig.boot();
        rig.tap_shown(0);
        let mut sent = Vec::new();
        for part in 0..parts {
            assert_eq!(rig.http.len(), 1, "exactly one part in flight");
            let args = rig.translate(&format!("part {part}"), "es");
            let (q, pair) = mymemory_query(&args);
            assert_eq!(pair, "autodetect|en");
            assert!(q.len() <= 450, "{} bytes", q.len());
            if let Some(end) = ends { assert!(q.ends_with(end), "parts end on a sentence: {q:?}"); }
            sent.push(q);
            assert!(rig.http.is_empty(), "the next part waits for the pause between requests");
            rig.tick();
        }
        assert!(rig.http.is_empty(), "{} parts in all", parts);
        let rejoined: String = sent.concat();
        assert_eq!(rejoined.replace(' ', ""), body.replace(' ', ""), "parts cover the whole message in order");
        assert!(rig.is_showing("msg_list", "part 0 part 1 part 2"), "{:?}", rig.seen("msg_list"));
    }
    // CJK targets are joined without spaces.
    let mut rig = Rig::launch(false);
    rig.queue_messages(&[message("$long:test", "Ana", &latin)]);
    rig.boot();
    rig.pick("target", ZH_CN);
    rig.tap_shown(0);
    for part in ["第一", "第二", "第三"] {
        rig.translate(part, "es");
        rig.tick();
    }
    assert!(rig.is_showing("msg_list", "第一第二第三"));
}

#[test]
fn html_entities_in_translations_are_unescaped() {
    let mut rig = booted();
    rig.tap_on("Hola amigos");
    rig.translate("It&#39;s &quot;fine&quot; &amp; &lt;ok&gt; &#233;t&#xE9; &#x4E2D; &hellip; &amp;amp; &bogus; &#1114112;", "es");
    assert!(rig.is_showing("msg_list", "It's \"fine\" & <ok> été 中 … &amp; &bogus; &#1114112;"), "{:?}", rig.seen("msg_list"));
}

/// MyMemory escapes every "/" as "\/", which the VM's own JSON reader does not understand.
#[test]
fn escaped_slashes_and_unicode_in_real_mymemory_replies_are_read_correctly() {
    let mut rig = Rig::launch(false);
    rig.queue_messages(&[
        message("$link:test", "Ana", "Mira esto: https://robrix.org/demo y dime sí/no"),
        message("$end:test", "Luis", "La demo está en https://robrix.org/demo"),
        message("$plain:test", "Ana", "¿Vamos al cine mañana?"),
        message("$slash:test", "Luis", "Usa C:\\temp\\/x 🎉"),
    ]);
    rig.boot();
    // Verbatim bodies as api.mymemory.translated.net sends them.
    rig.tap_on("Mira esto: https://robrix.org/demo y dime sí/no");
    rig.answer(200, r#"{"responseData":{"translatedText":"Check this out: https:\/\/robrix.org\/demo and tell me yes\/no","match":0.85,"detectedLanguage":"es"},"quotaFinished":false,"mtLangSupported":null,"responseDetails":"","responseStatus":200,"responderId":null,"exception_code":null,"matches":[{"id":"749174531","segment":"Mira esto: https:\/\/robrix.org\/demo y dime s\u00ed\/no","translation":"Check this out: https:\/\/robrix.org\/demo and tell me yes\/no","source":"es-ES","target":"en-GB","quality":"74","reference":null,"usage-count":2,"subject":"All","created-by":"MateCat","last-updated-by":"MateCat","create-date":"2024-07-03 10:00:00","last-update-date":"2024-07-03 10:00:00","match":0.85}]}"#);
    assert!(rig.is_showing("msg_list", "Check this out: https://robrix.org/demo and tell me yes/no"), "{:?}", rig.seen("msg_list"));
    assert!(rig.is_showing("msg_list", "from Spanish"));
    // A message ending in a link no longer breaks Translate all.
    rig.press("translate_all_btn");
    let mut order = Vec::new();
    for _ in 0..3 {
        let (_, args) = rig.http.front().expect("the batch keeps going").clone();
        let q = mymemory_query(&args).0;
        match q.as_str() {
            "Usa C:\\temp\\/x 🎉" => rig.answer(200, r#"{"responseData":{"translatedText":"Use C:\\temp\\\/x \ud83c\udf89","detectedLanguage":"es"},"quotaFinished":false,"responseDetails":"","responseStatus":200}"#),
            "¿Vamos al cine mañana?" => rig.answer(200, r#"{"responseData":{"translatedText":"Shall we go to the cinema tomorrow?","detectedLanguage":"es"},"quotaFinished":false,"responseDetails":"","responseStatus":200}"#),
            _ => rig.answer(200, r#"{"responseData":{"translatedText":"The demo is at https:\/\/robrix.org\/demo","match":0.99,"detectedLanguage":"es"},"quotaFinished":false,"mtLangSupported":null,"responseDetails":"","responseStatus":200,"responderId":null,"exception_code":null,"matches":[]}"#),
        };
        rig.tick();
        order.push(q);
    }
    assert_eq!(order, ["Usa C:\\temp\\/x 🎉", "¿Vamos al cine mañana?", "La demo está en https://robrix.org/demo"]);
    assert!(rig.http.is_empty());
    assert_eq!(rig.text("list_status"), "Translated 3 messages into English.");
    for text in ["The demo is at https://robrix.org/demo", "Shall we go to the cinema tomorrow?", "Use C:\\temp\\/x 🎉"] {
        assert!(rig.is_showing("msg_list", text), "{text} in {:?}", rig.seen("msg_list"));
    }
    let saved = rig.stored("cache.json");
    assert!(saved.as_array().unwrap().iter().any(|entry| entry["text"] == "The demo is at https://robrix.org/demo"), "the cache keeps the real text");
    // Emoji arrive as UTF-16 pairs, and accents as \u escapes (captured from the live API).
    rig.queue_messages(&[message("$emoji:test", "Ana", "Hola amigos 🎉 café")]);
    rig.click("Refresh");
    rig.tap_on("Hola amigos 🎉 café");
    rig.answer(200, r#"{"responseData":{"translatedText":"Hello \ud83c\udf89 coffee friends","match":0.85,"detectedLanguage":"es"},"quotaFinished":false,"mtLangSupported":null,"responseDetails":"","responseStatus":200,"responderId":null,"exception_code":null,"matches":[{"id":0,"segment":"Hola amigos \ud83c\udf89 caf\u00e9","translation":"Hello \ud83c\udf89 coffee friends","source":false,"target":"en-GB","quality":70,"reference":"Machine Translation.","usage-count":2,"subject":false,"created-by":"MT!","last-updated-by":"MT!","create-date":"2026-10-13 07:50:00","last-update-date":"2026-10-13 07:50:00","match":0.85,"penalty":null,"model":"neural"}]}"#);
    assert!(rig.is_showing("msg_list", "Hello 🎉 coffee friends"), "{:?}", rig.seen("msg_list"));
    rig.type_into("reply_input", "Great 👍");
    rig.act("translate_reply", &[]);
    rig.answer(200, r#"{"responseData":{"translatedText":"Genial \ud83d\udc4d \u00a1y \\\ud83d\ude02\/ \ud83c fin!","detectedLanguage":"en"},"responseStatus":200}"#);
    assert_eq!(rig.text("result_text"), "Genial 👍 ¡y \\😂/  fin!", "an escaped backslash stays, a lone surrogate is dropped");
    // A reply without responseStatus is unreadable, not "error ".
    rig.queue_messages(&spanish_room());
    rig.click("Refresh");
    rig.tap_on("Hola amigos");
    rig.answer(200, r#"{"responseData":{"translatedText":"Hello friends"}}"#);
    assert!(rig.is_showing("msg_list", "MyMemory sent a reply Translator couldn't read. Tap to try again."), "{:?}", rig.seen("msg_list"));
}

/// MyMemory echoes short texts with a random language, such as "ok" as Danish.
#[test]
fn unchanged_short_messages_need_no_translation_and_name_no_language() {
    let mut rig = Rig::launch(false);
    rig.queue_messages(&[message("$ok:test", "Ana", "ok"), message("$lol:test", "Luis", "lol 😂"),
        message("$num:test", "Ana", "12345"), message("$es:test", "Luis", "¿Vamos?")]);
    rig.boot();
    rig.press("translate_all_btn");
    for (text, detected) in [("Shall we?", "es"), ("12345", "lv"), ("LOL 😂", "fr"), ("ok", "da")] {
        rig.translate(text, detected);
        rig.tick();
    }
    let shown = rig.seen("msg_list");
    assert_eq!(shown.iter().filter(|seen| *seen == "No translation needed").count(), 3, "{shown:?}");
    for wrong in ["from Danish", "from French", "from Latvian", "LOL 😂"] {
        assert!(!shown.iter().any(|seen| seen == wrong), "{wrong} in {shown:?}");
    }
    assert!(rig.is_showing("msg_list", "Shall we?"));
    assert_eq!(rig.stored("detected.json"), json!([{"id": "es", "n": 1}]), "echoes say nothing about the room's language");
    assert_eq!(rig.text("reply_btn"), "Español ▼");
    // Tapping an unchanged message neither hides it nor asks again.
    rig.tap_on("ok");
    assert!(rig.http.is_empty());
    assert!(rig.is_showing("msg_list", "No translation needed"));
    // The same holds for your reply.
    rig.type_into("reply_input", "OK");
    rig.act("translate_reply", &[]);
    rig.translate("Ok", "it");
    assert_eq!(rig.text("result_text"), "OK");
    assert_eq!(rig.text("result_note"), "No translation needed");
}

#[test]
fn translate_all_keeps_one_request_in_flight_and_stops_at_twelve() {
    let mut rig = Rig::launch(false);
    let list: Vec<_> = (1..=15).map(|n| message(&format!("$m{n}:test"), "Ana", &format!("Mensaje {n}"))).collect();
    rig.queue_messages(&list);
    rig.boot();
    rig.act("translate_all", &[]);
    assert_eq!(rig.text("translate_all_btn"), "Stop");
    let mut sent = Vec::new();
    for n in 1..=12 {
        assert_eq!(rig.http.len(), 1, "one request in flight");
        assert_eq!(rig.text("list_status"), format!("Translating {n} of 12…"));
        let (_, args) = rig.http.front().unwrap().clone();
        let q = mymemory_query(&args).0;
        rig.translate(&q.replace("Mensaje", "Message"), "es");
        sent.push(q);
        assert!(rig.http.is_empty(), "the next message waits for the pause");
        rig.tick();
        if n < 12 {
            assert_eq!(rig.seen("msg_list").iter().filter(|seen| *seen == "Translating…").count(), 1, "the message sent after the pause says so");
        }
    }
    assert_eq!(sent, (4..=15).rev().map(|n| format!("Mensaje {n}")).collect::<Vec<_>>(), "the 12 newest, newest first");
    assert!(rig.http.is_empty(), "a click translates at most 12");
    assert_eq!(rig.text("list_status"), "Translated 12 messages into English.");
    assert_eq!(rig.text("translate_all_btn"), "Translate all");
    assert!(rig.is_showing("msg_list", "Message 15"));
    // The next click takes the rest and skips what is already translated.
    rig.act("translate_all", &[]);
    for n in [3, 2, 1] {
        let args = rig.translate(&format!("Message {n}"), "es");
        assert_eq!(mymemory_query(&args).0, format!("Mensaje {n}"));
        rig.tick();
    }
    assert!(rig.http.is_empty());
    rig.act("translate_all", &[]);
    assert!(rig.http.is_empty());
    assert_eq!(rig.text("list_status"), "Every message shown is already translated into English.");
}

/// Other tests reset the host's request budget at every step; this one lets real time pass instead.
#[test]
fn the_pause_between_requests_keeps_a_batch_within_the_host_request_budget() {
    let mut rig = Rig::launch(false);
    let list: Vec<_> = (1..=5).map(|n| message(&format!("$m{n}:test"), "Ana", &format!("Mensaje {n}"))).collect();
    rig.queue_messages(&list);
    rig.boot();
    rig.press("translate_all_btn");
    for n in (1..=5).rev() {
        assert_eq!(rig.http.len(), 1, "Mensaje {n} was sent, not refused for going too fast");
        rig.translate(&format!("Message {n}"), "es");
        std::thread::sleep(std::time::Duration::from_millis(1450));
        rig.fire_timers();
        rig.settle();
    }
    assert!(rig.http.is_empty());
    assert_eq!(rig.text("list_status"), "Translated 5 messages into English.");
    let failures: Vec<_> = rig.host.broker.failures().iter().map(|failure| failure.error.clone()).collect();
    assert!(failures.is_empty(), "{failures:?}");
}

#[test]
fn stopping_translate_all_drops_the_queued_messages() {
    let mut rig = Rig::launch(false);
    let list: Vec<_> = (1..=5).map(|n| message(&format!("$m{n}:test"), "Ana", &format!("Mensaje {n}"))).collect();
    rig.queue_messages(&list);
    rig.boot();
    rig.press("translate_all_btn");
    rig.press("translate_all_btn");
    assert_eq!(rig.text("list_status"), "Stopped. 0 of 5 translated.");
    assert_eq!(rig.text("translate_all_btn"), "Translate all");
    rig.translate("Message 5", "es");
    rig.tick();
    assert!(rig.http.is_empty(), "nothing else was sent after Stop");
    assert!(rig.is_showing("msg_list", "Message 5"), "the request already sent still lands");
}

#[test]
fn restarting_translate_all_counts_only_its_own_messages() {
    let mut rig = Rig::launch(false);
    let list: Vec<_> = (1..=5).map(|n| message(&format!("$m{n}:test"), "Ana", &format!("Mensaje {n}"))).collect();
    rig.queue_messages(&list);
    rig.boot();
    rig.press("translate_all_btn");
    rig.press("translate_all_btn");
    // Restarting while the stopped batch's request is still out picks the other four.
    rig.press("translate_all_btn");
    assert_eq!(rig.text("list_status"), "Translating 1 of 4…");
    assert_eq!(mymemory_query(&rig.translate("Message 5", "es")).0, "Mensaje 5");
    assert_eq!(rig.text("list_status"), "Translating 1 of 4…", "the old batch's message doesn't count toward this one");
    assert_eq!(rig.text("translate_all_btn"), "Stop");
    for n in [4, 3, 2] {
        rig.tick();
        assert_eq!(mymemory_query(&rig.translate(&format!("Message {n}"), "es")).0, format!("Mensaje {n}"));
        assert_eq!(rig.text("translate_all_btn"), "Stop", "still stoppable while a message waits");
    }
    rig.tick();
    assert_eq!(rig.text("list_status"), "Translating 4 of 4…");
    rig.translate("Message 1", "es");
    assert_eq!(rig.text("list_status"), "Translated 4 messages into English.");
    assert_eq!(rig.text("translate_all_btn"), "Translate all");
    rig.tick();
    assert!(rig.http.is_empty());
}

#[test]
fn translations_are_cached_by_message_and_language_across_refresh_and_relaunch() {
    let mut rig = booted();
    translate_row(&mut rig, "Hola amigos", "Hello friends", "es");
    // Tapping again hides and shows it without another request.
    rig.tap_on("Hola amigos");
    assert!(rig.http.is_empty());
    assert!(rig.is_showing("msg_list", "Translation hidden. Tap to show it."));
    assert!(!rig.is_showing("msg_list", "Hello friends"));
    rig.tap_on("Hola amigos");
    assert!(rig.is_showing("msg_list", "Hello friends"));
    rig.queue_messages(&spanish_room());
    rig.click("Refresh");
    assert!(rig.http.is_empty());
    assert!(rig.is_showing("msg_list", "Hello friends"), "a refresh keeps translations");
    // Translate all skips the cached message.
    rig.press("translate_all_btn");
    assert_eq!(mymemory_query(&rig.translate("Yes, at eight.", "es")).0, "Sí, a las ocho.");
    rig.tick();
    assert_eq!(mymemory_query(&rig.translate("Shall we go to the cinema tomorrow?", "es")).0, "¿Vamos al cine mañana?");
    rig.tick();
    assert!(rig.http.is_empty());
    let saved = rig.stored("cache.json");
    let keys: Vec<_> = saved.as_array().unwrap().iter().map(|entry| entry["key"].as_str().unwrap().to_string()).collect();
    assert_eq!(keys, ["$m1:test|en", "$m3:test|en", "$m2:test|en"]);
    assert_eq!(saved[0], json!({"key": "$m1:test|en", "text": "Hello friends", "source": "es", "is_same": false}));
    let mut rig = rig.relaunch();
    rig.queue_messages(&spanish_room());
    rig.boot();
    for text in ["Hello friends", "Shall we go to the cinema tomorrow?", "Yes, at eight."] {
        assert!(rig.is_showing("msg_list", text), "{text} comes back from storage");
    }
    rig.tap_on("Sí, a las ocho.");
    rig.press("translate_all_btn");
    assert!(rig.http.is_empty(), "nothing is translated twice");
    assert_eq!(rig.text("list_status"), "Every message shown is already translated into English.");
    // Another language is a separate translation.
    rig.pick("target", FR);
    assert!(!rig.is_showing("msg_list", "Hello friends"));
    rig.tap_on("Hola amigos");
    assert_eq!(mymemory_query(&rig.translate("Bonjour les amis", "es")).1, "autodetect|fr");
}

#[test]
fn quota_and_service_errors_are_specific_and_stop_the_queue() {
    let quota = php_json(&json!({"responseData": {"translatedText": "MYMEMORY WARNING: YOU USED ALL AVAILABLE FREE TRANSLATIONS FOR TODAY. NEXT AVAILABLE IN  10 HOURS 22 MINUTES 07 SECONDS VISIT HTTPS://MYMEMORY.TRANSLATED.NET/DOC/USAGELIMITS.PHP TO TRANSLATE MORE"},
        "quotaFinished": true, "responseStatus": 429}));
    let refused = php_json(&json!({"responseData": {"translatedText": "INVALID LANGUAGE PAIR"}, "quotaFinished": false,
        "responseStatus": "403", "responseDetails": "INVALID LANGUAGE PAIR SPECIFIED"}));
    let used_up = "MyMemory's free daily limit (about 5,000 characters) is used up. Try again tomorrow, or use a LibreTranslate server under Translation service.";
    for (status, body, error, row_hint, batch_hint) in [
        // The quota note already says when to try again, so no tap hint follows it.
        (200, quota.as_str(), used_up, "", ""),
        (429, quota.as_str(), used_up, "", ""),
        (200, refused.as_str(), "MyMemory couldn't translate this (error 403): INVALID LANGUAGE PAIR SPECIFIED.", " Tap to try again.", " Tap Translate all to try again."),
        (503, "<html>Service unavailable</html>", "MyMemory answered with HTTP error 503.", " Tap to try again.", " Tap Translate all to try again."),
        (200, "<html>not json</html>", "MyMemory sent a reply Translator couldn't read.", " Tap to try again.", " Tap Translate all to try again."),
        (200, &json!({"responseData": {"translatedText": "  "}, "responseStatus": 200}).to_string(), "MyMemory sent back an empty translation.",
            " Tap to try again.", " Tap Translate all to try again."),
    ] {
        let mut rig = booted();
        rig.press("translate_all_btn");
        rig.answer(status, body);
        rig.tick();
        assert!(rig.http.is_empty(), "{error}: queued messages are not sent after a failure");
        assert_eq!(rig.text("list_status"), format!("Stopped: {error}{batch_hint}"));
        assert!(rig.is_showing("msg_list", &format!("{error}{row_hint}")), "{:?}", rig.seen("msg_list"));
        // The visible retry works.
        rig.tap_on("Sí, a las ocho.");
        assert_eq!(rig.http.len(), 1);
        rig.translate("Yes, at eight.", "es");
        assert!(rig.is_showing("msg_list", "Yes, at eight."));
    }
    let mut rig = booted();
    rig.type_into("reply_input", "Thanks");
    rig.act("translate_reply", &[]);
    rig.answer(200, &quota);
    assert_eq!(rig.text("reply_status"), format!("{used_up} Your text is kept."));
}

#[test]
fn a_failure_explains_the_other_messages_you_tapped() {
    let mut rig = booted();
    rig.tap_on("Hola amigos");
    rig.tap_on("¿Vamos al cine mañana?");
    rig.tap_on("Sí, a las ocho.");
    assert_eq!(rig.http.len(), 1);
    assert_eq!(rig.seen("msg_list").iter().filter(|seen| *seen == "Waiting to translate…").count(), 2);
    rig.answer(503, "<html>Service unavailable</html>");
    rig.tick();
    assert!(rig.http.is_empty(), "nothing else is sent after a failure");
    let shown = rig.seen("msg_list");
    assert!(shown.iter().any(|seen| seen == "MyMemory answered with HTTP error 503. Tap to try again."), "{shown:?}");
    assert_eq!(shown.iter().filter(|seen| *seen == "Stopped after an error. Tap to try again.").count(), 2, "{shown:?}");
    rig.tap_on("¿Vamos al cine mañana?");
    assert_eq!(mymemory_query(&rig.translate("Shall we go to the cinema tomorrow?", "es")).0, "¿Vamos al cine mañana?");
}

#[test]
fn network_and_sharing_refusals_wait_for_a_visible_retry() {
    let mut rig = booted();
    rig.answers.is_network_allowed = false;
    rig.tap_on("Hola amigos");
    assert!(rig.http.is_empty());
    let error = "Internet access to api.mymemory.translated.net wasn't allowed. Tap to try again.";
    assert!(rig.is_showing("msg_list", error), "{:?}", rig.seen("msg_list"));
    let prompts = rig.trace.audit.prompts.len();
    rig.host.publish_grants(&rig.app, APP);
    rig.settle();
    rig.tick();
    assert_eq!(rig.trace.audit.prompts.len(), prompts, "a refusal is never retried automatically");
    rig.answers.is_network_allowed = true;
    rig.tap_on("Hola amigos");
    assert_eq!(rig.trace.audit.prompts.len(), prompts + 1, "tapping again asks again");
    rig.translate("Hello friends", "es");
    assert!(rig.is_showing("msg_list", "Hello friends"));

    let mut rig = booted();
    rig.answers.is_sharing_allowed = false;
    rig.tap_on("Hola amigos");
    assert_eq!(rig.trace.reviews, ["network.http"]);
    assert!(rig.http.is_empty());
    let error = "Sharing this room's text with api.mymemory.translated.net wasn't approved. Tap to try again.";
    assert!(rig.is_showing("msg_list", error), "{:?}", rig.seen("msg_list"));
    rig.tick();
    assert_eq!(rig.trace.reviews.len(), 1, "no automatic second review");
    rig.answers.is_sharing_allowed = true;
    rig.tap_on("Hola amigos");
    assert_eq!(rig.trace.reviews.len(), 2);
    rig.translate("Hello friends", "es");
    assert!(rig.is_showing("msg_list", "Hello friends"));
}

#[test]
fn blocked_internet_access_points_to_mini_apps_settings_instead_of_a_useless_retry() {
    let mut rig = booted();
    rig.deny(Permission::Network);
    rig.tap_on("Hola amigos");
    assert!(rig.http.is_empty());
    let failures: Vec<_> = rig.host.broker.failures().iter().map(|failure| failure.error.clone()).collect();
    assert!(failures.iter().any(|error| error.contains("blocked for this app")), "the real broker refused: {failures:?}");
    let blocked = "Internet access to api.mymemory.translated.net is blocked for Translator.";
    assert!(rig.is_showing("msg_list", &format!("{blocked} Allow it in Mini Apps settings, then tap again.")), "{:?}", rig.seen("msg_list"));
    rig.press("translate_all_btn");
    assert_eq!(rig.text("list_status"), format!("Stopped: {blocked} Allow it in Mini Apps settings, then tap Translate all again."));
    rig.type_into("reply_input", "Thanks");
    rig.act("translate_reply", &[]);
    assert_eq!(rig.text("reply_status"), format!("{blocked} Your text is kept. Allow it in Mini Apps settings, then tap Translate again."));
    assert!(rig.trace.audit.prompts.iter().all(|perm| *perm != Permission::Network), "a block never prompts");
    // Lifting the block in settings makes the same tap work.
    rig.set_grant(Permission::Network, GrantState::Ask);
    rig.tap_on("Hola amigos");
    rig.translate("Hello friends", "es");
    assert!(rig.is_showing("msg_list", "Hello friends"));
}

#[test]
fn host_refusal_messages_are_named_plainly() {
    for (error, expected) in [
        ("This permission group is blocked for this app or agent. Review its permissions in App Info or the AI room panel.",
            "Internet access to api.mymemory.translated.net is blocked for Translator. Allow it in Mini Apps settings, then tap again."),
        ("This capability is blocked for this app or agent. Review its capability setting in App Info or the AI room panel.",
            "Internet access to api.mymemory.translated.net is blocked for Translator. Allow it in Mini Apps settings, then tap again."),
        ("This request needs approval. Open the app in the foreground and review its permission prompt.",
            "Open Translator to approve sending text to api.mymemory.translated.net. Tap to try again."),
        ("Your permission is needed before this data can be sent.",
            "Sharing this room's text with api.mymemory.translated.net wasn't approved. Tap to try again."),
        ("This request was not approved.", "Sharing this room's text with api.mymemory.translated.net wasn't approved. Tap to try again."),
        ("too many requests, slow down", "Robrix asked Translator to slow down. Tap to try again."),
        ("connection reset", "Couldn't reach api.mymemory.translated.net: connection reset. Tap to try again."),
        ("Read access is blocked by this room's rule. Open Mini Apps > Inspect protection to review the deciding rule.",
            "Couldn't reach api.mymemory.translated.net: Read access is blocked by this room's rule. Open Mini Apps > Inspect protection to review the deciding rule. Tap to try again."),
    ] {
        let mut rig = booted();
        rig.tap_on("Hola amigos");
        rig.fail(error);
        assert!(rig.is_showing("msg_list", expected), "{error}: {:?}", rig.seen("msg_list"));
    }
}

#[test]
fn changing_the_language_ignores_a_reply_meant_for_the_old_one() {
    let mut rig = booted();
    rig.tap_on("Hola amigos");
    rig.pick("target", DE);
    assert!(!rig.is_showing("msg_list", "Translating…"), "the old request no longer shows as pending");
    rig.translate("Hello friends", "es");
    assert!(!rig.is_showing("msg_list", "Hello friends"), "a stale English reply never lands under German");
    rig.pick("target", EN);
    assert!(!rig.is_showing("msg_list", "Hello friends"), "nor is it cached for English");
    rig.tap_on("Hola amigos");
    assert_eq!(mymemory_query(&rig.translate("Hello friends", "es")).1, "autodetect|en");
    assert!(rig.is_showing("msg_list", "Hello friends"));
}

#[test]
fn a_message_list_read_from_before_a_revocation_is_ignored() {
    let mut rig = booted();
    rig.host.broker.forget_app(APP);
    rig.host.widget_click(&rig.app, "Refresh", &[]);
    // Serve the setup batch, but hold the read itself.
    let mut held = None;
    for _ in 0..10 {
        for ask in rig.host.process() {
            match ask {
                BrokerAsk::PermissionBatch { request, perms, .. } => {
                    rig.host.answer_stock_batch(&rig.app, APP, request, &perms, &perms, &mut rig.trace.audit);
                }
                BrokerAsk::Matrix { reply, .. } => held = Some(reply),
                BrokerAsk::Used { .. } => {}
                _ => panic!("unexpected request during refresh"),
            }
        }
        if held.is_some() { break; }
    }
    let held = held.expect("the refresh read is in flight");
    rig.deny(Permission::MatrixRoomRead);
    assert_eq!(rig.text("list_status"), "Reading this room is off, so the list won't refresh. Tap Refresh to ask again.");
    rig.reply(held, Ok(&json!({"messages": [message("$late:test", "Eve", "Llegó tarde")]}).to_string()));
    rig.settle();
    assert!(!rig.is_showing("msg_list", "Llegó tarde"), "a read from before the revocation is dropped");
    assert!(rig.is_showing("msg_list", "Hola amigos"), "messages shown earlier stay");
}

#[test]
fn huge_messages_are_cut_before_any_work_so_nothing_hits_the_script_limit() {
    let prose = "Una frase corta para la prueba de un mensaje enorme. ".repeat(1200);
    let cjk = "漢".repeat(20000);
    assert!(prose.len() > 60_000 && cjk.len() == 60_000);
    let mut rig = Rig::launch(false);
    rig.queue_messages(&[message("$prose:test", "Ana", &prose), message("$m1:test", "Luis", "Hola"), message("$cjk:test", "Ana", &cjk)]);
    rig.boot();
    let shown = rig.seen("msg_list");
    let cut = shown.iter().find(|text| text.starts_with("Una frase")).expect("the long message is listed");
    assert!(cut.len() <= 4100 && cut.ends_with(" …"), "{} bytes", cut.len());
    rig.act("translate_all", &[]);
    let mut parts = 0;
    while !rig.http.is_empty() {
        rig.translate("ok then", "es");
        rig.tick();
        parts += 1;
    }
    assert!(parts <= 17, "two cut messages of at most eight parts each, plus one: {parts}");
    assert_eq!(rig.text("list_status"), "Translated 3 messages into English.");
    // A live burst holding a huge message still lists and translates the rest.
    rig.act("set_live", &[true.into()]);
    rig.deliver(&[live("$huge:test", "Marta", &prose, false), live("$bye:test", "Marta", "Hasta luego", false)]);
    assert!(rig.is_showing("msg_list", "Hasta luego"));
    while !rig.http.is_empty() {
        let (_, args) = rig.http.front().unwrap().clone();
        let q = mymemory_query(&args).0;
        rig.translate(if q == "Hasta luego" { "See you later" } else { "ok then" }, "es");
        rig.tick();
    }
    assert!(rig.is_showing("msg_list", "See you later"));
}

// ---- Live translation

#[test]
fn live_translation_subscribes_translates_others_one_at_a_time_and_unsubscribes() {
    let mut rig = booted();
    rig.press("live_btn");
    assert_eq!(rig.trace.batches.last().unwrap(), &vec![Permission::MatrixRoomWatch], "watching is asked for only now");
    assert_eq!(rig.trace.audit.subscriptions, ["on_room_message"]);
    assert_eq!(rig.text("live_btn"), "● Translate new messages");
    assert_eq!(rig.text("live_note"), "On. New messages from others are translated as they arrive.");
    assert_eq!(rig.stored("settings.json")["is_live"], true);
    rig.deliver(&[
        live("$n1:test", "Marta", "Buenos días", false),
        live("$n2:test", "Owner", "Good morning", true),
        live("$n3:test", "Pablo", "¿Qué tal?", false),
    ]);
    for text in ["Buenos días", "Good morning", "¿Qué tal?"] { assert!(rig.is_showing("msg_list", text)); }
    assert_eq!(rig.shown_index("¿Qué tal?"), Some(0), "new messages appear on top");
    assert!(rig.is_showing("msg_list", "Marta") && rig.is_showing("msg_list", "Jul 3, 2024"), "named by display name, like history rows");
    assert_eq!(rig.http.len(), 1, "one request in flight");
    assert_eq!(mymemory_query(&rig.translate("Good morning", "es")).0, "Buenos días");
    assert!(rig.http.is_empty());
    rig.tick();
    assert_eq!(mymemory_query(&rig.translate("How are you?", "es")).0, "¿Qué tal?");
    rig.tick();
    assert!(rig.http.is_empty(), "your own messages are not sent for translation");
    assert!(rig.is_showing("msg_list", "How are you?"));
    rig.press("live_btn");
    assert_eq!(rig.trace.unsubscribed, ["on_room_message"]);
    assert_eq!(rig.text("live_btn"), "○ Translate new messages");
    assert_eq!(rig.stored("settings.json")["is_live"], false);
    let rows = rig.seen("msg_list").len();
    rig.deliver(&[live("$n4:test", "Marta", "Adiós", false)]);
    assert!(rig.http.is_empty());
    assert_eq!(rig.seen("msg_list").len(), rows, "nothing arrives once live translation is off");
    assert_eq!(rig.trace.batches.len(), 2, "turning it off asks for nothing");
}

#[test]
fn history_and_live_rows_name_people_the_same_way() {
    let mut rig = Rig::launch(false);
    // An older host's row has no display name, so it falls back to the short name.
    rig.queue_messages(&[
        json!({"event_id": "$h0:test", "room_id": ROOM, "sender": "eva", "sender_id": "@eva:test",
            "body": "Hola", "ts": 1720000000000_u64, "msgtype": "m.text"}),
        json!({"event_id": "$h1:test", "room_id": ROOM, "sender": "kevinaboos", "sender_id": "@kevinaboos:test",
            "sender_name": "Kevin Boos", "body": "Hola equipo", "ts": 1720000000000_u64, "msgtype": "m.text"}),
    ]);
    rig.boot();
    rig.act("set_live", &[true.into()]);
    rig.deliver(&[json!({"room_id": ROOM, "event_id": "$l1:test", "sender": "kevinaboos", "sender_id": "@kevinaboos:test",
        "sender_name": "Kevin Boos", "body": "Ya llegué", "ts": 1720000000000_u64, "msgtype": "m.text", "is_own": true})]);
    assert_eq!(rig.seen("msg_list"), ["K", "Kevin Boos", "Jul 3, 2024", "Ya llegué", "K", "Kevin Boos", "Jul 3, 2024", "Hola equipo",
        "E", "eva", "Jul 3, 2024", "Hola"]);
}

#[test]
fn a_refused_live_translation_pauses_until_resumed_and_resume_sends_during_the_tap() {
    let mut rig = booted();
    rig.press("live_btn");
    rig.answers.is_network_allowed = false;
    rig.deliver(&[live("$n1:test", "Marta", "Buenos días", false)]);
    assert!(rig.is_visible("resume_wrap"));
    assert_eq!(rig.text("live_note"), "Paused: Internet access to api.mymemory.translated.net wasn't allowed. Tap Resume to try again.");
    let prompts = rig.trace.audit.prompts.len();
    rig.deliver(&[live("$n2:test", "Marta", "Hasta luego", false)]);
    assert!(rig.http.is_empty(), "paused: new messages are listed but not sent");
    assert!(rig.is_showing("msg_list", "Hasta luego"));
    assert_eq!(rig.trace.audit.prompts.len(), prompts);
    // Resume sends while the tap is fresh, so Robrix can show its prompt again.
    rig.answers.is_network_allowed = true;
    rig.click("Resume");
    assert_eq!(rig.trace.audit.prompts.len(), prompts + 1, "the tap itself asks again");
    assert_eq!(rig.http.len(), 1);
    assert!(!rig.is_visible("resume_wrap"));
    assert_eq!(rig.text("live_note"), "On. New messages from others are translated as they arrive.");
    assert_eq!(mymemory_query(&rig.translate("Good morning", "es")).0, "Buenos días");
    rig.tick();
    assert_eq!(mymemory_query(&rig.translate("See you later", "es")).0, "Hasta luego", "what arrived while paused follows");
    rig.tick();
    rig.deliver(&[live("$n3:test", "Marta", "Gracias", false)]);
    assert_eq!(mymemory_query(&rig.translate("Thanks", "es")).0, "Gracias");
}

#[test]
fn the_live_note_never_says_on_while_live_translation_is_paused() {
    let mut rig = booted();
    rig.press("live_btn");
    rig.answers.is_network_allowed = false;
    rig.deliver(&[live("$n1:test", "Marta", "Buenos días", false)]);
    let paused = "Paused: Internet access to api.mymemory.translated.net wasn't allowed. Tap Resume to try again.";
    assert_eq!(rig.text("live_note"), paused);
    rig.deny(Permission::MatrixRoomWatch);
    assert_eq!(rig.text("live_note"), "Paused: watching this room is off. It resumes when it's allowed again.");
    rig.set_grant(Permission::MatrixRoomWatch, GrantState::Granted);
    assert_eq!(rig.trace.audit.subscriptions.len(), 2, "regained watching resubscribes");
    assert_eq!(rig.text("live_note"), paused, "the earlier failure still needs Resume");
    assert!(rig.is_visible("resume_wrap"));
    rig.answers.is_network_allowed = true;
    rig.deliver(&[live("$n2:test", "Marta", "Hasta luego", false)]);
    assert!(rig.http.is_empty(), "still paused");
}

#[test]
fn a_failed_startup_says_live_translation_did_not_start() {
    let mut rig = booted();
    rig.press("live_btn");
    rig.host.permissions.set(APP, Permission::MatrixRoomWatch, GrantState::Ask);
    let mut rig = rig.relaunch();
    rig.queue_messages(&spanish_room());
    rig.fire_timers();
    let (request, perms) = rig.host.take_stock_batch(APP);
    assert_eq!(perms, [Permission::MatrixRoomRead, Permission::MatrixRoomWatch, Permission::DeviceInfo]);
    rig.refuse(request, "This permission request was cancelled.");
    rig.settle();
    assert_eq!(rig.text("live_btn"), "● Translate new messages");
    assert_eq!(rig.text("live_note"), "Not started: This permission request was cancelled. Tap Refresh to try again.");
    assert!(rig.trace.audit.subscriptions.is_empty());
    rig.click("Refresh");
    assert_eq!(rig.trace.audit.subscriptions, ["on_room_message"]);
    assert_eq!(rig.text("live_note"), "On. New messages from others are translated as they arrive.");
}

#[test]
fn live_bursts_are_bounded() {
    let mut rig = booted();
    rig.act("set_live", &[true.into()]);
    let burst: Vec<_> = (1..=14).map(|n| live(&format!("$b{n}:test"), "Marta", &format!("Mensaje {n}"), false)).collect();
    rig.deliver(&burst);
    assert_eq!(rig.http.len(), 1);
    assert_eq!(rig.text("live_note"), "Busy: 4 new messages skipped. Tap one to translate it.");
    let mut sent = 0;
    while !rig.http.is_empty() {
        rig.translate("Message", "es");
        rig.tick();
        sent += 1;
    }
    assert_eq!(sent, 10, "at most ten new messages wait their turn");
    assert!(rig.is_showing("msg_list", "Mensaje 14"), "skipped messages are still listed for a tap");
}

#[test]
fn the_live_setting_is_saved_but_a_relaunch_still_respects_permissions() {
    let mut rig = booted();
    rig.act("set_live", &[true.into()]);
    // Approved groups answer the startup setup without a prompt, and watching resumes.
    let mut rig = rig.relaunch();
    rig.queue_messages(&spanish_room());
    rig.boot();
    assert!(rig.trace.audit.prompts.is_empty() && rig.trace.batches.is_empty(), "already approved groups do not prompt again");
    assert_eq!(rig.trace.audit.subscriptions, ["on_room_message"]);
    assert_eq!(rig.text("live_btn"), "● Translate new messages");
    // When watching needs asking again, it joins the one startup setup.
    rig.host.permissions.set(APP, Permission::MatrixRoomWatch, GrantState::Ask);
    let mut rig = rig.relaunch();
    rig.answers.is_watch_allowed = false;
    rig.queue_messages(&spanish_room());
    rig.boot();
    assert_eq!(rig.trace.batches, [vec![Permission::MatrixRoomRead, Permission::MatrixRoomWatch, Permission::DeviceInfo]]);
    assert!(rig.trace.audit.subscriptions.is_empty());
    // A declined watch turns the saved choice off instead of asking at every launch.
    assert_eq!(rig.text("live_btn"), "○ Translate new messages");
    assert_eq!(rig.text("live_note"), "Off: watching this room for new messages wasn't allowed. Turn it on to ask again.");
    assert_eq!(rig.stored("settings.json")["is_live"], false);
    assert!(rig.is_showing("msg_list", "Hola amigos"), "the selected read still loads");
    rig.host.permissions.set(APP, Permission::MatrixRoomRead, GrantState::Ask);
    let mut rig = rig.relaunch();
    rig.queue_messages(&spanish_room());
    rig.boot();
    assert_eq!(rig.trace.batches, [vec![Permission::MatrixRoomRead, Permission::DeviceInfo]], "no nagging on the next launch");
}

#[test]
fn refusing_to_watch_from_the_toggle_turns_it_back_off() {
    let mut rig = booted();
    rig.answers.is_batch_allowed = false;
    rig.act("set_live", &[true.into()]);
    assert!(rig.trace.audit.subscriptions.is_empty());
    assert_eq!(rig.text("live_btn"), "○ Translate new messages");
    assert_eq!(rig.text("live_note"), "Off: watching this room for new messages wasn't allowed. Turn it on to ask again.");
    rig.host.publish_grants(&rig.app, APP);
    rig.settle();
    assert_eq!(rig.trace.batches.len(), 2, "no automatic second ask");
    rig.answers.is_batch_allowed = true;
    rig.host.permissions.set(APP, Permission::MatrixRoomWatch, GrantState::Ask);
    rig.press("live_btn");
    assert_eq!(rig.trace.batches.len(), 3);
    assert_eq!(rig.trace.audit.subscriptions, ["on_room_message"]);
}

#[test]
fn revocation_resyncs_reads_and_live_updates_without_repeating_completed_work() {
    let mut rig = booted();
    rig.act("set_live", &[true.into()]);
    let prompts = rig.trace.audit.prompts.len();
    assert_eq!(rig.trace.reads.len(), 1);
    rig.deny(Permission::MatrixRoomRead);
    assert_eq!(rig.text("list_status"), "Reading this room is off, so the list won't refresh. Tap Refresh to ask again.");
    assert!(rig.is_showing("msg_list", "Hola amigos"), "messages shown earlier stay");
    assert_eq!(rig.trace.reads.len(), 1);
    rig.set_grant(Permission::MatrixRoomRead, GrantState::Granted);
    assert_eq!(rig.trace.reads.len(), 2, "a regained read loads once");
    rig.host.publish_grants(&rig.app, APP);
    rig.settle();
    assert_eq!(rig.trace.reads.len(), 2, "an unrelated grant repeats nothing");
    assert_eq!(rig.trace.audit.subscriptions.len(), 1);
    rig.deny(Permission::MatrixRoomWatch);
    assert_eq!(rig.text("live_note"), "Paused: watching this room is off. It resumes when it's allowed again.");
    rig.set_grant(Permission::MatrixRoomWatch, GrantState::Granted);
    assert_eq!(rig.trace.audit.subscriptions, ["on_room_message", "on_room_message"], "regained watching resubscribes once");
    assert_eq!(rig.text("live_note"), "On. New messages from others are translated as they arrive.");
    assert_eq!(rig.trace.audit.prompts.len(), prompts, "no prompt along the way");
    rig.host.publish_grants(&rig.app, APP);
    rig.settle();
    assert_eq!(rig.trace.audit.subscriptions.len(), 2);
}

// ---- Your reply

#[test]
fn reply_translation_drafts_and_copies_the_exact_text() {
    let mut rig = booted();
    rig.pick("reply", ES);
    assert_eq!(rig.text("reply_btn"), "Español ▼");
    assert!(!rig.is_visible("reply_hint_wrap"));
    rig.act("translate_reply", &[]);
    assert_eq!(rig.text("reply_status"), "Write your reply first, in any language.");
    assert!(rig.is_visible("reply_status_wrap"));
    assert!(rig.http.is_empty());
    rig.type_into("reply_input", "  See you tomorrow at ten  ");
    rig.click("Translate");
    assert_eq!(rig.text("reply_status"), "Translating into Español with MyMemory…");
    let args = rig.translate("Nos vemos mañana a las diez", "en");
    assert_eq!(args["url"], "https://api.mymemory.translated.net/get?q=See%20you%20tomorrow%20at%20ten&langpair=autodetect%7Ces");
    assert!(rig.is_visible("result_wrap"));
    assert_eq!(rig.text("result_text"), "Nos vemos mañana a las diez");
    assert_eq!(rig.text("result_note"), "Spanish · from English");
    assert_eq!(rig.text("reply_status"), "");
    assert!(!rig.is_visible("reply_status_wrap"), "no empty status line under the result");
    rig.press("insert_btn");
    assert_eq!(rig.trace.inserted, ["Nos vemos mañana a las diez"]);
    assert!(rig.trace.reviews.contains(&"composer.insert".to_string()), "text from a website gets an exact-action review");
    assert_eq!(rig.text("reply_status"), "Added to the message box. Review it there, then press Send.");
    rig.click("Copy");
    assert_eq!(rig.trace.copied, ["Nos vemos mañana a las diez"]);
    assert_eq!(rig.text("reply_status"), "Copied the translation.");
    assert_eq!(rig.input("reply_input"), "  See you tomorrow at ten  ", "your own text stays");
    rig.type_into("reply_input", "See you tomorrow at nine");
    rig.act_str("reply_edited", &["See you tomorrow at nine"]);
    assert_eq!(rig.text("reply_status"), "You changed your reply. Tap Translate to update the translation.");
    assert!(rig.trace.audit.services.iter().all(|service| service != "matrix.room.message.send"), "drafting never sends");
}

#[test]
fn reply_failures_keep_your_text_and_explain_what_happened() {
    let mut rig = booted();
    rig.pick("reply", ES);
    rig.type_into("reply_input", "Thanks a lot");
    rig.answers.is_network_allowed = false;
    rig.act("translate_reply", &[]);
    assert_eq!(rig.text("reply_status"),
        "Internet access to api.mymemory.translated.net wasn't allowed. Your text is kept. Tap Translate to try again.");
    assert_eq!(rig.input("reply_input"), "Thanks a lot");
    assert!(!rig.is_visible("result_wrap"));
    rig.answers.is_network_allowed = true;
    rig.act("translate_reply", &[]);
    rig.translate("Muchas gracias", "en");
    assert_eq!(rig.text("result_text"), "Muchas gracias");
    rig.answers.are_actions_allowed = false;
    rig.act("insert_reply", &[]);
    assert!(rig.trace.inserted.is_empty());
    assert_eq!(rig.text("reply_status"), "The message box wasn't changed: The request to use \"Draft a message for you\" was not approved.");
    assert_eq!(rig.input("reply_input"), "Thanks a lot");
    rig.deny(Permission::ClipboardWrite);
    rig.act("copy_reply", &[]);
    assert!(rig.trace.copied.is_empty());
    assert!(rig.text("reply_status").starts_with("Couldn't copy: "), "{}", rig.text("reply_status"));
    assert!(rig.is_visible("result_wrap"), "the translation stays available");
}

#[test]
fn a_newer_reply_replaces_an_older_one_still_in_flight() {
    let mut rig = booted();
    rig.pick("reply", ES);
    rig.type_into("reply_input", "one");
    rig.act("translate_reply", &[]);
    rig.type_into("reply_input", "two");
    rig.act("translate_reply", &[]);
    assert_eq!(rig.http.len(), 1, "still one request in flight");
    assert_eq!(mymemory_query(&rig.translate("uno", "en")).0, "one");
    assert!(!rig.is_visible("result_wrap"), "the older reply is ignored");
    rig.tick();
    assert_eq!(mymemory_query(&rig.translate("dos", "en")).0, "two");
    assert_eq!(rig.text("result_text"), "dos");
}

#[test]
fn a_reply_already_in_the_reply_language_is_offered_unchanged() {
    let mut rig = booted();
    rig.pick("reply", ES);
    rig.type_into("reply_input", "Nos vemos");
    rig.act("translate_reply", &[]);
    rig.answer(200, &mymemory_same());
    assert_eq!(rig.text("result_text"), "Nos vemos");
    assert_eq!(rig.text("result_note"), "Already in Spanish");
    rig.act("insert_reply", &[]);
    assert_eq!(rig.trace.inserted, ["Nos vemos"]);
}

#[test]
fn the_reply_language_follows_the_room_until_you_choose_one() {
    let mut rig = Rig::launch(false);
    rig.queue_messages(&[message("$m1:test", "Ana", "Hola"), message("$m2:test", "Jan", "Hallo"), message("$m3:test", "Luis", "Buenas")]);
    rig.boot();
    assert_eq!(rig.text("reply_btn"), "English ▼");
    rig.press("translate_all_btn");
    for (text, lang) in [("Hi there", "es"), ("Hello", "de"), ("Hello all", "es")] {
        rig.translate(text, lang);
        rig.tick();
    }
    assert_eq!(rig.text("reply_btn"), "Español ▼");
    assert_eq!(rig.text("reply_hint"), "Automatic · the language used most here");
    assert_eq!(rig.stored("detected.json"), json!([{"id": "es", "n": 2}, {"id": "de", "n": 1}]));
    rig.press("reply_btn");
    assert!(rig.is_visible("reply_menu_wrap"));
    assert!(rig.is_showing("reply_menu", "Automatic · Español"));
    let mut rig = rig.relaunch();
    rig.queue_messages(&spanish_room());
    rig.boot();
    assert_eq!(rig.text("reply_btn"), "Español ▼", "what this room speaks is remembered");
    rig.pick("reply", DE);
    rig.pick("target", FR);
    let mut rig = rig.relaunch();
    rig.queue_messages(&spanish_room());
    rig.boot();
    assert_eq!(rig.text("reply_btn"), "Deutsch ▼");
    assert_eq!(rig.text("target_btn"), "Français ▼");
    assert_eq!(rig.stored("settings.json")["reply"], "de");
}

/// Your own messages in your own language must not make the room look like it speaks it.
#[test]
fn the_reply_language_counts_only_messages_that_needed_translating() {
    let mut rig = Rig::launch(false);
    rig.queue_messages(&[
        message("$o1:test", "Owner", "Welcome, everyone"),
        message("$s1:test", "Ana", "Hola a todos"),
        message("$o2:test", "Owner", "Agenda is in the doc"),
        message("$s2:test", "Luis", "Gracias, lo leo ahora"),
        message("$o3:test", "Owner", "See you on Friday"),
    ]);
    rig.boot();
    rig.press("translate_all_btn");
    while let Some((_, args)) = rig.http.front().cloned() {
        let q = mymemory_query(&args).0;
        if q.starts_with("Hola") { rig.translate("Hello everyone", "es"); }
        else if q.starts_with("Gracias") { rig.translate("Thanks, I'll read it now", "es"); }
        else { rig.answer(200, &mymemory_same()); }
        rig.tick();
    }
    assert_eq!(rig.seen("msg_list").iter().filter(|seen| *seen == "Already in English").count(), 3);
    assert_eq!(rig.stored("detected.json"), json!([{"id": "es", "n": 2}]));
    assert_eq!(rig.text("reply_btn"), "Español ▼");
    rig.type_into("reply_input", "See you on Friday");
    rig.act("translate_reply", &[]);
    let args = rig.translate("Nos vemos el viernes", "en");
    assert_eq!(mymemory_query(&args).1, "autodetect|es");
    assert_eq!(rig.text("result_text"), "Nos vemos el viernes");
}

// ---- Languages, settings and layout

#[test]
fn language_menus_list_every_language_and_close_after_a_choice() {
    let mut rig = booted();
    assert!(!rig.is_visible("target_menu_wrap"));
    rig.press("target_btn");
    assert!(rig.is_visible("target_menu_wrap"));
    let chips = rig.seen("target_menu");
    assert_eq!(chips, ["Automatic · English", "English", "简体中文", "繁體中文", "日本語", "한국어", "Español", "Français", "Deutsch",
        "Italiano", "Português", "Русский", "Українська", "Polski", "Nederlands", "Türkçe", "Tiếng Việt", "Bahasa Indonesia", "Ελληνικά"]);
    rig.pick("target", ES);
    assert!(!rig.is_visible("target_menu_wrap"));
    assert_eq!(rig.text("target_btn"), "Español ▼");
    rig.pick("target", 0);
    assert_eq!(rig.text("target_btn"), "English ▼", "Automatic goes back to the device language");
    assert_eq!(rig.stored("settings.json")["target"], "");
    rig.press("reply_btn");
    rig.press("reply_btn");
    assert!(!rig.is_visible("reply_menu_wrap"));
    rig.press("reply_btn");
    rig.press("target_btn");
    assert!(!rig.is_visible("reply_menu_wrap") && rig.is_visible("target_menu_wrap"), "one menu at a time");
}

#[test]
fn a_libretranslate_server_gets_a_json_post_with_mapped_codes_and_the_saved_key() {
    let mut rig = booted();
    rig.press("service_btn");
    assert!(rig.is_visible("service_body"));
    assert_eq!(rig.text("site_line"), "Text you translate is sent to api.mymemory.translated.net.");
    assert!(rig.is_showing("service_body", "MyMemory is free and needs no account; its free use is limited to about 5,000 characters a day."));
    assert_eq!(rig.chosen("service_choice"), ["MyMemory"]);
    rig.tap_chip("service_choice", 1);
    assert!(rig.is_visible("lt_wrap"));
    assert_eq!(rig.chosen("service_choice"), ["LibreTranslate"], "the picked service shows as chosen while its server is entered");
    assert!(!rig.is_visible("quota_wrap"), "MyMemory's terms don't sit above the LibreTranslate form");
    assert_eq!(rig.text("service_status"), "Enter your server's address, then tap Save server.");
    for (server, problem) in [
        ("", "Enter your LibreTranslate server's address, like https://translate.example.org."),
        ("http://translate.example.org", "Use the server's https:// address."),
        ("https://user:pw@translate.example.org", "Enter just the server's address, like https://translate.example.org."),
        ("https://translate.example.org/?key=1", "Enter just the server's address, like https://translate.example.org."),
    ] {
        rig.type_into("server_input", server);
        rig.click("Save server");
        assert_eq!(rig.text("service_status"), problem, "{server}");
        assert_eq!(rig.text("service_btn"), "Translation service: MyMemory ▲");
    }
    rig.type_into("server_input", " https://translate.example.org/translate/ ");
    rig.type_into("key_input", "secret-key-1234");
    rig.click("Save server");
    assert_eq!(rig.text("service_status"), "Saved. Translations now go to translate.example.org.");
    assert_eq!(rig.input("key_input"), "", "the key is never shown again");
    assert_eq!(rig.input("server_input"), "https://translate.example.org");
    assert_eq!(rig.text("key_line"), "An API key ending in 1234 is saved.");
    assert_eq!(rig.text("site_line"), "Text you translate is sent to translate.example.org.");
    assert_eq!(rig.text("privacy_note"), "🔒 Text is sent to translate.example.org only when you translate it.");
    assert!(!rig.is_visible("quota_wrap"));
    assert_eq!(rig.chosen("service_choice"), ["LibreTranslate"]);
    let saved = rig.stored("settings.json");
    assert_eq!((saved["service"].as_str(), saved["server"].as_str(), saved["key"].as_str()),
        (Some("libre"), Some("https://translate.example.org"), Some("secret-key-1234")));
    rig.pick("target", ZH_TW);
    rig.tap_on("Hola amigos");
    assert_eq!(rig.trace.reviews, ["network.http"]);
    let reply = json!({"translatedText": "朋友們好", "detectedLanguage": {"confidence": 90, "language": "es"}}).to_string();
    let args = rig.answer(200, &reply);
    assert_eq!(args["url"], "https://translate.example.org/translate");
    assert_eq!(args["method"], "POST");
    assert_eq!(args["headers"], json!({"Content-Type": "application/json"}));
    let body: serde_json::Value = serde_json::from_str(args["body"].as_str().unwrap()).unwrap();
    assert_eq!(body, json!({"q": "Hola amigos", "source": "auto", "target": "zt", "format": "text", "api_key": "secret-key-1234"}));
    assert!(rig.is_showing("msg_list", "朋友們好"));
    assert!(rig.is_showing("msg_list", "from Spanish"));
    rig.pick("target", ZH_CN);
    rig.click("Forget key");
    assert!(!rig.is_visible("key_wrap"));
    assert_eq!(rig.text("service_status"), "The API key was removed.");
    rig.tap_on("Hola amigos");
    let args = rig.answer(403, &json!({"error": "Invalid API key"}).to_string());
    let body: serde_json::Value = serde_json::from_str(args["body"].as_str().unwrap()).unwrap();
    assert_eq!(body, json!({"q": "Hola amigos", "source": "auto", "target": "zh", "format": "text"}));
    assert!(rig.is_showing("msg_list", "The LibreTranslate server answered with HTTP error 403: Invalid API key. Tap to try again."));
    rig.tap_on("Hola amigos");
    rig.answer(200, "not json");
    assert!(rig.is_showing("msg_list", "The LibreTranslate server sent a reply Translator couldn't read. Tap to try again."));
    let mut rig = rig.relaunch();
    rig.queue_messages(&spanish_room());
    rig.boot();
    assert_eq!(rig.text("site_line"), "Text you translate is sent to translate.example.org.");
    rig.press("service_btn");
    assert_eq!(rig.chosen("service_choice"), ["LibreTranslate"]);
    assert_eq!(rig.input("server_input"), "https://translate.example.org");
    rig.tap_chip("service_choice", 0);
    assert_eq!(rig.text("service_status"), "Translations now go to api.mymemory.translated.net.");
    assert_eq!(rig.chosen("service_choice"), ["MyMemory"]);
    assert_eq!(rig.text("privacy_note"), PRIVACY, "the note always names the current site");
    rig.tap_on("Hola amigos");
    assert!(rig.http.front().unwrap().1["url"].as_str().unwrap().starts_with(MYMEMORY));
}

#[test]
fn a_saved_key_is_never_sent_to_a_different_server() {
    let mut rig = booted();
    rig.press("service_btn");
    rig.tap_chip("service_choice", 1);
    rig.type_into("server_input", "https://translate.example.org");
    rig.type_into("key_input", "secret-key-for-example-org");
    rig.click("Save server");
    // Re-saving the same server keeps its key.
    rig.type_into("server_input", "https://translate.example.org/translate");
    rig.click("Save server");
    assert_eq!(rig.text("key_line"), "An API key ending in -org is saved.");
    rig.type_into("server_input", "https://other-translate.example.net");
    rig.click("Save server");
    assert_eq!(rig.text("service_status"),
        "Saved. Translations now go to other-translate.example.net. The API key for translate.example.org was removed.");
    assert!(!rig.is_visible("key_wrap"));
    assert_eq!(rig.stored("settings.json")["key"], "");
    rig.tap_on("Hola amigos");
    let args = rig.answer(200, &json!({"translatedText": "Hello friends", "detectedLanguage": {"confidence": 90, "language": "es"}}).to_string());
    assert_eq!(args["url"], "https://other-translate.example.net/translate");
    let body: serde_json::Value = serde_json::from_str(args["body"].as_str().unwrap()).unwrap();
    assert_eq!(body, json!({"q": "Hola amigos", "source": "auto", "target": "en", "format": "text"}), "no key for another server");
    // A new key typed with a new server belongs to that server.
    rig.type_into("server_input", "https://third.example.com");
    rig.type_into("key_input", "third-key-9876");
    rig.click("Save server");
    assert_eq!(rig.text("service_status"), "Saved. Translations now go to third.example.com.");
    assert_eq!(rig.text("key_line"), "An API key ending in 9876 is saved.");
}

#[test]
fn switching_services_cancels_requests_meant_for_the_old_site() {
    let mut rig = booted();
    rig.press("service_btn");
    rig.press("translate_all_btn");
    assert_eq!(rig.http.len(), 1);
    rig.tap_chip("service_choice", 1);
    rig.type_into("server_input", "https://translate.example.org");
    rig.click("Save server");
    rig.translate("Yes, at eight.", "es");
    rig.tick();
    assert!(rig.http.is_empty(), "queued messages for MyMemory are not sent to the new server");
    assert!(!rig.is_showing("msg_list", "Yes, at eight."), "the old site's late reply is dropped");
    assert_eq!(rig.text("translate_all_btn"), "Translate all");
}

#[test]
fn narrow_and_short_panes_switch_to_compact_labels() {
    let mut rig = booted();
    assert!(rig.is_visible("refresh_wide") && !rig.is_visible("refresh_narrow"));
    rig.resize(200.0, 400.0);
    assert!(!rig.is_visible("refresh_wide") && rig.is_visible("refresh_narrow"), "Refresh leaves the picker row its width");
    assert_eq!(rig.text("privacy_note"), "🔒 Sent to MyMemory only when you translate it.");
    assert_eq!(rig.seen("msg_list")[..3], ["Ana", "Jul 3, 2024", "Sí, a las ocho."], "tiny rows leave out the avatars");
    assert!(!rig.seen("col").iter().any(|text| text == "Translator"), "the pane header names the app");
    assert_eq!(rig.text("target_hint"), "Automatic");
    assert_eq!(rig.text("reply_hint"), "Automatic");
    assert_eq!(rig.text("translate_all_btn"), "Translate all", "the main action keeps its whole name");
    assert_eq!(rig.text("live_btn"), "○ Live");
    assert_eq!(rig.text("insert_btn"), "Insert");
    assert_eq!(rig.text("service_btn"), "Service: MyMemory ▼");
    assert_eq!(rig.text("list_status"), "Tap a message to translate it");
    rig.press("service_btn");
    assert_eq!(rig.seen("service_choice"), ["MyMemory", "LibreTranslate"]);
    assert_eq!(rig.chosen("service_choice"), ["MyMemory"]);
    rig.resize(320.0, 600.0);
    assert_eq!(rig.text("live_btn"), "○ Live", "a 320-point pane keeps the compact labels");
    assert_eq!(rig.text("live_note"), "Off. New messages aren't translated.");
    rig.resize(1600.0, 900.0);
    assert!(rig.is_visible("refresh_wide") && !rig.is_visible("refresh_narrow"));
    assert_eq!(rig.text("privacy_note"), PRIVACY);
    assert!(!rig.seen("col").iter().any(|text| text == "Translator"));
    assert_eq!(rig.text("translate_all_btn"), "Translate all");
    assert_eq!(rig.text("live_btn"), "○ Translate new messages");
    assert_eq!(rig.text("insert_btn"), "Put in message box");
    assert_eq!(rig.text("service_btn"), "Translation service: MyMemory ▲");
    assert_eq!(rig.seen("service_choice"), ["MyMemory", "LibreTranslate"]);
    assert_eq!(rig.chosen("service_choice"), ["MyMemory"]);
    assert_eq!(rig.text("list_status"), "3 recent messages, newest first · tap one to translate it");
}

#[test]
fn very_long_messages_are_capped_at_eight_parts() {
    let body = "Una frase corta para la prueba. ".repeat(120);
    assert!(body.len() < 4000, "under the cut, so only the part cap applies");
    let mut rig = Rig::launch(false);
    rig.queue_messages(&[message("$huge:test", "Ana", body.trim())]);
    rig.boot();
    rig.tap_shown(0);
    let mut parts = 0;
    while !rig.http.is_empty() {
        rig.translate(&format!("p{parts}"), "es");
        rig.tick();
        parts += 1;
    }
    assert_eq!(parts, 8, "a message never costs more than eight requests");
    assert!(rig.is_showing("msg_list", "p0 p1 p2 p3 p4 p5 p6 p7 …"), "{:?}", rig.seen("msg_list"));
}

#[test]
fn an_empty_or_failed_list_explains_itself_once_and_refresh_reads_again() {
    let mut rig = Rig::launch(false);
    rig.queue_messages(&[]);
    rig.boot();
    assert!(rig.seen("msg_list").is_empty());
    assert_eq!(rig.seen("empty_wrap"), ["📭", "No messages yet", "Tap Refresh later, or turn on Translate new messages."]);
    assert!(!rig.is_visible("list_status_wrap"), "no second line repeating the empty state");
    assert!(!rig.is_visible("all_wrap"), "nothing to translate, so no Translate all");
    rig.queue_messages(&spanish_room());
    rig.click("Refresh");
    assert_eq!(rig.trace.reads.len(), 2);
    assert!(rig.is_showing("msg_list", "Hola amigos"));
    assert!(rig.is_visible("list_status_wrap") && rig.is_visible("all_wrap") && !rig.is_visible("empty_wrap"));
    // A homeserver failure is shown once, and a later permission change reads again because nothing loaded.
    let mut rig = Rig::launch(false);
    rig.boot();
    rig.host.broker.forget_app(APP);
    rig.host.widget_click(&rig.app, "Refresh", &[]);
    for _ in 0..10 {
        let mut is_done = false;
        for ask in rig.host.process() {
            if let BrokerAsk::Matrix { reply, .. } = ask {
                rig.reply(reply, Err("The homeserver didn't respond in time."));
                rig.host.cx.with_vm_and_async(|_| {});
                is_done = true;
            }
        }
        if is_done { break; }
    }
    rig.settle();
    let error = "Couldn't load messages: The homeserver didn't respond in time. Tap Refresh to try again.";
    let shown = rig.seen("col");
    assert_eq!(shown.iter().filter(|text| *text == error).count(), 1, "{shown:?}");
    let reads = rig.trace.reads.len();
    rig.host.publish_grants(&rig.app, APP);
    rig.settle();
    assert_eq!(rig.trace.reads.len(), reads + 1, "a permission change reads again because nothing loaded");
    rig.host.publish_grants(&rig.app, APP);
    rig.settle();
    assert_eq!(rig.trace.reads.len(), reads + 1, "and only once");
}

#[test]
fn a_full_room_and_cache_stay_within_the_script_budget() {
    let rig = Rig::launch(false);
    let list: Vec<_> = (1..=30).map(|n| message(&format!("$m{n}:test"), "Ana", &format!("Mensaje número {n} con algo de texto."))).collect();
    let cache: Vec<_> = (1..=250).map(|n| json!({"key": format!("$old{n}:test|en"), "text": format!("Old {n}"), "source": "es", "is_same": false})).collect();
    std::fs::write(rig.jail().join("cache.json"), serde_json::Value::Array(cache).to_string()).unwrap();
    let mut rig = rig.relaunch();
    rig.queue_messages(&list);
    rig.boot();
    rig.act("set_live", &[true.into()]);
    let burst: Vec<_> = (1..=25).map(|n| live(&format!("$b{n}:test"), "Marta", &format!("Nuevo {n}"), false)).collect();
    rig.deliver(&burst);
    assert_eq!(rig.text("list_status"), "40 recent messages, newest first · tap one to translate it", "the list keeps the newest 40");
    assert!(!rig.is_showing("msg_list", "Mensaje número 15 con algo de texto."), "the oldest rows give way");
    assert!(rig.is_showing("msg_list", "Mensaje número 16 con algo de texto."));
    while !rig.http.is_empty() {
        rig.translate("New", "es");
        rig.tick();
    }
    let saved = rig.stored("cache.json");
    assert_eq!(saved.as_array().unwrap().len(), 200, "the saved cache stays bounded");
    assert_eq!(saved[199]["key"], "$b10:test|en");
}

#[test]
fn the_saved_cache_stays_far_below_the_storage_file_limit() {
    let rig = Rig::launch(false);
    // Two hundred long Cyrillic translations would be over 1 MiB, which storage refuses to write.
    let text = "Пример длинного перевода для проверки размера. ".repeat(66);
    let cache: Vec<_> = (1..=200).map(|n| json!({"key": format!("$old{n}:test|en"), "text": text, "source": "ru", "is_same": false})).collect();
    let seeded = serde_json::Value::Array(cache).to_string();
    assert!(seeded.len() > 1_048_576, "{} bytes", seeded.len());
    std::fs::write(rig.jail().join("cache.json"), seeded).unwrap();
    let mut rig = rig.relaunch();
    rig.queue_messages(&spanish_room());
    rig.boot();
    rig.tap_on("Hola amigos");
    rig.translate("Hello friends", "es");
    assert!(rig.is_showing("msg_list", "Hello friends"), "the translation shows instead of hanging on Translating…");
    let bytes = std::fs::metadata(rig.jail().join("cache.json")).unwrap().len();
    assert!(bytes <= 400_000, "{bytes} bytes");
    let saved = rig.stored("cache.json");
    assert_eq!(saved.as_array().unwrap().last().unwrap()["key"], "$m1:test|en", "the newest translation is kept");
}

#[test]
fn replies_are_limited_to_a_thousand_characters() {
    let mut rig = booted();
    rig.type_into("reply_input", &"字".repeat(1001));
    rig.act("translate_reply", &[]);
    assert_eq!(rig.text("reply_status"), "Keep your reply under 1,000 characters.");
    assert!(rig.http.is_empty());
    rig.type_into("reply_input", &"字".repeat(1000));
    rig.act("translate_reply", &[]);
    assert_eq!(rig.http.len(), 1, "a full-length reply is still translated, in parts");
}

#[test]
fn a_blank_part_between_long_paragraphs_is_kept_without_a_request() {
    let body = format!("{}\n\n{}", "A".repeat(449), "B".repeat(450));
    let mut rig = Rig::launch(false);
    rig.queue_messages(&[message("$gap:test", "Ana", &body)]);
    rig.boot();
    rig.tap_shown(0);
    let mut sent = Vec::new();
    for _ in 0..5 {
        if let Some((_, args)) = rig.http.front() {
            let q = mymemory_query(args).0;
            let reply = if q.starts_with('A') { "first" } else { "second" };
            sent.push(q);
            rig.translate(reply, "es");
        }
        rig.tick();
    }
    assert_eq!(sent, ["A".repeat(449), "B".repeat(450)], "the blank part between them is never sent");
    assert!(rig.is_showing("msg_list", "first\n\nsecond"), "{:?}", rig.seen("msg_list"));
}

#[test]
fn a_refused_subscription_turns_live_translation_back_off() {
    let mut rig = booted();
    rig.host.broker.forget_app(APP);
    assert!(rig.host.call(&rig.app, "set_live", &[true.into()]));
    let mut was_refused = false;
    for _ in 0..10 {
        for ask in rig.host.process() {
            match ask {
                BrokerAsk::PermissionBatch { request, perms, .. } => {
                    rig.host.answer_stock_batch(&rig.app, APP, request, &perms, &perms, &mut rig.trace.audit);
                }
                BrokerAsk::Subscribe { reply, .. } => {
                    rig.reply(reply, Err("this mini-app is not attached to a room"));
                    rig.host.cx.with_vm_and_async(|_| {});
                    was_refused = true;
                }
                BrokerAsk::Used { .. } => {}
                _ => panic!("unexpected request while turning on live translation"),
            }
        }
        if was_refused { break; }
    }
    rig.settle();
    assert!(was_refused);
    assert_eq!(rig.text("live_btn"), "○ Translate new messages");
    assert_eq!(rig.text("live_note"), "Off: this mini-app is not attached to a room. Turn it on to ask again.");
    assert_eq!(rig.stored("settings.json")["is_live"], false);
    rig.host.publish_grants(&rig.app, APP);
    rig.settle();
    assert!(rig.trace.audit.subscriptions.is_empty(), "a refused subscription is not retried automatically");
}

#[test]
fn changing_the_reply_language_or_service_cancels_a_pending_reply() {
    let mut rig = booted();
    rig.pick("reply", ES);
    rig.type_into("reply_input", "See you");
    rig.act("translate_reply", &[]);
    rig.pick("reply", DE);
    assert_eq!(rig.text("reply_status"), "Tap Translate to translate your reply into Deutsch.");
    rig.translate("Nos vemos", "en");
    assert!(!rig.is_visible("result_wrap"), "the Spanish reply no longer applies");
    rig.act("translate_reply", &[]);
    rig.tick();
    rig.press("service_btn");
    rig.tap_chip("service_choice", 1);
    rig.type_into("server_input", "https://translate.example.org");
    rig.click("Save server");
    assert_eq!(rig.text("reply_status"), "Translation service changed. Tap Translate to try again.");
    rig.translate("Bis dann", "en");
    assert!(!rig.is_visible("result_wrap"), "a reply from the old site is dropped");
    assert_eq!(rig.input("reply_input"), "See you");
}

#[test]
fn recent_messages_show_relative_times() {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
    let mut rig = Rig::launch(false);
    let mut fresh = message("$now:test", "Ana", "Ahora");
    fresh["ts"] = now.into();
    let mut earlier = message("$min:test", "Luis", "Antes");
    earlier["ts"] = (now - 5 * 60_000).into();
    let mut hours = message("$hrs:test", "Eva", "Hace horas");
    hours["ts"] = (now - 3 * 3_600_000).into();
    rig.queue_messages(&[hours, earlier, fresh]);
    rig.boot();
    assert_eq!(rig.seen("msg_list"), ["A", "Ana", "Just now", "Ahora", "L", "Luis", "5m ago", "Antes", "E", "Eva", "3h ago", "Hace horas"]);
}

#[test]
fn corrupt_saved_files_fall_back_to_defaults() {
    let rig = booted();
    let jail = rig.jail();
    std::fs::write(jail.join("settings.json"), r#"{"target":"xx","reply":7,"service":"libre","server":"http://bad","key":5,"is_live":"yes"}"#).unwrap();
    std::fs::write(jail.join("cache.json"), r#"[{"key":"no-separator","text":"x"},{"key":"$m1:test|en","text":"","is_same":false},{"key":"$m2:test|en","text":"Ok","source":"es"},"junk"]"#).unwrap();
    std::fs::write(jail.join("detected.json"), r#"{"es":3}"#).unwrap();
    let mut rig = rig.relaunch();
    rig.queue_messages(&spanish_room());
    rig.boot();
    assert_eq!(rig.text("target_btn"), "English ▼");
    assert_eq!(rig.text("reply_btn"), "English ▼");
    assert_eq!(rig.text("privacy_note"), PRIVACY);
    assert_eq!(rig.text("live_btn"), "○ Translate new messages");
    assert!(rig.is_showing("msg_list", "Ok"), "the one valid cached translation is used");
    assert!(!rig.is_showing("msg_list", "x"));
}

// ---- The shared stock audits, for this app alone

/// A copy of `run_every_stock_app`'s loop body with the Translator arm: the primary action turns on
/// live translation, which asks for room watching and subscribes.
fn run_translator_audit(strict: bool) {
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
    match manifest.id.as_str() {
        "translator" => { assert!(host.call(&app, "set_live", &[true.into()])); }
        _ => unreachable!(),
    }
    host.settle_stock(&app, &manifest.id, &mut audit);
    let expected = match manifest.id.as_str() {
        "translator" => (1, 1),
        _ => (1, 0),
    };
    eprintln!("stock permission audit: {} strict={} initial={} action={} initial_reviews={} action_reviews={}",
        manifest.id, strict, initial_prompts, audit.prompts.len() - initial_prompts, initial_reviews, audit.reviews - initial_reviews);
    if !strict {
        assert_eq!((initial_prompts, audit.prompts.len() - initial_prompts), expected,
            "{} initial and primary-action permission groups: {:?}", manifest.id, audit.prompts);
        assert_eq!((initial_reviews, audit.reviews - initial_reviews), (0, 0),
            "{} must finish each reviewed sink without repeated reviews", manifest.id);
    }
    assert!(audit.prompts.len() <= manifest.permissions.len(), "{} exceeds its finite declared group budget", manifest.id);
    assert!(audit.prompts.iter().all(|permission| manifest.declares(*permission)), "{} prompts only for its declared groups", manifest.id);
    assert!(audit.reviews <= 3, "{} exceeds the finite review budget for its primary path", manifest.id);
    assert!(!audit.services.is_empty() || !audit.subscriptions.is_empty(), "{} must complete a real service path", manifest.id);
    assert_eq!(audit.subscriptions, ["on_room_message"]);
    let services = audit.services.len();
    let subscriptions = audit.subscriptions.len();
    host.publish_grants(&app, &manifest.id);
    host.settle_stock(&app, &manifest.id, &mut audit);
    assert_eq!(audit.services.len(), services, "{} reloads on an unrelated grant", manifest.id);
    assert_eq!(audit.subscriptions.len(), subscriptions, "{} subscribes again on an unrelated grant", manifest.id);
    let mut hooks = audit.subscriptions.clone(); hooks.sort(); hooks.dedup();
    assert_eq!(hooks.len(), audit.subscriptions.len(), "{} has duplicate subscriptions", manifest.id);
}

#[test]
fn translator_passes_the_stock_audit() {
    run_translator_audit(false);
}

#[test]
fn translator_passes_the_strict_stock_audit() {
    run_translator_audit(true);
}

#[test]
fn grouped_startup_finishes_a_denial_and_refreshes_after_a_new_approval() {
    let (expected, read) = (vec![Permission::MatrixRoomRead, Permission::DeviceInfo], Permission::MatrixRoomRead);
    let mut host = Harness::new();
    let (app, _) = host.launch_stock_permissions(APP, false, false);
    for permission in &expected { host.permissions.set(APP, *permission, GrantState::Denied); }
    host.publish_grants(&app, APP);
    host.boot();
    let (request, perms) = host.take_stock_batch(APP);
    assert_eq!(perms, expected, "translator requests only its startup dependencies");
    let mut audit = StockAudit::default();
    host.answer_stock_batch(&app, APP, request, &perms, &[], &mut audit);
    host.settle_stock(&app, APP, &mut audit);
    assert!(!host.stock_callbacks_paused());
    assert!(audit.services.is_empty(), "nothing refused is read");
    assert!(audit.subscriptions.is_empty());
    assert_eq!(app.widget(&host.cx, ids!(list_status)).text(),
        "Reading this room wasn't allowed. Tap Refresh to ask again, or allow it in Mini Apps settings.");
    host.publish_grants(&app, APP);
    host.settle_stock(&app, APP, &mut audit);
    assert_eq!(audit.batch_prompts, 1, "no asking again from a permission-change callback");
    for permission in &perms { host.permissions.set(APP, *permission, GrantState::Ask); }
    host.widget_click(&app, "Refresh", &[]);
    host.settle_stock(&app, APP, &mut audit);
    assert!(audit.services.contains(&"matrix.room.messages.read".to_string()));
    assert!(audit.batch_prompts <= 2);
    let reads = audit.services.len();
    for permission in &perms { host.permissions.set(APP, *permission, GrantState::Denied); }
    host.publish_grants(&app, APP);
    host.settle_stock(&app, APP, &mut audit);
    assert_eq!(audit.services.len(), reads, "no rereading revoked data");
    for permission in &perms { host.permissions.set(APP, *permission, GrantState::Granted); }
    host.publish_grants(&app, APP);
    host.settle_stock(&app, APP, &mut audit);
    assert!(audit.services.len() > reads, "refreshes after reapproval");
    assert_eq!(read, expected[0]);
}

#[test]
fn a_denied_read_explains_itself_once_in_the_empty_list() {
    let mut rig = Rig::launch(false);
    rig.answers.is_batch_allowed = false;
    rig.boot();
    assert!(rig.seen("msg_list").is_empty());
    assert_eq!(rig.seen("empty_wrap"), ["🔒", "Can't read this room",
        "Allow Translator to read this room's messages. You can still translate your own reply below.", "Ask again"]);
    assert!(!rig.is_visible("list_status_wrap"));
    rig.answers.is_batch_allowed = true;
    rig.queue_messages(&spanish_room());
    rig.click("Ask again");
    assert!(rig.is_showing("msg_list", "Hola amigos"));
}

#[test]
fn grouped_startup_uses_the_selected_read_without_requesting_unselected_groups() {
    let (expected, read) = (vec![Permission::MatrixRoomRead, Permission::DeviceInfo], Permission::MatrixRoomRead);
    let mut host = Harness::new();
    let (app, _) = host.launch_stock_permissions(APP, false, false);
    for permission in &expected { host.permissions.set(APP, *permission, GrantState::Denied); }
    host.publish_grants(&app, APP);
    host.boot();
    let (request, perms) = host.take_stock_batch(APP);
    assert_eq!(perms, expected);
    let mut audit = StockAudit::default();
    host.answer_stock_batch(&app, APP, request, &perms, &[read], &mut audit);
    host.settle_stock(&app, APP, &mut audit);
    assert_eq!(audit.batch_prompts, 1);
    assert_eq!(audit.prompts, [read], "an unselected group is never granted silently");
    assert_eq!(audit.services, ["matrix.room.messages.read"], "the selected read is used; unselected device facts are not");
    assert!(audit.subscriptions.is_empty());
    assert!(!host.stock_callbacks_paused());
    assert_eq!(host.permissions.state(APP, Permission::DeviceInfo), GrantState::Denied);
    let reads = audit.services.len();
    host.publish_grants(&app, APP);
    host.settle_stock(&app, APP, &mut audit);
    assert_eq!(audit.services.len(), reads, "a completed partial setup does not reload");
    assert_eq!(audit.batch_prompts, 1, "no nagging about unselected startup features");
}

#[test]
fn a_strict_refusal_finishes_and_retries_only_from_refresh() {
    let mut host = Harness::new();
    host.permissions.set_strict(true);
    let (app, _) = host.launch_stock_permissions(APP, false, false);
    host.call(&app, "on_app_resize", &[460_f64.into(), 700_f64.into()]);
    host.boot();
    host.widget_click(&app, "Refresh", &[]);
    assert!(host.deny_stock_pending(&app, APP) > 0, "translator exposes its first needed permission");
    assert!(!host.stock_callbacks_paused(), "the refusal finishes the original callback");
    assert!(host.broker.failures().iter().all(|failure| failure.error == "Permission denied."));
    host.publish_grants(&app, APP);
    for ask in host.process() { assert!(matches!(ask, BrokerAsk::Used { .. }), "no automatic retry of a denied request"); }
    let declared = host.apps.get(APP).unwrap().permissions.clone();
    for permission in declared { host.permissions.set(APP, Permission::from_str(&permission).unwrap(), GrantState::Ask); }
    host.widget_click(&app, "Refresh", &[]);
    let mut audit = StockAudit::default();
    host.settle_stock(&app, APP, &mut audit);
    assert!(!host.stock_callbacks_paused());
    assert!(audit.services.contains(&"matrix.room.messages.read".to_string()), "the visible retry completes approved work");
}

#[test]
fn live_updates_resume_after_revocation_and_regrant_without_prompting_again() {
    let mut host = Harness::new();
    let (app, _) = host.launch_stock(APP, false);
    host.boot();
    let mut audit = StockAudit::default();
    host.settle_stock(&app, APP, &mut audit);
    assert!(host.call(&app, "set_live", &[true.into()]));
    host.settle_stock(&app, APP, &mut audit);
    let hooks = audit.subscriptions.clone();
    assert_eq!(hooks, ["on_room_message"]);
    host.permissions.set(APP, Permission::MatrixRoomWatch, GrantState::Denied);
    host.publish_grants(&app, APP);
    host.settle_stock(&app, APP, &mut audit);
    let before = audit.subscriptions.len();
    host.permissions.set(APP, Permission::MatrixRoomWatch, GrantState::Granted);
    host.publish_grants(&app, APP);
    host.settle_stock(&app, APP, &mut audit);
    let restored = &audit.subscriptions[before..];
    assert_eq!(restored, ["on_room_message"], "live updates resume after regrant");
    assert!(audit.prompts.is_empty());
}

