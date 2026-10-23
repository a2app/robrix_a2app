//! Translated A2UI apps on the real Splash VM, host bridge and broker: user
//! input reaches the A2UI action message the app posts to its room.
#![allow(dead_code, unused_imports)] // the shared stock harness serves several test crates
include!("support/stock_harness.rs");

use a2app_core::manifest::MiniAppManifest;

fn fixture_app(name: &str) -> MiniAppManifest {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/a2ui").join(name);
    let mut manifest = a2app_core::bundle::parse(&std::fs::read_to_string(path).unwrap()).unwrap();
    manifest.id = "a2ui-test".into();
    manifest
}

fn event_messages(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/a2ui").join(name);
    let event: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    event["content"]["messages"].to_string()
}

impl Harness {
    /// The inline timeline copy, tagged with its message like the timeline does.
    fn launch_inline(&mut self, source: &str, tag: Option<&str>) -> WidgetRef {
        let host = self.cx.with_vm(|vm| {
            let value = vm.eval(script! { use mod.widgets.* Splash{} });
            WidgetRef::script_from_value(vm, value)
        });
        makepad_widgets::widget_tree::set_ui_root(&mut self.cx, &host);
        let heap = {
            let mut splash = host.borrow_mut::<Splash>().unwrap();
            splash.set_host_io_only(true);
            splash.set_allow_net(false);
            splash.set_host_tag(&mut self.cx, tag.map(str::to_string));
            self.load_splash(&mut splash, "a2ui-inline", source)
        };
        self.contexts.insert(heap, ContextId::App { account: ACCOUNT.into(), app: "a2ui-inline".into(), room: Some(ROOM.into()) });
        self.stock_hosts.push(host.clone());
        for line in source.lines() {
            if let Some((prefix, _)) = line.split_once(":=")
                && let Some(name) = prefix.split_whitespace().last()
            {
                host.widget(&self.cx, &[LiveId::from_str(name)]);
            }
        }
        host
    }

    /// `launch_stock_permissions` for a translated app instead of a stock one.
    fn launch_a2ui(&mut self, manifest: &MiniAppManifest) -> WidgetRef {
        self.review_effects = true;
        let app = manifest.id.clone();
        let context = ContextId::App { account: ACCOUNT.into(), app: app.clone(), room: Some(ROOM.into()) };
        self.flow.borrow_mut().register_context(&context).unwrap();
        self.apps.insert(manifest.clone());
        for permission in Permission::ALL.into_iter().filter(|permission| manifest.declares(*permission)) {
            self.permissions.set(&app, permission, GrantState::Granted);
        }
        self.permissions.set_matrix_write(true);
        let host = self.cx.with_vm(|vm| {
            let value = vm.eval(script! { use mod.widgets.* Splash{} });
            assert!(!value.is_err(), "host widget startup errors: {:?}", vm.take_errors());
            WidgetRef::script_from_value(vm, value)
        });
        makepad_widgets::widget_tree::set_ui_root(&mut self.cx, &host);
        let jail = self.flow.borrow().context_storage_path(&context).unwrap();
        std::fs::create_dir_all(&jail).unwrap();
        let heap = {
            let mut splash = host.borrow_mut::<Splash>().unwrap();
            splash.set_host_io_only(true);
            splash.set_allow_net(false);
            splash.set_host_tag(&mut self.cx, Some(instance_tag(&app, context.room())));
            splash.set_host_caps(&mut self.cx, self.permissions.granted_caps(manifest));
            splash.set_sandbox_dir(&mut self.cx, Some(jail));
            self.load_splash(&mut splash, &app, &manifest.source)
        };
        self.contexts.insert(heap, context);
        self.panes.insert(heap, PaneState { surface: "dock", side: None, foreground: true, width: 460.0, height: 700.0 });
        self.stock_hosts.push(host.clone());
        for line in manifest.source.lines() {
            if let Some((prefix, _)) = line.split_once(":=")
                && let Some(name) = prefix.split_whitespace().last()
            {
                host.widget(&self.cx, &[LiveId::from_str(name)]);
            }
        }
        let source = host.borrow::<Splash>().unwrap().view.source.clone();
        let vm_id = self.cx.script_ref_vm_id(&source).unwrap();
        self.cx.with_script_vm_id(vm_id, |vm| vm.bx.captured_errors = Some(Vec::new()));
        host
    }

