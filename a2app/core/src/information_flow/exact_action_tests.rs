use super::*;
use super::tests::TestRoot;

fn setup() -> (TestRoot, Registry, ContextId, u64, SensitiveAction) {
    let root = TestRoot::new();
    let mut registry = root.registry();
    let context = ContextId::Agent { account: "alice".into(), room: "!private:example".into() };
    registry.register_context(&context).unwrap();
    registry.add_influences(&context, [Influence::Model("provider".into())]).unwrap();
    let epoch = registry.context_epoch(&context).unwrap();
    let action = SensitiveAction { kind: "network.POST".into(), target: "https://example.org".into() };
    (root, registry, context, epoch, action)
}

fn pending(registry: &mut Registry, context: &ContextId, epoch: u64, action: &SensitiveAction, payload: &serde_json::Value) -> ActionDecision {
    assert!(registry.check_exact_action_for_activation(context, epoch, action, payload).is_err());
    registry.recent_action_decisions().unwrap().pop().unwrap()
}

fn approve(registry: &mut Registry, decision: &ActionDecision) -> u64 {
    registry.grant_exact_action_for_activation(&decision.context, decision.request.as_ref().unwrap().id,
        &decision.influences, decision.epoch).unwrap()
}

#[test]
fn exact_review_is_captured_canonical_and_hidden_from_debug_and_disk() {
    let (root, mut registry, context, epoch, action) = setup();
    let payload = serde_json::json!({ "headers": { "z": "last", "a": "first" }, "body": "secret\nmessage", "url": "https://example.org/one" });
    let decision = pending(&mut registry, &context, epoch, &action, &payload);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&decision.request.as_ref().unwrap().payload).unwrap(), payload);
    assert!(!format!("{decision:?}").contains("secret"));
    let metadata = fs::read_to_string(root.0.join(METADATA_FILE)).unwrap();
    assert!(!metadata.contains("secret"));
    assert!(!metadata.contains("network.POST"));
    let id = decision.request.as_ref().unwrap().id;
    let reordered: serde_json::Value = serde_json::from_str(r#"{"url":"https://example.org/one","body":"secret\nmessage","headers":{"a":"first","z":"last"}}"#).unwrap();
    assert_eq!(pending(&mut registry, &context, epoch, &action, &reordered).request.unwrap().id, id);
}

#[test]
fn only_exact_commit_consumes_once_and_replay_needs_new_review() {
    let (_root, mut registry, context, epoch, action) = setup();
    let payload = serde_json::json!({ "text": "reviewed message" });
    let decision = pending(&mut registry, &context, epoch, &action, &payload);
    approve(&mut registry, &decision);
    // Legacy operation-only probes must never launder this into session consent.
    assert!(registry.ensure_action_allowed(&context, &action).is_err());
    for _ in 0..10 { registry.check_exact_action_for_activation(&context, epoch, &action, &payload).unwrap(); }
    for changed in [serde_json::json!({ "text": "different" }), serde_json::json!({ "text": "reviewed message", "extra": true })] {
        assert!(registry.commit_exact_action_for_activation(&context, epoch, &action, &changed).is_err());
    }
    let other = SensitiveAction { target: "https://attacker.example".into(), ..action.clone() };
    assert!(registry.commit_exact_action_for_activation(&context, epoch, &other, &payload).is_err());
    registry.commit_exact_action_for_activation(&context, epoch, &action, &payload).unwrap();
    assert!(registry.commit_exact_action_for_activation(&context, epoch, &action, &payload).is_err());
    assert!(registry.grant_exact_action_for_activation(&context, decision.request.unwrap().id, &decision.influences, epoch).is_err());
}

