//! Real Splash examples, host callbacks and durable app state. No live HTTP or Matrix.
use std::{path::PathBuf, sync::atomic::{AtomicU64, Ordering}};
use makepad_widgets::{*, splash::Splash, splash_host::{SplashHostRequest, take_splash_host_requests, splash_host_respond, SplashRespondOutcome}};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

struct App {
    cx: Cx,
    host: WidgetRef,
    root: PathBuf,
}

impl App {
    fn launch(id: &str, files: &[(&str, &str)]) -> Self {
        let root = std::env::temp_dir().join(format!("background_app_{}_{}", std::process::id(), NEXT_ROOT.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&root).unwrap();
        for (path, text) in files { std::fs::write(root.join(path), text).unwrap(); }
        let mut cx = Cx::new(Box::new(|_, _| {}));
        cx.with_vm(makepad_widgets::script_mod);
        let mut app = Self { cx, host: WidgetRef::empty(), root };
        app.reload(id);
        app
    }

    fn reload(&mut self, id: &str) {
        self.host.set_text(&mut self.cx, "");
        self.host = self.cx.with_vm(|vm| {
            let value = vm.eval(script! { use mod.widgets.* Splash{} });
            WidgetRef::script_from_value(vm, value)
        });
        makepad_widgets::widget_tree::set_ui_root(&mut self.cx, &self.host);
        let source = a2app_core::builtin::stock(id).unwrap().source;
        let mut splash = self.host.borrow_mut::<Splash>().unwrap();
        splash.set_host_io_only(true);
        splash.set_allow_net(false);
        splash.set_host_tag(&mut self.cx, Some(id.into()));
        splash.set_sandbox_dir(&mut self.cx, Some(self.root.clone()));
        splash.set_text(&mut self.cx, &source);
    }

    fn run(&mut self, payload: serde_json::Value) {
        assert!(self.host.borrow_mut::<Splash>().unwrap().call_script_fn_with_strings(
            &mut self.cx, id!(on_background), &[&payload.to_string()]));
    }

    fn requests(&mut self) -> Vec<SplashHostRequest> {
        self.cx.with_vm_and_async(|_| {});
        take_splash_host_requests()
    }

    fn one(&mut self, service: &str) -> SplashHostRequest {
        let mut requests = self.requests();
        assert_eq!(requests.len(), 1, "expected only {service}, got {requests:?}");
        let request = requests.pop().unwrap();
        assert_eq!(request.service, service);
        request
    }

    fn reply(&mut self, req: &SplashHostRequest, result: Result<&str, &str>) {
        assert_eq!(splash_host_respond(&mut self.cx, req.heap_key, req.req_id, result), SplashRespondOutcome::Delivered);
    }

    fn completion(&mut self, run_id: u64, success: bool) {
        let request = self.one("background.complete");
        assert_eq!(serde_json::from_str::<serde_json::Value>(&request.args_json).unwrap(), serde_json::json!({"run_id":run_id,"success":success}));
        assert_eq!(splash_host_respond(&mut self.cx, request.heap_key, request.req_id, Ok("{}")), SplashRespondOutcome::NoCallback);
        assert!(self.requests().is_empty());
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.host.set_text(&mut self.cx, "");
        let _ = take_splash_host_requests();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn background_examples_pass_real_strict_script_validation() {
    let mut cx = Cx::new(Box::new(|_, _| {}));
    cx.with_vm(makepad_widgets::script_mod);
    for id in ["website-watch", "reminder", "keyword-alert"] {
        let stock = a2app_core::builtin::stock(id).unwrap();
        assert!(stock.supports_background());
        let errors = makepad_widgets::splash::validate_splash_body_with_host_io(&mut cx, &stock.source);
        assert!(errors.is_empty(), "{id} failed strict validation: {errors:?}");
    }
}

#[test]
fn website_notifies_after_match_and_retains_deduplication_across_restart() {
    let mut app = App::launch("website-watch", &[]);
    app.run(serde_json::json!({"run_id":1}));
    let fetch = app.one("network.http");
    assert_eq!(serde_json::from_str::<serde_json::Value>(&fetch.args_json).unwrap()["url"], "https://example.com/");
    app.reply(&fetch, Ok(r#"{"status":200,"body":"<h1>Example Domain</h1>"}"#));
    let notify = app.one("notify.post");
    assert!(notify.args_json.contains("Example Domain"));
    app.reply(&notify, Ok("{}"));
    app.completion(1, true);
    assert_eq!(std::fs::read_to_string(app.root.join("website_match.json")).unwrap(), "true");

    app.reload("website-watch");
    app.run(serde_json::json!({"run_id":2}));
    let fetch = app.one("network.http");
    app.reply(&fetch, Ok(r#"{"status":200,"body":"Example Domain"}"#));
    app.completion(2, true);

    app.run(serde_json::json!({"run_id":3}));
    let fetch = app.one("network.http");
    app.reply(&fetch, Ok(r#"{"status":200,"body":"Not here"}"#));
    app.completion(3, true);
    assert_eq!(std::fs::read_to_string(app.root.join("website_match.json")).unwrap(), "false");
}

#[test]
fn website_room_reporting_waits_for_actual_send_and_retries_failures() {
    let mut app = App::launch("website-watch", &[("website_watch.json", r#"{"url":"https://status.example.test/","keyword":"resolved","post":true}"#)]);
    app.run(serde_json::json!({"run_id":10}));
    let fetch = app.one("network.http");
    app.reply(&fetch, Ok(r#"{"status":200,"body":"incident resolved"}"#));
    let send = app.one("matrix.send_message");
    app.reply(&send, Err("Room writes are disabled. Open Mini Apps and enable room writes."));
    app.completion(10, false);
    assert!(!app.root.join("website_match.json").exists());
    app.run(serde_json::json!({"run_id":11}));
    let fetch = app.one("network.http");
    app.reply(&fetch, Ok(r#"{"status":200,"body":"incident resolved"}"#));
    let send = app.one("matrix.send_message");
    app.reply(&send, Ok(r#"{"event_id":"$sent:test"}"#));
    app.completion(11, true);
}

#[test]
fn website_fetch_failure_finishes_without_a_notification() {
    let mut app = App::launch("website-watch", &[]);
    app.run(serde_json::json!({"run_id":12}));
    let fetch = app.one("network.http");
    app.reply(&fetch, Err("Internet permission is denied."));
    app.completion(12, false);
}

#[test]
fn reminder_restores_saved_text_and_acknowledges_only_after_popup_result() {
    let mut app = App::launch("reminder", &[("reminder.txt", "Prepare the release notes.")]);
    for run in [20, 21] {
        app.run(serde_json::json!({"run_id":run}));
        let popup = app.one("notify.post");
        assert_eq!(serde_json::from_str::<serde_json::Value>(&popup.args_json).unwrap()["body"], "Prepare the release notes.");
        app.reply(&popup, Ok("{}"));
        app.completion(run, true);
        app.reload("reminder");
    }
}

#[test]
fn keyword_condition_ignores_own_messages_and_completes_empty_batches() {
    let mut app = App::launch("keyword-alert", &[("keyword.txt", "urgent")]);
    app.run(serde_json::json!({"run_id":30,"messages":[
        {"body":"urgent but mine", "is_own":true}, {"body":"ordinary update", "is_own":false}
    ]}));
    app.completion(30, true);
    app.run(serde_json::json!({"run_id":31,"messages":[
        {"body":"urgent release", "is_own":false}, {"body":"urgent fix", "is_own":false}
    ]}));
    let popup = app.one("notify.post");
    assert!(popup.args_json.contains("2 new messages"));
    app.reply(&popup, Ok("{}"));
    app.completion(31, true);
}
