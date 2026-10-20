//! Link Library: the real stock source through the Splash VM, host bridge and broker.
#![allow(dead_code, unused_imports)] // the shared stock harness serves several test crates
include!("support/stock_harness.rs");

use std::time::{SystemTime, UNIX_EPOCH};

const APP: &str = "link-library";
const MINUTE: u64 = 60_000;
const HOUR: u64 = 3_600_000;
const LIVE_LINK: &str = r#"[{"room_id":"!private:test","event_id":"$link:test","sender":"alice","sender_id":"@alice:test","sender_name":"Alice","body":"Notes: https://example.org/guide","ts":1720000000000,"msgtype":"m.text","is_own":false}]"#;

fn now_ms() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64 }

/// A `matrix.read_messages` / `matrix.older_messages` row; `sender` is the localpart and `sender_name` the display name.
fn message(id: &str, name: &str, body: &str, ts: u64) -> serde_json::Value {
    let mut row = bare_message(id, name, body, ts);
    row["sender_name"] = name.into();
    row
}

/// The same row from an older host, without `sender_name`.
fn bare_message(id: &str, name: &str, body: &str, ts: u64) -> serde_json::Value {
    let user = name.to_lowercase();
    serde_json::json!({"event_id": id, "sender": user, "sender_id": format!("@{user}:test"), "body": body,
        "ts": ts, "msgtype": "m.text", "room_id": ROOM})
}

/// An `on_room_message` row, which also carries the display name.
fn live(id: &str, user: &str, display: &str, body: &str, ts: u64) -> serde_json::Value {
    serde_json::json!({"room_id": ROOM, "event_id": id, "sender": user, "sender_id": format!("@{user}:test"),
        "sender_name": display, "body": body, "ts": ts, "msgtype": "m.text", "is_own": false})
}

/// The latest messages, oldest first, as Robrix answers `matrix.read_messages`.
fn window(now: u64) -> serde_json::Value {
    serde_json::json!({"messages": [
        message("$m1:test", "Alice", "Release notes: https://github.com/makepad/makepad/releases/tag/v1.0.", now - 5 * HOUR),
        message("$m2:test", "Bob", "https://docs.rs/makepad-widgets", now - 4 * HOUR),
        message("$m3:test", "Carol", "Same thing (HTTPS://GitHub.com/makepad/makepad/releases/tag/v1.0) — worth a read", now - 3 * HOUR),
        message("$m4:test", "Alice", "Join us at https://matrix.to/#/#robrix:matrix.org!", now - 2 * HOUR),
        message("$m5:test", "Dave", "No links here, just chatting", now - HOUR),
        message("$m6:test", "Bob", "Try www.rust-lang.org, and see https://en.wikipedia.org/wiki/Rust_(programming_language)).", now - 30 * MINUTE),
    ]})
}

/// Several sites with two or more links each, so the site chips show; rows from an older host, without display names.
fn sites_window(now: u64) -> serde_json::Value {
    serde_json::json!({"messages": [
        bare_message("$c1:test", "Ann", "https://github.com/a", now - 6 * MINUTE),
        bare_message("$c2:test", "Ann", "https://docs.rs/a", now - 5 * MINUTE),
        bare_message("$c3:test", "Ben", "https://github.com/b", now - 4 * MINUTE),
        bare_message("$c4:test", "Ben", "https://example.org/a", now - 3 * MINUTE),
        bare_message("$c5:test", "Cy", "https://docs.rs/b", now - 2 * MINUTE),
        bare_message("$c6:test", "Cy", "https://www.github.com/c", now - MINUTE),
    ]})
}

fn collect_texts(widget: &WidgetRef, out: &mut Vec<String>) {
    let text = widget.text();
    if !text.is_empty() { out.push(text); }
    let mut children = Vec::new();
    widget.children(&mut |_, child| children.push(child));
    for child in children { collect_texts(&child, out); }
}

fn children(host: &Harness, app: &WidgetRef, list: &str) -> Vec<WidgetRef> {
    let list = app.widget(&host.cx, &[LiveId::from_str(list)]);
    let mut children = Vec::new();
    list.children(&mut |_, child| children.push(child));
    children
}

/// The texts of each rendered child of a list, in order.
fn items(host: &Harness, app: &WidgetRef, list: &str) -> Vec<Vec<String>> {
    children(host, app, list).iter().map(|child| { let mut out = Vec::new(); collect_texts(child, &mut out); out }).collect()
}

/// The rendered rows; an empty list draws a blank spacer, which isn't a row.
fn rows(host: &Harness, app: &WidgetRef) -> Vec<Vec<String>> {
    items(host, app, "link_list").into_iter().filter(|row| !row.is_empty()).collect()
}

fn sites(host: &Harness, app: &WidgetRef) -> Vec<String> {
    rows(host, app).into_iter().map(|row| row[0].clone()).collect()
}

/// The rendered row whose site line starts with `site`.
fn row(host: &Harness, app: &WidgetRef, site: &str) -> Vec<String> {
    rows(host, app).into_iter().find(|row| row[0].starts_with(site)).unwrap_or_else(|| panic!("a row for {site}"))
}

fn chips(host: &Harness, app: &WidgetRef) -> Vec<String> {
    items(host, app, "chip_list").into_iter().map(|chip| chip.join(" ")).collect()
}

/// The "shared N×" count in a row's meta line, or 1 when it has none.
fn shared_count(meta: &str) -> u32 {
    meta.split("shared ").nth(1).map(|n| n.trim_end_matches('×').parse().unwrap()).unwrap_or(1)
}

/// The background color of a rendered list child, as packed RGBA.
fn bg_color(host: &mut Harness, app: &WidgetRef, widget: &WidgetRef) -> u32 {
    let source = app.borrow::<Splash>().unwrap().view.source.clone();
    let vm_id = host.cx.script_ref_vm_id(&source).unwrap();
    host.cx.with_script_vm_id(vm_id, |vm| {
        let bg = vm.bx.heap.value(widget.script_source(), id!(draw_bg).into(), NoTrap);
        vm.bx.heap.value(bg.as_object().unwrap(), id!(color).into(), NoTrap).as_color().unwrap()
    })
}

fn text(host: &Harness, app: &WidgetRef, name: &str) -> String {
    app.widget(&host.cx, &[LiveId::from_str(name)]).text()
}

fn shown(host: &Harness, app: &WidgetRef, name: &str) -> bool {
    app.widget(&host.cx, &[LiveId::from_str(name)]).visible()
}