#[test]
fn review_cannot_cross_context_activation_or_new_influences() {
    let (_root, mut registry, context, epoch, action) = setup();
    let payload = serde_json::json!({ "arguments": [1, 2, 3] });
    let decision = pending(&mut registry, &context, epoch, &action, &payload);
    approve(&mut registry, &decision);
    let other = ContextId::Agent { account: "bob".into(), room: "!private:example".into() };
    registry.register_context(&other).unwrap();
    registry.add_influences(&other, decision.influences.clone()).unwrap();
    let other_epoch = registry.context_epoch(&other).unwrap();
    assert!(registry.commit_exact_action_for_activation(&other, other_epoch, &action, &payload).is_err());
    registry.add_influences(&context, [Influence::InternetOrigin("https://new.example".into())]).unwrap();
    assert!(registry.commit_exact_action_for_activation(&context, epoch, &action, &payload).is_err());
    assert!(registry.grant_exact_action_for_activation(&context, decision.request.unwrap().id, &decision.influences, epoch).is_err());
    let current = pending(&mut registry, &context, epoch, &action, &payload);
    approve(&mut registry, &current);
    registry.remove_context(&context);
    registry.register_context(&context).unwrap();
    let reopened = registry.context_epoch(&context).unwrap();
    assert!(registry.commit_exact_action_for_activation(&context, epoch, &action, &payload).is_err());
    assert!(registry.commit_exact_action_for_activation(&context, reopened, &action, &payload).is_err());
    assert!(registry.grant_exact_action_for_activation(&context, current.request.unwrap().id, &current.influences, reopened).is_err());
}

#[test]
fn cancel_revoke_and_bounded_eviction_remove_once_authority() {
    let (_root, mut registry, context, epoch, action) = setup();
    let payload = serde_json::json!({ "text": "message" });
    let decision = pending(&mut registry, &context, epoch, &action, &payload);
    let authority = approve(&mut registry, &decision);
    registry.revoke_authority(authority).unwrap();
    assert!(registry.commit_exact_action_for_activation(&context, epoch, &action, &payload).is_err());
    approve(&mut registry, &decision);
    assert!(registry.cancel_exact_action(decision.request.as_ref().unwrap().id).unwrap());
    assert!(registry.commit_exact_action_for_activation(&context, epoch, &action, &payload).is_err());
    let current = pending(&mut registry, &context, epoch, &action, &payload);
    approve(&mut registry, &current);
    for index in 0..70 { pending(&mut registry, &context, epoch, &action, &serde_json::json!({ "index": index })); }
    assert!(registry.authorities().unwrap().is_empty());
    assert!(registry.grant_exact_action_for_activation(&context, current.request.unwrap().id, &current.influences, epoch).is_err());
    assert!(registry.commit_exact_action_for_activation(&context, epoch, &action, &payload).is_err());
}

#[test]
fn explicit_session_consent_is_separate_and_unknown_or_large_reviews_fail_closed() {
    let (_root, mut registry, context, epoch, action) = setup();
    let influences = registry.influences(&context).unwrap();
    assert!(registry.grant_exact_action_for_activation(&context, 1, &influences, epoch).is_err());
    assert!(registry.grant_authority(&context, action.clone(), AuthoritySession::Once { request_id: 1 }).is_err());
    let too_large = serde_json::json!({ "text": "x".repeat(65537) });
    assert!(registry.commit_exact_action_for_activation(&context, epoch, &action, &too_large).is_err());
    assert!(registry.recent_action_decisions().unwrap().last().unwrap().request.is_none());
    let mut nested = serde_json::Value::Null;
    for _ in 0..34 { nested = serde_json::json!([nested]); }
    assert!(registry.commit_exact_action_for_activation(&context, epoch, &action, &nested).is_err());
    registry.grant_authority(&context, action.clone(), AuthoritySession::RobrixSession).unwrap();
    for i in 0..3 { registry.commit_exact_action_for_activation(&context, epoch, &action, &serde_json::json!({ "text": i })).unwrap(); }
    registry.commit_exact_action_for_activation(&context, epoch, &action, &too_large).unwrap();
    registry.commit_exact_action_for_activation(&context, epoch, &action, &nested).unwrap();

}


#[test]
fn closing_the_owning_room_cancels_pending_review_and_approval() {
    let (_root, mut registry, context, epoch, action) = setup();
    let payload = serde_json::json!({ "text": "message" });
    let decision = pending(&mut registry, &context, epoch, &action, &payload);
    approve(&mut registry, &decision);
    registry.close_room_session("bob", context.room().unwrap()).unwrap();
    registry.check_exact_action_for_activation(&context, epoch, &action, &payload).unwrap();
    registry.close_room_session(context.account(), context.room().unwrap()).unwrap();
    assert!(registry.commit_exact_action_for_activation(&context, epoch, &action, &payload).is_err());
    assert!(registry.grant_exact_action_for_activation(&context, decision.request.unwrap().id, &decision.influences, epoch).is_err());
}
