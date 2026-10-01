//! Host-owned provenance at the boundaries of mini-apps and room agents.
//!
//! Labels describe everything a context may know, not just an outgoing string.
//! Each compartment retains its storage provenance across restarts. Shared
//! source code carries a separate provenance floor into every compartment.

use a2app_core::capabilities::{Capability, Direction, FlowContract, FlowSource};
use a2app_core::information_flow::{self as flow, ContextId, Label, ReaderScope, Recipient, SharingDuration, Source};
use a2app_core::services;
use makepad_widgets::splash_host::SplashHostRequest;

#[cfg(test)]
thread_local! { pub(super) static TEST_ACCOUNT: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) }; }

pub fn account() -> Result<String, String> {
    #[cfg(test)]
    if let Some(account) = TEST_ACCOUNT.with(|account| account.borrow().clone()) { return Ok(account); }
    crate::sliding_sync::get_client().and_then(|client| client.user_id().map(ToString::to_string))
        .ok_or_else(|| "Sign in before running mini-apps or agents.".into())
}

pub fn app_context(app: &str, room: Option<&str>) -> Result<ContextId, String> {
    Ok(ContextId::App { account: account()?, app: app.into(), room: room.map(str::to_owned) })
}

pub fn agent_context(room: &str) -> Result<ContextId, String> {
    Ok(ContextId::Agent { account: account()?, room: room.into() })
}

pub fn prepare_agent(room: &str) -> Result<ContextId, String> {
    let context = agent_context(room)?;
    flow::register_context(&context)?;
    // A fresh session starts knowing only its own room: agent memory is
    // per-session, so a source a previous session joined (a legacy app's
    // `UnknownPrivate` code from `list_apps`, a room since left) must not
    // permanently wedge this one.
    flow::reset_agent_session_provenance(&context)?;
    flow::add_sources(&context, [room_source(&context, room)])?;
    flow::add_influences(&context, [flow::Influence::RoomContent { account: context_account(&context).into(), room: room.into() }])?;
    Ok(context)
}

/// The baseline sharing rules an AI room cannot function without.
///
/// A freshly created AI room starts with no sharing rules, so its very first
/// model call and its own reply/activity/turn state events are refused with
/// "Information flow blocked". Grant the room's own content — plus the
/// account's room directory and account-level metadata (the installed-app
/// list) that its default tools read — to the currently configured model
/// service and to the homeserver origin, so an AI room works out of the box.
///
/// Rules are additive and idempotent: an exact rule that already exists is
/// skipped, so this is safe to call on every session start — which is also how
/// a newly selected model gets its own rule. A rule the user later revokes in
/// the Data Sharing editor stays revoked until this runs again.
pub fn ensure_agent_default_sharing(context: &ContextId, room: &str, model: Option<&str>, homeserver: Option<&str>) {
    let mut recipients = Vec::new();
    if let Some(model) = model.filter(|model| !model.is_empty()) {
        recipients.push(Recipient::ModelProvider(model.to_string()));
    }
    if let Some(homeserver) = homeserver {
        if let Ok(recipient) = Recipient::network_origin(homeserver) {
            recipients.push(recipient);
        }
    }
    // The agent's own room: the automatic same-room rule only covers the
    // room's own source, so once the label has grown to include the directory
    // or account sources, a read/post/cursor write to this room would be
    // refused unless those are allowed here too.
    recipients.push(Recipient::MatrixRoom { account: context_account(context).into(), room: room.into() });
    if recipients.is_empty() {
        return;
    }
    for source in [room_source(context, room), directory_source(context), account_source(context)] {
        for recipient in &recipients {
            if flow::sharing_allows_for_reader(&source, recipient, context).unwrap_or(false) {
                continue;
            }
            // A grant can only fail on a registry write; surface it, since the
            // session otherwise fails later with a bare "Information flow blocked".
            if let Err(error) = flow::grant_sharing(
                source.clone(),
                recipient.clone(),
                ReaderScope::Context(context.clone()),
                SharingDuration::Permanent,
            ) {
                makepad_widgets::log!("AI Rooms: couldn't grant the default sharing rule {source:?} -> {recipient:?}: {error}");
            }
        }
    }
}

