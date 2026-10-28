//! Names, limits and the label envelope for data mini-apps save to the user's
//! Matrix account, either globally or as private notes for one room.

use std::collections::BTreeSet;

use serde::Deserialize;
use serde_json::Value;

use crate::information_flow::{ContextId, Label, Source};

/// The event-type prefix of every app's saved data, global or per room.
pub const TYPE_PREFIX: &str = "rs.robius.a2app.appdata.";
/// The longest app id that can name saved data.
pub const MAX_APP_ID_LEN: usize = 64;
/// The longest key.
pub const MAX_KEY_LEN: usize = 64;
/// The largest saved object, as compact JSON.
pub const MAX_CONTENT_BYTES: usize = 16 * 1024;
/// Key names one writing compartment may use, and the most keys one read names.
pub const MAX_KEYS: usize = 32;
/// The most sources one saved value's label may carry.
pub const MAX_LABEL_SOURCES: usize = 256;
/// The envelope version this build writes and reads.
pub const ENVELOPE_VERSION: u64 = 1;
/// The exact-action target of every global write; room notes target their room.
pub const WRITE_TARGET: &str = "account-data";

pub const INVALID_APP_ID: &str = "This app's id can't be used to save account data.";
pub const INVALID_KEY: &str = "Keys are 1 to 64 lowercase letters, digits, '-' or '_', and start with a letter or digit.";
pub const CONTENT_NOT_OBJECT: &str = "Saved content must be a JSON object, or null to delete the key.";
pub const CONTENT_TOO_LARGE: &str = "Saved content is too large: each key holds at most 16 KiB of JSON.";
pub const CONTENT_TOO_COMPLEX: &str = "Saved content has too many values to show for review. Store fewer values under each key.";
pub const TOO_MANY_SOURCES: &str = "This app has used data from too many rooms to save it to your account.";
pub const FOREIGN_SOURCE: &str = "This app has used another account's data, so it can't save to this account.";
pub const UNREADABLE: &str = "This saved data has a format this version of Robrix can't read.";
pub const TOO_MANY_KEYS: &str = "This app has already used its 32 keys here. Reuse one of them.";
pub const STILL_SYNCING: &str = "Robrix is still syncing your account. Try again in a moment.";
pub const PUBLIC_APP: &str = "Public mini-apps can't save to your account.";
pub const WRONG_COMPARTMENT: &str = "This instance can't use this app's saved data.";

/// Which saved data a write changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    /// Global account data, shared by every instance of the app.
    Global,
    /// The attached room's notes.
    RoomNotes,
}

impl Kind {
    /// The kind a write capability saves, by catalog id.
    pub fn of_write(capability: &str) -> Option<Kind> {
        match capability {
            "matrix.account.data.write" => Some(Kind::Global),
            "matrix.room.account_data.write" => Some(Kind::RoomNotes),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Kind::Global => "global",
            Kind::RoomNotes => "notes",
        }
    }
}

/// What a read or the change hook takes from one stored value.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stored {
    /// The saved object; None when it was never saved or was deleted.
    pub content: Option<Value>,
    /// The writer's sources, joined into the reader before it sees `content`.
    pub label: Label,
}

fn is_slug_byte(byte: u8) -> bool {
    byte.is_ascii_lowercase() || byte.is_ascii_digit()
}

/// `[a-z0-9]+(-[a-z0-9]+)*`, at most 64 chars. Fails closed, since ids loaded
/// from disk are only checked for path safety.
pub fn validate_app_id(app_id: &str) -> Result<(), String> {
    let is_valid = app_id.len() <= MAX_APP_ID_LEN
        && app_id.split('-').all(|part| !part.is_empty() && part.bytes().all(is_slug_byte));
    if is_valid { Ok(()) } else { Err(INVALID_APP_ID.into()) }
}

