use super::*;
use super::tests::TestRoot;

fn app(account: &str, room: &str) -> ContextId {
    ContextId::App { account: account.into(), app: "tool".into(), room: Some(room.into()) }
}

fn source() -> Source { Source::Room { account: "alice".into(), room: "private".into() } }
fn site() -> Recipient { Recipient::network_origin("https://example.com/page").unwrap() }
fn internet() -> Influence { Influence::InternetOrigin("https://example.com".into()) }
fn action() -> SensitiveAction { SensitiveAction { kind: "matrix.invite".into(), target: "room/user".into() } }

#[test]
fn scoped_sharing_distinguishes_accounts_apps_and_exact_contexts() {
    let root = TestRoot::new();
    let mut registry = root.registry();
    let a = app("alice", "first");
    let b = app("alice", "second");
    let foreign = app("bob", "first");
    let agent = ContextId::Agent { account: "alice".into(), room: "first".into() };
    for context in [&a, &b, &foreign, &agent] {
        registry.register_context(context).unwrap();
        registry.add_sources(context, [source()]).unwrap();
    }
    let exact = registry.grant_sharing(source(), site(), ReaderScope::Context(a.clone()), SharingDuration::Permanent).unwrap();
    assert!(registry.ensure_allowed(&a, &site()).is_ok());
    for context in [&b, &foreign, &agent] { assert!(registry.ensure_allowed(context, &site()).is_err()); }
    assert!(registry.ensure_labels_allowed(&[source()].into(), &site()).is_err());
    registry.revoke_sharing(exact).unwrap();
    registry.grant_sharing(source(), site(), ReaderScope::App { account: "alice".into(), app: "tool".into() }, SharingDuration::Permanent).unwrap();
    assert!(registry.ensure_allowed(&a, &site()).is_ok());
    assert!(registry.ensure_allowed(&b, &site()).is_ok());
    assert!(registry.ensure_allowed(&foreign, &site()).is_err());
    assert!(registry.ensure_allowed(&agent, &site()).is_err());
}

#[test]
fn session_rules_never_persist_and_expire_only_at_their_boundary() {
    let root = TestRoot::new();
    let mut registry = root.registry();
    let a = app("alice", "first");
    registry.register_context(&a).unwrap();
    registry.add_sources(&a, [source()]).unwrap();
    let permanent = registry.grant_sharing(source(), Recipient::ModelProvider("model".into()), ReaderScope::AllReaders, SharingDuration::Permanent).unwrap();
    registry.grant_sharing(source(), site(), ReaderScope::Context(a.clone()), SharingDuration::RoomSession { account: "alice".into(), room: "first".into() }).unwrap();
    registry.close_room_session("bob", "first").unwrap();
    assert!(registry.ensure_allowed(&a, &site()).is_ok());
    let mut restarted = root.registry();
    restarted.register_context(&a).unwrap();
    assert!(restarted.ensure_allowed(&a, &site()).is_err());
    assert_eq!(restarted.sharing_grants().unwrap().len(), 1);
    assert_eq!(restarted.sharing_grants().unwrap()[0].id, permanent);
    registry.close_room_session("alice", "first").unwrap();
    assert!(registry.ensure_allowed(&a, &site()).is_err());
    registry.grant_sharing(source(), site(), ReaderScope::AllReaders, SharingDuration::RobrixSession).unwrap();
    registry.end_session().unwrap();
    registry.register_context(&a).unwrap();
    assert!(registry.ensure_allowed(&a, &site()).is_err());
    assert_eq!(registry.labels(&a).unwrap(), [source()].into());
}

#[test]
fn legacy_policy_edit_does_not_erase_a_scoped_allowance() {
    let root = TestRoot::new();
    let mut registry = root.registry();
    let context = app("alice", "first");
    registry.register_context(&context).unwrap();
    registry.add_sources(&context, [source()]).unwrap();
    registry.grant_sharing(source(), site(), ReaderScope::Context(context.clone()), SharingDuration::Permanent).unwrap();
    registry.set_policy(source(), FlowPolicy { recipients: [site()].into() }).unwrap();
    registry.set_policy(source(), FlowPolicy::default()).unwrap();
    assert!(registry.policy_for(&source()).unwrap().recipients.is_empty());
    assert!(registry.ensure_allowed(&context, &site()).is_ok());
    assert!(registry.grant_sharing(Source::UnknownPrivate, site(), ReaderScope::Context(context), SharingDuration::RobrixSession).is_err());
}

