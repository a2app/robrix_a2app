//! Data mini-apps save to the user's Matrix account, globally or as notes for the
//! attached room. Reads use only the synced store; each value carries its writer's label.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use a2app_core::account_data::{self as saved, Stored};
use a2app_core::information_flow::{self as flow, ContextId, Influence, Label};
use a2app_core::permissions::RoomAccess;
use matrix_sdk::Client;
use matrix_sdk::ruma::{OwnedRoomId, OwnedUserId};
use matrix_sdk::ruma::events::{GlobalAccountDataEventType, RoomAccountDataEventType};
use matrix_sdk::ruma::serde::Raw;
use serde_json::{json, Value};

use super::policy::{self, MatrixAuthorization};
use crate::sliding_sync::{current_user_id, get_client};

const MISSING_AUTHORIZATION: &str = "Missing host information-flow authorization.";

/// The account whose store a sync has filled. Before that, a read can't tell
/// "never saved" from "not synced yet".
static SYNCED_ACCOUNT: Mutex<Option<OwnedUserId>> = Mutex::new(None);
static NEXT_WRITE: AtomicU64 = AtomicU64::new(1);

/// The room list first loaded for `account`, so its store holds account data now.
pub fn mark_store_synced(account: Option<OwnedUserId>) {
    mark_store_synced_for(account, current_user_id());
}

/// Marks `account` only while it is still the one `signed_in`.
fn mark_store_synced_for(account: Option<OwnedUserId>, signed_in: Option<OwnedUserId>) {
    if account.is_some() && account == signed_in { set_synced_account(account); }
}

/// A logout or a new client: reads wait for that login's first sync.
pub fn reset_store_synced() {
    set_synced_account(None);
}

fn set_synced_account(account: Option<OwnedUserId>) {
    *SYNCED_ACCOUNT.lock().unwrap() = account;
}

fn ensure_synced(client: &Client) -> Result<(), String> {
    let synced = SYNCED_ACCOUNT.lock().unwrap();
    if synced.is_some() && synced.as_deref() == client.user_id() { Ok(()) } else { Err(saved::STILL_SYNCING.into()) }
}

/// Writes are ordered as the UI thread sent them.
pub fn next_write_sequence() -> u64 {
    NEXT_WRITE.fetch_add(1, Ordering::Relaxed)
}

/// Refuses unless `context` is the private compartment of the instance
/// (`app_id`, `room`) that asked.
pub fn check_instance(context: &ContextId, app_id: &str, room: Option<&str>, is_write: bool) -> Result<(), String> {
    saved::check_compartment(context, app_id, room).map_err(|error|
        // Public instances can't read saved data either; only a write says "save".
        if !is_write && error == saved::PUBLIC_APP { saved::WRONG_COMPARTMENT.into() } else { error })
}