/// `[a-z0-9][a-z0-9_-]{0,63}`.
pub fn validate_key(key: &str) -> Result<(), String> {
    let is_valid = key.len() <= MAX_KEY_LEN
        && key.bytes().next().is_some_and(is_slug_byte)
        && key.bytes().all(|byte| is_slug_byte(byte) || matches!(byte, b'-' | b'_'));
    if is_valid { Ok(()) } else { Err(INVALID_KEY.into()) }
}

/// `rs.robius.a2app.appdata.<app_id>.<key>`, for global data and room notes alike.
pub fn event_type(app_id: &str, key: &str) -> Result<String, String> {
    validate_app_id(app_id)?;
    validate_key(key)?;
    Ok(format!("{TYPE_PREFIX}{app_id}.{key}"))
}

/// The `(app_id, key)` an app data type names; None for every other type.
pub fn split_event_type(ty: &str) -> Option<(&str, &str)> {
    let (app_id, key) = ty.strip_prefix(TYPE_PREFIX)?.split_once('.')?;
    (validate_app_id(app_id).is_ok() && validate_key(key).is_ok()).then_some((app_id, key))
}

/// The exact payload a write's review shows and its sink commits; None deletes.
pub fn write_payload(key: &str, content: Option<&Value>) -> Value {
    serde_json::json!({ "key": key, "content": content })
}

/// A saved object: at most `MAX_CONTENT_BYTES` of compact JSON, small enough
/// for an effect review to show in full together with its key.
pub fn validate_content(key: &str, content: &Value) -> Result<(), String> {
    if !content.is_object() { return Err(CONTENT_NOT_OBJECT.into()); }
    if !is_compact_within(content, MAX_CONTENT_BYTES) { return Err(CONTENT_TOO_LARGE.into()); }
    if !crate::information_flow::is_reviewable_payload(&write_payload(key, Some(content))) {
        return Err(CONTENT_TOO_COMPLEX.into());
    }
    Ok(())
}