fn library(host: &Harness, context: &ContextId) -> serde_json::Value {
    let path = host.flow.borrow().context_storage_path(context).unwrap().join("library.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn saved_urls(saved: &serde_json::Value) -> Vec<String> {
    saved["links"].as_array().unwrap().iter().map(|link| link["url"].as_str().unwrap().to_owned()).collect()
}

fn saved_link(saved: &serde_json::Value, url: &str) -> serde_json::Value {
    saved["links"].as_array().unwrap().iter().find(|link| link["url"] == url)
        .unwrap_or_else(|| panic!("{url} is in the saved library")).clone()
}

fn write_library(host: &Harness, context: &ContextId, text: &str) {
    host.flow.borrow_mut().register_context(context).unwrap();
    let jail = host.flow.borrow().context_storage_path(context).unwrap();
    std::fs::create_dir_all(&jail).unwrap();
    std::fs::write(jail.join("library.json"), text).unwrap();
}

fn room_context() -> ContextId {
    ContextId::App { account: ACCOUNT.into(), app: APP.into(), room: Some(ROOM.into()) }
}

/// A granted instance that read `latest` and finished its sliced work, ready for interaction.
fn open_library(host: &mut Harness, latest: serde_json::Value) -> (WidgetRef, ContextId, StockAudit) {
    host.queue_matrix_reply("matrix.room.messages.read", latest);
    let (app, context) = host.launch_stock(APP, false);
    host.boot();
    assert!(host.call(&app, "on_app_resize", &[460_f64.into(), 700_f64.into()]));
    let mut audit = StockAudit::default();
    host.settle_stock(&app, APP, &mut audit);
    drain(host, &app, &mut audit);
    (app, context, audit)
}

/// Closes the instance and opens the same room's library again, reading `latest`.
fn reopen(host: &mut Harness, app: &WidgetRef, context: &ContextId, latest: serde_json::Value) -> WidgetRef {
    host.retire_stock(app, context);
    host.queue_matrix_reply("matrix.room.messages.read", latest);
    let (next, _) = host.launch_stock(APP, false);
    host.boot();
    assert!(host.call(&next, "on_app_resize", &[460_f64.into(), 700_f64.into()]));
    let mut audit = StockAudit::default();
    host.settle_stock(&next, APP, &mut audit);
    drain(host, &next, &mut audit);
    next
}

/// Mirrors `launch_stock_permissions` for a context the stock launcher doesn't build.
fn launch_in(host: &mut Harness, context: ContextId, granted: bool) -> WidgetRef {
    host.review_effects = true;
    let manifest = a2app_core::builtin::stock(APP).unwrap();
    host.flow.borrow_mut().register_context(&context).unwrap();
    host.apps.insert(manifest.clone());
    for permission in Permission::ALL.into_iter().filter(|permission| granted && manifest.declares(*permission)) {
        host.permissions.set(APP, permission, GrantState::Granted);
    }
    let widget = host.cx.with_vm(|vm| {
        let value = vm.eval(script! { use mod.widgets.* Splash{} });
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
    host.contexts.insert(heap, context);
    host.panes.insert(heap, PaneState { surface: "dock", side: None, foreground: true, width: 460.0, height: 700.0 });
    host.stock_hosts.push(widget.clone());
    for line in manifest.source.lines() {
        if let Some((prefix, _)) = line.split_once(":=") {
            if let Some(name) = prefix.split_whitespace().last() { widget.widget(&host.cx, &[LiveId::from_str(name)]); }
        }
    }
    let source = widget.borrow::<Splash>().unwrap().view.source.clone();
    let vm_id = host.cx.script_ref_vm_id(&source).unwrap();
    host.cx.with_script_vm_id(vm_id, |vm| vm.bx.captured_errors = Some(Vec::new()));
    widget
}

/// Approves the exact-effect review of one tap and fixtures the native result,
/// so a test never opens a browser or overwrites the clipboard.
fn finish_reviewed(host: &mut Harness, context: &ContextId, service: &str, result: Result<&str, &str>) -> serde_json::Value {
    let (request, review) = effect_review(host.process());
    assert_eq!(request.service, service);
    let args: serde_json::Value = serde_json::from_str(&request.args_json).unwrap();
    host.flow.borrow_mut().approve_effect_once(&review).unwrap();
    host.flow.borrow_mut().commit_effect_for_activation(context, review.epoch, review.recipient.as_ref(), review.action.as_ref(), &args).unwrap();
    let reply = Reply { heap_key: request.heap_key, req_id: request.req_id };
    match result {
        Ok(data) => host.reply(reply, data),
        Err(error) => services::respond(&mut host.cx, reply, Err(error)),
    }
    host.cx.with_vm_and_async(|_| {});
    host.assert_callback_errors(service);
    args
}

/// Approves a navigation tap's review and returns the one host action it asked for.
fn host_action(host: &mut Harness) -> (Reply, HostAction) {
    let mut pending: VecDeque<BrokerAsk> = host.process().into();
    let mut found = None;
    while let Some(ask) = pending.pop_front() {
        match ask {
            BrokerAsk::FlowReview { request, review } => {
                assert!(request.service.starts_with("nav."));
                host.flow.borrow_mut().approve_effect_session(&review, SharingDuration::RobrixSession).unwrap();
                pending.extend(host.dispatch(Some(request)));
            }
            BrokerAsk::HostAction { reply, action, .. } => assert!(found.replace((reply, action)).is_none()),
            BrokerAsk::Used { .. } => {}
            _ => panic!("a navigation tap makes exactly one host action"),
        }
    }
    found.expect("the tap reached the host")
}

fn answer(host: &mut Harness, reply: Reply, data: &str) {
    host.reply(reply, data);
    host.cx.with_vm_and_async(|_| {});
    host.assert_callback_errors("reply");
}

/// Approves the sharing review of the one pending older page and returns its read.
fn pending_page(host: &mut Harness, before: Option<&str>) -> Reply {
    for _ in 0..10 {
        let mut pending: VecDeque<BrokerAsk> = host.process().into();
        while let Some(ask) = pending.pop_front() {
            match ask {
                BrokerAsk::FlowReview { request, review } => {
                    assert_eq!(request.service, "matrix.older_messages");
                    assert_eq!(review.recipient, Some(Recipient::network_origin("https://homeserver.test").unwrap()),
                        "a page anchored on an event id is reviewed for the homeserver");
                    host.flow.borrow_mut().approve_effect_session(&review, SharingDuration::RobrixSession).unwrap();
                    pending.extend(host.dispatch(Some(request)));
                }
                BrokerAsk::Matrix { reply, call: services::MatrixServiceCall::OlderMessages { before: asked, limit }, .. } => {
                    assert_eq!(asked.as_deref(), before, "pages walk back from the oldest message read");
                    assert_eq!(limit, 50);
                    assert!(pending.iter().all(|ask| matches!(ask, BrokerAsk::Used { .. })), "one page at a time");
                    return reply;
                }
                BrokerAsk::Used { .. } => {}
                _ => panic!("a scan only reads older pages"),
            }
        }
    }
    panic!("the scan did not request an older page");
}

fn answer_page(host: &mut Harness, before: Option<&str>, page: serde_json::Value) {
    let reply = pending_page(host, before);
    answer(host, reply, &page.to_string());
}

/// Runs startup until the latest-messages read, answering subscriptions on the way.
fn next_read(host: &mut Harness, hooks: &mut Vec<String>) -> (Reply, services::MatrixServiceCall) {
    for _ in 0..20 {
        let mut found = None;
        for ask in host.process() {
            match ask {
                BrokerAsk::Matrix { reply, call, .. } => assert!(found.replace((reply, call)).is_none(), "one read at a time"),
                BrokerAsk::Subscribe { reply, hook, .. } => {
                    hooks.push(hook.into());
                    host.reply(reply, r#"{"subscribed":true}"#);
                }
                BrokerAsk::Used { .. } => {}
                _ => panic!("startup only reads and subscribes"),
            }
        }
        if let Some(found) = found { return found; }
    }
    panic!("no latest-messages read");
}

/// Fires pending timers until the app's sliced work and saves are done.
fn drain(host: &mut Harness, app: &WidgetRef, audit: &mut StockAudit) {
    for _ in 0..80 {
        if host.cx.script_data.timers.timers.is_empty() { return; }
        host.boot();
        host.settle_stock(app, APP, audit);
    }
    panic!("the app kept scheduling work");
}

fn no_requests(host: &mut Harness) {
    for ask in host.process() {
        assert!(matches!(ask, BrokerAsk::Used { .. }), "no request may start here");
    }
}

/// A homeserver page: up to 50 messages strictly older than `before` (all, if none), oldest first.
fn page_of(timeline: &[serde_json::Value], before: Option<&str>) -> serde_json::Value {
    let end = match before {
        Some(id) => timeline.iter().position(|m| m["event_id"] == id).expect("the anchor is in the timeline"),
        None => timeline.len(),
    };
    let start = end.saturating_sub(50);
    serde_json::json!({"messages": timeline[start..end].to_vec(), "has_more": start > 0})
}

/// Taps Scan whenever it shows and answers every page from `timeline` until the scan ends.
fn scan_to_start(host: &mut Harness, app: &WidgetRef, timeline: &[serde_json::Value]) -> Vec<Option<String>> {
    let mut anchors = Vec::new();
    for _ in 0..200 {
        if text(host, app, "scan_line").starts_with("Reached") { return anchors; }
        if shown(host, app, "scan_btn_wrap") {
            host.broker.forget_app(APP);
            host.widget_click(app, "Scan older messages", &[]);
        }
        let mut pending: VecDeque<BrokerAsk> = host.process().into();
        while let Some(ask) = pending.pop_front() {
            match ask {
                BrokerAsk::FlowReview { request, review } => {
                    host.flow.borrow_mut().approve_effect_session(&review, SharingDuration::RobrixSession).unwrap();
                    pending.extend(host.dispatch(Some(request)));
                }
                BrokerAsk::Matrix { reply, call: services::MatrixServiceCall::OlderMessages { before, .. }, .. } => {
                    anchors.push(before.clone());
                    answer(host, reply, &page_of(timeline, before.as_deref()).to_string());
                }
                BrokerAsk::Used { .. } => {}
                _ => panic!("a scan only reads older pages"),
            }
        }
        if !host.cx.script_data.timers.timers.is_empty() {
            host.broker.forget_app(APP);
            host.boot();
        }
    }
    panic!("the scan never reached the start");
}

/// Answers a parked setup batch the way Robrix does when no popup is needed: a group answers
/// `permission_setup_status == Granted`, so one blocked ability turns its whole group false,
/// and the grants reach the running app before the reply does.
fn answer_batch_like_robrix(host: &mut Harness, app: &WidgetRef, request: SplashHostRequest, perms: &[Permission]) -> serde_json::Value {
    let mut answers = serde_json::Map::new();
    for perm in perms {
        let status = host.batch_status(APP, &request, *perm);
        assert_ne!(status, Effective::NeedsPrompt, "this batch needs no popup");
        answers.insert(perm.as_str().into(), (status == Effective::Granted).into());
    }
    host.publish_grants(app, APP);
    let granted = answers.values().all(|answer| answer.as_bool() == Some(true));
    let body = serde_json::json!({"granted": granted, "permissions": answers});
    host.reply(Reply { heap_key: request.heap_key, req_id: request.req_id }, &body.to_string());
    host.cx.with_vm_and_async(|_| {});
    host.assert_callback_errors("setup reply");
    body
}

/// Calls a guest function the way Robrix delivers a hook: under the real 64 ms entry budget.
/// Returns the instructions the entry used, out of its 200,000.
/// A production entry keeps its exact instruction cap, but gets ten times the UI's 64 ms
/// wall budget: a debug build descheduled by parallel tests runs several times slower.
fn with_entry_budget<R>(host: &mut Harness, app: &WidgetRef, run: impl FnOnce(&mut Harness) -> R) -> R {
    let source = app.borrow::<Splash>().unwrap().view.source.clone();
    let vm_id = host.cx.script_ref_vm_id(&source).unwrap();
    let limit = std::time::Duration::from_millis(640);
    let previous = host.cx.with_script_vm_id(vm_id, |vm| vm.bx.run_budget.replace(ScriptRunBudget::from_durations(limit, limit, 512)));
    let result = run(host);
    host.cx.with_script_vm_id(vm_id, |vm| vm.bx.run_budget = previous);
    result
}

fn call_production(host: &mut Harness, app: &WidgetRef, name: &str, args: &[&str]) -> usize {
    let called = with_entry_budget(host, app, |host| {
        app.borrow_mut::<Splash>().unwrap().call_script_fn_with_strings(&mut host.cx, LiveId::from_str(name), args)
    });
    assert!(called, "{name} exists");
    let used = instructions_used(host, app);
    host.cx.with_vm_and_async(|_| {});
    host.assert_callback_errors(name);
    used
}

/// Types into the search box through its own on_change, under the entry budget above.
fn type_query(host: &mut Harness, app: &WidgetRef, query: &str) {
    let search = app.widget(&host.cx, &[LiveId::from_str("search")]);
    search.set_text(&mut host.cx, query);
    let source = app.borrow::<Splash>().unwrap().view.source.clone();
    let vm_id = host.cx.script_ref_vm_id(&source).unwrap();
    let arg = host.cx.with_script_vm_id(vm_id, |vm| vm.new_string_with(|_, out| out.push_str(query)));
    with_entry_budget(host, app, |host| host.widget_callback(app, &search, id!(on_change), &[arg]));
}

fn instructions_used(host: &mut Harness, app: &WidgetRef) -> usize {
    let source = app.borrow::<Splash>().unwrap().view.source.clone();
    let vm_id = host.cx.script_ref_vm_id(&source).unwrap();
    host.cx.with_script_vm_id_trusted(vm_id, |vm| vm.last_limit_consumed())
}

/// Evaluates the stock source the way Robrix opens an app: `set_text` under the real entry
/// budget, without the harness's longer headless budget.
fn production_eval(host: &mut Harness) -> Vec<String> {
    let manifest = a2app_core::builtin::stock(APP).unwrap();
    let context = room_context();
    host.flow.borrow_mut().register_context(&context).unwrap();
    host.apps.insert(manifest.clone());
    let widget = host.cx.with_vm(|vm| {
        let value = vm.eval(script! { use mod.widgets.* Splash{} });
        WidgetRef::script_from_value(vm, value)
    });
    makepad_widgets::widget_tree::set_ui_root(&mut host.cx, &widget);
    let jail = host.flow.borrow().context_storage_path(&context).unwrap();
    std::fs::create_dir_all(&jail).unwrap();
    let mut splash = widget.borrow_mut::<Splash>().unwrap();
    splash.set_host_io_only(true);
    splash.set_allow_net(false);
    splash.set_host_tag(&mut host.cx, Some(instance_tag(APP, context.room())));
    splash.set_host_caps(&mut host.cx, host.permissions.granted_caps(&manifest));
    splash.set_sandbox_dir(&mut host.cx, Some(jail));
    splash.set_debug_name(APP);
    splash.set_text(&mut host.cx, " ");
    let vm_id = host.cx.script_ref_vm_id(&splash.view.source).unwrap();
    host.cx.with_script_vm_id_trusted(vm_id, |vm| vm.bx.captured_errors = Some(Vec::new()));
    splash.set_text(&mut host.cx, &manifest.source);
    let mut errors = CALLBACK_ERRORS.with(|errors| std::mem::take(&mut *errors.borrow_mut()));
    errors.extend(host.cx.with_script_vm_id_trusted(vm_id, |vm| vm.take_errors()).into_iter().map(|error| format!("{error:?}")));
    drop(splash);
    widget.set_text(&mut host.cx, "");
    errors
}

/// A saved library, newest first: each link keeps `shares` detailed shares, `ids` bare ids,
/// and counts two more it no longer tracks.
fn full_library(links: usize, shares: usize, ids: usize) -> String {
    let now = now_ms();
    let snippet = "A long message about the link, written to fill the whole snippet with words so the saved file is as large as it gets: one two three four";
    let rows: Vec<_> = (0..links).map(|i| {
        let site = format!("site-{}.example", i % 40);
        let shares: Vec<_> = (0..shares).map(|k| serde_json::json!({
            "e": format!("$saved-{i}-{k}:test"), "s": format!("person{}", (i + k) % 25),
            "u": format!("@person{}:test", (i + k) % 25), "t": now - (i * 40 + k) as u64 * MINUTE, "x": snippet,
        })).collect();
        let more: Vec<_> = (0..ids).map(|k| serde_json::json!({
            "e": format!("$older-{i}-{k}:test"), "u": format!("@person{}:test", (i + k + 7) % 25), "t": now - (i * 40 + 10 + k) as u64 * MINUTE,
        })).collect();
        serde_json::json!({"url": format!("https://{site}/page/{i}"), "count": shares.len() + ids + 2,
            "first_ts": now - (i * 40 + 39) as u64 * MINUTE, "shares": shares, "more": more})
    }).collect();
    serde_json::json!({"v": 2, "room_id": ROOM, "links": rows, "before": "$saved-oldest:test", "is_at_start": false,
        "scanned": 5000, "newest_ts": now, "gaps": [], "names": []}).to_string()
}

fn dense_messages(prefix: &str, count: usize, per_message: usize, newest: u64) -> Vec<serde_json::Value> {
    (0..count).map(|i| {
        let body = (0..per_message).map(|k| format!("see https://dense-{}.example/{prefix}/{i}/{k}?ref=chat,", k % 5))
            .collect::<Vec<_>>().join(" and ");
        message(&format!("${prefix}-{i}:test"), "Zed", &body, newest - (count - i) as u64 * 1000)
    }).collect()
}

/// Eight people in the latest 30 messages all share the same project link.
fn popular_window(now: u64) -> serde_json::Value {
    let messages: Vec<_> = (0..8).map(|i| message(&format!("$pop{i}:test"), &format!("User{i}"),
        &format!("have you seen https://github.com/project-robius/robrix ? ({i})"), now - (20 - i as u64) * MINUTE)).collect();
    serde_json::json!({"messages": messages})
}

#[test]
fn the_source_evaluates_well_within_the_production_startup_budget() {
    // Robrix's release build evaluates the source under a 64 ms entry deadline. This debug build
    // runs it several times slower (about 15 ms), so meeting 64 ms here leaves that headroom.
    // A tighter wall-clock bound only measured how busy the test machine was.
    let is_within = (0..5).any(|_| production_eval(&mut Harness::new()).is_empty());
    assert!(is_within, "the source doesn't evaluate within Robrix's 64 ms startup budget");
    // A full saved library is parsed at boot, not during the startup evaluation.
    let is_within_with_file = (0..5).any(|_| {
        let mut host = Harness::new();
        write_library(&host, &room_context(), &full_library(400, 6, 4));
        production_eval(&mut host).is_empty()
    });
    assert!(is_within_with_file, "a saved library slows the startup evaluation");
}

#[test]
fn boot_reads_the_latest_messages_and_lists_their_links_newest_first() {
    let mut host = Harness::new();
    let (app, context) = host.launch_stock(APP, false);
    host.boot();
    // The grouped setup is already approved here; the read asks for exactly 30.
    let mut hooks = Vec::new();
    let (reply, call) = next_read(&mut host, &mut hooks);
    assert!(matches!(call, services::MatrixServiceCall::ReadMessages { limit: 30 }));
    answer(&mut host, reply, &window(now_ms()).to_string());
    let mut audit = StockAudit::default();
    host.settle_stock(&app, APP, &mut audit);
    hooks.extend(audit.subscriptions.iter().cloned());
    hooks.sort();
    assert_eq!(hooks, ["on_room_message", "on_room_message_changed"]);
    assert!(audit.services.is_empty(), "no reads beyond the latest messages: {:?}", audit.services);
    assert!(audit.prompts.is_empty());

    assert_eq!(text(&host, &app, "summary"), "5 links · 5 sites · 3 people");
    assert_eq!(text(&host, &app, "status"), "Watching for new links.");
    assert_eq!(text(&host, &app, "room_line"), "Links people share in Private room");
    assert_eq!(text(&host, &app, "list_head"), "Newest first");
    // Each link shows as its site in the message it came with.
    assert_eq!(rows(&host, &app), [
        vec!["rust-lang.org", "Home page", "“Try rust-lang.org, and see en.wikipedia.org).”", "Bob · 30m ago"],
        vec!["en.wikipedia.org", "/wiki/Rust_(programming_language)", "“Try rust-lang.org, and see en.wikipedia.org).”", "Bob · 30m ago"],
        vec!["matrix.to", "#robrix:matrix.org", "“Join us at #robrix:matrix.org!”", "Alice · 2h ago"],
        vec!["github.com", "/makepad/makepad/releases/tag/v1.0", "“Same thing (github.com) — worth a read”", "Carol · 3h ago · shared 2×"],
        vec!["docs.rs", "/makepad-widgets", "Just the link", "Bob · 4h ago"],
    ]);
    assert_eq!(chips(&host, &app), ["All 5"]);
    assert!(!shown(&host, &app, "chip_wrap"), "no site has two links, so there are no site chips");
    assert!(shown(&host, &app, "head_wrap") && shown(&host, &app, "search_wrap") && shown(&host, &app, "body"));
    assert!(!shown(&host, &app, "empty_wrap") && !shown(&host, &app, "detail_wrap") && !shown(&host, &app, "more_wrap"));
    assert!(shown(&host, &app, "scan_wrap") && shown(&host, &app, "scan_btn_wrap") && !shown(&host, &app, "stop_wrap"));
    assert_eq!(text(&host, &app, "scan_line"), "Scanned 6 messages · 5 links");

    let saved = library(&host, &context);
    assert_eq!(saved["v"], 2);
    assert_eq!(saved["room_id"], ROOM);
    assert_eq!(saved["before"], "$m1:test", "the scan cursor is the oldest message read");
    assert_eq!(saved["scanned"], 6);
    assert_eq!(saved["gaps"], serde_json::json!([]));
    let github = saved_link(&saved, "https://github.com/makepad/makepad/releases/tag/v1.0");
    assert_eq!(github["count"], 2, "scheme and host case don't split a link");
    assert_eq!(github["shares"][0]["e"], "$m3:test");
    assert_eq!(github["shares"][1]["e"], "$m1:test");
    assert_eq!(github["shares"][1]["x"], "Release notes: github.com.");
    assert_eq!(github["more"], serde_json::json!([]));
}

#[test]
fn extraction_trims_punctuation_balances_brackets_and_skips_lookalikes() {
    let mut host = Harness::new();
    let now = now_ms();
    let bodies = [
        ("$e1:test", "Read <https://angle.example/a>, then **https://bold.example/b**"),
        ("$e2:test", "Query https://example.com/search?q=a,b&x=(1). Port: http://localhost:8080/app"),
        ("$e3:test", "mail alice@example.org or foowww.notalink.com or https:// nothing"),
        ("$e4:test", "Twice https://twice.example/x https://TWICE.example/x and [www.square.example/path]"),
        ("$e5:test", "Room link matrix:r/robrix:matrix.org and HTTPS://EXAMPLE.COM"),
        ("$e6:test", "Wrapped (see https://wrapped.example/a_(b)) and “https://quoted.example/q”…"),
        ("$e7:test", "[the docs](https://docs.example/start) are short"),
        // Inline code and CJK punctuation end a URL; www. right after CJK text still counts.
        ("$e8:test", "run `curl https://get.example.sh` and see `https://api.example.com/v1/users`"),
        ("$e9:test", "文档在这里https://example.com/docs，请看。（详见https://example.org/a）谢谢"),
        ("$e10:test", "このページhttps://example.jp/page。よろしく 文www.cjk-www.example/x"),
    ];
    let messages: Vec<_> = bodies.iter().enumerate()
        .map(|(i, (id, body))| message(id, "Erin", body, now - (20 - i as u64) * MINUTE)).collect();
    let (app, context, _) = open_library(&mut host, serde_json::json!({"messages": messages}));
    let saved = library(&host, &context);
    let mut urls = saved_urls(&saved);
    urls.sort();
    assert_eq!(urls, [
        "http://localhost:8080/app",
        "https://angle.example/a",
        "https://api.example.com/v1/users",
        "https://bold.example/b",
        "https://docs.example/start",
        "https://example.com",
        "https://example.com/docs",
        "https://example.com/search?q=a,b&x=(1)",
        "https://example.jp/page",
        "https://example.org/a",
        "https://get.example.sh",
        "https://quoted.example/q",
        "https://twice.example/x",
        "https://wrapped.example/a_(b)",
        "https://www.cjk-www.example/x",
        "https://www.square.example/path",
        "matrix:r/robrix:matrix.org",
    ]);
    assert_eq!(saved_link(&saved, "https://twice.example/x")["count"], 1, "one message shares a link once");
    assert_eq!(row(&host, &app, "localhost")[1], "/app");
    assert_eq!(row(&host, &app, "square.example")[1], "/path");
    assert_eq!(row(&host, &app, "cjk-www.example")[1], "/x");
    assert_eq!(&row(&host, &app, "matrix")[..2], ["matrix", "#robrix:matrix.org"], "a room alias opens in the browser");
    assert!(rows(&host, &app).iter().any(|row| row[0] == "example.com" && row[1] == "Home page"), "a bare host is its home page");
    let snippet = |url: &str| saved_link(&saved, url)["shares"][0]["x"].as_str().unwrap().to_owned();
    assert_eq!(snippet("https://angle.example/a"), "Read angle.example, then bold.example");
    assert_eq!(snippet("https://wrapped.example/a_(b)"), "Wrapped (see wrapped.example) and “quoted.example”…");
    assert_eq!(snippet("https://docs.example/start"), "the docs are short");
    assert_eq!(snippet("https://get.example.sh"), "run curl get.example.sh and see api.example.com");
    assert_eq!(snippet("https://example.com/docs"), "文档在这里example.com，请看。（详见example.org）谢谢");
}

#[test]
fn long_paths_and_messages_are_shortened_at_word_breaks() {
    let mut host = Harness::new();
    let now = now_ms();
    let long_path = format!("https://long.example/{}", "segment/".repeat(12));
    let long_body = format!("{} {}", "word ".repeat(40).trim(), "https://words.example/a");
    let (app, context, _) = open_library(&mut host, serde_json::json!({"messages": [
        message("$p1:test", "Erin", &long_path, now - 2 * MINUTE),
        message("$p2:test", "Erin", &long_body, now - MINUTE),
    ]}));
    let rows = rows(&host, &app);
    assert_eq!(rows[0][0], "words.example");
    // Rows cut the message shorter than the saved snippet the detail card shows.
    let snippet = &rows[0][2];
    assert!(snippet.ends_with("word…”") && snippet.chars().count() <= 92, "{snippet}");
    let saved = saved_link(&library(&host, &context), "https://words.example/a")["shares"][0]["x"].as_str().unwrap().to_owned();
    assert!(saved.ends_with("word…") && saved.chars().count() <= 140, "{saved}");
    assert_eq!(rows[1][1].chars().count(), 64, "{}", rows[1][1]);
    assert!(rows[1][1].ends_with('…'));
}

#[test]
fn an_empty_library_says_what_to_try_next() {
    let mut host = Harness::new();
    let now = now_ms();
    let (app, _, mut audit) = open_library(&mut host, serde_json::json!({"messages": [
        message("$n1:test", "Ann", "hello", now - 2 * MINUTE),
        message("$n2:test", "Ben", "no links here", now - MINUTE),
    ]}));
    // The empty card speaks for an empty library; the summary row stays out of its way.
    assert!(!shown(&host, &app, "head_wrap") && !shown(&host, &app, "body") && !shown(&host, &app, "search_wrap"));
    assert!(shown(&host, &app, "empty_wrap"));
    assert_eq!(text(&host, &app, "empty_title"), "No links yet");
    assert_eq!(text(&host, &app, "empty_text"), "None of the latest 2 messages has a link. New links show up here as people share them.");
    assert!(!shown(&host, &app, "ask_wrap") && !shown(&host, &app, "reset_wrap"));
    assert!(shown(&host, &app, "scan_wrap"));
    host.broker.forget_app(APP);
    assert!(host.call(&app, "scan_older", &[]));
    answer_page(&mut host, Some("$n1:test"), serde_json::json!({"messages": [message("$n0:test", "Cy", "hi", now - HOUR)], "has_more": false}));
    drain(&mut host, &app, &mut audit);
    assert_eq!(text(&host, &app, "empty_title"), "No links in this room");
    assert_eq!(text(&host, &app, "empty_text"), "Scanned 3 messages back to the start; nobody has shared a link yet. New links show up here as people share them.");
    // A link arriving live replaces the empty state.
    let batch = serde_json::json!([live("$n3:test", "ann", "Ann", "finally https://first-link.example", now)]);
    assert!(host.call_strings(&app, "on_room_message", &[&batch.to_string()]));
    assert!(!shown(&host, &app, "empty_wrap") && shown(&host, &app, "body") && shown(&host, &app, "head_wrap"));
    assert_eq!(text(&host, &app, "summary"), "1 link · 1 site · 1 person");
}

#[test]
fn an_empty_room_without_live_updates_offers_refresh() {
    let mut host = Harness::new();
    host.queue_matrix_reply("matrix.room.messages.read", serde_json::json!({"messages": []}));
    host.queue_matrix_reply("matrix.room.messages.read", serde_json::json!({"messages": []}));
    let (app, _) = host.launch_stock_permissions(APP, false, false);
    host.boot();
    let (request, perms) = host.take_stock_batch(APP);
    let mut audit = StockAudit::default();
    host.answer_stock_batch(&app, APP, request, &perms, &[Permission::MatrixRoomRead], &mut audit);
    host.settle_stock(&app, APP, &mut audit);
    drain(&mut host, &app, &mut audit);
    assert_eq!(text(&host, &app, "empty_title"), "No links yet");
    assert_eq!(text(&host, &app, "empty_text"), "Robrix has no recent messages here. Tap Refresh to check for new ones.");
    assert!(shown(&host, &app, "ask_wrap") && shown(&host, &app, "retry_wrap") && !shown(&host, &app, "ask_btn_wrap"));
    assert_eq!(text(&host, &app, "retry_btn"), "Refresh");
    host.widget_click(&app, "Refresh", &[]);
    host.settle_stock(&app, APP, &mut audit);
    assert_eq!(audit.services, ["matrix.room.messages.read", "matrix.room.messages.read"]);
}

#[test]
fn search_filters_instantly_across_urls_sites_people_and_messages() {
    let mut host = Harness::new();
    let (app, _, _) = open_library(&mut host, window(now_ms()));
    app.text_input(&host.cx, ids!(search)).set_text(&mut host.cx, "alice");
    assert!(host.call_strings(&app, "set_query", &["alice"]));
    host.boot();
    no_requests(&mut host);
    assert_eq!(sites(&host, &app), ["matrix.to", "github.com"]);
    assert_eq!(text(&host, &app, "list_head"), "2 matches");
    assert!(shown(&host, &app, "clear_wrap"));
    for (query, expected) in [
        // The message that shared en.wikipedia.org also mentions rust-lang.org by its site.
        ("wiki RUST", vec!["rust-lang.org", "en.wikipedia.org"]),
        ("worth a read", vec!["github.com"]),
        ("docs.rs", vec!["docs.rs"]),
        ("@carol:test", vec!["github.com"]),
        ("(programming", vec!["en.wikipedia.org"]),
        ("   ", vec!["rust-lang.org", "en.wikipedia.org", "matrix.to", "github.com", "docs.rs"]),
    ] {
        assert!(host.call_strings(&app, "set_query", &[query]));
        host.boot();
        assert_eq!(sites(&host, &app), expected, "{query:?}");
    }
    assert_eq!(text(&host, &app, "list_head"), "Newest first");
    assert!(host.call_strings(&app, "set_query", &["zzz"]));
    host.boot();
    assert!(!shown(&host, &app, "body") && shown(&host, &app, "empty_wrap"));
    assert_eq!(text(&host, &app, "empty_title"), "No matches");
    assert_eq!(text(&host, &app, "empty_text"), "No links match “zzz”.");
    assert!(shown(&host, &app, "reset_wrap") && !shown(&host, &app, "ask_wrap"));
    host.widget_click(&app, "Clear filters", &[]);
    host.boot();
    assert_eq!(rows(&host, &app).len(), 5);
    assert_eq!(app.text_input(&host.cx, ids!(search)).text(), "");
    assert!(!shown(&host, &app, "clear_wrap") && !shown(&host, &app, "empty_wrap"));
    no_requests(&mut host);
}

#[test]
fn searching_shows_the_message_that_matched() {
    let mut host = Harness::new();
    let now = now_ms();
    let (app, _, _) = open_library(&mut host, serde_json::json!({"messages": [
        message("$s1:test", "Diego", "Link previews should only go through a proxy we control: https://github.com/project-robius/robrix/discussions/987", now - 6 * HOUR),
        message("$s2:test", "Wei", "GitHub 上的讨论在这里 https://github.com/project-robius/robrix/discussions/987 大家可以继续补充", now - 2 * HOUR),
        message("$s3:test", "Ann", "https://docs.rs/makepad-widgets", now - HOUR),
    ]}));
    assert_eq!(row(&host, &app, "github.com")[3], "Wei · 2h ago · shared 2×");
    assert!(host.call_strings(&app, "set_query", &["proxy"]));
    host.boot();
    assert_eq!(rows(&host, &app), [vec!["github.com", "/project-robius/robrix/discussions/987",
        "“Link previews should only go through a proxy we control: github.com”", "Diego · 6h ago · shared 2×"]]);
    // The detail card and Show in chat follow the message the row shows.
    assert!(host.call(&app, "select_row", &[0_u32.into()]));
    assert_eq!(text(&host, &app, "d_snippet"), "“Link previews should only go through a proxy we control: github.com”");
    host.broker.forget_app(APP);
    assert!(host.call(&app, "show_in_chat", &[]));
    let (reply, action) = host_action(&mut host);
    assert!(matches!(&action, HostAction::JumpToEvent { event_id, .. } if event_id == "$s1:test"), "Show in chat jumps to the matched share");
    answer(&mut host, reply, "{}");
    // A person's name picks their share too.
    assert!(host.call_strings(&app, "set_query", &["wei"]));
    host.boot();
    assert_eq!(rows(&host, &app)[0][3], "Wei · 2h ago · shared 2×");
    assert!(host.call(&app, "clear_query", &[]));
    host.boot();
    assert_eq!(row(&host, &app, "github.com")[2], "“GitHub 上的讨论在这里 github.com 大家可以继续补充”");
}

#[test]
fn site_chips_count_links_filter_the_list_and_mark_the_choice() {
    let mut host = Harness::new();
    let (app, _, mut audit) = open_library(&mut host, sites_window(now_ms()));
    // Only sites with two or more links get a chip.
    assert_eq!(chips(&host, &app), ["All 6", "github.com 3", "docs.rs 2"]);
    assert!(shown(&host, &app, "chip_wrap"));
    let chip = |host: &mut Harness, index: usize| {
        let widget = children(host, &app, "chip_list")[index].clone();
        bg_color(host, &app, &widget)
    };
    let (on, off) = (chip(&mut host, 0), chip(&mut host, 1));
    assert_ne!(on, off, "the selected chip stands out");
    assert!(host.call(&app, "pick_chip", &[1_u32.into()]));
    host.boot();
    drain(&mut host, &app, &mut audit);
    no_requests(&mut host);
    assert_eq!(text(&host, &app, "list_head"), "3 links on github.com");
    assert_eq!(sites(&host, &app), ["github.com", "github.com", "github.com"]);
    assert_eq!((chip(&mut host, 0), chip(&mut host, 1)), (off, on));
    // Search narrows within the chosen site.
    assert!(host.call_strings(&app, "set_query", &["ben"]));
    host.boot();
    assert_eq!(rows(&host, &app).len(), 1);
    assert_eq!(text(&host, &app, "list_head"), "1 match on github.com");
    assert!(host.call_strings(&app, "set_query", &["cy"]));
    host.boot();
    assert_eq!(rows(&host, &app).len(), 1, "www.github.com is the same site");
    assert!(host.call_strings(&app, "set_query", &["nobody"]));
    host.boot();
    assert_eq!(text(&host, &app, "empty_text"), "No links on github.com match “nobody”.");
    assert!(host.call(&app, "clear_query", &[]));
    host.boot();
    // Tapping the chosen chip again shows every site.
    assert!(host.call(&app, "pick_chip", &[1_u32.into()]));
    host.boot();
    assert_eq!(rows(&host, &app).len(), 6);
    assert_eq!(chip(&mut host, 0), on);
    drain(&mut host, &app, &mut audit);
    no_requests(&mut host);
}

#[test]
fn chip_caps_follow_the_pane_width_and_long_hosts_are_shortened() {
    let mut host = Harness::new();
    let now = now_ms();
    let hosts = ["one.example", "two.example", "three.example", "four.example", "five.example", "six.example",
        "robrix-ci-artifacts.s3.eu-central-1.amazonaws.com"];
    let mut messages = Vec::new();
    for (i, site) in hosts.iter().enumerate() {
        for k in 0..(9 - i) {
            messages.push(message(&format!("$h{i}-{k}:test"), "Ann", &format!("https://{site}/{k}"), now - ((i * 10 + k + 1) as u64) * MINUTE));
        }
    }
    messages.reverse();
    let (app, _, _) = open_library(&mut host, serde_json::json!({"messages": messages}));
    assert_eq!(chips(&host, &app), ["All 42", "one.example 9", "two.example 8", "three.example 7", "four.example 6", "five.example 5"]);
    assert!(host.call(&app, "on_app_resize", &[300_f64.into(), 700_f64.into()]));
    assert_eq!(chips(&host, &app).len(), 4, "a narrow pane keeps the three busiest sites");
    assert!(host.call(&app, "on_app_resize", &[1200_f64.into(), 700_f64.into()]));
    assert_eq!(chips(&host, &app).last().unwrap(), "robrix-ci-artifacts.s3.… 3", "{:?}", chips(&host, &app));
    // The shortened chip still filters by the whole host.
    assert!(host.call(&app, "pick_chip", &[7_u32.into()]));
    host.boot();
    assert_eq!(rows(&host, &app).len(), 3);
    assert_eq!(text(&host, &app, "list_head"), "3 links on robrix-ci-artifacts.s3.eu-central-1.amazonaws.com");
}

#[test]
fn the_detail_card_lists_every_sharer_and_each_action_is_one_request_from_a_tap() {
    let mut host = Harness::new();
    let (app, context, _) = open_library(&mut host, window(now_ms()));
    assert!(host.call(&app, "select_row", &[3_u32.into()]));
    no_requests(&mut host);
    assert!(shown(&host, &app, "detail_wrap"));
    assert_eq!(text(&host, &app, "d_site"), "github.com");
    assert_eq!(text(&host, &app, "d_url"), "https://github.com/makepad/makepad/releases/tag/v1.0");
    assert_eq!(text(&host, &app, "d_snippet"), "“Same thing (github.com) — worth a read”");
    assert_eq!(text(&host, &app, "d_head"), "SHARED 2 TIMES BY 2 PEOPLE");
    assert_eq!(items(&host, &app, "d_people"), [vec!["C", "Carol", "3h ago"], vec!["A", "Alice", "5h ago"]], "each sharer beside an avatar");
    assert_eq!(text(&host, &app, "d_open"), "Open");
    assert!(!shown(&host, &app, "d_note_wrap"));
    let row = children(&host, &app, "link_list")[3].clone();
    let other = children(&host, &app, "link_list")[0].clone();
    assert_ne!(bg_color(&mut host, &app, &row), bg_color(&mut host, &app, &other), "the chosen row stands out");

    // Show in chat jumps to the latest message that shared the link.
    assert!(host.call(&app, "show_in_chat", &[]));
    assert_eq!(text(&host, &app, "d_note"), "Finding the message…");
    let (reply, action) = host_action(&mut host);
    assert!(matches!(&action, HostAction::JumpToEvent { room: Some(room), event_id } if room == ROOM && event_id == "$m3:test"));
    answer(&mut host, reply, "{}");
    assert_eq!(text(&host, &app, "d_note"), "✅ Showing the message in the chat.");

    // Open hands exactly this URL to the browser, after its review.
    host.broker.forget_app(APP);
    assert!(host.call(&app, "open_link", &[]));
    let args = finish_reviewed(&mut host, &context, "url.open", Ok("{}"));
    assert_eq!(args, serde_json::json!({"url": "https://github.com/makepad/makepad/releases/tag/v1.0"}));
    assert_eq!(text(&host, &app, "d_note"), "✅ Opened in your browser.");

    // Copy writes exactly the URL.
    assert!(host.call(&app, "copy_link", &[]));
    let args = finish_reviewed(&mut host, &context, "clipboard.write", Ok("{}"));
    assert_eq!(args, serde_json::json!({"text": "https://github.com/makepad/makepad/releases/tag/v1.0"}));
    assert_eq!(text(&host, &app, "d_note"), "✅ Copied the link.");

    // A failed open says why and stays available.
    host.broker.forget_app(APP);
    assert!(host.call(&app, "open_link", &[]));
    finish_reviewed(&mut host, &context, "url.open", Err("couldn't open link: no browser"));
    assert_eq!(text(&host, &app, "d_note"), "❗ Couldn't open link: no browser.");
    assert!(host.call(&app, "open_link", &[]));
    finish_reviewed(&mut host, &context, "url.open", Ok("{}"));
    assert_eq!(text(&host, &app, "d_note"), "✅ Opened in your browser.");

    // Tapping the chosen row again, or ×, closes the card.
    assert!(host.call(&app, "select_row", &[3_u32.into()]));
    assert!(!shown(&host, &app, "detail_wrap"));
    assert!(host.call(&app, "select_row", &[0_u32.into()]));
    assert!(host.call(&app, "close_detail", &[]));
    assert!(!shown(&host, &app, "detail_wrap"));
    no_requests(&mut host);
}

#[test]
fn matrix_links_open_in_robrix_only_when_robrix_can_resolve_them() {
    let mut host = Harness::new();
    let now = now_ms();
    let (app, context, _) = open_library(&mut host, serde_json::json!({"messages": [
        message("$r1:test", "Ann", "Join us at https://matrix.to/#/#robrix:matrix.org", now - 6 * MINUTE),
        message("$r2:test", "Ann", "dm me https://matrix.to/#/@alice:matrix.org", now - 5 * MINUTE),
        message("$r3:test", "Ann", "see https://matrix.to/#/!room:matrix.org/$event?via=matrix.org", now - 4 * MINUTE),
        message("$r4:test", "Ann", "or matrix:r/robrix:matrix.org", now - 3 * MINUTE),
        message("$r5:test", "Ann", "or matrix:u/bob:matrix.org", now - 2 * MINUTE),
        message("$r6:test", "Ann", "encoded https://matrix.to/#/%23robrix-dev:matrix.org", now - MINUTE),
    ]}));
    let shown_rows: Vec<_> = rows(&host, &app).into_iter().map(|row| (row[0].clone(), row[1].clone())).collect();
    assert_eq!(shown_rows, [
        ("matrix.to".to_owned(), "#robrix-dev:matrix.org".to_owned()),
        ("matrix · opens in Robrix".to_owned(), "@bob:matrix.org".to_owned()),
        ("matrix".to_owned(), "#robrix:matrix.org".to_owned()),
        ("matrix.to · opens in Robrix".to_owned(), "!room:matrix.org/$event".to_owned()),
        ("matrix.to · opens in Robrix".to_owned(), "@alice:matrix.org".to_owned()),
        ("matrix.to".to_owned(), "#robrix:matrix.org".to_owned()),
    ]);
    // Robrix can't resolve room aliases, so those go to the browser's matrix.to page.
    for (index, url) in [(5_u32, "https://matrix.to/#/#robrix:matrix.org"), (2, "https://matrix.to/#/#robrix:matrix.org"),
        (0, "https://matrix.to/#/%23robrix-dev:matrix.org")]
    {
        assert!(host.call(&app, "select_row", &[index.into()]));
        assert_eq!(text(&host, &app, "d_open"), "Open");
        host.broker.forget_app(APP);
        assert!(host.call(&app, "open_link", &[]));
        assert_eq!(finish_reviewed(&mut host, &context, "url.open", Ok("{}")), serde_json::json!({"url": url}));
        assert_eq!(text(&host, &app, "d_note"), "✅ Opened in your browser.");
    }
    // Copy keeps the link as it was shared.
    assert!(host.call(&app, "select_row", &[2_u32.into()]));
    host.broker.forget_app(APP);
    assert!(host.call(&app, "copy_link", &[]));
    assert_eq!(finish_reviewed(&mut host, &context, "clipboard.write", Ok("{}")), serde_json::json!({"text": "matrix:r/robrix:matrix.org"}));
    // People, room ids and their messages open inside Robrix.
    for (index, expected) in [(4_u32, "https://matrix.to/#/@alice:matrix.org"), (3, "https://matrix.to/#/!room:matrix.org/$event?via=matrix.org"),
        (1, "matrix:u/bob:matrix.org")]
    {
        assert!(host.call(&app, "select_row", &[index.into()]));
        assert_eq!(text(&host, &app, "d_open"), "Open in Robrix");
        host.broker.forget_app(APP);
        assert!(host.call(&app, "open_link", &[]));
        let (reply, action) = host_action(&mut host);
        assert!(matches!(&action, HostAction::OpenLink { url, .. } if url == expected), "{expected} opens in Robrix");
        answer(&mut host, reply, "{}");
        assert_eq!(text(&host, &app, "d_note"), "✅ Opened in Robrix.");
    }
}

#[test]
fn refused_actions_show_the_reason_and_keep_the_buttons_working() {
    let mut host = Harness::new();
    let (app, _, mut audit) = open_library(&mut host, window(now_ms()));
    host.permissions.set(APP, Permission::OpenUrl, GrantState::Denied);
    host.permissions.set(APP, Permission::RobrixNavigation, GrantState::Denied);
    host.publish_grants(&app, APP);
    host.settle_stock(&app, APP, &mut audit);
    assert!(host.call(&app, "select_row", &[0_u32.into()]));
    for action in ["open_link", "show_in_chat"] {
        host.broker.forget_app(APP);
        assert!(host.call(&app, action, &[]));
        for ask in host.process() { assert!(matches!(ask, BrokerAsk::Used { .. }), "{action} is refused by the broker"); }
        host.cx.with_vm_and_async(|_| {});
        assert!(text(&host, &app, "d_note").starts_with("❗ "), "{action}: {}", text(&host, &app, "d_note"));
    }
    // Allowing it again lets the same button work.
    host.permissions.set(APP, Permission::RobrixNavigation, GrantState::Granted);
    host.publish_grants(&app, APP);
    host.broker.forget_app(APP);
    assert!(host.call(&app, "show_in_chat", &[]));
    let (reply, _) = host_action(&mut host);
    answer(&mut host, reply, "{}");
    assert_eq!(text(&host, &app, "d_note"), "✅ Showing the message in the chat.");
}

#[test]
fn an_open_link_makes_room_for_itself_in_stacked_layouts() {
    let mut host = Harness::new();
    let now = now_ms();
    let long_url = format!("https://builds.example/robrix/{}?X-Amz-Signature={}", "nightly/".repeat(12), "f".repeat(120));
    let mut messages = sites_window(now)["messages"].as_array().unwrap().clone();
    for (i, who) in ["Ann", "Ben", "Cy", "Dee", "Eve"].iter().enumerate() {
        messages.push(bare_message(&format!("$b{i}:test"), who, &format!("nightly {long_url}"), now - (5 - i as u64) * 1000));
    }
    let (app, context, _) = open_library(&mut host, serde_json::json!({"messages": messages}));
    assert!(shown(&host, &app, "chip_wrap") && shown(&host, &app, "scan_wrap"));
    assert!(host.call(&app, "select_row", &[0_u32.into()]));
    assert!(!shown(&host, &app, "chip_wrap") && !shown(&host, &app, "scan_wrap"), "the card takes the chips' and scan card's room");
    // A long URL is cut in the card but opened and copied whole.
    let url = text(&host, &app, "d_url");
    assert!(url.ends_with('…') && url.chars().count() <= 100, "{url}");
    assert_eq!(items(&host, &app, "d_people"), [vec!["E", "eve", "Just now"], vec!["D", "dee", "Just now"], vec!["C", "cy", "Just now"],
        vec!["and 2 earlier shares, the first just now"]]);
    host.broker.forget_app(APP);
    assert!(host.call(&app, "copy_link", &[]));
    assert_eq!(finish_reviewed(&mut host, &context, "clipboard.write", Ok("{}"))["text"], long_url.as_str());
    assert!(host.call(&app, "close_detail", &[]));
    assert!(shown(&host, &app, "chip_wrap") && shown(&host, &app, "scan_wrap"));
    // A running scan keeps its Stop button, card or not.
    host.broker.forget_app(APP);
    assert!(host.call(&app, "scan_older", &[]));
    assert!(host.call(&app, "select_row", &[0_u32.into()]));
    assert!(shown(&host, &app, "scan_wrap") && shown(&host, &app, "stop_wrap"));
    assert!(host.call(&app, "stop_scan", &[]));
    assert!(host.call(&app, "close_detail", &[]));
    // A wide pane shows the card beside the list, with every sharer, and keeps the rest.
    assert!(host.call(&app, "on_app_resize", &[1200_f64.into(), 900_f64.into()]));
    assert!(host.call(&app, "select_row", &[0_u32.into()]));
    assert!(shown(&host, &app, "chip_wrap") && shown(&host, &app, "scan_wrap") && shown(&host, &app, "s_card_wrap"));
    assert_eq!(items(&host, &app, "s_people").len(), 5);
}

#[test]
fn live_messages_edits_and_redactions_keep_the_library_current() {
    let mut host = Harness::new();
    let now = now_ms();
    let (app, context, mut audit) = open_library(&mut host, window(now));
    let batch = serde_json::json!([live("$l1:test", "alice", "Alice Liddell",
        "New build: https://github.com/makepad/makepad/releases/tag/v1.0 and https://example.org/x", now - MINUTE)]);
    assert!(host.call_strings(&app, "on_room_message", &[&batch.to_string()]));
    no_requests(&mut host);
    assert_eq!(sites(&host, &app), ["github.com", "example.org", "rust-lang.org", "en.wikipedia.org", "matrix.to", "docs.rs"]);
    assert_eq!(rows(&host, &app)[0][2], "“New build: github.com and example.org”");
    assert_eq!(rows(&host, &app)[0][3], "Alice Liddell · 1m ago · shared 3×");
    assert_eq!(rows(&host, &app)[4][3], "Alice Liddell · 2h ago", "a newer display name replaces the one history gave");
    assert_eq!(text(&host, &app, "summary"), "6 links · 6 sites · 3 people");
    // The same delivery twice changes nothing.
    assert!(host.call_strings(&app, "on_room_message", &[&batch.to_string()]));
    assert_eq!(rows(&host, &app)[0][3], "Alice Liddell · 1m ago · shared 3×");
    // An edit re-reads that message's links.
    assert!(host.call_strings(&app, "on_room_message_changed", &[r#"{"room_id":"!private:test","event_id":"$l1:test","edited":true,"redacted":false,"body":"Only https://example.org/x now"}"#]));
    assert_eq!(sites(&host, &app), ["example.org", "rust-lang.org", "en.wikipedia.org", "matrix.to", "github.com", "docs.rs"]);
    assert_eq!(rows(&host, &app)[0][2], "“Only example.org now”");
    assert_eq!(rows(&host, &app)[4][3], "Carol · 3h ago · shared 2×");
    // An edit of a link's newest share that keeps the link shows the new text.
    assert!(host.call_strings(&app, "on_room_message_changed", &[r#"{"event_id":"$m3:test","edited":true,"redacted":false,"body":"Same thing, newer tag (https://github.com/makepad/makepad/releases/tag/v1.0)"}"#]));
    assert_eq!(row(&host, &app, "github.com")[2..], ["“Same thing, newer tag (github.com)”", "Carol · 3h ago · shared 2×"]);
    // A redaction removes only what that message shared.
    assert!(host.call_strings(&app, "on_room_message_changed", &[r#"{"event_id":"$m4:test","edited":false,"redacted":true,"body":""}"#]));
    assert!(!sites(&host, &app).contains(&"matrix.to".to_owned()));
    assert!(host.call_strings(&app, "on_room_message_changed", &[r#"{"event_id":"$m3:test","edited":false,"redacted":true,"body":""}"#]));
    // Its newest share is now Alice's, five hours ago, so it moves below docs.rs.
    assert_eq!(sites(&host, &app), ["example.org", "rust-lang.org", "en.wikipedia.org", "docs.rs", "github.com"]);
    assert_eq!(rows(&host, &app)[4], ["github.com", "/makepad/makepad/releases/tag/v1.0", "“Release notes: github.com.”", "Alice Liddell · 5h ago"]);
    // Edits of messages it never saw, and junk payloads, are ignored.
    assert!(host.call_strings(&app, "on_room_message_changed", &[r#"{"event_id":"$unknown:test","edited":true,"redacted":false,"body":"https://ghost.example"}"#]));
    assert!(host.call_strings(&app, "on_room_message_changed", &["not json"]));
    assert!(host.call_strings(&app, "on_room_message", &[r#"[{"body":"https://no-id.example"}, 7, null]"#]));
    assert_eq!(rows(&host, &app).len(), 5);
    assert_eq!(text(&host, &app, "summary"), "5 links · 5 sites · 2 people");
    // Live changes are saved shortly after, without a request.
    drain(&mut host, &app, &mut audit);
    let saved = library(&host, &context);
    assert!(!saved_urls(&saved).contains(&"https://matrix.to/#/#robrix:matrix.org".to_owned()));
    assert_eq!(saved_link(&saved, "https://example.org/x")["shares"][0]["s"], "Alice Liddell");
    assert_eq!(saved["names"], serde_json::json!([["@bob:test", "Bob"], ["@dave:test", "Dave"], ["@alice:test", "Alice Liddell"], ["@carol:test", "Carol"]]));
    no_requests(&mut host);
}

#[test]
fn clipped_bodies_and_reply_fallbacks_are_not_misread() {
    let mut host = Harness::new();
    let now = now_ms();
    // Robrix clips bodies to 500 characters, cutting this message's last URL short.
    let full = format!("Notes https://keep.example/a {} https://github.com/project-robius/robrix/pull/1234", "word ".repeat(90).trim());
    let clipped: String = full.chars().take(500).collect();
    assert!(clipped.ends_with(" https://github.com/pr"), "{clipped}");
    let (app, context, mut audit) = open_library(&mut host, serde_json::json!({"messages": [
        message("$g1:test", "Alice", "Notes: https://example.org/guide", now - HOUR),
        message("$g2:test", "Ann", &clipped, now - 30 * MINUTE),
        message("$g3:test", "Ann", "Ends with a link https://whole.example/path", now - 20 * MINUTE),
    ]}));
    let mut urls = saved_urls(&library(&host, &context));
    urls.sort();
    assert_eq!(urls, ["https://example.org/guide", "https://keep.example/a", "https://whole.example/path"]);
    // A live reply that still carries the quoted fallback doesn't re-share the quoted link.
    let replies = serde_json::json!([
        live("$g4:test", "bob", "Bob", "> <@alice:test> Notes: https://example.org/guide\n\nThanks, reading it now", now - 2 * MINUTE),
        live("$g5:test", "bob", "Bob", "> <@ann:test> Ends with a link\n> more quote\n\nAnd mine: https://mine.example/x", now - MINUTE),
    ]);
    assert!(host.call_strings(&app, "on_room_message", &[&replies.to_string()]));
    drain(&mut host, &app, &mut audit);
    assert_eq!(row(&host, &app, "example.org")[2..], ["“Notes: example.org”", "Alice · 1h ago"]);
    assert_eq!(row(&host, &app, "mine.example")[2..], ["“And mine: mine.example”", "Bob · 1m ago"]);
}

#[test]
fn snippets_read_as_sentences_and_bare_links_say_so() {
    let mut host = Harness::new();
    let now = now_ms();
    let bodies = [
        "https://a.example/x :)", "https://b.example/x :D", "lol https://c.example/x ^^", "https://d.example/x !!",
        "**https://e.example/x**", "Icons: https://phosphoricons.com/ and https://lucide.dev/icons/ both MIT",
        "Testflight build is up, link in https://testflight.apple.com/join/AbCd1234",
    ];
    let messages: Vec<_> = bodies.iter().enumerate().map(|(i, body)| message(&format!("$t{i}:test"), "Erin", body, now - (10 - i as u64) * MINUTE)).collect();
    let (app, _, _) = open_library(&mut host, serde_json::json!({"messages": messages}));
    let snippets: Vec<_> = rows(&host, &app).into_iter().rev().map(|row| row[2].clone()).collect();
    assert_eq!(snippets, [
        "Just the link", "Just the link", "“lol c.example ^^”", "Just the link", "Just the link",
        "“Icons: phosphoricons.com and lucide.dev both MIT”", "“Icons: phosphoricons.com and lucide.dev both MIT”",
        "“Testflight build is up, link in testflight.apple.com”",
    ]);
    assert!(host.call(&app, "select_row", &[7_u32.into()]));
    assert_eq!(text(&host, &app, "d_snippet"), "Just the link", "the row and the card say the same");
}

#[test]
fn a_live_message_before_the_first_read_does_not_hide_older_messages_from_the_count() {
    let mut host = Harness::new();
    let now = now_ms();
    let (app, _) = host.launch_stock(APP, false);
    host.boot();
    let mut hooks = Vec::new();
    let (reply, _) = next_read(&mut host, &mut hooks);
    let batch = serde_json::json!([live("$early:test", "erin", "Erin", "first https://early-live.example", now - 10_000)]);
    assert!(host.call_strings(&app, "on_room_message", &[&batch.to_string()]));
    answer(&mut host, reply, &window(now).to_string());
    assert_eq!(text(&host, &app, "scan_line"), "Scanned 7 messages · 6 links");
    assert_eq!(sites(&host, &app)[0], "early-live.example");
    // The same message delivered again by a later read is not counted twice.
    assert!(host.call_strings(&app, "on_room_message", &[&batch.to_string()]));
    assert_eq!(text(&host, &app, "scan_line"), "Scanned 7 messages · 6 links");
}

#[test]
fn share_counts_hold_across_refresh_reopen_and_edits() {
    let mut host = Harness::new();
    let now = now_ms();
    let (app, context, mut audit) = open_library(&mut host, popular_window(now));
    let count = |host: &Harness, app: &WidgetRef| shared_count(&rows(host, app)[0][3]);
    assert_eq!(count(&host, &app), 8);
    let saved = saved_link(&library(&host, &context), "https://github.com/project-robius/robrix");
    assert_eq!((saved["shares"].as_array().unwrap().len(), saved["more"].as_array().unwrap().len()), (6, 2),
        "shares past the six with details keep their ids");
    for _ in 0..2 {
        host.queue_matrix_reply("matrix.room.messages.read", popular_window(now));
        host.broker.forget_app(APP);
        host.widget_click(&app, "Refresh", &[]);
        host.settle_stock(&app, APP, &mut audit);
        drain(&mut host, &app, &mut audit);
        assert_eq!(count(&host, &app), 8, "a Refresh re-reads the same eight messages");
    }
    let mut app = app;
    for _ in 0..2 {
        app = reopen(&mut host, &app, &context, popular_window(now));
        assert_eq!(count(&host, &app), 8, "reopening re-reads the same eight messages");
    }
    assert!(host.call(&app, "select_row", &[0_u32.into()]));
    assert_eq!(text(&host, &app, "d_head"), "SHARED 8 TIMES BY 8 PEOPLE");
    // Editing the oldest share, which only kept its id, leaves the count alone.
    assert!(host.call_strings(&app, "on_room_message_changed", &[r#"{"event_id":"$pop0:test","edited":true,"redacted":false,"body":"have you seen https://github.com/project-robius/robrix ? (fixed typo)"}"#]));
    assert_eq!(count(&host, &app), 8);
    // An edit that drops the link, and a redaction, each take one share away.
    assert!(host.call_strings(&app, "on_room_message_changed", &[r#"{"event_id":"$pop1:test","edited":true,"redacted":false,"body":"never mind"}"#]));
    assert_eq!(count(&host, &app), 7);
    assert!(host.call_strings(&app, "on_room_message_changed", &[r#"{"event_id":"$pop7:test","edited":false,"redacted":true,"body":""}"#]));
    assert_eq!(count(&host, &app), 6);
    drain(&mut host, &app, &mut audit);
    let saved = saved_link(&library(&host, &context), "https://github.com/project-robius/robrix");
    assert_eq!((saved["count"].as_u64(), saved["shares"].as_array().unwrap().len(), saved["more"].as_array().unwrap().len()), (Some(6), 5, 1));
}

#[test]
fn scanning_walks_back_page_by_page_to_the_start_of_the_room() {
    let mut host = Harness::new();
    let now = now_ms();
    let (app, context, mut audit) = open_library(&mut host, window(now));
    host.broker.forget_app(APP);
    host.widget_click(&app, "Scan older messages", &[]);
    assert!(shown(&host, &app, "stop_wrap") && !shown(&host, &app, "scan_btn_wrap"));
    answer_page(&mut host, Some("$m1:test"), serde_json::json!({"messages": [
        message("$o1:test", "Erin", "Old docs https://old.example/a", now - 9 * HOUR),
        message("$o2:test", "Erin", "* https://edited.example/b", now - 8 * HOUR),
        message("$o3:test", "Frank", "nothing here", now - 7 * HOUR),
    ], "has_more": true}));
    assert_eq!(text(&host, &app, "scan_line"), "Scanning… 9 messages · 6 links");
    // The next page waits a second, for the request budget.
    no_requests(&mut host);
    host.broker.forget_app(APP);
    host.boot();
    answer_page(&mut host, Some("$o1:test"), serde_json::json!({"messages": [
        message("$o0:test", "Gail", "first link ever https://first.example", now - 20 * HOUR),
    ], "has_more": false}));
    drain(&mut host, &app, &mut audit);
    assert_eq!(text(&host, &app, "scan_line"), "Reached the start of the room · 10 messages · 7 links");
    assert_eq!(text(&host, &app, "scan_note_label"), "Found 2 new links.");
    assert!(!shown(&host, &app, "scan_btn_wrap") && !shown(&host, &app, "stop_wrap") && !shown(&host, &app, "scan_err_wrap"));
    assert_eq!(sites(&host, &app).last().unwrap(), "first.example");
    let saved = library(&host, &context);
    assert_eq!((saved["before"].as_str(), saved["is_at_start"].as_bool()), (Some("$o0:test"), Some(true)));
    assert!(!saved_urls(&saved).iter().any(|url| url.contains("edited")), "an edit's fallback text isn't a new share");
    assert!(host.call(&app, "scan_older", &[]));
    no_requests(&mut host);
    assert_eq!(text(&host, &app, "scan_note_label"), "You've reached the start of the room.");
}

#[test]
fn one_click_scans_at_most_four_pages_and_stop_ends_it_early() {
    let mut host = Harness::new();
    let now = now_ms();
    let (app, _, mut audit) = open_library(&mut host, window(now));
    let page = |n: u64| serde_json::json!({"messages": [message(&format!("$q{n}:test"), "Erin", &format!("page {n} https://page{n}.example"), now - (10 + n) * HOUR)], "has_more": true});
    host.broker.forget_app(APP);
    assert!(host.call(&app, "scan_older", &[]));
    let mut before = "$m1:test".to_owned();
    for n in 1..=4 {
        host.broker.forget_app(APP);
        if n > 1 { host.boot(); }
        answer_page(&mut host, Some(&before), page(n));
        before = format!("$q{n}:test");
    }
    host.boot();
    no_requests(&mut host);
    assert_eq!(text(&host, &app, "scan_note_label"), "Found 4 new links. Tap Scan older to keep going.");
    assert!(shown(&host, &app, "scan_btn_wrap") && !shown(&host, &app, "stop_wrap"));
    // The next click continues where it stopped; Stop while a page loads ends after it.
    host.broker.forget_app(APP);
    assert!(host.call(&app, "scan_older", &[]));
    let reply = pending_page(&mut host, Some("$q4:test"));
    assert!(host.call(&app, "stop_scan", &[]));
    assert_eq!(text(&host, &app, "scan_note_label"), "Stopping after this page…");
    answer(&mut host, reply, &page(5).to_string());
    assert_eq!(text(&host, &app, "scan_note_label"), "Stopped. Found 1 new link.");
    drain(&mut host, &app, &mut audit);
    no_requests(&mut host);
    assert!(sites(&host, &app).contains(&"page5.example".to_owned()));
    // Stop between pages cancels the next one.
    host.broker.forget_app(APP);
    assert!(host.call(&app, "scan_older", &[]));
    answer_page(&mut host, Some("$q5:test"), page(6));
    assert!(host.call(&app, "stop_scan", &[]));
    assert_eq!(text(&host, &app, "scan_note_label"), "Stopped. Found 1 new link.");
    host.boot();
    no_requests(&mut host);
}

#[test]
fn a_refused_scan_explains_why_and_retries_only_from_the_button() {
    let mut host = Harness::new();
    let (app, _, mut audit) = open_library(&mut host, window(now_ms()));
    host.broker.forget_app(APP);
    assert!(host.call(&app, "scan_older", &[]));
    let (request, review) = effect_review(host.process());
    assert_eq!(request.service, "matrix.older_messages");
    let args: serde_json::Value = serde_json::from_str(&request.args_json).unwrap();
    assert_eq!(args, serde_json::json!({"before": "$m1:test", "limit": 50}));
    host.flow.borrow_mut().cancel_effect(review.id).unwrap();
    services::respond(&mut host.cx, Reply { heap_key: request.heap_key, req_id: request.req_id },
        Err("You didn't allow sharing this with your homeserver."));
    host.cx.with_vm_and_async(|_| {});
    // The refusal shows in the error style, apart from neutral progress notes.
    assert!(shown(&host, &app, "scan_err_wrap") && !shown(&host, &app, "scan_note_wrap"));
    assert_eq!(text(&host, &app, "scan_err"), "Couldn't scan older messages: You didn't allow sharing this with your homeserver.");
    assert!(shown(&host, &app, "scan_btn_wrap") && !shown(&host, &app, "stop_wrap"));
    // Nothing retries on its own, not even after a permission change.
    host.publish_grants(&app, APP);
    host.boot();
    no_requests(&mut host);
    // The button asks again.
    host.broker.forget_app(APP);
    host.widget_click(&app, "Scan older messages", &[]);
    assert!(!shown(&host, &app, "scan_err_wrap"));
    answer_page(&mut host, Some("$m1:test"), serde_json::json!({"messages": [], "has_more": false}));
    drain(&mut host, &app, &mut audit);
    assert_eq!(text(&host, &app, "scan_note_label"), "No new links in those messages.");
    assert_eq!(text(&host, &app, "scan_line"), "Reached the start of the room · 6 messages · 5 links");
}

#[test]
fn an_empty_older_page_with_more_history_is_not_the_start_of_the_room() {
    let mut host = Harness::new();
    let now = now_ms();
    let (app, context, mut audit) = open_library(&mut host, window(now));
    host.broker.forget_app(APP);
    assert!(host.call(&app, "scan_older", &[]));
    // Undecryptable history, or a long run of joins and leaves, comes back as an empty page.
    answer_page(&mut host, Some("$m1:test"), serde_json::json!({"messages": [], "has_more": true}));
    drain(&mut host, &app, &mut audit);
    no_requests(&mut host);
    assert_eq!(text(&host, &app, "scan_line"), "Scanned 6 messages · 5 links");
    assert_eq!(text(&host, &app, "scan_note_label"), "No readable messages further back; older ones may be encrypted. No new links in those messages.");
    assert!(shown(&host, &app, "scan_btn_wrap"), "Scan stays available");
    let saved = library(&host, &context);
    assert_eq!((saved["before"].as_str(), saved["is_at_start"].as_bool()), (Some("$m1:test"), Some(false)));
    // Another tap asks once more.
    host.broker.forget_app(APP);
    host.widget_click(&app, "Scan older messages", &[]);
    answer_page(&mut host, Some("$m1:test"), serde_json::json!({"messages": [message("$o1:test", "Erin", "https://older.example", now - 9 * HOUR)], "has_more": false}));
    drain(&mut host, &app, &mut audit);
    assert_eq!(text(&host, &app, "scan_line"), "Reached the start of the room · 7 messages · 6 links");
}

#[test]
fn blocking_only_older_history_explains_why_scanning_is_unavailable() {
    let mut host = Harness::new();
    let (app, _, mut audit) = open_library(&mut host, window(now_ms()));
    host.permissions.set_capability(APP, "matrix.room.messages.paginate", GrantState::Denied);
    host.publish_grants(&app, APP);
    host.settle_stock(&app, APP, &mut audit);
    assert!(shown(&host, &app, "scan_wrap"));
    assert_eq!(text(&host, &app, "scan_line"), "Scanning older messages is turned off for Link Library in Mini Apps settings.");
    assert!(!shown(&host, &app, "scan_btn_wrap") && !shown(&host, &app, "stop_wrap"));
    assert_eq!(audit.services, ["matrix.room.messages.read"], "the latest messages stay readable");
    assert_eq!(rows(&host, &app).len(), 5);
}

#[test]
fn blocking_one_ability_still_reads_and_watches_the_rest_on_the_next_launch() {
    for (blocked, hooks, status) in [
        ("matrix.room.messages.paginate", vec!["on_room_message", "on_room_message_changed"], "Watching for new links."),
        ("on_room_message_changed", vec!["on_room_message"], "Watching for new links; edits and deletions aren't followed."),
    ] {
        let mut host = Harness::new();
        host.queue_matrix_reply("matrix.room.messages.read", window(now_ms()));
        let (app, _) = host.launch_stock(APP, false);
        host.permissions.set_capability(APP, blocked, GrantState::Denied);
        host.publish_grants(&app, APP);
        host.boot();
        assert!(host.call(&app, "on_app_resize", &[460_f64.into(), 700_f64.into()]));
        let (request, perms) = host.take_stock_batch(APP);
        // Robrix answers a group false when any one of its abilities is blocked.
        let answer = answer_batch_like_robrix(&mut host, &app, request, &perms);
        assert_eq!(answer["granted"], false, "{blocked}");
        let mut audit = StockAudit::default();
        host.settle_stock(&app, APP, &mut audit);
        drain(&mut host, &app, &mut audit);
        assert_eq!(audit.services, ["matrix.room.messages.read"], "{blocked}");
        assert_eq!(audit.subscriptions, hooks, "{blocked}");
        assert_eq!(rows(&host, &app).len(), 5, "{blocked}");
        assert_eq!(text(&host, &app, "status"), status, "{blocked}");
        assert!(audit.prompts.is_empty() && audit.batch_prompts == 0);
    }
}

#[test]
fn a_denied_setup_explains_itself_and_reads_only_after_a_visible_retry() {
    let mut host = Harness::new();
    host.queue_matrix_reply("matrix.room.messages.read", window(now_ms()));
    let (app, _) = host.launch_stock_permissions(APP, false, false);
    for permission in [Permission::MatrixRoomRead, Permission::MatrixRoomWatch] { host.permissions.set(APP, permission, GrantState::Denied); }
    host.publish_grants(&app, APP);
    host.boot();
    let (request, perms) = host.take_stock_batch(APP);
    assert_eq!(perms, [Permission::MatrixRoomRead, Permission::MatrixRoomWatch]);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&request.args_json).unwrap()["retry"], false);
    let mut audit = StockAudit::default();
    host.answer_stock_batch(&app, APP, request, &perms, &[], &mut audit);
    host.settle_stock(&app, APP, &mut audit);
    assert!(audit.services.is_empty() && audit.subscriptions.is_empty());
    assert_eq!(text(&host, &app, "empty_title"), "Can't read this room");
    assert_eq!(text(&host, &app, "empty_text"), "Allow Link Library to read this room's messages to collect the links people share. It only reads; it never posts.");
    assert_eq!(text(&host, &app, "ask_btn"), "Ask again");
    assert!(shown(&host, &app, "ask_wrap") && shown(&host, &app, "ask_btn_wrap") && !shown(&host, &app, "retry_wrap") && shown(&host, &app, "empty_wrap"));
    assert!(!shown(&host, &app, "head_wrap") && !shown(&host, &app, "scan_wrap") && !shown(&host, &app, "search_wrap") && !shown(&host, &app, "body"));
    // A permission change elsewhere can't reopen the prompt.
    host.publish_grants(&app, APP);
    host.settle_stock(&app, APP, &mut audit);
    assert_eq!(audit.batch_prompts, 1);
    assert!(audit.services.is_empty());
    // The visible control asks again, as an explicit retry.
    for permission in [Permission::MatrixRoomRead, Permission::MatrixRoomWatch] { host.permissions.set(APP, permission, GrantState::Ask); }
    host.widget_click(&app, "Ask again", &[]);
    let (request, perms) = host.take_stock_batch(APP);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&request.args_json).unwrap()["retry"], true);
    host.answer_stock_batch(&app, APP, request, &perms, &perms, &mut audit);
    host.settle_stock(&app, APP, &mut audit);
    assert_eq!(audit.batch_prompts, 2);
    assert_eq!(audit.services, ["matrix.room.messages.read"]);
    assert_eq!(audit.subscriptions, ["on_room_message", "on_room_message_changed"]);
    assert_eq!(text(&host, &app, "summary"), "5 links · 5 sites · 3 people");
    assert_eq!(text(&host, &app, "status"), "Watching for new links.");
}

#[test]
fn a_read_error_says_why_once_and_offers_try_again() {
    let mut host = Harness::new();
    let (app, _) = host.launch_stock(APP, false);
    host.boot();
    let mut hooks = Vec::new();
    let (reply, _) = next_read(&mut host, &mut hooks);
    services::respond(&mut host.cx, reply, Err("The homeserver didn't answer in time."));
    host.cx.with_vm_and_async(|_| {});
    let mut audit = StockAudit::default();
    host.settle_stock(&app, APP, &mut audit);
    assert_eq!(text(&host, &app, "empty_title"), "Couldn't read this room");
    assert_eq!(text(&host, &app, "empty_text"), "The homeserver didn't answer in time.");
    assert_eq!(text(&host, &app, "retry_btn"), "Try again");
    assert!(shown(&host, &app, "retry_wrap") && !shown(&host, &app, "ask_btn_wrap"), "a retry, not a request to allow reading");
    assert!(!shown(&host, &app, "head_wrap") && !shown(&host, &app, "scan_wrap"), "nothing repeats the error or offers scanning yet");
    // Nothing retries on its own; the button does.
    host.publish_grants(&app, APP);
    no_requests(&mut host);
    host.queue_matrix_reply("matrix.room.messages.read", window(now_ms()));
    host.widget_click(&app, "Try again", &[]);
    host.settle_stock(&app, APP, &mut audit);
    assert_eq!(rows(&host, &app).len(), 5);
    assert_eq!(text(&host, &app, "status"), "Watching for new links.");
}

#[test]
fn reading_without_live_updates_never_subscribes_and_refresh_rereads() {
    let mut host = Harness::new();
    let now = now_ms();
    host.queue_matrix_reply("matrix.room.messages.read", window(now));
    host.queue_matrix_reply("matrix.room.messages.read", window(now));
    let (app, _) = host.launch_stock_permissions(APP, false, false);
    host.boot();
    let (request, perms) = host.take_stock_batch(APP);
    let mut audit = StockAudit::default();
    host.answer_stock_batch(&app, APP, request, &perms, &[Permission::MatrixRoomRead], &mut audit);
    host.settle_stock(&app, APP, &mut audit);
    assert_eq!(audit.services, ["matrix.room.messages.read"]);
    assert!(audit.subscriptions.is_empty());
    assert_eq!(audit.prompts, [Permission::MatrixRoomRead]);
    assert_eq!(text(&host, &app, "status"), "Live updates are off. Tap Refresh to check for new links.");
    assert_eq!(rows(&host, &app).len(), 5);
    host.publish_grants(&app, APP);
    host.settle_stock(&app, APP, &mut audit);
    assert_eq!((audit.services.len(), audit.subscriptions.len(), audit.batch_prompts), (1, 0, 1));
    // Refresh asks once more; here the user allows live updates this time.
    host.broker.forget_app(APP);
    host.widget_click(&app, "Refresh", &[]);
    host.settle_stock(&app, APP, &mut audit);
    assert_eq!(audit.services, ["matrix.room.messages.read", "matrix.room.messages.read"]);
    assert_eq!(audit.subscriptions, ["on_room_message", "on_room_message_changed"]);
    assert_eq!(rows(&host, &app).len(), 5, "a re-read adds nothing twice");
    assert_eq!(text(&host, &app, "scan_line"), "Scanned 6 messages · 5 links");
}

#[test]
fn revoking_and_restoring_access_resyncs_without_prompting_again() {
    let mut host = Harness::new();
    let (app, _, mut audit) = open_library(&mut host, window(now_ms()));
    let reads = audit.services.len();
    // Reading revoked: saved links stay, nothing is re-read, and the scan control hides.
    host.permissions.set(APP, Permission::MatrixRoomRead, GrantState::Denied);
    host.publish_grants(&app, APP);
    host.settle_stock(&app, APP, &mut audit);
    assert_eq!(audit.services.len(), reads);
    assert_eq!(text(&host, &app, "status"), "Reading this room is off, so only new links are added. Tap Refresh to ask again.");
    assert_eq!(rows(&host, &app).len(), 5);
    assert!(!shown(&host, &app, "scan_wrap"));
    host.permissions.set(APP, Permission::MatrixRoomRead, GrantState::Granted);
    host.publish_grants(&app, APP);
    host.settle_stock(&app, APP, &mut audit);
    assert_eq!(audit.services.len(), reads + 1, "an allowed read that hadn't completed runs again");
    assert!(shown(&host, &app, "scan_wrap"));
    // Live updates revoked, then restored: both hooks come back, with no prompt.
    host.permissions.set(APP, Permission::MatrixRoomWatch, GrantState::Denied);
    host.publish_grants(&app, APP);
    host.settle_stock(&app, APP, &mut audit);
    assert_eq!(text(&host, &app, "status"), "Live updates are off. Tap Refresh to check for new links.");
    let before = audit.subscriptions.len();
    host.permissions.set(APP, Permission::MatrixRoomWatch, GrantState::Granted);
    host.publish_grants(&app, APP);
    host.settle_stock(&app, APP, &mut audit);
    assert_eq!(&audit.subscriptions[before..], ["on_room_message", "on_room_message_changed"]);
    assert_eq!(text(&host, &app, "status"), "Watching for new links.");
    assert!(audit.prompts.is_empty() && audit.batch_prompts == 0);
    // A grant elsewhere repeats nothing.
    let done = (audit.services.len(), audit.subscriptions.len());
    host.publish_grants(&app, APP);
    host.settle_stock(&app, APP, &mut audit);
    assert_eq!((audit.services.len(), audit.subscriptions.len()), done);
}

#[test]
fn replies_from_an_earlier_read_scan_or_selection_are_ignored() {
    let mut host = Harness::new();
    let now = now_ms();
    let (app, context) = host.launch_stock(APP, false);
    host.boot();
    let mut hooks = Vec::new();
    let (first, _) = next_read(&mut host, &mut hooks);
    // Reading is revoked and restored while the first read is in flight.
    host.permissions.set(APP, Permission::MatrixRoomRead, GrantState::Denied);
    host.publish_grants(&app, APP);
    host.permissions.set(APP, Permission::MatrixRoomRead, GrantState::Granted);
    host.publish_grants(&app, APP);
    let (second, _) = next_read(&mut host, &mut hooks);
    answer(&mut host, second, &window(now).to_string());
    answer(&mut host, first, &serde_json::json!({"messages": [message("$stale:test", "Old", "https://stale.example", now)]}).to_string());
    assert!(!sites(&host, &app).contains(&"stale.example".to_owned()));
    assert_eq!(rows(&host, &app).len(), 5);

    // A page that arrives after reading was revoked mid-scan is dropped.
    host.broker.forget_app(APP);
    assert!(host.call(&app, "scan_older", &[]));
    let page = pending_page(&mut host, Some("$m1:test"));
    host.permissions.set(APP, Permission::MatrixRoomRead, GrantState::Denied);
    host.publish_grants(&app, APP);
    answer(&mut host, page, &serde_json::json!({"messages": [message("$late:test", "Old", "https://late.example", now - 9 * HOUR)], "has_more": true}).to_string());
    host.boot();
    no_requests(&mut host);
    assert!(!sites(&host, &app).contains(&"late.example".to_owned()));
    host.permissions.set(APP, Permission::MatrixRoomRead, GrantState::Granted);
    host.publish_grants(&app, APP);
    let (reread, _) = next_read(&mut host, &mut hooks);
    answer(&mut host, reread, &window(now).to_string());

    // An action's reply for an earlier selection doesn't speak for the new one.
    assert!(host.call(&app, "select_row", &[0_u32.into()]));
    host.broker.forget_app(APP);
    assert!(host.call(&app, "open_link", &[]));
    let (request, review) = effect_review(host.process());
    assert!(host.call(&app, "select_row", &[1_u32.into()]));
    host.flow.borrow_mut().approve_effect_once(&review).unwrap();
    let args: serde_json::Value = serde_json::from_str(&request.args_json).unwrap();
    assert_eq!(args["url"], "https://www.rust-lang.org");
    host.flow.borrow_mut().commit_effect_for_activation(&context, review.epoch, review.recipient.as_ref(), review.action.as_ref(), &args).unwrap();
    answer(&mut host, Reply { heap_key: request.heap_key, req_id: request.req_id }, "{}");
    assert_eq!(text(&host, &app, "d_site"), "en.wikipedia.org");
    assert!(!shown(&host, &app, "d_note_wrap"));
    // The finished request no longer blocks the button.
    assert!(host.call(&app, "open_link", &[]));
    let args = finish_reviewed(&mut host, &context, "url.open", Ok("{}"));
    assert_eq!(args["url"], "https://en.wikipedia.org/wiki/Rust_(programming_language)");
}

#[test]
fn the_library_reopens_instantly_and_merges_without_counting_twice() {
    let mut host = Harness::new();
    let now = now_ms();
    let (app, context, _) = open_library(&mut host, window(now));
    host.retire_stock(&app, &context);
    let mut messages = window(now)["messages"].as_array().unwrap()[3..].to_vec();
    messages.push(message("$m7:test", "Erin", "Fresh: https://fresh.example/new", now - MINUTE));
    host.queue_matrix_reply("matrix.room.messages.read", serde_json::json!({"messages": messages}));
    let (app, _) = host.launch_stock(APP, false);
    // Boot parses the file; the next tick rebuilds the links, before any request finishes.
    host.boot();
    host.boot();
    assert_eq!(rows(&host, &app).len(), 5, "saved links show before any request finishes");
    assert_eq!(text(&host, &app, "summary"), "5 links · 5 sites · 3 people");
    let mut audit = StockAudit::default();
    host.settle_stock(&app, APP, &mut audit);
    drain(&mut host, &app, &mut audit);
    assert_eq!(audit.services, ["matrix.room.messages.read"]);
    assert_eq!(sites(&host, &app)[0], "fresh.example");
    assert_eq!(rows(&host, &app).len(), 6);
    assert_eq!(rows(&host, &app)[4][3], "Carol · 3h ago · shared 2×", "a re-read message isn't counted twice");
    assert_eq!(text(&host, &app, "scan_line"), "Scanned 7 messages · 6 links");
    assert_eq!(library(&host, &context)["before"], "$m1:test", "the saved scan position is kept");
}

#[test]
fn corrupt_or_foreign_saved_files_fall_back_safely() {
    let now = now_ms();
    let share = |e: &str, t: u64| serde_json::json!({"e": e, "s": "ann", "u": "@ann:test", "t": t, "x": "kept"});
    let valid = |url: &str, t: u64| serde_json::json!({"url": url, "count": 1, "first_ts": t, "shares": [share(&format!("$s{t}:test"), t)]});
    for (label, file, expected) in [
        ("not json", "{not json".to_owned(), 5),
        ("an older version", serde_json::json!({"v": 1, "links": [valid("https://ok.example/a", now)]}).to_string(), 5),
        ("not an object", "[1, 2, 3]".to_owned(), 5),
        ("mixed", serde_json::json!({"v": 2, "room_id": ROOM, "before": 7, "scanned": "x", "newest_ts": "y",
            "gaps": [1, {"ts": "x", "before": "$a"}, {"ts": 5}, null], "names": [1, ["a"], ["@x:test", 5]], "links": [
            valid("https://ok.example/newest", now - HOUR),
            "junk", 4, null,
            {"url": "javascript:alert(1)", "shares": [share("$e:test", now)]},
            {"url": "https://bad-shares.example", "shares": [{"e": "", "t": 1}, 3]},
            valid("https://ok.example/newest", now - 2 * HOUR),
            valid("https://ok.example/out-of-order", now),
            valid("https://ok.example/older", now - 3 * HOUR),
            {"url": "https://ids.example/a", "count": "9", "shares": [share("$i1:test", now - 4 * HOUR)],
                "more": ["x", {"e": "$i2:test"}, {"e": "$i1:test", "t": 1, "u": "@a:test"}, {"e": "$i3:test", "t": now, "u": "@a:test"},
                    {"e": "$i4:test", "t": now - 5 * HOUR, "u": "@b:test"}]},
        ]}).to_string(), 8),
        ("another room", serde_json::json!({"v": 2, "room_id": "!other:test", "links": [valid("https://other-room.example", now)]}).to_string(), 5),
        ("larger than the app writes", full_library(260, 6, 0), 5),
    ] {
        let mut host = Harness::new();
        write_library(&host, &room_context(), &file);
        let (app, context, _) = open_library(&mut host, window(now));
        assert_eq!(rows(&host, &app).len(), expected, "{label}: {:?}", sites(&host, &app));
        assert!(!sites(&host, &app).iter().any(|site| site == "evil" || site == "bad-shares.example" || site == "other-room.example"), "{label}");
        let saved = library(&host, &context);
        assert_eq!(saved["links"].as_array().unwrap().len(), expected, "{label} is rewritten cleanly");
        if label == "mixed" {
            let ids = saved_link(&saved, "https://ids.example/a");
            assert_eq!(ids["more"], serde_json::json!([{"e": "$i4:test", "t": now - 5 * HOUR, "u": "@b:test"}]), "only well-formed, older ids are kept");
            assert_eq!(ids["count"], 2);
            assert_eq!(saved["gaps"], serde_json::json!([]));
        }
    }
}

#[test]
fn a_gap_since_the_last_visit_is_scanned_before_older_history() {
    let mut host = Harness::new();
    let now = now_ms();
    let (app, context, _) = open_library(&mut host, serde_json::json!({"messages": [
        message("$a1:test", "Ann", "early https://early.example", now - 30 * HOUR),
        message("$a2:test", "Ann", "later https://later.example", now - 29 * HOUR),
    ]}));
    // Many messages arrived while the app was closed: the new window starts after the saved one ends.
    let app = reopen(&mut host, &app, &context, serde_json::json!({"messages": [
        message("$b1:test", "Ben", "today https://today.example", now - 2 * HOUR),
    ]}));
    let saved = library(&host, &context);
    assert_eq!(saved["before"], "$b1:test");
    assert_eq!(saved["gaps"], serde_json::json!([{"before": "$a1:test", "ts": now - 29 * HOUR, "is_at_start": false}]));
    host.broker.forget_app(APP);
    assert!(host.call(&app, "scan_older", &[]));
    // The page reaches back into the saved range: those messages aren't counted again.
    answer_page(&mut host, Some("$b1:test"), serde_json::json!({"messages": [
        message("$a2:test", "Ann", "later https://later.example", now - 29 * HOUR),
        message("$g1:test", "Gus", "missed https://missed.example", now - 10 * HOUR),
    ], "has_more": true}));
    assert!(sites(&host, &app).contains(&"missed.example".to_owned()));
    assert_eq!(text(&host, &app, "scan_line"), "Scanning… 4 messages · 4 links");
    // Then it continues from where the earlier visit stopped.
    host.broker.forget_app(APP);
    host.boot();
    answer_page(&mut host, Some("$a1:test"), serde_json::json!({"messages": [], "has_more": false}));
    let mut audit = StockAudit::default();
    drain(&mut host, &app, &mut audit);
    let saved = library(&host, &context);
    assert_eq!(saved["gaps"], serde_json::json!([]));
    assert_eq!(saved["is_at_start"], true);
}

#[test]
fn scanning_bridges_every_gap_between_visits() {
    let mut host = Harness::new();
    let now = now_ms();
    // The room's whole text timeline, oldest first: three busy days, one link each.
    let mut timeline = Vec::new();
    for (day, start) in [("one", 72_u64), ("two", 48), ("three", 24)] {
        for i in 0..40_u64 {
            let body = if i == 5 { format!("day {day} https://day-{day}.example") } else { format!("chat {day} {i}") };
            timeline.push(message(&format!("${day}{i}:test"), "Ann", &body, now - start * HOUR + i * MINUTE));
        }
    }
    let latest_until = |end: usize| serde_json::json!({"messages": timeline[end - 30..end].to_vec()});
    // A visit after each day reads only its latest 30 messages.
    let (app, context, _) = open_library(&mut host, latest_until(40));
    let app = reopen(&mut host, &app, &context, latest_until(80));
    let app = reopen(&mut host, &app, &context, latest_until(120));
    assert_eq!(library(&host, &context)["gaps"].as_array().unwrap().len(), 2);
    let anchors = scan_to_start(&mut host, &app, &timeline);
    assert_eq!(anchors, [Some("$three10:test".to_owned()), Some("$two10:test".to_owned()), Some("$one10:test".to_owned())]);
    let mut urls = saved_urls(&library(&host, &context));
    urls.sort();
    assert_eq!(urls, ["https://day-one.example", "https://day-three.example", "https://day-two.example"]);
    assert_eq!(text(&host, &app, "scan_line"), "Reached the start of the room · 120 messages · 3 links");
}

#[test]
fn without_a_room_it_explains_where_to_open_it_and_reads_nothing() {
    let mut host = Harness::new();
    let app = launch_in(&mut host, ContextId::App { account: ACCOUNT.into(), app: APP.into(), room: None }, true);
    host.boot();
    assert!(host.call(&app, "on_app_resize", &[460_f64.into(), 700_f64.into()]));
    let mut audit = StockAudit::default();
    host.settle_stock(&app, APP, &mut audit);
    assert!(audit.services.is_empty() && audit.subscriptions.is_empty() && audit.batch_prompts == 0);
    assert_eq!(text(&host, &app, "room_line"), "Not attached to a room");
    assert_eq!(text(&host, &app, "empty_title"), "No room attached");
    assert_eq!(text(&host, &app, "empty_text"), "Open Link Library from a room to see the links people share.");
    assert!(shown(&host, &app, "empty_wrap") && !shown(&host, &app, "ask_wrap"));
    assert!(!shown(&host, &app, "head_wrap") && !shown(&host, &app, "scan_wrap") && !shown(&host, &app, "search_wrap"));
    assert!(host.call(&app, "scan_older", &[]));
    assert!(host.call_strings(&app, "on_room_message", &[LIVE_LINK]));
    host.publish_grants(&app, APP);
    no_requests(&mut host);
    assert_eq!(rows(&host, &app).len(), 0);
}

#[test]
fn layout_tiers_follow_the_pane_size() {
    let mut host = Harness::new();
    let (app, _, _) = open_library(&mut host, window(now_ms()));
    assert!(shown(&host, &app, "title_wrap"));
    // Wide panes put the details beside the list.
    assert!(host.call(&app, "on_app_resize", &[1200_f64.into(), 900_f64.into()]));
    assert!(shown(&host, &app, "side_wrap") && shown(&host, &app, "s_empty_wrap") && !shown(&host, &app, "s_card_wrap"));
    assert!(host.call(&app, "select_row", &[3_u32.into()]));
    assert!(shown(&host, &app, "s_card_wrap") && !shown(&host, &app, "detail_wrap"));
    assert_eq!(text(&host, &app, "s_site"), "github.com");
    assert_eq!(items(&host, &app, "s_people"), [vec!["C", "Carol", "3h ago"], vec!["A", "Alice", "5h ago"]]);
    // Narrow, short panes stack the card, shorten labels and drop the room line.
    assert!(host.call(&app, "on_app_resize", &[300_f64.into(), 400_f64.into()]));
    assert!(!shown(&host, &app, "title_wrap") && !shown(&host, &app, "side_wrap"));
    assert!(shown(&host, &app, "detail_wrap"));
    assert_eq!(text(&host, &app, "d_site"), "github.com");
    assert_eq!(text(&host, &app, "summary"), "5 links · 5 sites", "a narrow summary fits on one line");
    assert!(host.call(&app, "close_detail", &[]));
    assert_eq!(text(&host, &app, "scan_btn"), "Scan older");
    assert_eq!(text(&host, &app, "scan_line"), "Scanned 6 messages");
    assert!(host.call(&app, "on_app_resize", &[460_f64.into(), 700_f64.into()]));
    assert!(shown(&host, &app, "title_wrap"));
    assert_eq!(text(&host, &app, "scan_btn"), "Scan older messages");
    assert_eq!(text(&host, &app, "summary"), "5 links · 5 sites · 3 people");
}

#[test]
fn tiny_panes_stack_controls_under_their_text() {
    let mut host = Harness::new();
    let (app, _, _) = open_library(&mut host, sites_window(now_ms()));
    assert!(shown(&host, &app, "refresh_wide") && !shown(&host, &app, "refresh_narrow") && shown(&host, &app, "chip_wrap"));
    // Below 300 points, Refresh and the scan button move under their text, which keeps the full width.
    assert!(host.call(&app, "on_app_resize", &[220_f64.into(), 600_f64.into()]));
    assert!(!shown(&host, &app, "refresh_wide") && shown(&host, &app, "refresh_narrow"));
    assert!(shown(&host, &app, "scan_top_wrap") && !shown(&host, &app, "scan_line_wrap") && shown(&host, &app, "scan_btn_wrap"));
    assert_eq!(text(&host, &app, "scan_line_top"), "Scanned 6 messages");
    // Site chips give way to the search unless a site is picked, which stays undoable.
    assert!(!shown(&host, &app, "chip_wrap"));
    assert!(host.call(&app, "pick_chip", &[1_u32.into()]));
    assert!(shown(&host, &app, "chip_wrap"));
    assert!(host.call(&app, "on_app_resize", &[460_f64.into(), 700_f64.into()]));
    assert!(shown(&host, &app, "refresh_wide") && !shown(&host, &app, "refresh_narrow") && shown(&host, &app, "scan_line_wrap"));
}

#[test]
fn robrix_errors_read_as_sentences_without_repeating_the_lead_in() {
    let mut host = Harness::new();
    let (app, _) = host.launch_stock(APP, false);
    host.boot();
    let mut hooks = Vec::new();
    let (reply, _) = next_read(&mut host, &mut hooks);
    services::respond(&mut host.cx, reply, Err("couldn't read messages: room not joined"));
    host.cx.with_vm_and_async(|_| {});
    let mut audit = StockAudit::default();
    host.settle_stock(&app, APP, &mut audit);
    assert_eq!(text(&host, &app, "empty_title"), "Couldn't read this room");
    assert_eq!(text(&host, &app, "empty_text"), "Room not joined.");
}

#[test]
fn a_new_filter_starts_its_list_at_the_top() {
    let mut host = Harness::new();
    let (app, _, mut audit) = open_library(&mut host, sites_window(now_ms()));
    assert!(host.cx.script_data.timers.timers.is_empty());
    // Splash can't scroll a list: a new site draws it empty for a frame, so the scroll bar
    // settles at the top, and the matches follow on the next tick.
    assert!(host.call(&app, "pick_chip", &[1_u32.into()]));
    assert!(rows(&host, &app).is_empty());
    assert_eq!(text(&host, &app, "list_head"), "3 links on github.com");
    host.boot();
    no_requests(&mut host);
    assert_eq!(sites(&host, &app), ["github.com", "github.com", "github.com"]);
    // Starting a search resets once; later keystrokes keep the list and add one follow-up render.
    for query in ["b", "be", "ben"] { type_query(&mut host, &app, query); }
    assert!(host.cx.script_data.timers.timers.len() <= 2, "no timer per keystroke");
    drain(&mut host, &app, &mut audit);
    no_requests(&mut host);
    assert_eq!(rows(&host, &app).len(), 1);
    assert_eq!(text(&host, &app, "list_head"), "1 match on github.com");
    // Selecting a row redraws the list in place.
    assert!(host.call(&app, "select_row", &[0_u32.into()]));
    assert_eq!(rows(&host, &app).len(), 1);
}

#[test]
fn heavy_entries_stay_within_the_production_budgets() {
    let mut host = Harness::new();
    let now = now_ms();
    let context = room_context();
    // Boot parses this file and later ticks rebuild it, each within the 200,000-instruction cap.
    write_library(&host, &context, &full_library(160, 6, 2));
    let (app, context, mut audit) = open_library(&mut host, serde_json::json!({"messages": dense_messages("w", 30, 8, now - MINUTE)}));
    assert!(text(&host, &app, "summary").starts_with("400 links · "), "{}", text(&host, &app, "summary"));
    assert_eq!(rows(&host, &app).len(), 50, "rows render in steps of 50");
    assert_eq!(text(&host, &app, "more_btn"), "Show 50 more");
    with_entry_budget(&mut host, &app, |host| host.widget_click(&app, "Show 50 more", &[]));
    assert_eq!(rows(&host, &app).len(), 100);
    // Typing searches all 400 links on every keystroke, through the search box itself.
    for query in ["s", "si", "site-1", "site-12 person", "nothing-matches-this"] { type_query(&mut host, &app, query); }
    assert_eq!(text(&host, &app, "empty_title"), "No matches");
    // The empty card appears with the deferred redraw.
    host.boot();
    with_entry_budget(&mut host, &app, |host| host.widget_click(&app, "Clear filters", &[]));
    host.boot();
    drain(&mut host, &app, &mut audit);
    // Saved links keep six shares' details and more ids; the rest are counted and dated.
    type_query(&mut host, &app, "site-0.example/page/0");
    host.boot();
    let list = app.widget(&host.cx, &[LiveId::from_str("link_list")]);
    host.widget_callback(&app, &list, id!(on_item_tap), &[0_u32.into()]);
    assert_eq!(text(&host, &app, "d_head"), "SHARED 10 TIMES BY 8+ PEOPLE");
    let people = items(&host, &app, "d_people");
    assert_eq!(people.last().unwrap(), &["and 7 earlier shares, the first 39m ago"], "{people:?}");
    assert!(host.call(&app, "close_detail", &[]));
    // With results on screen, the search box's own clear button.
    with_entry_budget(&mut host, &app, |host| host.widget_click(&app, "×", &[]));
    host.boot();
    // A burst of link-dense live messages, through Robrix's own hook entry.
    let burst: Vec<_> = dense_messages("live", 30, 8, now).into_iter().map(|mut m| { m["sender_name"] = "Zed".into(); m }).collect();
    let used = call_production(&mut host, &app, "on_room_message", &[&serde_json::Value::Array(burst).to_string()]);
    assert!(used < 120_000, "a hook slice used {used} of its 200,000 instructions");
    drain(&mut host, &app, &mut audit);
    assert!(text(&host, &app, "summary").starts_with("400 links · "));
    let used = call_production(&mut host, &app, "on_room_message_changed", &[r#"{"event_id":"$live-0:test","edited":false,"redacted":true,"body":""}"#]);
    assert!(used < 120_000, "{used}");
    drain(&mut host, &app, &mut audit);
    // A full page of link-dense history is older than every kept link.
    host.broker.forget_app(APP);
    assert!(host.call(&app, "scan_older", &[]));
    answer_page(&mut host, Some("$saved-oldest:test"), serde_json::json!({"messages": dense_messages("old", 50, 8, now - 400 * HOUR), "has_more": true}));
    drain(&mut host, &app, &mut audit);
    assert_eq!(text(&host, &app, "scan_note_label"), "The library keeps the newest 400 links, so older ones aren't kept.");
    // The saved file stays small enough to parse within one entry, and reopening rebuilds it.
    let path = host.flow.borrow().context_storage_path(&context).unwrap().join("library.json");
    let bytes = std::fs::metadata(&path).unwrap().len();
    assert!(bytes <= 260_000, "{bytes} bytes");
    let app = reopen(&mut host, &app, &context, serde_json::json!({"messages": dense_messages("w", 30, 8, now - MINUTE)}));
    assert!(text(&host, &app, "summary").ends_with(" people"), "{}", text(&host, &app, "summary"));
    assert_eq!(rows(&host, &app).len(), 50);
}

/// The body of the shared `run_every_stock_app` audit for this app alone.
fn audit_link_library(strict: bool) {
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
    // The shared audit's arm for "link-library":
    assert!(host.call_strings(&app, "on_room_message", &[LIVE_LINK]));
    assert!(host.call(&app, "select_row", &[0_u32.into()]));
    assert!(host.call(&app, "show_in_chat", &[]));
    host.settle_stock(&app, &manifest.id, &mut audit);
    assert_eq!(audit.actions, ["event"]);
    eprintln!("stock permission audit: {} strict={} initial={} action={} initial_reviews={} action_reviews={}",
        manifest.id, strict, initial_prompts, audit.prompts.len() - initial_prompts, initial_reviews, audit.reviews - initial_reviews);
    if !strict {
        assert_eq!((initial_prompts, audit.prompts.len() - initial_prompts), (2, 1), "{:?}", audit.prompts);
        assert_eq!((initial_reviews, audit.reviews - initial_reviews), (0, 1));
    }
    assert!(audit.prompts.len() <= manifest.permissions.len());
    assert!(audit.prompts.iter().all(|permission| manifest.declares(*permission)));
    assert!(audit.reviews <= 3);
    assert!(!audit.services.is_empty() || !audit.subscriptions.is_empty());
    let services = audit.services.len();
    let subscriptions = audit.subscriptions.len();
    host.publish_grants(&app, &manifest.id);
    host.settle_stock(&app, &manifest.id, &mut audit);
    assert_eq!(audit.services.len(), services, "reloads on an unrelated grant");
    assert_eq!(audit.subscriptions.len(), subscriptions, "subscribes again on an unrelated grant");
    let mut hooks = audit.subscriptions.clone(); hooks.sort(); hooks.dedup();
    assert_eq!(hooks.len(), audit.subscriptions.len(), "duplicate subscriptions");
}

#[test]
fn shared_audit_arm_passes() {
    audit_link_library(false);
}

#[test]
fn shared_audit_arm_passes_with_strict_permissions() {
    audit_link_library(true);
}

/// The shared `grouped_stock_startups_*`, refusal and resubscribe suites, run for this app.
#[test]
fn shared_grouped_startup_suites_pass() {
    let expected = vec![Permission::MatrixRoomRead, Permission::MatrixRoomWatch];
    // A denial finishes, and a new approval refreshes.
    let mut host = Harness::new();
    let (app, _) = host.launch_stock_permissions(APP, false, false);
    for permission in &expected { host.permissions.set(APP, *permission, GrantState::Denied); }
    host.publish_grants(&app, APP);
    host.boot();
    let (request, perms) = host.take_stock_batch(APP);
    assert_eq!(perms, expected);
    let mut audit = StockAudit::default();
    host.answer_stock_batch(&app, APP, request, &perms, &[], &mut audit);
    host.settle_stock(&app, APP, &mut audit);
    assert!(!host.stock_callbacks_paused());
    assert!(audit.services.is_empty() && audit.subscriptions.is_empty());
    assert_eq!(audit.batch_prompts, 1);
    host.publish_grants(&app, APP);
    host.settle_stock(&app, APP, &mut audit);
    assert_eq!(audit.batch_prompts, 1);
    assert!(audit.services.is_empty());
    for permission in &perms { host.permissions.set(APP, *permission, GrantState::Ask); }
    host.widget_click_any(&app, &["Ask again", "Refresh"], &[]);
    host.settle_stock(&app, APP, &mut audit);
    assert!(!audit.services.is_empty());
    assert!(!host.stock_callbacks_paused());
    assert!(audit.batch_prompts <= 2);
    let reads = audit.services.len();
    let hooks = audit.subscriptions.len();
    for permission in &perms { host.permissions.set(APP, *permission, GrantState::Denied); }
    host.publish_grants(&app, APP);
    host.settle_stock(&app, APP, &mut audit);
    assert_eq!(audit.services.len(), reads);
    assert_eq!(audit.subscriptions.len(), hooks);
    for permission in &perms { host.permissions.set(APP, *permission, GrantState::Granted); }
    host.publish_grants(&app, APP);
    host.settle_stock(&app, APP, &mut audit);
    assert!(audit.services.len() > reads, "refreshes after reapproval");

    // A partial approval uses the selected read only.
    let mut host = Harness::new();
    let (app, _) = host.launch_stock_permissions(APP, false, false);
    for permission in &expected { host.permissions.set(APP, *permission, GrantState::Denied); }
    host.publish_grants(&app, APP);
    host.boot();
    let (request, perms) = host.take_stock_batch(APP);
    let mut audit = StockAudit::default();
    host.answer_stock_batch(&app, APP, request, &perms, &[Permission::MatrixRoomRead], &mut audit);
    host.settle_stock(&app, APP, &mut audit);
    assert_eq!(audit.batch_prompts, 1);
    assert_eq!(audit.prompts, [Permission::MatrixRoomRead]);
    assert!(!audit.services.is_empty());
    assert!(audit.subscriptions.is_empty());
    let reads = audit.services.len();
    host.publish_grants(&app, APP);
    host.settle_stock(&app, APP, &mut audit);
    assert_eq!(audit.services.len(), reads);
    assert_eq!(audit.batch_prompts, 1);

    // Strict refusals finish and retry only from the visible control.
    let mut host = Harness::new();
    host.permissions.set_strict(true);
    let (app, _) = host.launch_stock_permissions(APP, false, false);
    host.call(&app, "on_app_resize", &[460_f64.into(), 700_f64.into()]);
    // Setup asks at startup; Refresh only appears once there are links.
    host.boot();
    assert!(host.deny_stock_pending(&app, APP) > 0);
    assert!(!host.stock_callbacks_paused());
    assert!(host.broker.failures().iter().all(|failure| failure.error == "Permission denied."));
    host.publish_grants(&app, APP);
    for ask in host.process() { assert!(matches!(ask, BrokerAsk::Used { .. }), "no automatic retry"); }
    for permission in host.apps.get(APP).unwrap().permissions.clone() {
        host.permissions.set(APP, Permission::from_str(&permission).unwrap(), GrantState::Ask);
    }
    host.widget_click_any(&app, &["Ask again", "Refresh"], &[]);
    let mut audit = StockAudit::default();
    host.settle_stock(&app, APP, &mut audit);
    assert!(!host.stock_callbacks_paused());
    assert!(!audit.services.is_empty() || !audit.subscriptions.is_empty());

    // Revoking and regranting live updates resubscribes without a prompt.
    let mut host = Harness::new();
    let (app, _) = host.launch_stock(APP, false);
    host.boot();
    let mut audit = StockAudit::default();
    host.settle_stock(&app, APP, &mut audit);
    let hooks = audit.subscriptions.clone();
    host.permissions.set(APP, Permission::MatrixRoomWatch, GrantState::Denied);
    host.publish_grants(&app, APP);
    host.settle_stock(&app, APP, &mut audit);
    let before = audit.subscriptions.len();
    host.permissions.set(APP, Permission::MatrixRoomWatch, GrantState::Granted);
    host.publish_grants(&app, APP);
    host.settle_stock(&app, APP, &mut audit);
    let restored = &audit.subscriptions[before..];
    assert!(!restored.is_empty() && restored.iter().all(|hook| hooks.contains(hook)));
    assert!(audit.prompts.is_empty());
}