/// The requesting compartment and activation, once it proves to be the
/// instance's own: the request's app and room, on the client's account.
fn compartment<'a>(authorization: &'a MatrixAuthorization, client: &Client, app_id: &str,
    room_id: Option<&OwnedRoomId>, is_write: bool) -> Result<(&'a ContextId, u64), String>
{
    let context = authorization.flow_context.as_ref().ok_or("Missing host information-flow context.")?;
    let epoch = authorization.flow_epoch.ok_or("Missing information-flow activation.")?;
    check_instance(context, app_id, authorization.origin_room.as_deref(), is_write)?;
    if room_id.is_some_and(|room_id| context.room() != Some(room_id.as_str())) {
        return Err(saved::WRONG_COMPARTMENT.into());
    }
    if client.user_id().map(|user| user.as_str()) != Some(context.account()) {
        return Err("The account changed. Reopen this mini-app.".into());
    }
    Ok((context, epoch))
}

/// This app's saved data (`room_id` None) or its notes for that room, by key.
pub(super) async fn read(room_id: Option<OwnedRoomId>, app_id: String, keys: Vec<String>, is_batch: bool,
    authorization: Option<&MatrixAuthorization>) -> Result<String, String>
{
    let client = get_client().ok_or("not logged in")?;
    read_with_client(&client, room_id.as_ref(), &app_id, &keys, is_batch, authorization).await
}

async fn read_with_client(client: &Client, room_id: Option<&OwnedRoomId>, app_id: &str, keys: &[String], is_batch: bool,
    authorization: Option<&MatrixAuthorization>) -> Result<String, String>
{
    ensure_synced(client)?;
    let authorization = authorization.ok_or(MISSING_AUTHORIZATION)?;
    let (context, epoch) = compartment(authorization, client, app_id, room_id, false)?;
    let room = room_id.map(|room_id| client.get_room(room_id).ok_or("room not found")).transpose()?;
    let mut items = Vec::with_capacity(keys.len());
    let (mut label, mut is_value_returned) = (Label::new(), false);
    for key in keys {
        let ty = saved::event_type(app_id, key)?;
        let value = match &room {
            Some(room) => room.account_data(RoomAccountDataEventType::from(ty)).await
                .map_err(|error| format!("couldn't read this app's notes: {error}"))?
                .map(|event| event.get_field::<Value>("content").ok().flatten().ok_or(saved::UNREADABLE)),
            None => client.account().account_data_raw(GlobalAccountDataEventType::from(ty)).await
                .map_err(|error| format!("couldn't read this app's saved data: {error}"))?
                .map(|content| content.deserialize_as_unchecked::<Value>().map_err(|_| saved::UNREADABLE)),
        };
        match value.transpose().map_err(String::from).and_then(|value| saved::unwrap(value.as_ref(), context.account())) {
            Ok(stored) => {
                is_value_returned |= stored != Stored::default();
                label.extend(stored.label);
                items.push(json!({ "key": key, "content": stored.content }));
            }
            // Its content never reaches the app, so its label isn't joined.
            Err(error) => items.push(json!({ "key": key, "content": null, "error": error })),
        }
    }
    authorization.check_current_permission()?;
    // Writers' labels reach the reader before their content does.
    flow::add_sources_for_activation(context, epoch, label)?;
    if room.is_some() && is_value_returned {
        // This app writes its notes from any device; the room's members never do.
        flow::add_influences_for_activation(context, epoch, [Influence::Unknown])?;
    }
    let reply = if is_batch { json!({ "items": items }) } else { items.pop().ok_or("No key to read.")? };
    Ok(reply.to_string())
}

/// Saves `content` under `key` (None deletes it) in this app's data, or in its
/// notes for `room_id`.
pub(super) async fn write(room_id: Option<OwnedRoomId>, app_id: String, key: String, content: Option<Value>, sequence: u64,
    authorization: Option<&MatrixAuthorization>) -> Result<String, String>
{
    let client = get_client().ok_or("not logged in")?;
    write_with_client(&client, room_id.as_ref(), &app_id, &key, content.as_ref(), sequence, authorization).await
}

async fn write_with_client(client: &Client, room_id: Option<&OwnedRoomId>, app_id: &str, key: &str, content: Option<&Value>,
    sequence: u64, authorization: Option<&MatrixAuthorization>) -> Result<String, String>
{
    ensure_synced(client)?;
    let authorization = authorization.ok_or(MISSING_AUTHORIZATION)?;
    let (context, _) = compartment(authorization, client, app_id, room_id, true)?;
    if let Some(content) = content { saved::validate_content(key, content)?; }
    let ty = saved::event_type(app_id, key)?;
    let room = room_id.map(|room_id| client.get_room(room_id).ok_or("room not found")).transpose()?;
    let homeserver = client.homeserver().to_string();
    let slot = (context.account().to_string(), room_id.map(ToString::to_string), ty.clone());
    ordered_write(slot, sequence, async {
        // Also checked before the review, so nobody approves a save that can't happen.
        saved::wrap(&flow::labels(context)?, context.account(), None)?;
        let target = room_id.map_or(saved::WRITE_TARGET, |room_id| room_id.as_str());
        if room_id.is_some() { policy::ensure_room_access(target, RoomAccess::Write)?; }
        policy::commit_sensitive_target_at(target, &saved::write_payload(key, content), Some(&homeserver)).await?;
        if room_id.is_some() { policy::ensure_room_access(target, RoomAccess::Write)?; }
        // The label after the review, so every later reader joins what the writer knew.
        let stored = Raw::new(&saved::wrap(&flow::labels(context)?, context.account(), content)?)
            .map_err(|error| error.to_string())?;
        let result = match &room {
            Some(room) => policy::audit_server_operation(&homeserver,
                room.set_account_data_raw(RoomAccountDataEventType::from(ty.clone()), stored.cast_unchecked())).await.map(|_| ()),
            None => policy::audit_server_operation(&homeserver,
                client.account().set_account_data_raw(GlobalAccountDataEventType::from(ty.clone()), stored.cast_unchecked())).await.map(|_| ()),
        };
        result.map_err(|error| format!("couldn't save this app's data: {error}"))
    }).await?;
    Ok(json!({ "key": key }).to_string())
}

/// One saved value: (account, room or none, type).
type Slot = (String, Option<String>, String);
type Writer = Arc<tokio::sync::Mutex<u64>>;
/// The last applied sequence per slot, locked across its review and save.
static WRITERS: LazyLock<Mutex<HashMap<Slot, Writer>>> = LazyLock::new(Default::default);

/// Runs `write` unless a newer write to `slot` has already succeeded. A failed
/// write leaves the mark alone, so an older write still runs after it.
async fn ordered_write(slot: Slot, sequence: u64,
    write: impl Future<Output = Result<(), String>>) -> Result<(), String>
{
    let writer = WRITERS.lock().unwrap().entry(slot).or_default().clone();
    let mut last_applied = writer.lock().await;
    if sequence <= *last_applied { return Ok(()); }
    write.await?;
    *last_applied = sequence;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::time::Duration;
    use a2app_core::information_flow::{Recipient, SensitiveAction, Source};
    use a2app_core::permissions::{GrantState, Permission, PermissionStore};

    const USER: &str = "@saver:account-data.test";
    const ROOM: &str = "!notes:account-data.test";

    /// A loopback homeserver: `/sync` serves the queued global and room account
    /// data, and each account-data PUT is kept as (path, body).
    struct MockHomeserver {
        url: String,
        global: Arc<Mutex<Vec<Value>>>,
        notes: Arc<Mutex<Vec<Value>>>,
        puts: Arc<Mutex<Vec<(String, Value)>>>,
        is_stopped: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl MockHomeserver {
        fn new() -> Self {
            use std::io::{Read, Write};
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let (global, notes, puts, is_stopped) = <(Arc<Mutex<Vec<Value>>>, Arc<Mutex<Vec<Value>>>, Arc<Mutex<Vec<(String, Value)>>>, Arc<AtomicBool>)>::default();
            let (global_events, note_events, captured, is_stopping) = (global.clone(), notes.clone(), puts.clone(), is_stopped.clone());
            let mut syncs = 0;
            let thread = std::thread::spawn(move || while !is_stopping.load(Ordering::Acquire) {
                let mut stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => { std::thread::sleep(Duration::from_millis(5)); continue; }
                    Err(_) => break,
                };
                // macOS hands the listener's O_NONBLOCK to accepted streams.
                stream.set_nonblocking(false).unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                let mut input = Vec::new();
                let header_end = loop {
                    let mut buffer = [0; 4096];
                    let count = match stream.read(&mut buffer) { Ok(count) if count > 0 => count, _ => break None };
                    input.extend_from_slice(&buffer[..count]);
                    if let Some(offset) = input.windows(4).position(|window| window == b"\r\n\r\n") { break Some(offset + 4); }
                };
                let Some(header_end) = header_end else { continue };
                let header = String::from_utf8_lossy(&input[..header_end]).into_owned();
                let length = header.lines().find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().ok()).flatten()
                }).unwrap_or(0);
                while input.len() < header_end + length {
                    let mut buffer = [0; 4096];
                    match stream.read(&mut buffer) { Ok(count) if count > 0 => input.extend_from_slice(&buffer[..count]), _ => break }
                }
                let mut request_line = header.lines().next().unwrap_or_default().split_whitespace();
                let (method, path) = (request_line.next().unwrap_or_default(), request_line.next().unwrap_or_default().to_string());
                let (status, response) = if path.contains("/sync") {
                    // The SDK skips a response that repeats the previous token.
                    syncs += 1;
                    (200, json!({ "next_batch": format!("mock-{syncs}"), "account_data": { "events": *global_events.lock().unwrap() },
                        "rooms": { "join": { "!notes:account-data.test": {
                            "state": { "events": [{ "type": "m.room.member", "state_key": USER, "sender": USER,
                                "event_id": "$member", "origin_server_ts": 1, "content": { "membership": "join" } }] },
                            "timeline": { "events": [], "limited": false, "prev_batch": "mock-prev" },
                            "account_data": { "events": *note_events.lock().unwrap() },
                        } } } }))
                } else if method == "PUT" && path.contains("/account_data/") {
                    captured.lock().unwrap().push((path, serde_json::from_slice(&input[header_end..]).unwrap_or_default()));
                    (200, json!({}))
                } else if path.contains("/keys/upload") {
                    (200, json!({ "one_time_key_counts": { "signed_curve25519": 50 } }))
                } else if path.contains("/keys/query") {
                    (200, json!({ "device_keys": {}, "failures": {} }))
                } else {
                    (404, json!({ "errcode": "M_NOT_FOUND", "error": "not mocked" }))
                };
                let body = response.to_string();
                let _ = write!(stream, "HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            });
            Self { url, global, notes, puts, is_stopped, thread: Some(thread) }
        }

        async fn client(&self) -> Client {
            use matrix_sdk::{authentication::matrix::MatrixSession, ruma::{api::MatrixVersion, owned_device_id}, SessionMeta, SessionTokens};
            let http = matrix_sdk::reqwest::Client::builder().no_proxy().build().unwrap();
            let client = Client::builder().homeserver_url(&self.url).http_client(http)
                .server_versions([MatrixVersion::V1_11]).build().await.unwrap();
            client.restore_session(MatrixSession {
                meta: SessionMeta { user_id: OwnedUserId::try_from(USER).unwrap(), device_id: owned_device_id!("MOCKSAVER") },
                tokens: SessionTokens { access_token: "mock-saver-token".into(), refresh_token: None },
            }).await.unwrap();
            self.sync(&client).await;
            client
        }

        async fn sync(&self, client: &Client) {
            client.sync_once(matrix_sdk::config::SyncSettings::default().timeout(Duration::ZERO)).await.unwrap();
        }

        fn last_put(&self) -> (String, Value) {
            self.puts.lock().unwrap().last().cloned().expect("an account-data PUT")
        }
    }

    impl Drop for MockHomeserver {
        fn drop(&mut self) {
            self.is_stopped.store(true, Ordering::Release);
            if let Some(thread) = self.thread.take() { thread.join().unwrap(); }
        }
    }

    /// Signed in as `USER` with every account-data ability of `app` granted,
    /// and the store synced; all of it undone on drop.
    struct SignedIn {
        app: &'static str,
        store: PermissionStore,
        contexts: Vec<ContextId>,
        previous_account: Option<String>,
    }

    impl SignedIn {
        fn new(app: &'static str) -> Self {
            let previous_account = crate::a2app::information_flow::TEST_ACCOUNT.with(|account| account.replace(Some(USER.into())));
            let mut store = PermissionStore::default();
            store.set_matrix_write(true);
            for permission in [Permission::MatrixAccountRead, Permission::MatrixAccountWrite, Permission::MatrixRoomManage] {
                store.set(app, permission, GrantState::Granted);
            }
            policy::publish_permission_policy(&store);
            let user = OwnedUserId::try_from(USER).unwrap();
            mark_store_synced_for(Some(user.clone()), Some(user));
            Self { app, store, contexts: Vec::new(), previous_account }
        }

        fn context(&mut self, room: Option<&str>) -> ContextId {
            let context = ContextId::App { account: USER.into(), app: self.app.into(), room: room.map(str::to_owned) };
            flow::register_context(&context).unwrap();
            self.contexts.push(context.clone());
            context
        }

        fn authorization(&self, capability: &str, context: &ContextId, args: Value) -> MatrixAuthorization {
            MatrixAuthorization::new(self.app, capability, context.room(), &self.store).with_flow(context.clone()).with_payload(args)
        }

        /// The user approves exactly this save once, as its review would.
        fn approve_once(&self, server: &MockHomeserver, context: &ContextId, capability: &str, target: &str, payload: &Value) {
            let homeserver = Recipient::network_origin(&server.url).unwrap();
            let action = SensitiveAction { kind: capability.into(), target: target.into() };
            let review = flow::prepare_effect_for_activation(context, flow::context_epoch(context).unwrap(),
                Some(&homeserver), Some(&action), payload).unwrap();
            assert!(!review.allowed, "data reaches the homeserver only after a review");
            flow::approve_effect_once(&review).unwrap();
        }
    }

    impl Drop for SignedIn {
        fn drop(&mut self) {
            for context in &self.contexts { let _ = flow::remove_context(context); }
            policy::publish_permission_policy(&PermissionStore::default());
            reset_store_synced();
            crate::a2app::information_flow::TEST_ACCOUNT.with(|account| { account.replace(self.previous_account.take()); });
        }
    }

    fn room_source(room: &str) -> Source {
        Source::Room { account: USER.into(), room: room.into() }
    }

    #[tokio::test]
    async fn an_older_write_is_skipped_only_after_a_newer_one_succeeded() {
        let slot = || (USER.to_string(), None, format!("{}ordered-test.k", saved::TYPE_PREFIX));
        let ran = AtomicUsize::new(0);
        let attempt = |result: Result<(), String>| {
            let ran = &ran;
            async move { ran.fetch_add(1, Ordering::SeqCst); result }
        };
        assert!(ordered_write(slot(), 5, attempt(Err("Denied".into()))).await.is_err());
        ordered_write(slot(), 3, attempt(Ok(()))).await.unwrap();
        assert_eq!(ran.load(Ordering::SeqCst), 2, "a failed newer write never hides an older one");
        ordered_write(slot(), 6, attempt(Ok(()))).await.unwrap();
        ordered_write(slot(), 4, attempt(Ok(()))).await.unwrap();
        ordered_write(slot(), 6, attempt(Ok(()))).await.unwrap();
        assert_eq!(ran.load(Ordering::SeqCst), 3, "older and repeated writes answer success without saving");
        let other = (USER.to_string(), Some(ROOM.to_string()), format!("{}ordered-test.k", saved::TYPE_PREFIX));
        ordered_write(other, 1, attempt(Ok(()))).await.unwrap();
        assert_eq!(ran.load(Ordering::SeqCst), 4, "room notes of the same key are ordered apart");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn nothing_is_read_or_saved_before_this_logins_first_sync() {
        let _review_lock = crate::a2app::effect_review::TEST_LOCK.lock().unwrap();
        let server = MockHomeserver::new();
        let client = server.client().await;
        let keys = ["prefs".to_string()];
        for synced in [None, Some(OwnedUserId::try_from("@previous:account-data.test").unwrap())] {
            set_synced_account(synced);
            assert_eq!(read_with_client(&client, None, "sync-test", &keys, false, None).await.unwrap_err(), saved::STILL_SYNCING);
            assert_eq!(write_with_client(&client, None, "sync-test", "prefs", None, next_write_sequence(), None).await.unwrap_err(),
                saved::STILL_SYNCING);
        }
        let (user, other) = (OwnedUserId::try_from(USER).unwrap(), OwnedUserId::try_from("@other:account-data.test").unwrap());
        mark_store_synced(Some(user.clone()));
        mark_store_synced_for(Some(user.clone()), Some(other));
        mark_store_synced_for(Some(user.clone()), None);
        mark_store_synced_for(None, None);
        assert_eq!(read_with_client(&client, None, "sync-test", &keys, false, None).await.unwrap_err(), saved::STILL_SYNCING,
            "only the signed-in client's own room list marks its store");
        // Past the gate, a call without authorization fails on that instead.
        mark_store_synced_for(Some(user.clone()), Some(user));
        assert_eq!(read_with_client(&client, None, "sync-test", &keys, false, None).await.unwrap_err(), MISSING_AUTHORIZATION);
        assert_eq!(write_with_client(&client, None, "sync-test", "prefs", None, next_write_sequence(), None).await.unwrap_err(),
            MISSING_AUTHORIZATION);
        reset_store_synced();
        assert_eq!(read_with_client(&client, None, "sync-test", &keys, false, None).await.unwrap_err(), saved::STILL_SYNCING,
            "a logout or a new client closes the gate again");
        assert!(server.puts.lock().unwrap().is_empty());
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn a_saved_value_and_its_delete_carry_the_writers_label_into_other_instances() {
        let _review_lock = crate::a2app::effect_review::TEST_LOCK.lock().unwrap();
        let server = MockHomeserver::new();
        let client = server.client().await;
        let mut signed = SignedIn::new("round-trip-test");
        let (writer, reader) = (signed.context(Some(ROOM)), signed.context(None));
        let secret = room_source("!secret:account-data.test");
        flow::add_sources(&writer, [secret.clone()]).unwrap();
        let changes = Arc::new(Mutex::new(Vec::new()));
        let seen = changes.clone();
        crate::a2app::account_data_watch::watch(&client, USER.into(), move |change| seen.lock().unwrap().push(change));

        let ty = saved::event_type(signed.app, "prefs").unwrap();
        let content = json!({"theme": "dark"});
        let capability = "matrix.account.data.write";
        signed.approve_once(&server, &writer, capability, saved::WRITE_TARGET, &saved::write_payload("prefs", Some(&content)));
        let write = signed.authorization(capability, &writer, json!({"key": "prefs", "content": content}));
        let reply = policy::with_authorization(write.clone(), write_with_client(&client, None, signed.app, "prefs", Some(&content),
            next_write_sequence(), Some(&write))).await.unwrap();
        assert_eq!(reply, r#"{"key":"prefs"}"#);
        let (path, stored) = server.last_put();
        assert!(path.ends_with(&format!("/account_data/{ty}")) && !path.contains("/rooms/"), "{path}");
        let saved_value = saved::unwrap(Some(&stored), USER).unwrap();
        assert_eq!(saved_value.content.as_ref(), Some(&content));
        assert!(saved_value.label.contains(&secret));

        // The sync echo reaches the detached instance, and the room's label arrives first.
        server.global.lock().unwrap().extend([
            json!({"type": ty, "content": stored}),
            json!({"type": saved::event_type(signed.app, "broken").unwrap(), "content": {"v": 2, "label": [], "data": {}}}),
            json!({"type": "m.direct", "content": {}}),
            json!({"type": "io.element.custom", "content": {"x": 1}}),
        ]);
        server.sync(&client).await;
        server.sync(&client).await;
        let posted: Vec<(String, String, Label)> = changes.lock().unwrap().iter()
            .map(|change| (change.app_id.clone(), change.key.clone(), change.label.clone())).collect();
        assert_eq!(posted, [(signed.app.to_string(), "prefs".to_string(), saved_value.label.clone())],
            "one change per new value; other types and unreadable values never reach an app");
        assert!(!flow::labels(&reader).unwrap().contains(&secret));
        let keys = ["prefs", "broken", "missing"].map(String::from);
        let read = signed.authorization("matrix.account.data.read", &reader, json!({"keys": keys}));
        let reply: Value = serde_json::from_str(&read_with_client(&client, None, signed.app, &keys, true, Some(&read)).await.unwrap()).unwrap();
        assert_eq!(reply, json!({"items": [
            {"key": "prefs", "content": {"theme": "dark"}},
            {"key": "broken", "content": null, "error": saved::UNREADABLE},
            {"key": "missing", "content": null},
        ]}));
        assert!(flow::labels(&reader).unwrap().contains(&secret));
        assert!(!flow::influences(&reader).unwrap().contains(&Influence::Unknown), "global reads get Unknown from their contract");

        // A delete is an envelope too, so it still carries the label.
        signed.approve_once(&server, &writer, capability, saved::WRITE_TARGET, &saved::write_payload("prefs", None));
        let delete = signed.authorization(capability, &writer, json!({"key": "prefs", "content": null}));
        policy::with_authorization(delete.clone(), write_with_client(&client, None, signed.app, "prefs", None,
            next_write_sequence(), Some(&delete))).await.unwrap();
        let (_, deleted) = server.last_put();
        assert_eq!(deleted["data"], Value::Null);
        assert!(saved::unwrap(Some(&deleted), USER).unwrap().label.contains(&secret));
        server.global.lock().unwrap()[0] = json!({"type": ty, "content": deleted});
        server.sync(&client).await;
        let read = signed.authorization("matrix.account.data.read", &reader, json!({"key": "prefs"}));
        let reply = read_with_client(&client, None, signed.app, &keys[..1], false, Some(&read)).await.unwrap();
        assert_eq!(serde_json::from_str::<Value>(&reply).unwrap(), json!({"key": "prefs", "content": null}));
        assert_eq!(changes.lock().unwrap().len(), 2, "the delete is a change too");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn notes_read_with_their_writers_label_as_unknown_input_and_save_to_the_room() {
        let _review_lock = crate::a2app::effect_review::TEST_LOCK.lock().unwrap();
        let server = MockHomeserver::new();
        let client = server.client().await;
        let mut signed = SignedIn::new("notes-test");
        let instance = signed.context(Some(ROOM));
        let room_id = OwnedRoomId::try_from(ROOM).unwrap();
        let ty = saved::event_type(signed.app, "todo").unwrap();
        // This app saved the note on another device, after reading another room.
        let elsewhere = room_source("!elsewhere:account-data.test");
        let note = saved::wrap(&[elsewhere.clone()].into(), USER, Some(&json!({"done": false}))).unwrap();
        server.notes.lock().unwrap().push(json!({"type": ty, "content": note}));
        server.sync(&client).await;
        let read = signed.authorization("matrix.room.account_data.read", &instance, json!({"key": "todo"}));
        let reply = read_with_client(&client, Some(&room_id), signed.app, &["todo".into()], false, Some(&read)).await.unwrap();
        assert_eq!(serde_json::from_str::<Value>(&reply).unwrap(), json!({"key": "todo", "content": {"done": false}}));
        assert!(flow::labels(&instance).unwrap().contains(&elsewhere));
        assert!(flow::influences(&instance).unwrap().contains(&Influence::Unknown), "notes aren't the room members' words");

        let capability = "matrix.room.account_data.write";
        let content = json!({"done": true});
        signed.approve_once(&server, &instance, capability, ROOM, &saved::write_payload("todo", Some(&content)));
        let write = signed.authorization(capability, &instance, json!({"key": "todo", "content": content}));
        policy::with_authorization(write.clone(), write_with_client(&client, Some(&room_id), signed.app, "todo", Some(&content),
            next_write_sequence(), Some(&write))).await.unwrap();
        let (path, stored) = server.last_put();
        assert!(path.contains("/rooms/") && path.ends_with(&format!("/account_data/{ty}")), "{path}");
        let saved_note = saved::unwrap(Some(&stored), USER).unwrap();
        assert_eq!(saved_note.content, Some(content));
        assert!(saved_note.label.contains(&elsewhere));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn only_the_requesting_instances_own_compartment_reaches_saved_data() {
        let _review_lock = crate::a2app::effect_review::TEST_LOCK.lock().unwrap();
        let server = MockHomeserver::new();
        let client = server.client().await;
        let mut signed = SignedIn::new("identity-test");
        let room_id = OwnedRoomId::try_from(ROOM).unwrap();
        let keys = ["prefs".to_string()];
        let with_context = |capability: &str, context: ContextId, origin: Option<&str>| {
            let mut authorization = MatrixAuthorization::new(signed.app, capability, origin, &signed.store);
            authorization.flow_context = Some(context);
            authorization.flow_epoch = Some(1);
            authorization
        };
        let other_app = ContextId::App { account: USER.into(), app: "another-app".into(), room: None };
        let read = with_context("matrix.account.data.read", other_app.clone(), None);
        assert_eq!(read_with_client(&client, None, signed.app, &keys, false, Some(&read)).await.unwrap_err(), saved::WRONG_COMPARTMENT);
        let write = with_context("matrix.account.data.write", other_app, None);
        assert_eq!(write_with_client(&client, None, signed.app, "prefs", None, next_write_sequence(), Some(&write)).await.unwrap_err(),
            saved::WRONG_COMPARTMENT);
        let public = ContextId::PublicApp { account: USER.into(), app: signed.app.into() };
        let write = with_context("matrix.account.data.write", public.clone(), None);
        assert_eq!(write_with_client(&client, None, signed.app, "prefs", None, next_write_sequence(), Some(&write)).await.unwrap_err(),
            saved::PUBLIC_APP);
        let read = with_context("matrix.account.data.read", public, None);
        assert_eq!(read_with_client(&client, None, signed.app, &keys, false, Some(&read)).await.unwrap_err(), saved::WRONG_COMPARTMENT);
        // Another room's instance never reaches this room's notes.
        let other_room = signed.context(Some("!other:account-data.test"));
        let read = signed.authorization("matrix.room.account_data.read", &other_room, json!({"key": "prefs"}));
        assert_eq!(read_with_client(&client, Some(&room_id), signed.app, &keys, false, Some(&read)).await.unwrap_err(),
            saved::WRONG_COMPARTMENT);
        let detached = signed.context(None);
        assert_eq!(read_with_client(&client, None, signed.app, &keys, false, None).await.unwrap_err(), MISSING_AUTHORIZATION);
        let read = signed.authorization("matrix.account.data.read", &detached, json!({"key": "prefs"}));
        assert!(read_with_client(&client, None, signed.app, &keys, false, Some(&read)).await.is_ok());
        assert!(server.puts.lock().unwrap().is_empty());
    }
}