/// User-supplied source is private account input, including future versions.
pub fn record_source_edit(manifest: &a2app_core::manifest::MiniAppManifest) -> Result<ContextId, String> {
    let app = manifest.id.as_str();
    let context = app_context(app, None)?;
    // An existing source file can itself retain room data, even with an empty
    // storage jail. Newly imported/generated apps record known sources first.
    flow::register_context_with_legacy_data(&context, manifest_has_private_source(manifest))?;
    flow::add_sources(&context, [account_source(&context)])?;
    flow::add_influences(&context, [flow::Influence::MiniApp { account: context_account(&context).into(), app: app.into() }])?;
    flow::record_app_code_from(app, &context)?;
    Ok(context)
}

/// Only untouched stock code/metadata with no retained source history has a
/// known public origin when migrating an app with no recorded provenance.
pub fn manifest_has_private_source(manifest: &a2app_core::manifest::MiniAppManifest) -> bool {
    if !manifest.builtin { return true; }
    let Some(stock) = a2app_core::builtin::stock(&manifest.id) else { return true };
    if manifest.source != stock.source || manifest.name != stock.name
        || manifest.description != stock.description || manifest.icon != stock.icon
        || manifest.tint != stock.tint || manifest.permissions != stock.permissions
        || manifest.permission_reasons != stock.permission_reasons || manifest.capabilities != stock.capabilities
        || manifest.shortcuts != stock.shortcuts
        || serde_json::to_value(&manifest.widget).ok() != serde_json::to_value(&stock.widget).ok()
        || serde_json::to_value(&manifest.scope).ok() != serde_json::to_value(&stock.scope).ok()
    { return true; }
    // Restoring an older version must not launder its unknown sources through
    // a currently-stock working copy. Existing app provenance makes this
    // conservative migration flag irrelevant for newly recorded versions.
    match std::fs::read_dir(a2app_core::data_root().join("apps").join(&manifest.id).join("versions")) {
        Ok(mut entries) => entries.next().is_some(),
        Err(error) => error.kind() != std::io::ErrorKind::NotFound,
    }
}

pub fn context_account(context: &ContextId) -> &str {
    match context { ContextId::App { account, .. } | ContextId::PublicApp { account, .. } | ContextId::Agent { account, .. } => account }
}

pub fn current_context(context: &ContextId) -> Result<(), String> {
    if account()? != context_account(context) {
        return Err("The account changed. Reopen this mini-app or agent.".into());
    }
    flow::labels(context).map(|_| ())
}

pub fn context_for_heap(heap: usize) -> Result<ContextId, String> {
    let context = super::instances::context_of_heap(heap)
        .ok_or_else(|| "This mini-app instance is no longer running.".to_string())?;
    current_context(&context)?;
    Ok(context)
}

pub fn room_source(context: &ContextId, room: &str) -> Source {
    Source::Room { account: context_account(context).into(), room: room.into() }
}

pub fn account_source(context: &ContextId) -> Source {
    Source::Account { account: context_account(context).into() }
}

/// The agent's room/space directory (names, ids, counts) as one source, so a
/// single provider rule covers the whole directory and other recipients do not
/// inherit every listed room.
pub fn directory_source(context: &ContextId) -> Source {
    Source::RoomDirectory { account: context_account(context).into() }
}

pub fn ensure_room_output(context: &ContextId, room: &str) -> Result<(), String> {
    current_context(context)?;
    flow::ensure_allowed(context, &Recipient::MatrixRoom {
        account: context_account(context).into(), room: room.into(),
    })
}

/// Remember explicit room identifiers in a host response before exposing it.
/// Repeated/derived data keeps its original source even across async results.
pub fn record_response(context: &ContextId, value: &serde_json::Value) -> Result<(), String> {
    current_context(context)?;
    let mut sources = Label::new();
    collect_room_sources(context_account(context), value, &mut sources);
    let influences = sources.iter().filter_map(|source| match source {
        Source::Room { account, room } => Some(flow::Influence::RoomContent { account: account.clone(), room: room.clone() }),
        _ => None,
    }).collect::<Vec<_>>();
    flow::add_sources(context, sources)?;
    flow::add_influences(context, influences)
}