    fn type_into(&mut self, app: &WidgetRef, current: &str, text: &str) {
        let input = visible_control(app, current).unwrap_or_else(|| panic!("an input showing {current:?}"));
        self.type_into_widget(app, &input, text);
    }

    fn type_into_widget(&mut self, app: &WidgetRef, input: &WidgetRef, text: &str) {
        input.set_text(&mut self.cx, text);
        let source = app.borrow::<Splash>().unwrap().view.source.clone();
        let vm_id = self.cx.script_ref_vm_id(&source).unwrap();
        let text = self.cx.with_script_vm_id(vm_id, |vm| vm.new_string_with(|_, out| out.push_str(text)));
        self.widget_callback(app, input, id!(on_change), &[text]);
    }

    /// The one message the app posts after the click, approved like a user would.
    fn posted_body(&mut self) -> serde_json::Value {
        let mut asks = self.process();
        if let Some(i) = asks.iter().position(|ask| matches!(ask, BrokerAsk::FlowReview { .. })) {
            let BrokerAsk::FlowReview { request, review } = asks.remove(i) else { unreachable!() };
            self.flow.borrow_mut().approve_effect_once(&review).unwrap();
            asks = self.dispatch(Some(request));
        }
        let (reply, body) = stock_send(asks);
        self.reply(reply, "{}");
        self.cx.with_vm_and_async(|_| {});
        self.process();
        let mut body: serde_json::Value = serde_json::from_str(&body)
            .unwrap_or_else(|e| panic!("posted body is JSON ({e}): {body}"));
        let action = body.get_mut("userAction").or(None).map(|_| "userAction").unwrap_or("action");
        let stamp = body[action].as_object_mut().unwrap().remove("timestamp").expect("a timestamp");
        assert_recent_utc(stamp.as_str().unwrap());
        body
    }
}

/// `YYYY-MM-DDTHH:MM:SSZ` within a few minutes of now.
fn assert_recent_utc(stamp: &str) {
    let digits = |range: std::ops::Range<usize>| stamp[range].parse::<i64>().unwrap();
    assert!(stamp.len() == 20 && stamp.ends_with('Z') && &stamp[10..11] == "T", "{stamp}");
    let (y, m, d) = (digits(0..4), digits(5..7), digits(8..10));
    // Howard Hinnant's days_from_civil.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let days = era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719468;
    let secs = days * 86400 + digits(11..13) * 3600 + digits(14..16) * 60 + digits(17..19);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    assert!((now - secs).abs() < 300, "{stamp} is not now");
}

#[test]
fn v08_booking_form_posts_a_user_action_with_the_edited_data() {
    let manifest = fixture_app("booking_v08.jsonl");
    let mut host = Harness::new();
    let app = host.launch_a2ui(&manifest);
    host.type_into(&app, "Ada", "Grace \"G\" Hopper");
    assert!(visible_control(&app, "Grace \"G\" Hopper").is_some(), "the bound caption follows the input");
    let veg = visible_control(&app, "Vegetarian \"only\"").expect("the checkbox");
    host.widget_callback(&app, &veg, id!(on_click), &[true.into()]);
    host.widget_click(&app, "⚪ Patio", &[]);
    host.widget_click(&app, "⚪ Inside", &[]);
    assert!(visible_control(&app, "🔘 Inside").is_some());
    assert!(visible_control(&app, "⚪ Patio").is_some(), "a single choice replaces the last one");
    host.widget_click(&app, "Book now", &[]);
    let body = host.posted_body();
    assert_eq!(body, serde_json::json!({"userAction": {
        "name": "book_table", "surfaceId": "booking", "sourceComponentId": "book",
        "context": {
            "restaurant": "Luigi's",
            "reservation": {"name": "Grace \"G\" Hopper", "guests": "2", "date": "2026-10-24", "vegetarian": true, "seating": ["inside"]},
            "guests": "2",
        },
    }}));
    assert!(visible_control(&app, "Sent book_table to the room.").is_some());
}