#[test]
fn migration_inherits_v1_shared_labels_and_preserves_old_files() {
    let root = TestRoot::new();
    let old = root.0.join("app_data/tool");
    fs::create_dir_all(&old).unwrap();
    fs::write(old.join("memory"), "private retained state").unwrap();
    let legacy = serde_json::json!({ "version": 1,
        "policies": [[source(), {"recipients": [site()]}]],
        "app_labels": { "tool": [source()] },
        "agent_labels": [{ "account": "alice", "room": "first", "label": [source()] }]
    });
    fs::write(root.0.join(METADATA_FILE), legacy.to_string()).unwrap();
    let mut registry = root.registry();
    let a = app("alice", "first");
    let b = app("bob", "second");
    for context in [&a, &b] {
        registry.register_context(context).unwrap();
        assert_eq!(registry.labels(context).unwrap(), [source()].into());
        assert_eq!(registry.influences(context).unwrap(), [Influence::Unknown].into());
        assert!(registry.ensure_allowed(context, &site()).is_ok());
        let path = registry.context_storage_path(context).unwrap();
        assert_ne!(path, old);
        assert!(!path.exists());
    }
    assert_eq!(fs::read_to_string(old.join("memory")).unwrap(), "private retained state");
    assert_eq!(registry.app_storage_paths("tool").unwrap().len(), 3);
    let disk: serde_json::Value = serde_json::from_slice(&fs::read(root.0.join(METADATA_FILE)).unwrap()).unwrap();
    assert_eq!(disk["version"], 2);
    assert!(disk.get("app_labels").is_none());
}

#[test]
fn clearance_blocks_private_input_and_integrity_transfer_atomically() {
    let root = TestRoot::new();
    let mut registry = root.registry();
    let private = app("alice", "private");
    let public = ContextId::PublicApp { account: "alice".into(), app: "tool".into() };
    registry.register_context(&private).unwrap();
    registry.register_context(&public).unwrap();
    registry.add_sources(&private, [source()]).unwrap();
    registry.add_influences(&private, [internet()]).unwrap();
    assert!(registry.transfer(&private, &public).is_err());
    assert!(registry.labels(&public).unwrap().is_empty());
    assert!(registry.influences(&public).unwrap().is_empty());
    assert!(registry.add_sources(&public, [source()]).is_err());
    assert!(registry.set_clearance(&public, None).is_err());
    assert!(registry.set_clearance(&private, Some(Label::new())).is_err());
    assert_ne!(registry.context_storage_path(&private).unwrap(), registry.context_storage_path(&public).unwrap());
    let mut restarted = root.registry();
    restarted.register_context(&public).unwrap();
    assert_eq!(restarted.clearance(&public).unwrap(), Some(Label::new()));
}

#[test]
fn private_code_cannot_be_laundered_through_a_public_compartment() {
    let root = TestRoot::new();
    let mut registry = root.registry();
    let public = ContextId::PublicApp { account: "alice".into(), app: "tool".into() };
    registry.register_context(&public).unwrap();
    registry.add_code_sources("tool", [source()]).unwrap();
    assert!(registry.labels(&public).is_err());
    assert!(registry.context_storage_path(&public).is_err());
    registry.remove_context(&public);
    assert!(registry.register_context(&public).is_err());
    let private = app("alice", "private");
    registry.register_context(&private).unwrap();
    assert_eq!(registry.labels(&private).unwrap(), [source()].into());
    let mut restarted = root.registry();
    assert!(restarted.register_context(&public).is_err());
}

#[test]
fn clearance_is_durable_and_a_trusted_increase_does_not_clear_labels() {
    let root = TestRoot::new();
    let mut registry = root.registry();
    let context = app("alice", "private");
    registry.register_context(&context).unwrap();
    registry.set_clearance(&context, Some([source()].into())).unwrap();
    registry.add_sources(&context, [source()]).unwrap();
    let another = Source::Account { account: "alice".into() };
    assert!(registry.add_sources(&context, [another.clone()]).is_err());
    registry.add_code_sources("tool", [another.clone()]).unwrap();
    assert!(registry.labels(&context).is_err());
    registry.set_clearance(&context, None).unwrap();
    assert_eq!(registry.labels(&context).unwrap(), [source(), another].into());
}