/// A directory result (list_rooms / list_spaces / space_info / space_rooms)
/// labels the context with one [`Source::RoomDirectory`] instead of one source
/// per listed room. The room names are still other people's words, so the
/// directory stays untrusted input.
pub fn record_directory_response(context: &ContextId) -> Result<(), String> {
    current_context(context)?;
    let source = directory_source(context);
    let influence = match &source {
        Source::RoomDirectory { account } => flow::Influence::RoomDirectory { account: account.clone() },
        _ => unreachable!(),
    };
    flow::add_sources(context, [source])?;
    flow::add_influences(context, [influence])
}

pub fn check_response(reply: services::Reply, data: &str) -> Result<(), String> {
    let context = context_for_heap(reply.heap_key)?;
    if let Ok(value) = serde_json::from_str(data) { record_response(&context, &value)?; }
    Ok(())
}

fn collect_room_sources(account: &str, value: &serde_json::Value, sources: &mut Label) {
    match value {
        serde_json::Value::Object(object) => {
            for (key, value) in object {
                // Some Matrix results index their entries by room id rather
                // than repeating it in the value (for example sync maps).
                collect_room_id(account, key, sources);
                if matches!(key.as_str(), "room_id" | "space_id" | "successor_room_id") {
                    if let Some(room) = value.as_str() { collect_room_id(account, room, sources); }
                }
                if matches!(key.as_str(), "joined" | "left" | "changed" | "room_ids" | "space_ids" | "rooms" | "spaces") {
                    collect_room_id_list(account, value, sources);
                }
                collect_room_sources(account, value, sources);
            }
        }
        serde_json::Value::Array(values) => {
            for value in values { collect_room_sources(account, value, sources); }
        }
        _ => {}
    }
}

fn collect_room_id(account: &str, value: &str, sources: &mut Label) {
    if matrix_sdk::ruma::RoomId::parse(value).is_ok() {
        sources.insert(Source::Room { account: account.into(), room: value.into() });
    }
}

fn collect_room_id_list(account: &str, value: &serde_json::Value, sources: &mut Label) {
    if let serde_json::Value::Array(values) = value {
        for value in values {
            if let Some(room) = value.as_str() { collect_room_id(account, room, sources); }
        }
    }
}

/// Runs after ordinary capability authorization, before any service executes.
pub fn check_request(request: &SplashHostRequest, capability: &Capability, args: &serde_json::Value, registry: &a2app_core::manifest::AppRegistry) -> Result<(), String> {
    let context = context_for_heap(request.heap_key)?;
    let (app, room) = match &context {
        ContextId::App { app, room, .. } => (app.as_str(), room.as_deref()),
        ContextId::PublicApp { app, .. } => (app.as_str(), None),
        _ => return Err("Invalid mini-app context.".into()),
    };
    if a2app_core::manifest::instance_tag(app, room) != request.app_tag {
        return Err("Mini-app identity mismatch.".into());
    }
    if capability.direction != Direction::Outgoing || !capability.wire.contains(&request.service.as_str()) {
        return Err("The service does not match its information-flow contract.".into());
    }
    let contract = capability.flow_contract().ok_or("This service has no information-flow contract.")?;
    let target = services::permission_context(&request.service, args, room).target_room;
    check_output(&context, contract, args, target)?;
    record_contract_source(&context, contract, room, target, registry)?;
    // Deferred Matrix/network/UI effects capture their resolved contents at
    // the final sink. Immediate platform effects commit this immutable call.
    let deferred = request.service.starts_with("matrix.")
        || matches!(capability.id, "host.composer.insert" | "host.composer.reply_to" | "host.nav.app");
    if !deferred && let Some(action) = contract.sensitive_action(capability.id, args, target) {
        flow::commit_exact_action_for_activation(&context, flow::context_epoch(&context)?, &action, args)?;
    }
    Ok(())
}

fn check_output(context: &ContextId, contract: FlowContract, args: &serde_json::Value, target: Option<&str>) -> Result<(), String> {
    let homeserver = crate::sliding_sync::get_client().map(|client| client.homeserver().to_string());
    if let Some(recipient) = contract.recipient(context_account(context), target, args, homeserver.as_deref())? {
        flow::ensure_allowed(context, &recipient)?;
    }
    Ok(())
}

