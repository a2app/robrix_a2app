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
    flow::add_sources(&context, [room_source(&context, room)])?;
    flow::add_influences(&context, [flow::Influence::RoomContent { account: context_account(&context).into(), room: room.into() }])?;
    Ok(context)
}

/// Starts a fresh agent session. The agent's per-session memory does not
/// survive a restart, so a source a previous session joined (a legacy app's
/// `UnknownPrivate` code from `list_apps`, a room since left) must not
/// permanently wedge this one. Only [`start_ai_session`] calls this, once per
/// session; every later tool call uses [`prepare_agent`], which never resets.
pub fn begin_agent_session(room: &str) -> Result<ContextId, String> {
    let context = agent_context(room)?;
    flow::register_context(&context)?;
    flow::begin_agent_session(&context)?;
    Ok(context)
}

/// Appends the re-ask instruction to a flow refusal caused by a grown label.
/// The agent must not retry the same call: it asks again with the newly
/// revealed need so the user can approve it for the rest of the turn.
pub fn with_task_reask_hint(error: String) -> String {
    if error.contains("Information flow blocked") {
        format!("{error} This refusal is because the label grew since the last permission request. Call request_task_permissions again with the new need (the room, URL or mini-app tool this task now requires); a need already declined this turn must not be requested again.")
    } else {
        error
    }
}