#[test]
fn integrity_transfer_persists_and_authority_matches_exact_reviewed_action() {
    let root = TestRoot::new();
    let mut registry = root.registry();
    let from = app("alice", "private");
    let to = ContextId::Agent { account: "alice".into(), room: "private".into() };
    for context in [&from, &to] { registry.register_context(context).unwrap(); }
    registry.add_influences(&from, [internet()]).unwrap();
    registry.transfer(&from, &to).unwrap();
    let expected = registry.influences(&to).unwrap();
    assert!(expected.contains(&internet()));
    assert!(expected.contains(&Influence::MiniApp { account: "alice".into(), app: "tool".into() }));
    assert!(registry.ensure_action_allowed(&to, &action()).is_err());
    registry.grant_authority_checked(&to, action(), AuthoritySession::RobrixSession, &expected).unwrap();
    assert!(registry.ensure_action_allowed(&to, &action()).is_ok());
    assert!(registry.ensure_action_allowed(&from, &action()).is_err());
    let different = SensitiveAction { target: "other/user".into(), ..action() };
    assert!(registry.ensure_action_allowed(&to, &different).is_err());
    let different = SensitiveAction { kind: "matrix.send".into(), ..action() };
    assert!(registry.ensure_action_allowed(&to, &different).is_err());
    let mut restarted = root.registry();
    restarted.register_context(&to).unwrap();
    assert_eq!(restarted.influences(&to).unwrap(), expected);
    assert!(restarted.authorities().unwrap().is_empty());
    assert!(restarted.ensure_action_allowed(&to, &action()).is_err());
}

#[test]
fn later_influences_require_fresh_review_and_context_close_revokes_authority() {
    let root = TestRoot::new();
    let mut registry = root.registry();
    let context = app("alice", "private");
    registry.register_context(&context).unwrap();
    registry.add_influences(&context, [internet()]).unwrap();
    let reviewed = registry.influences(&context).unwrap();
    registry.grant_authority_checked(&context, action(), AuthoritySession::RobrixSession, &reviewed).unwrap();
    registry.add_influences(&context, [Influence::Model("model".into())]).unwrap();
    assert!(registry.ensure_action_allowed(&context, &action()).is_err());
    assert!(registry.grant_authority_checked(&context, action(), AuthoritySession::RobrixSession, &reviewed).is_err());
    let reviewed = registry.influences(&context).unwrap();
    registry.grant_authority_checked(&context, action(), AuthoritySession::RoomSession { account: "alice".into(), room: "private".into() }, &reviewed).unwrap();
    registry.close_room_session("alice", "private").unwrap();
    assert!(registry.ensure_action_allowed(&context, &action()).is_err());
    registry.grant_authority_checked(&context, action(), AuthoritySession::RobrixSession, &reviewed).unwrap();
    registry.remove_context(&context);
    registry.register_context(&context).unwrap();
    assert!(registry.authorities().unwrap().is_empty());
    assert!(registry.ensure_action_allowed(&context, &action()).is_err());
}

#[test]
fn confidentiality_grants_and_action_authority_do_not_imply_each_other() {
    let root = TestRoot::new();
    let mut registry = root.registry();
    let context = app("alice", "private");
    registry.register_context(&context).unwrap();
    registry.add_sources(&context, [source()]).unwrap();
    registry.add_influences(&context, [internet()]).unwrap();
    registry.grant_sharing(source(), site(), ReaderScope::Context(context.clone()), SharingDuration::Permanent).unwrap();
    assert!(registry.ensure_allowed(&context, &site()).is_ok());
    assert!(registry.ensure_action_allowed(&context, &action()).is_err());
    registry.grant_authority(&context, action(), AuthoritySession::RobrixSession).unwrap();
    registry.set_policy(source(), FlowPolicy::default()).unwrap();
    let grant = registry.sharing_grants().unwrap().pop().unwrap();
    registry.revoke_sharing(grant.id).unwrap();
    assert!(registry.ensure_action_allowed(&context, &action()).is_ok());
    assert!(registry.ensure_allowed(&context, &site()).is_err());
}

#[test]
fn trusted_diagnostics_show_exact_missing_sources_and_coalesce_polling() {
    let root = TestRoot::new();
    let mut registry = root.registry();
    let context = app("alice", "private");
    registry.register_context(&context).unwrap();
    let account = Source::Account { account: "alice".into() };
    registry.add_sources(&context, [source(), account.clone()]).unwrap();
    registry.set_policy(source(), FlowPolicy { recipients: [site()].into() }).unwrap();
    for _ in 0..300 { assert!(registry.ensure_allowed(&context, &site()).is_err()); }
    let diagnostics = registry.recent_decisions().unwrap();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].sources, [source(), account.clone()].into());
    assert_eq!(diagnostics[0].denied_sources, [account.clone()].into());
    registry.set_policy(account, FlowPolicy { recipients: [site()].into() }).unwrap();
    assert!(registry.ensure_allowed(&context, &site()).is_ok());
    assert_eq!(registry.recent_decisions().unwrap().len(), 2);
    registry.add_influences(&context, [internet()]).unwrap();
    for _ in 0..300 { assert!(registry.ensure_action_allowed(&context, &action()).is_err()); }
    assert_eq!(registry.recent_action_decisions().unwrap().len(), 1);
    assert_eq!(registry.contexts().unwrap()[0].influences, [internet(), Influence::RoomContent { account: "alice".into(), room: "private".into() }].into());
}