#[test]
fn v08_template_list_buttons_carry_their_own_item() {
    let manifest = fixture_app("list_v08.json");
    let mut host = Harness::new();
    let app = host.launch_a2ui(&manifest);
    assert!(visible_control(&app, "Sushi Go").is_some());
    assert!(visible_control(&app, "4.5").is_some());
    let mut picks = Vec::new();
    fn collect(root: &WidgetRef, text: &str, out: &mut Vec<WidgetRef>) {
        if root.text() == text { out.push(root.clone()); }
        root.children(&mut |_, child| collect(&child, text, out));
    }
    collect(&app, "Pick", &mut picks);
    assert_eq!(picks.len(), 2);
    host.widget_callback(&app, &picks[1], id!(on_click), &[]);
    let body = host.posted_body();
    assert_eq!(body["userAction"]["name"], "select");
    assert_eq!(body["userAction"]["context"], serde_json::json!({"id": "r2"}));
}

#[test]
fn v09_form_posts_an_action_with_typed_values() {
    let manifest = fixture_app("form_v09.jsonl");
    let mut host = Harness::new();
    let app = host.launch_a2ui(&manifest);
    fn inputs(root: &WidgetRef, out: &mut Vec<WidgetRef>) {
        if root.borrow::<TextInput>().is_some() { out.push(root.clone()); }
        root.children(&mut |_, child| inputs(&child, out));
    }
    let mut found = Vec::new();
    inputs(&app, &mut found);
    assert_eq!(found.len(), 1);
    let source = app.borrow::<Splash>().unwrap().view.source.clone();
    let vm_id = host.cx.script_ref_vm_id(&source).unwrap();
    let typed = host.cx.with_script_vm_id(vm_id, |vm| vm.new_string_with(|_, out| out.push_str("me@example.com\n")));
    host.widget_callback(&app, &found[0], id!(on_change), &[typed]);
    host.widget_click(&app, "Sign up", &[]);
    let body = host.posted_body();
    assert_eq!(body["version"], "v0.9");
    let action = &body["action"];
    assert_eq!(action["name"], "signup");
    assert_eq!(action["context"]["email"], "me@example.com\n");
    assert_eq!(action["context"]["news"], true);
    assert_eq!(action["context"]["volume"].as_f64(), Some(3.0));
}

#[test]
fn a_huge_template_list_still_loads_within_the_script_budget() {
    let rows: Vec<serde_json::Value> = (0..1000).map(|i| serde_json::json!({"name": format!("Row {i}")})).collect();
    let payload = serde_json::json!([
        {"surfaceUpdate": {"surfaceId": "big", "components": [
            {"id": "root", "component": {"List": {"children": {"template": {"componentId": "row", "dataBinding": "/rows"}}}}},
            {"id": "row", "component": {"Card": {"child": "row_text"}}},
            {"id": "row_text", "component": {"Text": {"text": {"path": "name"}}}}
        ]}},
        {"dataModelUpdate": {"surfaceId": "big", "path": "/", "contents": []}},
        {"beginRendering": {"surfaceId": "big", "root": "root"}}
    ]);
    let mut text = payload.to_string();
    text.push('\n');
    text.push_str(&serde_json::json!({"version": "v0.9", "updateDataModel": {"surfaceId": "big", "path": "/rows", "value": rows}}).to_string());
    let mut manifest = a2app_core::bundle::parse(&text).unwrap();
    manifest.id = "a2ui-big".into();
    let mut host = Harness::new();
    let app = host.launch_a2ui(&manifest);
    // The list itself is one node; each row is a card and its text.
    assert!(visible_control(&app, "Row 0").is_some());
    assert!(visible_control(&app, "Row 198").is_some());
    assert!(visible_control(&app, "Row 199").is_none());
    assert!(visible_control(&app, "This A2UI surface was too large to show in full.").is_some());
}