/// Whether `value` fits in `limit` bytes of compact JSON, counted without building it.
fn is_compact_within(value: &Value, limit: usize) -> bool {
    struct Counter { remaining: usize }
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.remaining = self.remaining.checked_sub(bytes.len())
                .ok_or_else(|| std::io::Error::other("over the limit"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }
    serde_json::to_writer(Counter { remaining: limit }, value).is_ok()
}

/// A label entry this account can read back; sources without an account always can.
fn is_own_source(source: &Source, account: &str) -> bool {
    match source {
        Source::Account { account: owner } | Source::RoomDirectory { account: owner } => !owner.is_empty() && owner == account,
        Source::Room { account: owner, room } => !owner.is_empty() && owner == account && !room.is_empty(),
        Source::UnknownPrivate => true,
    }
}

/// The stored content for `data` (None deletes the key), carrying the writer's
/// label so every reader joins it. Refuses labels `unwrap` would not read back.
pub fn wrap(label: &Label, account: &str, data: Option<&Value>) -> Result<Value, String> {
    if label.len() > MAX_LABEL_SOURCES { return Err(TOO_MANY_SOURCES.into()); }
    if !label.iter().all(|source| is_own_source(source, account)) { return Err(FOREIGN_SOURCE.into()); }
    if data.is_some_and(|data| !data.is_object()) { return Err(CONTENT_NOT_OBJECT.into()); }
    Ok(serde_json::json!({ "v": ENVELOPE_VERSION, "label": label, "data": data }))
}

/// A stored value as this account reads it. Anything this build can't read
/// exactly is an error, never an `UnknownPrivate` label.
pub fn unwrap(stored: Option<&Value>, account: &str) -> Result<Stored, String> {
    let Some(stored) = stored else { return Ok(Stored::default()) };
    let object = stored.as_object().ok_or(UNREADABLE)?;
    // Another client's delete, or an object it wrote without an envelope.
    if object.is_empty() { return Ok(Stored::default()); }
    let Some(version) = object.get("v") else {
        return Ok(Stored { content: Some(stored.clone()), label: Label::new() });
    };
    let (Some(stored_label), Some(data)) = (object.get("label"), object.get("data")) else { return Err(UNREADABLE.into()) };
    if object.len() != 3 || version.as_u64() != Some(ENVELOPE_VERSION) { return Err(UNREADABLE.into()); }
    let content = match data {
        Value::Null => None,
        Value::Object(_) => Some(data.clone()),
        _ => return Err(UNREADABLE.into()),
    };
    let entries = stored_label.as_array()
        .filter(|entries| entries.len() <= MAX_LABEL_SOURCES)
        .ok_or(UNREADABLE)?;
    let mut label = Label::new();
    for entry in entries {
        let source = Source::deserialize(entry).map_err(|_| UNREADABLE)?;
        // Extra fields or another spelling could be a protection this build ignores.
        let is_exact = serde_json::to_value(&source).is_ok_and(|exact| &exact == entry);
        if !is_exact || !is_own_source(&source, account) { return Err(UNREADABLE.into()); }
        label.insert(source);
    }
    Ok(Stored { content, label })
}

/// The key-name index entry for writes of `kind` from `context`: one set per
/// account, app, writer room (none when detached) and kind.
pub fn index_key(context: &ContextId, kind: Kind) -> Result<String, String> {
    let ContextId::App { account, app, room } = context else {
        let error = if matches!(context, ContextId::PublicApp { .. }) { PUBLIC_APP } else { WRONG_COMPARTMENT };
        return Err(error.into());
    };
    validate_app_id(app)?;
    if account.is_empty() || room.as_deref() == Some("") || (kind == Kind::RoomNotes && room.is_none()) {
        return Err(WRONG_COMPARTMENT.into());
    }
    // A JSON array keeps ids that contain separators from colliding.
    Ok(serde_json::json!([kind.as_str(), account, app, room]).to_string())
}

/// Refuses a key this compartment hasn't used once it has used `MAX_KEYS` names.
pub fn check_key_slot(used: Option<&BTreeSet<String>>, key: &str) -> Result<(), String> {
    match used {
        Some(used) if used.len() >= MAX_KEYS && !used.contains(key) => Err(TOO_MANY_KEYS.into()),
        _ => Ok(()),
    }
}

/// Refuses unless `context` is the private compartment of the instance (`app_id`,
/// `room`) that asked: an app dir named "notes@x" splits into app "notes" by its tag.
pub fn check_compartment(context: &ContextId, app_id: &str, room: Option<&str>) -> Result<(), String> {
    match context {
        ContextId::App { app, room: own_room, .. } if app == app_id && own_room.as_deref() == room => validate_app_id(app_id),
        ContextId::PublicApp { .. } => Err(PUBLIC_APP.into()),
        _ => Err(WRONG_COMPARTMENT.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ALICE: &str = "@alice:example.org";

    fn app(app: &str, room: Option<&str>) -> ContextId {
        ContextId::App { account: ALICE.into(), app: app.into(), room: room.map(str::to_string) }
    }

    fn room(room: &str) -> Source {
        Source::Room { account: ALICE.into(), room: room.into() }
    }

    fn writer_label() -> Label {
        [Source::Account { account: ALICE.into() }, room("!a:example.org"),
            Source::RoomDirectory { account: ALICE.into() }, Source::UnknownPrivate].into_iter().collect()
    }

    fn rooms(count: usize) -> Label {
        (0..count).map(|n| room(&format!("!r{n}:example.org"))).collect()
    }

    #[test]
    fn app_ids_follow_the_slug_grammar_and_fail_closed() {
        let (longest, too_long) = ("x".repeat(MAX_APP_ID_LEN), "x".repeat(MAX_APP_ID_LEN + 1));
        for id in ["a", "0", "notes", "tip-calc", "a1-b2-c3", "9lives", longest.as_str()] {
            assert_eq!(validate_app_id(id), Ok(()), "{id}");
        }
        for id in ["", "-", "a-", "-a", "a--b", "Notes", "notes@x", "notes.x", "no tes", "a_b", "a/b", "..",
            "é", "notes\0", too_long.as_str()]
        {
            assert_eq!(validate_app_id(id).unwrap_err(), INVALID_APP_ID, "{id:?}");
        }
    }

    #[test]
    fn keys_follow_their_grammar() {
        let (longest, too_long) = ("k".repeat(MAX_KEY_LEN), "k".repeat(MAX_KEY_LEN + 1));
        for key in ["a", "0", "prefs", "a_b-c", "9-_", "draft-2026_10", longest.as_str()] {
            assert_eq!(validate_key(key), Ok(()), "{key}");
        }
        for key in ["", "_a", "-a", "A", "Prefs", "a.b", "m.direct", "a b", "a/b", "é", "a\n", too_long.as_str()] {
            assert_eq!(validate_key(key).unwrap_err(), INVALID_KEY, "{key:?}");
        }
        assert!(INVALID_KEY.contains(&format!("1 to {MAX_KEY_LEN}")));
        assert!(CONTENT_TOO_LARGE.contains(&format!("{} KiB", MAX_CONTENT_BYTES / 1024)));
    }

    #[test]
    fn event_types_round_trip_through_split() {
        assert_eq!(event_type("notes", "prefs").unwrap(), "rs.robius.a2app.appdata.notes.prefs");
        let longest = ("x".repeat(MAX_APP_ID_LEN), format!("k{}", "_".repeat(MAX_KEY_LEN - 1)));
        for (app_id, key) in [("notes", "prefs"), ("tip-calc", "0"), ("a", "a_b-c"), (longest.0.as_str(), longest.1.as_str())] {
            let ty = event_type(app_id, key).unwrap();
            assert!(ty.len() <= 255, "Matrix caps event types at 255 bytes");
            assert_eq!(split_event_type(&ty), Some((app_id, key)));
        }
        assert_eq!(event_type("notes@x", "prefs").unwrap_err(), INVALID_APP_ID);
        assert_eq!(event_type("notes", "m.direct").unwrap_err(), INVALID_KEY);
    }

    #[test]
    fn split_rejects_every_type_it_did_not_build() {
        for ty in ["m.direct", "m.secret_storage.default_key", "rs.robius.robrix.ai_session_data", "rs.robius.a2app",
            "rs.robius.a2app.data", "rs.robius.a2app.appdata.", "rs.robius.a2app.appdata.notes",
            "rs.robius.a2app.appdata.notes.", "rs.robius.a2app.appdata..prefs", "rs.robius.a2app.appdata.notes.prefs.extra",
            "rs.robius.a2app.appdata.Notes.prefs", "rs.robius.a2app.appdata.notes@x.prefs", "rs.robius.a2app.appdata.notes._prefs",
            "rs.robius.a2app.appdata.-notes.prefs", "RS.ROBIUS.A2APP.APPDATA.notes.prefs", " rs.robius.a2app.appdata.notes.prefs",
            "rs.robius.a2app.appdata.notes.prefs ", "xrs.robius.a2app.appdata.notes.prefs", "rs.robius.a2app.appdatanotes.prefs"]
        {
            assert_eq!(split_event_type(ty), None, "{ty}");
        }
    }

    #[test]
    fn content_is_a_small_object_a_review_can_show_in_full() {
        assert_eq!(validate_content("prefs", &json!({"theme": "dark", "sizes": [1, 2, 3]})), Ok(()));
        for content in [json!([1]), json!("text"), json!(3), json!(true), json!(null)] {
            assert_eq!(validate_content("prefs", &content).unwrap_err(), CONTENT_NOT_OBJECT, "{content}");
        }
        // A JSON object of exactly `len` compact bytes.
        let object_of = |len: usize| json!({ "data": "x".repeat(len - 11) });
        assert_eq!(serde_json::to_string(&object_of(MAX_CONTENT_BYTES)).unwrap().len(), MAX_CONTENT_BYTES);
        assert_eq!(validate_content("prefs", &object_of(MAX_CONTENT_BYTES)), Ok(()));
        assert_eq!(validate_content("prefs", &object_of(MAX_CONTENT_BYTES + 1)).unwrap_err(), CONTENT_TOO_LARGE);
        // Escapes count toward the cap: every control character takes six bytes.
        assert_eq!(validate_content("prefs", &json!({ "data": "\u{1}".repeat(MAX_CONTENT_BYTES / 6) })).unwrap_err(), CONTENT_TOO_LARGE);
        // A 64x64 grid is about 8 KiB of JSON but far past what a review can show.
        let grid = json!({ "cells": vec![vec![0; 64]; 64] });
        assert!(serde_json::to_string(&grid).unwrap().len() < MAX_CONTENT_BYTES);
        assert_eq!(validate_content("grid", &grid).unwrap_err(), CONTENT_TOO_COMPLEX);
        assert!(!crate::information_flow::is_reviewable_payload(&write_payload("grid", Some(&grid))));
        let mut deep = json!({});
        for _ in 0..40 { deep = json!({ "next": deep }); }
        assert_eq!(validate_content("deep", &deep).unwrap_err(), CONTENT_TOO_COMPLEX);
    }

    #[test]
    fn writes_review_exactly_the_key_and_content() {
        let content = json!({"theme": "dark"});
        assert_eq!(write_payload("prefs", Some(&content)), json!({"key": "prefs", "content": {"theme": "dark"}}));
        assert_eq!(write_payload("prefs", None), json!({"key": "prefs", "content": null}));
        assert!(crate::information_flow::is_reviewable_payload(&write_payload("prefs", Some(&content))));
    }

    #[test]
    fn saved_values_and_deletes_round_trip_with_the_writer_label() {
        let data = json!({"theme": "dark", "nested": {"list": [1, "two", null]}});
        for label in [writer_label(), Label::new(), rooms(MAX_LABEL_SOURCES)] {
            let stored = wrap(&label, ALICE, Some(&data)).unwrap();
            let mut fields: Vec<&str> = stored.as_object().unwrap().keys().map(String::as_str).collect();
            fields.sort_unstable();
            assert_eq!(fields, ["data", "label", "v"]);
            assert_eq!(stored["v"], json!(ENVELOPE_VERSION));
            assert_eq!(unwrap(Some(&stored), ALICE).unwrap(), Stored { content: Some(data.clone()), label: label.clone() });
            // A delete is an envelope too, so it carries the deleter's label.
            let deleted = wrap(&label, ALICE, None).unwrap();
            assert_eq!(deleted["data"], Value::Null);
            assert_eq!(unwrap(Some(&deleted), ALICE).unwrap(), Stored { content: None, label });
        }
        let empty = wrap(&writer_label(), ALICE, Some(&json!({}))).unwrap();
        assert_eq!(unwrap(Some(&empty), ALICE).unwrap().content, Some(json!({})), "a saved empty object is not a delete");
    }

    #[test]
    fn wrap_refuses_labels_it_could_not_read_back() {
        assert_eq!(wrap(&rooms(MAX_LABEL_SOURCES + 1), ALICE, None).unwrap_err(), TOO_MANY_SOURCES);
        for foreign in [Source::Account { account: "@bob:example.org".into() },
            Source::Room { account: "@bob:example.org".into(), room: "!a:example.org".into() },
            Source::RoomDirectory { account: "@bob:example.org".into() }, Source::Room { account: ALICE.into(), room: String::new() }]
        {
            let mut label = writer_label();
            label.insert(foreign);
            assert_eq!(wrap(&label, ALICE, Some(&json!({}))).unwrap_err(), FOREIGN_SOURCE);
        }
        for data in [json!([1]), json!("text"), json!(null), json!(0)] {
            assert_eq!(wrap(&writer_label(), ALICE, Some(&data)).unwrap_err(), CONTENT_NOT_OBJECT, "{data}");
        }
    }

    #[test]
    fn values_from_other_clients_read_without_a_label() {
        assert_eq!(unwrap(None, ALICE).unwrap(), Stored::default(), "a key that was never saved");
        assert_eq!(unwrap(Some(&json!({})), ALICE).unwrap(), Stored::default(), "another client's delete");
        for foreign in [json!({"theme": "dark"}), json!({"label": [], "data": {"x": 1}}), json!({"version": 1, "data": null})] {
            assert_eq!(unwrap(Some(&foreign), ALICE).unwrap(), Stored { content: Some(foreign.clone()), label: Label::new() });
        }
    }

    #[test]
    fn unwrap_refuses_anything_it_cannot_read_exactly() {
        let good = wrap(&writer_label(), ALICE, Some(&json!({"theme": "dark"}))).unwrap();
        let with = |field: &str, value: Value| {
            let mut stored = good.clone();
            stored[field] = value;
            stored
        };
        let without = |field: &str| {
            let mut stored = good.clone();
            stored.as_object_mut().unwrap().remove(field);
            stored
        };
        let label = |entry: Value| with("label", json!([entry]));
        let unreadable = [
            json!([1]), json!("text"), json!(7), json!(null), json!(true),
            with("v", json!(2)), with("v", json!(0)), with("v", json!("1")), with("v", json!(1.0)), with("v", json!(-1)),
            with("v", json!(null)), with("protection", json!(true)), without("label"), without("data"),
            with("data", json!("text")), with("data", json!([1])), with("data", json!(3)), with("data", json!(false)),
            with("label", json!({})), with("label", json!(null)), with("label", json!("Account")),
            label(json!({"Website": {"origin": "https://example.org"}})),
            label(json!({"Account": {"account": ALICE, "device": "PHONE"}})),
            label(json!({"Account": {}})), label(json!(7)), label(json!("UnknownPublic")),
            label(json!({"Account": {"account": "@bob:example.org"}})),
            label(json!({"Room": {"account": "@bob:example.org", "room": "!a:example.org"}})),
            label(json!({"RoomDirectory": {"account": "@bob:example.org"}})),
            label(json!({"Room": {"account": ALICE, "room": ""}})),
            label(json!({"Account": {"account": ""}})),
            with("label", serde_json::to_value(rooms(MAX_LABEL_SOURCES + 1)).unwrap()),
        ];
        for stored in unreadable {
            assert_eq!(unwrap(Some(&stored), ALICE).unwrap_err(), UNREADABLE, "{stored}");
        }
        assert_eq!(unwrap(Some(&good), "@bob:example.org").unwrap_err(), UNREADABLE, "another account's saved label");
        assert!(unwrap(Some(&label(json!("UnknownPrivate"))), ALICE).unwrap().label.contains(&Source::UnknownPrivate));
    }

    #[test]
    fn index_keys_separate_writing_compartments_and_kinds() {
        let detached = index_key(&app("notes", None), Kind::Global).unwrap();
        let room_a = index_key(&app("notes", Some("!a:s")), Kind::Global).unwrap();
        let room_b = index_key(&app("notes", Some("!b:s")), Kind::Global).unwrap();
        let notes_a = index_key(&app("notes", Some("!a:s")), Kind::RoomNotes).unwrap();
        let other_app = index_key(&app("tasks", Some("!a:s")), Kind::Global).unwrap();
        let other_account = index_key(&ContextId::App { account: "@bob:s".into(), app: "notes".into(), room: Some("!a:s".into()) }, Kind::Global).unwrap();
        let keys: BTreeSet<_> = [&detached, &room_a, &room_b, &notes_a, &other_app, &other_account].into_iter().collect();
        assert_eq!(keys.len(), 6, "every compartment and kind has its own set");
        assert_eq!(index_key(&app("notes", Some("!a:s")), Kind::Global).unwrap(), room_a, "the key is stable");
        // Ids containing the JSON separators still can't collide.
        let tricky = ContextId::App { account: "@a\",\"notes\",\"!b:s".into(), app: "x".into(), room: None };
        assert!(!keys.contains(&index_key(&tricky, Kind::Global).unwrap()));
        assert_eq!(index_key(&app("notes", None), Kind::RoomNotes).unwrap_err(), WRONG_COMPARTMENT, "notes need a room");
        assert_eq!(index_key(&app("notes", Some("")), Kind::Global).unwrap_err(), WRONG_COMPARTMENT);
        assert_eq!(index_key(&app("notes@x", Some("!a:s")), Kind::Global).unwrap_err(), INVALID_APP_ID);
        let public = ContextId::PublicApp { account: ALICE.into(), app: "notes".into() };
        assert_eq!(index_key(&public, Kind::Global).unwrap_err(), PUBLIC_APP);
        let agent = ContextId::Agent { account: ALICE.into(), room: "!a:s".into() };
        assert_eq!(index_key(&agent, Kind::Global).unwrap_err(), WRONG_COMPARTMENT);
    }

    #[test]
    fn a_compartment_gets_a_fixed_number_of_key_names() {
        assert!(TOO_MANY_KEYS.contains(&MAX_KEYS.to_string()));
        assert_eq!(check_key_slot(None, "first"), Ok(()));
        let mut used: BTreeSet<String> = (1..MAX_KEYS).map(|n| format!("k{n}")).collect();
        assert_eq!(check_key_slot(Some(&used), "last"), Ok(()));
        used.insert("last".into());
        assert_eq!(check_key_slot(Some(&used), "k1"), Ok(()), "a used name can be saved again");
        assert_eq!(check_key_slot(Some(&used), "last"), Ok(()));
        assert_eq!(check_key_slot(Some(&used), "new").unwrap_err(), TOO_MANY_KEYS);
    }

    #[test]
    fn only_the_two_write_capabilities_have_a_kind() {
        assert_eq!(Kind::of_write("matrix.account.data.write"), Some(Kind::Global));
        assert_eq!(Kind::of_write("matrix.room.account_data.write"), Some(Kind::RoomNotes));
        for id in ["matrix.account.data.read", "matrix.room.account_data.read", "on_account_data_changed", "matrix.room.favorite.set"] {
            assert_eq!(Kind::of_write(id), None, "{id}");
        }
    }

    #[test]
    fn only_the_requesting_instances_private_compartment_may_use_its_data() {
        assert_eq!(check_compartment(&app("notes", Some("!r:s")), "notes", Some("!r:s")), Ok(()));
        assert_eq!(check_compartment(&app("notes", None), "notes", None), Ok(()));
        for (context, app_id, room) in [
            (app("notes", Some("!r:s")), "notes", None), (app("notes", None), "notes", Some("!r:s")),
            (app("notes", Some("!r:s")), "notes", Some("!other:s")), (app("tasks", Some("!r:s")), "notes", Some("!r:s")),
            // The tag "notes@x@!r:s" splits into app "notes" and room "x@!r:s".
            (app("notes@x", Some("!r:s")), "notes", Some("x@!r:s")), (app("notes@x", None), "notes", Some("x")),
            (ContextId::Agent { account: ALICE.into(), room: "!r:s".into() }, "notes", Some("!r:s")),
        ] {
            assert_eq!(check_compartment(&context, app_id, room).unwrap_err(), WRONG_COMPARTMENT, "{context:?}");
        }
        let public = ContextId::PublicApp { account: ALICE.into(), app: "notes".into() };
        assert_eq!(check_compartment(&public, "notes", None).unwrap_err(), PUBLIC_APP);
        assert_eq!(check_compartment(&app("Notes", None), "Notes", None).unwrap_err(), INVALID_APP_ID);
    }
}
