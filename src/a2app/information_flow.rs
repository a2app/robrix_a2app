//! Host-owned provenance at the boundaries of mini-apps and room agents.
//!
//! Labels describe everything a context may know, not just an outgoing string.
//! Each compartment retains its storage provenance across restarts. Shared
//! source code carries a separate provenance floor into every compartment.

use a2app_core::capabilities::{Capability, Direction, FlowContract, FlowSource};
use a2app_core::information_flow::{self as flow, ContextId, Label, Recipient, Source};
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
    flow::add_sources(&context, [room_source(&context, room)])?;
    flow::add_influences(&context, [flow::Influence::RoomContent { account: context_account(&context).into(), room: room.into() }])?;
    Ok(context)
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

/// Untouched bundled code has a known public origin.
///
/// Version history is tracked separately; recording a bundled release does
/// not turn its current source into private user input.
pub fn manifest_has_private_source(manifest: &a2app_core::manifest::MiniAppManifest) -> bool {
    if !manifest.builtin { return true; }
    let Some(stock) = a2app_core::builtin::stock(&manifest.id) else { return true };
    !a2app_core::builtin::matches_default(manifest, &stock)
}

/// Recover bundled execution without making unrecorded historical code public.
pub fn reconcile_builtin_manifest(manifest: &a2app_core::manifest::MiniAppManifest) -> Result<(), String> {
    if manifest_has_private_source(manifest) { return Err("Only an unchanged built-in app can start with public code.".into()); }
    let history = a2app_core::persistence::export_history(&manifest.id).map_err(|error| error.to_string())?;
    let unrecorded_private_history = history.iter().any(|snapshot|
        !snapshot.matches_manifest(manifest)
            && (snapshot.version.origin != a2app_core::versions::VersionOrigin::Stock || snapshot.version.imported));
    if unrecorded_private_history && flow::code_labels(&manifest.id).unwrap_or_default().is_empty() {
        flow::add_code_sources(&manifest.id, [Source::UnknownPrivate])?;
    }
    flow::reconcile_builtin_code(&manifest.id)
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
pub fn check_request(request: &SplashHostRequest, capability: &Capability, args: &serde_json::Value, registry: &a2app_core::manifest::AppRegistry) -> Result<Option<flow::EffectReview>, String> {
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
    // Deferred Matrix/network/UI effects capture their resolved contents at
    // the final sink. Immediate platform effects commit this immutable call.
    let deferred = request.service.starts_with("matrix.") || request.service == "network.http"
        || matches!(capability.id, "host.composer.insert" | "host.composer.reply_to" | "host.nav.app");
    let final_effect = deferred && (contract.privileged_effect || request.service == "network.http");
    if !final_effect {
        let homeserver = crate::sliding_sync::get_client().map(|client| client.homeserver().to_string());
        let recipient = contract.recipient(context_account(&context), target, args, homeserver.as_deref())?;
        let action = (!deferred).then(|| contract.sensitive_action(capability.id, args, target)).flatten();
        let epoch = flow::context_epoch(&context)?;
        let review = flow::prepare_effect_for_activation(&context, epoch, recipient.as_ref(), action.as_ref(), args)?;
        if !review.allowed { return Ok(Some(review)); }
        flow::commit_effect_for_activation(&context, epoch, recipient.as_ref(), action.as_ref(), args)?;
    }
    record_contract_source(&context, contract, room, target, registry)?;
    Ok(None)
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

    #[test]
    fn archived_stock_recovers_an_empty_execution_context_but_keeps_history_protected() {
        let mut stock = a2app_core::builtin::stock("public-web").unwrap();
        a2app_core::persistence::ensure_current_version(&mut stock,
            a2app_core::versions::VersionOrigin::Stock, "Built-in default", 0, 0).unwrap();
        assert!(!manifest_has_private_source(&stock), "the host's own version archive must not make bundled code private");
        let context = ContextId::App { account: "@stock-recovery:test".into(), app: stock.id.clone(), room: None };
        flow::register_context_with_legacy_data(&context, true).unwrap();
        assert!(flow::labels(&context).unwrap().contains(&Source::UnknownPrivate));
        flow::remove_context(&context).unwrap();
        reconcile_builtin_manifest(&stock).unwrap();
        flow::register_context(&context).unwrap();
        assert!(flow::labels(&context).unwrap().is_empty());
        assert!(flow::influences(&context).unwrap().is_empty());
        assert!(flow::code_labels(&stock.id).unwrap().contains(&Source::UnknownPrivate), "reading old code must still inherit its sources");
        flow::remove_context(&context).unwrap();
    }
}