/// The baseline sharing rules an AI room cannot function without.
///
/// A freshly created AI room starts with no sharing rules, so its first model
/// call and its own reply/activity rows would be refused with "Information flow
/// blocked". The defaults are exactly:\n///
/// - the room/space directory to the current model provider, and the room's
///   own source to the current model provider;
/// - the room's own source and the directory to the homeserver origin and to
///   the room itself. These four are plumbing: the reply and activity writes
///   are checked against the whole label, so the room's own output cannot be
///   written back unless the sources it holds are allowed to reach the room
///   and the homeserver that stores the unencrypted state.
///
/// The account source is deliberately not defaulted: account-level data such
/// as the installed-app list is covered by the information-flow rules the
/// task prompt derives when a task reads it.
///
/// Apply once per `(room, recipient)`: `applied` is the persisted marker set,
/// so a new model provider gets its own rules while a rule the user revoked
/// is not recreated. Returns whether any marker was newly recorded.
pub fn ensure_agent_default_sharing(
    context: &ContextId,
    room: &str,
    model: Option<&str>,
    homeserver: Option<&str>,
    applied: &mut std::collections::BTreeSet<String>,
) -> bool {
    let own_room_source = room_source(context, room);
    let directory = directory_source(context);
    let own_room = Recipient::MatrixRoom { account: context_account(context).into(), room: room.into() };
    // The exact default set: (recipient, source) pairs, one marker per
    // recipient because each recipient's source set is fixed.
    let mut rules: Vec<(Recipient, Source)> = Vec::new();
    if let Some(model) = model.filter(|model| !model.is_empty()) {
        let provider = Recipient::ModelProvider(model.to_string());
        rules.push((provider.clone(), directory.clone()));
        rules.push((provider, own_room_source.clone()));
    }
    if let Some(homeserver) = homeserver.and_then(|homeserver| Recipient::network_origin(homeserver).ok()) {
        rules.push((homeserver.clone(), own_room_source.clone()));
        rules.push((homeserver, directory.clone()));
    }
    rules.push((own_room.clone(), own_room_source));
    rules.push((own_room, directory));

    let mut changed = false;
    // Group by recipient: the marker guards the recipient's whole rule set, so
    // the first session applies all of them and a later session applies none.
    let mut by_recipient: std::collections::BTreeMap<Recipient, Vec<Source>> = std::collections::BTreeMap::new();
    for (recipient, source) in rules {
        by_recipient.entry(recipient).or_default().push(source);
    }
    for (recipient, sources) in by_recipient {
        let marker = format!("{room}|{}", serde_json::to_string(&recipient).unwrap_or_default());
        if !applied.insert(marker) {
            // This recipient's defaults were already applied once; a revoked
            // rule stays revoked.
            continue;
        }
        changed = true;
        for source in sources {
            // The same-room source/recipient pair is implicitly allowed, so it
            // needs no stored grant; skip anything already covered.
            if flow::sharing_allows_for_reader(&source, &recipient, context).unwrap_or(false) {
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
    changed
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

/// An upgrade pointer belongs to the original room's tombstone.
///
/// The successor worker records a target source only if it reads that room.
pub fn record_matrix_response(context: &ContextId, capability: Option<&str>, value: &serde_json::Value) -> Result<(), String> {
    if capability == Some("matrix.room.successor.read") {
        let mut value = value.clone();
        if let Some(object) = value.as_object_mut() { object.remove("room_id"); }
        record_response(context, &value)
    } else { record_response(context, value) }
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
    let target_room = super::runtime::permission_target_room(&request.service, args, room);
    let target = target_room.as_deref();
    // Deferred Matrix/network/UI effects capture their resolved contents at
    // the final sink. Immediate platform effects commit this immutable call.
    let deferred = request.service.starts_with("matrix.") || request.service == "network.http"
        || matches!(capability.id, "host.composer.insert" | "host.composer.reply_to" | "host.nav.app");
    let final_effect = deferred && (contract.privileged_effect || request.service == "network.http"
        || matches!(contract.output, a2app_core::capabilities::FlowOutput::MatrixSearch
            | a2app_core::capabilities::FlowOutput::MatrixServer | a2app_core::capabilities::FlowOutput::MatrixPagination));
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

/// Include an approved subscription's possible inputs before its first event.
///
/// No room contents are read here. Visible setup can approve later actions
/// using the same source and influence floor as the subscribed event.
pub fn record_subscription(heap: usize, name: &str) -> Result<(), String> {
    let context = context_for_heap(heap)?;
    let capability = a2app_core::capabilities::for_hook(name).ok_or("Unknown subscribed event.")?;
    let contract = capability.flow_contract().ok_or("Missing subscribed event data-flow contract.")?;
    let room = context.room();
    record_contract_source(&context, contract, room, room, &a2app_core::manifest::AppRegistry::default())
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
    fn prepare_agent_keeps_provenance_gained_during_the_turn() {
        let account = format!("prepare-agent-reset-{}", std::process::id());
        TEST_ACCOUNT.with(|account_ref| *account_ref.borrow_mut() = Some(account.clone()));
        let room = "!prepare-agent-reset:example.org".to_string();
        let context = prepare_agent(&room).unwrap();
        let read_source = Source::Room { account: account.clone(), room: "!read-mid-turn:example.org".into() };
        a2app_core::information_flow::add_sources(&context, [read_source.clone()]).unwrap();
        // A later read, fetch or app-tool call runs `prepare_agent` again; it
        // must not drop a source the agent already holds.
        let again = prepare_agent(&room).unwrap();
        TEST_ACCOUNT.with(|account_ref| *account_ref.borrow_mut() = None);
        assert_eq!(context, again);
        assert!(a2app_core::information_flow::labels(&context).unwrap().contains(&read_source),
            "a mid-turn prepare_agent must not reset the agent's information-flow label");
    }

    #[test]
    fn a_flow_refusal_tells_the_agent_to_re_ask_for_the_new_need() {
        let refusal = String::from("Information flow blocked: this context has private data that is not allowed to reach this recipient. Review the blocked flow in Mini Apps.");
        let hint = with_task_reask_hint(refusal);
        assert!(hint.contains("request_task_permissions"));
        assert!(hint.contains("label grew"));
        // An unresolvable unknown-source refusal must not send the agent in a
        // re-ask loop: no sharing rule can ever release it.
        let unknown = String::from("Stored data with unknown sources cannot be shared.");
        assert_eq!(with_task_reask_hint(unknown.clone()), unknown);
    }

    #[test]
    fn default_sharing_is_directory_and_own_room_only_and_applied_once() {
        let account = format!("default-sharing-{}", std::process::id());
        TEST_ACCOUNT.with(|account_ref| *account_ref.borrow_mut() = Some(account.clone()));
        let room = "!default-sharing:example.org";
        let context = prepare_agent(room).unwrap();
        let provider = format!("provider-default-{}", std::process::id());
        let homeserver = "https://hs.default.sharing.example.org";
        let mut applied = std::collections::BTreeSet::new();
        assert!(ensure_agent_default_sharing(&context, room, Some(&provider), Some(homeserver), &mut applied));

        let reader = ReaderScope::Context(context.clone());
        let grants: Vec<_> = a2app_core::information_flow::sharing_grants().unwrap()
            .into_iter().filter(|grant| grant.reader == reader).collect();
        let own = room_source(&context, room);
        let directory = directory_source(&context);
        let model = Recipient::ModelProvider(provider.clone());
        let hs = Recipient::network_origin(homeserver).unwrap();
        let own_room = Recipient::MatrixRoom { account: account.clone(), room: room.into() };
        let expected = [
            (directory.clone(), model.clone()),
            (own.clone(), model.clone()),
            (own.clone(), hs.clone()),
            (directory.clone(), hs.clone()),
            (directory.clone(), own_room.clone()),
        ];
        assert_eq!(grants.len(), expected.len(), "exactly the documented defaults");
        for (source, recipient) in &expected {
            assert!(grants.iter().any(|grant| &grant.source == source && &grant.recipient == recipient),
                "missing {source:?} -> {recipient:?}");
        }
        // The own-room source to the own room is implicitly allowed, so it
        // needs no stored rule.
        assert!(a2app_core::information_flow::sharing_allows_for_reader(&own, &own_room, &context).unwrap());
        assert!(!grants.iter().any(|grant| matches!(grant.source, Source::Account { .. })),
            "account-level data must not be shared by default");

        // Revoking one rule and re-running the defaults for the same recipient
        // must not recreate it.
        let revoked = grants.iter().find(|grant| grant.source == directory && grant.recipient == model).unwrap().id;
        a2app_core::information_flow::revoke_sharing(revoked).unwrap();
        assert!(!ensure_agent_default_sharing(&context, room, Some(&provider), Some(homeserver), &mut applied));
        assert!(!a2app_core::information_flow::sharing_grants().unwrap().iter().any(|grant| grant.id == revoked));

        // A newly selected provider gets its own rules without touching the old ones.
        let provider_two = format!("provider-two-{}", std::process::id());
        assert!(ensure_agent_default_sharing(&context, room, Some(&provider_two), Some(homeserver), &mut applied));
        let grants = a2app_core::information_flow::sharing_grants().unwrap();
        assert!(grants.iter().any(|grant| grant.source == directory
            && grant.recipient == Recipient::ModelProvider(provider_two.clone())));
        TEST_ACCOUNT.with(|account_ref| *account_ref.borrow_mut() = None);
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