#[test]
fn activation_epoch_rejects_old_work_after_same_compartment_reopens() {
    let root = TestRoot::new();
    let mut registry = root.registry();
    let context = app("alice", "private");
    registry.register_context(&context).unwrap();
    registry.add_sources(&context, [source()]).unwrap();
    registry.grant_sharing(source(), site(), ReaderScope::Context(context.clone()), SharingDuration::Permanent).unwrap();
    let first = registry.context_epoch(&context).unwrap();
    let reviewed = registry.influences(&context).unwrap();
    registry.register_context(&context).unwrap();
    assert_eq!(registry.context_epoch(&context).unwrap(), first);
    registry.remove_context(&context);
    assert!(registry.ensure_context_epoch(&context, first).is_err());
    registry.register_context(&context).unwrap();
    let second = registry.context_epoch(&context).unwrap();
    assert_ne!(second, first);
    assert!(registry.grant_authority_for_activation(&context, action(), AuthoritySession::RobrixSession, &reviewed, first).is_err());
    assert!(registry.add_influences_for_activation(&context, first, [internet()]).is_err());
    assert_eq!(registry.influences(&context).unwrap(), reviewed);
    assert!(registry.add_sources_for_activation(&context, first, [Source::UnknownPrivate]).is_err());
    assert_eq!(registry.labels(&context).unwrap(), [source()].into());
    registry.grant_authority(&context, action(), AuthoritySession::RobrixSession).unwrap();
    assert!(registry.ensure_allowed(&context, &site()).is_ok());
    assert!(registry.ensure_action_allowed(&context, &action()).is_ok());
    assert!(registry.ensure_context_epoch(&context, first).is_err());
    assert!(registry.ensure_context_epoch(&context, second).is_ok());
    registry.end_session().unwrap();
    registry.register_context(&context).unwrap();
    assert!(registry.ensure_context_epoch(&context, second).is_err());
}

#[test]
fn late_activation_drop_does_not_remove_reopened_context_or_authority() {
    let root = TestRoot::new();
    let mut registry = root.registry();
    let context = app("alice", "private");
    registry.register_context(&context).unwrap();
    registry.add_sources(&context, [source()]).unwrap();
    let first = registry.context_epoch(&context).unwrap();
    registry.remove_context_for_activation(&context, first).unwrap();
    registry.register_context(&context).unwrap();
    let current = registry.context_epoch(&context).unwrap();
    registry.grant_authority(&context, action(), AuthoritySession::RobrixSession).unwrap();
    assert!(registry.remove_context_for_activation(&context, first).is_err());
    assert_eq!(registry.context_epoch(&context).unwrap(), current);
    assert!(registry.ensure_action_allowed(&context, &action()).is_ok());
    registry.remove_context_for_activation(&context, current).unwrap();
    assert!(registry.context_epoch(&context).is_err());
    assert!(registry.authorities().unwrap().is_empty());
    registry.register_context(&context).unwrap();
    assert_eq!(registry.labels(&context).unwrap(), [source()].into());
}

#[test]
fn persisted_ephemeral_grants_and_modified_public_clearance_fail_closed() {
    let root = TestRoot::new();
    let mut metadata = Metadata::default();
    metadata.next_id = 2;
    metadata.grants.push(SharingGrant { id: 1, source: source(), recipient: site(), reader: ReaderScope::AllReaders, duration: SharingDuration::RobrixSession });
    fs::write(root.0.join(METADATA_FILE), serde_json::to_vec(&metadata).unwrap()).unwrap();
    assert!(Registry::open(&root.0).is_err());
    metadata.grants.clear();
    metadata.code.insert("tool".into(), StoredProvenance::default());
    metadata.contexts.push(StoredContext { context: ContextId::PublicApp { account: "alice".into(), app: "tool".into() }, provenance: StoredProvenance::default(), clearance: None });
    fs::write(root.0.join(METADATA_FILE), serde_json::to_vec(&metadata).unwrap()).unwrap();
    assert!(Registry::open(&root.0).is_err());
}