fn record_contract_source(
    context: &ContextId,
    contract: FlowContract,
    room: Option<&str>,
    target: Option<&str>,
    registry: &a2app_core::manifest::AppRegistry,
) -> Result<(), String> {
    let sources = contract.source_labels(context_account(context), room, target)?;
    flow::add_sources(context, sources.clone())?;
    if contract.untrusted_content {
        let influences = sources.into_iter().map(|source| match source {
            Source::Room { account, room } => flow::Influence::RoomContent { account, room },
            _ => flow::Influence::Unknown,
        });
        flow::add_influences(context, influences)?;
    }
    if matches!(contract.source, FlowSource::InstalledAppCode | FlowSource::IpcAppCode) {
        for manifest in registry.iter().filter(|manifest| contract.source != FlowSource::IpcAppCode
            || manifest.declares(a2app_core::permissions::Permission::Ipc)) {
            // Register only to migrate unknown legacy code; retained room
            // state is not part of the metadata returned by apps_list.
            let source = app_context(&manifest.id, None)?;
            flow::register_context_with_legacy_data(&source, manifest_has_private_source(manifest))?;
            flow::add_sources(context, flow::code_labels(&manifest.id)?)?;
            flow::add_influences(context, flow::code_influences(&manifest.id)?)?;
        }
    }
    Ok(())
}

pub fn record_hook(heap: usize, hook: makepad_widgets::LiveId, args: &[&str]) -> Result<(), String> {
    let context = context_for_heap(heap)?;
    let capability = a2app_core::capabilities::CATALOG.iter().find(|capability| {
        capability.direction == Direction::Incoming
            && capability.wire.iter().any(|name| makepad_widgets::LiveId::from_str(name) == hook)
    }).ok_or("This hook has no information-flow contract.")?;
    let contract = capability.flow_contract().ok_or("This hook has no information-flow contract.")?;
    let room = match &context { ContextId::App { room, .. } => room.as_deref(), _ => None };
    // Hooks currently never enumerate app source metadata; their peer source
    // is transferred by the host delivery route before this call.
    record_contract_source(&context, contract, room, room, &a2app_core::manifest::AppRegistry::default())?;
    for arg in args {
        if let Ok(value) = serde_json::from_str(arg) { record_response(&context, &value)?; }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_room_results_keep_every_room_source() {
        let mut label = Label::new();
        collect_room_sources("@owner:server", &serde_json::json!({
            "rooms": [{"room_id":"!one:s"}, {"space_id":"!space:s"}],
            "result": {"room_id":"!two:s", "body":"!not_an_identifier:s"},
        }), &mut label);
        assert_eq!(label.len(), 3);
        assert!(label.contains(&Source::Room { account: "@owner:server".into(), room: "!two:s".into() }));
    }

    #[test]
    fn room_hook_arrays_and_room_keyed_maps_keep_all_sources() {
        let mut label = Label::new();
        collect_room_sources("@owner:server", &serde_json::json!({
            "joined": ["!one:s"], "left": ["!two:s"], "changed": ["!three:s"],
            "rooms": {"!four:s": {"name": "Fourth"}},
            "body": "!not_a_source:s", "room_ids": ["not a room", "!five:s"],
        }), &mut label);
        assert_eq!(label.len(), 5);
        for room in ["!one:s", "!two:s", "!three:s", "!four:s", "!five:s"] {
            assert!(label.contains(&Source::Room { account: "@owner:server".into(), room: room.into() }));
        }
    }

    #[test]
    fn modified_builtin_code_or_metadata_is_not_assumed_public() {
        let stock = a2app_core::builtin::stock("room-peek").unwrap();
        let mut code = stock.clone();
        code.source.push_str("\n// Room-derived private value\n");
        assert!(manifest_has_private_source(&code));
        let mut name = stock.clone();
        name.name = "Room-derived title".into();
        assert!(manifest_has_private_source(&name));
        let mut imported = stock;
        imported.builtin = false;
        assert!(manifest_has_private_source(&imported));
    }
}
