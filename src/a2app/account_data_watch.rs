//! The per-login handler behind `on_account_data_changed`. It posts which of an
//! app's saved account-data keys changed, with each saved value's label.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Arc, Mutex};

use a2app_core::account_data;
use a2app_core::information_flow::Label;
use makepad_widgets::{Cx, SignalToUI};
use matrix_sdk::Client;
use matrix_sdk::ruma::events::AnyGlobalAccountDataEvent;
use matrix_sdk::ruma::serde::Raw;

/// One app key whose saved value changed, on this device or another.
#[derive(Clone, Debug)]
pub struct A2AppAccountDataEvent {
    pub account: String,
    pub app_id: String,
    pub key: String,
    /// The saved value's label, joined into each subscriber before its hook runs.
    pub label: Label,
}

impl A2AppAccountDataEvent {
    /// Whether this came from the signed-in account's sync.
    pub fn is_current(&self) -> bool {
        super::information_flow::account().is_ok_and(|account| account == self.account)
    }
}

/// Watches this login's account data. The handler lives and dies with `client`.
pub fn register(client: &Client) {
    let Some(account) = client.user_id().map(ToString::to_string) else { return };
    watch(client, account, |event| {
        Cx::post_action(event);
        SignalToUI::set_ui_signal();
    });
}

/// Hands every changed app key to `post`. It runs inside the sync loop, so it only filters.
pub(crate) fn watch(client: &Client, account: String, post: impl Fn(A2AppAccountDataEvent) + Clone + Send + Sync + 'static) {
    let seen = Arc::new(Mutex::new(HashMap::new()));
    client.add_event_handler(move |event: Raw<AnyGlobalAccountDataEvent>| {
        if let Some(change) = changed_key(&account, &seen, &event) { post(change); }
        async {}
    });
}

/// The app key an event changes. None for other types, for a value already seen
/// this login, and for a value this build can't read.
fn changed_key(account: &str, seen: &Mutex<HashMap<String, u64>>, event: &Raw<AnyGlobalAccountDataEvent>) -> Option<A2AppAccountDataEvent> {
    let ty = event.get_field::<String>("type").ok()??;
    let (app_id, key) = account_data::split_event_type(&ty)?;
    let content = event.get_field::<serde_json::Value>("content").ok().flatten();
    let mut hasher = DefaultHasher::new();
    serde_json::to_string(&content).ok()?.hash(&mut hasher);
    let hash = hasher.finish();
    // The first sighting this login counts as a change.
    if seen.lock().unwrap().insert(ty.clone(), hash) == Some(hash) { return None; }
    let stored = account_data::unwrap(content.as_ref(), account).ok()?;
    Some(A2AppAccountDataEvent { account: account.into(), app_id: app_id.into(), key: key.into(), label: stored.label })
}

#[cfg(test)]
mod tests {
    use super::*;
    use a2app_core::information_flow::Source;

    const ALICE: &str = "@alice:watch.test";

    fn event(ty: &str, content: serde_json::Value) -> Raw<AnyGlobalAccountDataEvent> {
        Raw::from_json_string(serde_json::json!({ "type": ty, "content": content }).to_string()).unwrap()
    }

    #[test]
    fn only_readable_changes_to_app_keys_are_posted_with_their_label() {
        let seen = Mutex::new(HashMap::new());
        let room = Source::Room { account: ALICE.into(), room: "!a:watch.test".into() };
        let saved = account_data::wrap(&[room.clone()].into_iter().collect(), ALICE, Some(&serde_json::json!({"theme": "dark"}))).unwrap();
        let ty = account_data::event_type("notes", "prefs").unwrap();
        let change = changed_key(ALICE, &seen, &event(&ty, saved.clone())).unwrap();
        assert_eq!((change.account.as_str(), change.app_id.as_str(), change.key.as_str()), (ALICE, "notes", "prefs"));
        assert_eq!(change.label, [room.clone()].into_iter().collect());
        assert!(changed_key(ALICE, &seen, &event(&ty, saved)).is_none(), "an unchanged value, like a reconnect's resend");
        let deleted = account_data::wrap(&[room.clone()].into_iter().collect(), ALICE, None).unwrap();
        assert_eq!(changed_key(ALICE, &seen, &event(&ty, deleted)).unwrap().label, [room].into_iter().collect(),
            "a delete carries the deleter's label");
        let foreign = changed_key(ALICE, &seen, &event(&ty, serde_json::json!({"from": "another client"}))).unwrap();
        assert!(foreign.label.is_empty());
        for ty in ["m.direct", "m.secret_storage.default_key", "rs.robius.robrix.ai_session_data", "rs.robius.a2app.notes.prefs",
            "rs.robius.a2app.appdata.notes@x.prefs", "rs.robius.a2app.appdata.notes.prefs.extra"]
        {
            assert!(changed_key(ALICE, &seen, &event(ty, serde_json::json!({"x": 1}))).is_none(), "{ty}");
        }
        let unreadable = serde_json::json!({"v": 2, "label": [], "data": {}});
        assert!(changed_key(ALICE, &seen, &event(&ty, unreadable)).is_none());
        let other_account = account_data::wrap(&[Source::Account { account: ALICE.into() }].into_iter().collect(), ALICE, None).unwrap();
        assert!(changed_key("@bob:watch.test", &Mutex::new(HashMap::new()), &event(&ty, other_account)).is_none(),
            "another account's label is unreadable here");
    }

    #[test]
    fn events_are_fenced_to_the_signed_in_account() {
        let previous = super::super::information_flow::TEST_ACCOUNT.with(|account| account.replace(Some(ALICE.into())));
        let mut event = A2AppAccountDataEvent { account: ALICE.into(), app_id: "notes".into(), key: "prefs".into(), label: Label::new() };
        assert!(event.is_current());
        event.account = "@bob:watch.test".into();
        assert!(!event.is_current());
        super::super::information_flow::TEST_ACCOUNT.with(|account| { account.replace(previous); });
    }
}