fn inputs(root: &WidgetRef, out: &mut Vec<WidgetRef>) {
    if root.borrow::<TextInput>().is_some() { out.push(root.clone()); }
    root.children(&mut |_, child| inputs(&child, out));
}

#[test]
fn matrix_contact_form_posts_its_v09_action() {
    let mut manifest = a2app_core::bundle::parse(&event_messages("contact_event.json")).unwrap();
    manifest.id = "a2ui-contact".into();
    let mut host = Harness::new();
    let app = host.launch_a2ui(&manifest);
    let mut found = Vec::new();
    inputs(&app, &mut found);
    assert_eq!(found.len(), 1);
    host.type_into_widget(&app, &found[0], "Ada Lovelace");
    let subscribe = visible_control(&app, "Subscribe to the newsletter").expect("the checkbox");
    host.widget_callback(&app, &subscribe, id!(on_click), &[true.into()]);
    host.widget_click(&app, "Send", &[]);
    assert_eq!(host.posted_body(), serde_json::json!({"version": "v0.9", "action": {
        "name": "submitContactForm", "surfaceId": "main", "sourceComponentId": "submit",
        "context": {"formId": "contact_form_1", "contact": {"name": "Ada Lovelace", "subscribe": true}},
    }}));
}

/// The contact event as the timeline draws it, with `sendDataModel` on.
fn inline_contact_app() -> MiniAppManifest {
    let messages = event_messages("contact_event.json").replace(r#""surfaceId":"main"}"#, r#""surfaceId":"main","sendDataModel":true}"#);
    assert!(messages.contains("sendDataModel"));
    let mut manifest = a2app_core::builtin::stock(a2app_core::a2ui::APP_ID).expect("the built-in A2UI app");
    manifest.source = a2app_core::a2ui::to_inline_splash(&messages, "$surface:test", None).unwrap().source;
    manifest
}

#[test]
fn inline_buttons_answer_their_message_through_the_a2ui_app() {
    let mut host = Harness::new();
    let app = host.launch_a2ui(&inline_contact_app());
    let mut found = Vec::new();
    inputs(&app, &mut found);
    host.type_into_widget(&app, &found[0], "Alice");
    host.widget_click(&app, "Send", &[]);
    let mut asks = host.process();
    if let Some(i) = asks.iter().position(|ask| matches!(ask, BrokerAsk::FlowReview { .. })) {
        let BrokerAsk::FlowReview { request, review } = asks.remove(i) else { unreachable!() };
        assert_eq!(request.service, "matrix.a2ui_action");
        host.flow.borrow_mut().approve_effect_once(&review).unwrap();
        asks = host.dispatch(Some(request));
    }
    let (event_id, content) = asks.into_iter().find_map(|ask| match ask {
        BrokerAsk::Matrix { call: services::MatrixServiceCall::A2uiAction { event_id, content }, .. } => Some((event_id, content)),
        _ => None,
    }).expect("the broker sends the answer");
    assert_eq!(event_id, "$surface:test");
    let mut body: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert_recent_utc(body["action"].as_object_mut().unwrap().remove("timestamp").unwrap().as_str().unwrap());
    assert_eq!(body, serde_json::json!({"version": "v0.9", "action": {
        "name": "submitContactForm", "surfaceId": "main", "sourceComponentId": "submit",
        "context": {"formId": "contact_form_1", "contact": {"name": "Alice", "subscribe": false}},
    }, "dataModel": {"contact": {"name": "Alice", "subscribe": false}}}));
}

#[test]
fn inline_buttons_ask_before_the_a2ui_app_may_send() {
    let mut host = Harness::new();
    let manifest = inline_contact_app();
    let app = host.launch_a2ui(&manifest);
    host.permissions.set(&manifest.id, Permission::MatrixRoomSend, GrantState::Ask);
    host.widget_click(&app, "Send", &[]);
    let asks = host.process();
    assert!(asks.iter().all(|ask| !matches!(ask, BrokerAsk::Matrix { .. })), "nothing is sent without a grant");
    assert!(asks.iter().any(|ask| matches!(ask, BrokerAsk::Prompt { perm: Permission::MatrixRoomSend, .. })),
        "the user is asked: {}", asks.len());
}

#[test]
fn an_untagged_inline_copy_cannot_post_and_says_how_to() {
    let source = a2app_core::a2ui::to_inline_splash(&event_messages("contact_event.json"), "$surface:test", None).unwrap().source;
    let mut host = Harness::new();
    let app = host.launch_inline(&source, None);
    host.widget_click(&app, "Send", &[]);
    assert!(host.process().into_iter().all(|ask| !matches!(ask, BrokerAsk::Matrix { .. })));
    host.cx.with_vm_and_async(|_| {});
    assert!(visible_control(&app, "This interactive message can't answer from here.").is_some());
}

/// The kitchen-sink surface as the timeline runs it: as the A2UI app, granted.
fn kitchen_sink(host: &mut Harness) -> (WidgetRef, String) {
    let mut manifest = a2app_core::builtin::stock(a2app_core::a2ui::APP_ID).unwrap();
    manifest.source = a2app_core::a2ui::to_inline_splash(&event_messages("kitchen_sink_event.json"), "$sink:test", None).unwrap().source;
    let source = manifest.source.clone();
    (host.launch_a2ui(&manifest), source)
}

fn is_shown(app: &WidgetRef, text: &str) -> bool {
    visible_control(app, text).is_some()
}

/// The text inputs in tree order: name, email, age, the toppings filter, pickup.
fn text_inputs(app: &WidgetRef) -> Vec<WidgetRef> {
    let mut found = Vec::new();
    inputs(app, &mut found);
    found
}

/// Presses Save and returns what the broker sends, or `None` if nothing was.
fn press_save(host: &mut Harness, app: &WidgetRef) -> Option<serde_json::Value> {
    host.widget_click(app, "Save", &[]);
    let mut asks = host.process();
    if let Some(i) = asks.iter().position(|ask| matches!(ask, BrokerAsk::FlowReview { .. })) {
        let BrokerAsk::FlowReview { request, review } = asks.remove(i) else { unreachable!() };
        host.flow.borrow_mut().approve_effect_once(&review).unwrap();
        asks = host.dispatch(Some(request));
    }
    let content = asks.into_iter().find_map(|ask| match ask {
        BrokerAsk::Matrix { call: services::MatrixServiceCall::A2uiAction { content, .. }, .. } => Some(content),
        _ => None,
    })?;
    Some(serde_json::from_str(&content).unwrap())
}

#[test]
fn kitchen_sink_inputs_bind_validate_and_answer() {
    let mut host = Harness::new();
    let (app, source) = kitchen_sink(&mut host);
    let fields = text_inputs(&app);
    assert_eq!(fields.len(), 5, "{source}");
    let (name, email, age, filter, pickup) = (&fields[0], &fields[1], &fields[2], &fields[3], &fields[4]);

    // Live text: the greeting follows the name field.
    host.type_into_widget(&app, name, "Grace");
    assert!(is_shown(&app, "Hello, Grace! You have 3 items."));

    // Input checks: messages show under their fields and block the press.
    host.type_into_widget(&app, name, "");
    assert!(is_shown(&app, "Name is required."));
    host.type_into_widget(&app, email, "nope");
    assert!(is_shown(&app, "Enter a valid email."));
    host.type_into_widget(&app, age, "12");
    assert!(is_shown(&app, "Age must be 18 to 130."));
    assert!(press_save(&mut host, &app).is_none(), "invalid input is never sent");
    assert!(is_shown(&app, "Check the highlighted fields first."));

    // The button's own check still blocks a too-short name.
    host.type_into_widget(&app, name, "G");
    host.type_into_widget(&app, email, "g@example.org");
    host.type_into_widget(&app, age, "36");
    assert!(!is_shown(&app, "Name is required.") && !is_shown(&app, "Enter a valid email."));
    assert!(press_save(&mut host, &app).is_none());
    assert!(is_shown(&app, "Name is too short."));
    host.type_into_widget(&app, name, "Grace");

    // Tabs: the chosen tab's pane and tinted chip show.
    assert!(!is_shown(&app, "Size"));
    let options_off = visible_control(&app, "Options").unwrap();
    host.widget_callback(&app, &options_off, id!(on_click), &[]);
    assert!(is_shown(&app, "Size") && !is_shown(&app, "Name"));

    // Chips, checkboxes with a filter, and radio rows.
    host.widget_click(&app, "Large", &[]);
    host.type_into_widget(&app, filter, "CH");
    assert!(is_shown(&app, "Cheese") && is_shown(&app, "Chili") && !is_shown(&app, "Basil"), "the filter ignores case");
    let cheese = visible_control(&app, "Cheese").unwrap();
    host.widget_callback(&app, &cheese, id!(on_click), &[true.into()]);
    host.widget_click(&app, "⚪ Courier", &[]);
    assert!(is_shown(&app, "🔘 Courier") && is_shown(&app, "⚪ Pickup"));

    // A typed date-time becomes ISO 8601.
    host.type_into_widget(&app, pickup, "2026-10-25 09:30");

    // Back on the first tab, Save answers with the whole edited model.
    let profile = visible_control(&app, "Profile").unwrap();
    host.widget_callback(&app, &profile, id!(on_click), &[]);
    let mut body = press_save(&mut host, &app).expect("a valid form is sent");
    body["action"].as_object_mut().unwrap().remove("timestamp");
    assert_eq!(body["action"]["context"]["user"], serde_json::json!({"name": "Grace", "email": "g@example.org", "age": "36", "count": 3}));
    let order = &body["dataModel"]["order"];
    assert_eq!(order["size"], serde_json::json!(["l"]));
    assert_eq!(order["toppings"], serde_json::json!(["cheese"]));
    assert_eq!(order["delivery"], serde_json::json!(["courier"]));
    assert_eq!(order["pickup"], "2026-10-25T09:30");
    assert_eq!(order["spice"].as_f64(), Some(5.0));
}

/// One v0.9 surface `s` as the timeline runs it (the A2UI app, granted).
fn launch_surface(host: &mut Harness, version: &str, create: serde_json::Value, components: serde_json::Value, model: Option<serde_json::Value>) -> WidgetRef {
    let mut messages = vec![
        serde_json::json!({"version": version, "createSurface": create}),
        serde_json::json!({"version": version, "updateComponents": {"surfaceId": "s", "components": components}}),
    ];
    if let Some(model) = model {
        messages.push(serde_json::json!({"version": version, "updateDataModel": {"surfaceId": "s", "value": model}}));
    }
    let mut manifest = a2app_core::builtin::stock(a2app_core::a2ui::APP_ID).unwrap();
    manifest.source = a2app_core::a2ui::to_inline_splash(&serde_json::Value::Array(messages).to_string(), "$s:test", None).unwrap().source;
    host.launch_a2ui(&manifest)
}

/// Presses `button` and returns the answer the broker sends, if any.
fn press(host: &mut Harness, app: &WidgetRef, button: &str) -> Option<serde_json::Value> {
    host.widget_click(app, button, &[]);
    let mut asks = host.process();
    if let Some(i) = asks.iter().position(|ask| matches!(ask, BrokerAsk::FlowReview { .. })) {
        let BrokerAsk::FlowReview { request, review } = asks.remove(i) else { unreachable!() };
        host.flow.borrow_mut().approve_effect_once(&review).unwrap();
        asks = host.dispatch(Some(request));
    }
    let content = asks.into_iter().find_map(|ask| match ask {
        BrokerAsk::Matrix { call: services::MatrixServiceCall::A2uiAction { content, .. }, .. } => Some(content),
        _ => None,
    })?;
    let mut body: serde_json::Value = serde_json::from_str(&content).unwrap();
    body["action"].as_object_mut().unwrap().remove("timestamp");
    Some(body)
}

#[test]
fn answers_include_inputs_the_model_had_no_key_for() {
    let mut host = Harness::new();
    let app = launch_surface(&mut host, "v0.9", serde_json::json!({"surfaceId": "s", "sendDataModel": true}), serde_json::json!([
        {"id": "root", "component": "Column", "children": ["email", "go"]},
        {"id": "email", "component": "TextField", "label": "Email", "value": {"path": "/form/email"}},
        {"id": "go", "component": "Button", "child": "go_label", "action": {"event": {"name": "go", "context": {"form": {"path": "/form"}}}}},
        {"id": "go_label", "component": "Text", "text": "Go"}
    ]), None);
    host.type_into_widget(&app, &text_inputs(&app)[0], "ada@example.org");
    let body = press(&mut host, &app, "Go").expect("an answer");
    assert_eq!(body["action"]["context"]["form"], serde_json::json!({"email": "ada@example.org"}));
    assert_eq!(body["dataModel"], serde_json::json!({"form": {"email": "ada@example.org"}}));
}

#[test]
fn date_times_accept_iso_8601_and_enforce_bounds() {
    let mut host = Harness::new();
    let app = launch_surface(&mut host, "v0.9", serde_json::json!({"surfaceId": "s"}), serde_json::json!([
        {"id": "root", "component": "Column", "children": ["when", "go"]},
        {"id": "when", "component": "DateTimeInput", "label": "When", "enableDate": true, "enableTime": true,
         "min": "2026-11-01", "max": "2026-11-30", "value": {"path": "/when"}},
        {"id": "go", "component": "Button", "child": "go_label", "action": {"event": {"name": "go", "context": {"when": {"path": "/when"}}}}},
        {"id": "go_label", "component": "Text", "text": "Go"}
    ]), Some(serde_json::json!({"when": "2026-11-05T19:00:00.000Z"})));
    assert_eq!(press(&mut host, &app, "Go").expect("an untouched ISO value passes")["action"]["context"]["when"], "2026-11-05T19:00:00.000Z");
    host.type_into_widget(&app, &text_inputs(&app)[0], "2027-01-05");
    assert!(press(&mut host, &app, "Go").is_none());
    assert!(visible_control(&app, "Pick a value from 2026-11-01 to 2026-11-30.").is_some());
    host.type_into_widget(&app, &text_inputs(&app)[0], "next tuesday");
    assert!(visible_control(&app, "Use the format YYYY-MM-DDTHH:MM, e.g. 2026-10-23T14:30.").is_some());
}

#[test]
fn radios_stay_chosen_and_context_text_follows_inputs() {
    let mut host = Harness::new();
    let app = launch_surface(&mut host, "v0.9", serde_json::json!({"surfaceId": "s"}), serde_json::json!([
        {"id": "root", "component": "Column", "children": ["qty", "pick", "go"]},
        {"id": "qty", "component": "TextField", "label": "Quantity", "value": {"path": "/qty"}},
        {"id": "pick", "component": "ChoicePicker", "label": "Contact", "options": [{"label": "Email", "value": "email"}, {"label": "Phone", "value": "phone"}], "value": {"path": "/via"}},
        {"id": "go", "component": "Button", "child": "go_label", "action": {"event": {"name": "go", "context": {
            "summary": {"call": "formatString", "args": {"value": "${/qty} units"}}, "via": {"path": "/via"}}}}},
        {"id": "go_label", "component": "Text", "text": "Go"}
    ]), Some(serde_json::json!({"qty": "1", "via": ["email"]})));
    host.widget_click(&app, "🔘 Email", &[]);
    assert!(visible_control(&app, "🔘 Email").is_some(), "clicking the chosen radio keeps it chosen");
    host.type_into_widget(&app, &text_inputs(&app)[0], "7");
    let body = press(&mut host, &app, "Go").expect("an answer");
    assert_eq!(body["action"]["context"], serde_json::json!({"summary": "7 units", "via": ["email"]}));
}

#[test]
fn unsupported_functions_post_nothing_and_v1_binds_only_at_paths() {
    let mut host = Harness::new();
    let app = launch_surface(&mut host, "v1.0", serde_json::json!({"surfaceId": "s", "dataModel": {"x": "VAL"}}), serde_json::json!([
        {"id": "root", "component": "Column", "children": ["copy", "go"]},
        {"id": "copy", "component": "Button", "child": "copy_label", "action": {"functionCall": {"@call": "copyText", "args": {}}}},
        {"id": "copy_label", "component": "Text", "text": "Copy"},
        {"id": "go", "component": "Button", "child": "go_label", "action": {"event": {"name": "go", "context": {
            "data": {"path": "/x"}, "bound": {"@path": "/x"}, "escaped": {"@@path": "/x"}}}}},
        {"id": "go_label", "component": "Text", "text": "Go"}
    ]), None);
    assert!(press(&mut host, &app, "Copy").is_none(), "a function call never reaches the agent");
    assert!(visible_control(&app, "This button runs a function Robrix doesn't support.").is_some());
    let body = press(&mut host, &app, "Go").expect("an answer");
    assert_eq!(body["version"], "v1.0");
    assert_eq!(body["action"]["context"], serde_json::json!({"data": {"path": "/x"}, "bound": "VAL", "escaped": {"@path": "/x"}}));
}

#[test]
fn inline_surfaces_export_what_the_user_entered() {
    let mut host = Harness::new();
    let app = launch_surface(&mut host, "v0.9", serde_json::json!({"surfaceId": "s"}), serde_json::json!([
        {"id": "root", "component": "Column", "children": ["name", "ok"]},
        {"id": "name", "component": "TextField", "label": "Name", "value": {"path": "/name"}},
        {"id": "ok", "component": "CheckBox", "label": "OK", "value": {"path": "/ok"}}
    ]), Some(serde_json::json!({"name": "", "ok": false})));
    host.type_into_widget(&app, &text_inputs(&app)[0], "Grace");
    let state = app.widget(&host.cx, &[LiveId::from_str(a2app_core::a2ui::STATE_WIDGET)]).text();
    let edits = a2app_core::a2ui::Edits::parse(&state, 1).unwrap_or_else(|| panic!("exported state parses: {state:?}"));
    let mut values = edits.values.clone();
    values.sort_by(|a, b| a.1.cmp(&b.1));
    assert_eq!(values, [("s".into(), "/name".into(), serde_json::json!("Grace")), ("s".into(), "/ok".into(), serde_json::json!(false))]);
}

#[test]
fn iso_bounds_compare_real_characters_and_passwords_stay_private() {
    let mut host = Harness::new();
    let app = launch_surface(&mut host, "v0.9", serde_json::json!({"surfaceId": "s"}), serde_json::json!([
        {"id": "root", "component": "Column", "children": ["when", "secret", "go"]},
        {"id": "when", "component": "DateTimeInput", "label": "When", "enableDate": true, "enableTime": true,
         "min": "2026-11-01", "max": "2026-11-30", "value": {"path": "/when"}},
        {"id": "secret", "component": "TextField", "label": "Password", "variant": "obscured", "value": {"path": "/pw"}},
        {"id": "go", "component": "Button", "child": "go_label", "action": {"event": {"name": "go"}}},
        {"id": "go_label", "component": "Text", "text": "Go"}
    ]), Some(serde_json::json!({"when": "2026-11-01T19:00:00.000Z", "pw": ""})));
    assert!(press(&mut host, &app, "Go").is_some(), "a time on the first allowed day is in range");
    host.type_into_widget(&app, &text_inputs(&app)[0], "2026-11-30 10:00");
    assert!(visible_control(&app, "Pick a value from 2026-11-01 to 2026-11-30.").is_none(), "and so is a time on the last");
    host.type_into_widget(&app, &text_inputs(&app)[0], "2026-12-01 10:00");
    assert!(visible_control(&app, "Pick a value from 2026-11-01 to 2026-11-30.").is_some());
    host.type_into_widget(&app, &text_inputs(&app)[1], "hunter2");
    let state = app.widget(&host.cx, &[LiveId::from_str(a2app_core::a2ui::STATE_WIDGET)]).text();
    assert!(state.contains("/when") && !state.contains("hunter2") && !state.contains("/pw"), "{state}");
}
