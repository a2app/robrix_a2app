//! Message previews preserve the content actually committed by each sender.
use super::*;
use serde_json::Value;

fn is_text_message(capability: &str) -> bool {
    matches!(capability, "matrix.room.message.send" | "matrix.rooms.message.send"
        | "matrix.room.message.reply" | "matrix.room.thread.reply")
}

fn plain(body: &str) -> PermissionMessagePreview {
    PermissionMessagePreview { body: body.into(), formatted_html: None }
}

fn media_caption(payload: &Value) -> Option<PermissionMessagePreview> {
    let media = payload.get("media").unwrap_or(payload);
    let body = media["caption"].as_str().filter(|caption| !caption.is_empty())?;
    Some(plain(body))
}

/// Matrix mini-app text APIs send literal text. Extra guest-provided rich
/// fields cannot change the preview of a format the native sender ignores.
fn from_bridge(request: &SplashHostRequest) -> Option<PermissionMessagePreview> {
    let capability = a2app_core::capabilities::for_service(&request.service)?;
    let args: Value = serde_json::from_str(&request.args_json).ok()?;
    if is_text_message(capability.id) {
        let (_, room) = a2app_core::manifest::split_instance_tag(&request.app_tag);
        let call = services::matrix::parse(&request.service, &args, room.is_some()).ok()?;
        return match call {
            services::MatrixServiceCall::SendMessage { body } | services::MatrixServiceCall::RoomsSend { body, .. }
                | services::MatrixServiceCall::Reply { body, .. } => Some(plain(&body)),
            _ => None,
        };
    }
    if capability.id == "matrix.media.send" { return media_caption(&args); }
    None
}

/// The flow review contains the final native event body. HTML is recognized
/// only for the Matrix custom-HTML format, with its plain fallback retained.
pub(super) fn from_flow_payload(capability: &str, payload: &Value, agent: bool) -> Option<PermissionMessagePreview> {
    if matches!(capability, "matrix.media.send" | "matrix.media.upload") { return media_caption(payload); }
    if !is_text_message(capability) { return None; }
    let content = payload.get("parameters").unwrap_or(payload);
    if let Some(body) = content["body"].as_str() {
        let formatted_html = (content["format"].as_str() == Some("org.matrix.custom.html"))
            .then(|| content["formatted_body"].as_str().map(str::to_owned)).flatten();
        return Some(PermissionMessagePreview { body: body.into(), formatted_html });
    }
    // Agent custom message captures retain Markdown source as `text` and use
    // the same host formatter as AiReplyContent immediately before posting.
    if agent && let Some(text) = content["text"].as_str() {
        return Some(PermissionMessagePreview {
            body: text.into(),
            formatted_html: content["formatted"].as_str().map(str::to_owned)
                .or_else(|| crate::a2app::ai_room_events::agent_reply_formatted_html(text)),
        });
    }
    None
}

#[cfg_attr(not(unix), allow(unused_variables))]
pub(super) fn from_parked(state: &A2AppState, parked: &[ParkedRequest]) -> Option<PermissionMessagePreview> {
    parked.iter().find_map(|request| match request {
        ParkedRequest::Bridge(Some(request)) => from_bridge(request),
        ParkedRequest::Bridge(None) => None,
        ParkedRequest::AppMedia(post) => post.message_preview(),
        #[cfg(unix)]
        ParkedRequest::AiTool { job: SessionJob::PostRoomMessage { text, .. }, .. } => {
            Some(PermissionMessagePreview { body: text.clone(),
                formatted_html: crate::a2app::ai_room_events::agent_reply_formatted_html(text) })
        }
        #[cfg(unix)]
        ParkedRequest::AiTool { room_id, job: SessionJob::PostRoomMedia { draft_id, .. } } =>
            ai_media::message_preview(state, room_id, draft_id),
        #[cfg(unix)]
        ParkedRequest::AiTool { .. } | ParkedRequest::NetworkAccess { .. } => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request(service: &str, args: Value) -> SplashHostRequest {
        SplashHostRequest { app_tag: a2app_core::manifest::instance_tag("roll-call", Some("!origin:preview-test")),
            heap_key: 0, req_id: 1, service: service.into(), args_json: args.to_string(), may_prompt: true }
    }

    fn with_state(test: impl FnOnce(&A2AppState)) {
        struct Restore(Option<A2AppState>);
        impl Drop for Restore {
            fn drop(&mut self) { A2APP.with(|state| { state.replace(self.0.take()); }); }
        }
        let _restore = Restore(A2APP.with(|state| state.replace(None)));
        initialize_background_test(builtin::stock("roll-call").unwrap());
        with_a2app(|state| test(state));
    }

    #[test]
    fn ordinary_text_preview_uses_complete_native_plain_body_with_a_separate_title() {
        let body = format!("**Literal bold markers**, <b>literal tags</b> and Unicode: {}\nSecond line\nThird line\nFourth line", "猫🦀".repeat(200));
        for service in ["matrix.send_message", "matrix.rooms_send", "matrix.reply", "matrix.thread_reply"] {
            let request = request(service, json!({"body":format!("  {body}  "), "room_id":"!target:preview-test", "event_id":"$event:preview-test",
                "format":"org.matrix.custom.html", "formatted_body":"<b>Ignored by the plain native API</b>"}));
            let preview = from_bridge(&request).unwrap();
            assert_eq!(preview.body, body, "{service}: preserve all content after native parser's whitespace normalization");
            assert!(preview.formatted_html.is_none(), "{service}: guest formatting fields cannot change a plain send");
            let capability = a2app_core::capabilities::for_service(service).unwrap();
            let title = permission_action(capability, &json!({"body":body}));
            assert!(!title.contains("Literal bold"));
            if matches!(service, "matrix.send_message" | "matrix.rooms_send") { assert_eq!(title, "Post this message:"); }
        }
    }

    #[test]
    fn canonical_flow_preview_retains_html_and_plain_fallback_without_truncation() {
        let body = "Full literal fallback\nsecond\nthird\nfourth";
        let html = "<p><strong>Actual bold</strong> &amp; <em>emphasis</em></p><ul><li>one</li><li>two</li><li>three</li><li>four</li></ul>";
        for capability in ["matrix.room.message.send", "matrix.rooms.message.send", "matrix.room.message.reply", "matrix.room.thread.reply"] {
            let content = json!({"msgtype":"m.text", "body":body, "format":"org.matrix.custom.html", "formatted_body":html});
            let preview = from_flow_payload(capability, &content, false).unwrap();
            assert_eq!(preview.body, body);
            assert_eq!(preview.formatted_html.as_deref(), Some(html));
            for replacement in [json!(null), json!("unknown.format"), json!(42)] {
                let mut content = content.clone();
                content["format"] = replacement;
                let preview = from_flow_payload(capability, &content, false).unwrap();
                assert_eq!(preview.body, body);
                assert!(preview.formatted_html.is_none(), "only canonical Matrix HTML can override the plain body");
            }
        }
    }

    #[test]
    fn nonmessage_and_malformed_inputs_never_become_a_message_preview() {
        let payload = json!({"body":"request body", "text":"agent text", "caption":"not a media message",
            "format":"org.matrix.custom.html", "formatted_body":"<b>unrelated rich data</b>"});
        for capability in ["network.http", "matrix.room.messages.search", "matrix.room.info.read", "device.clipboard.write", "apps.generate"] {
            assert!(from_flow_payload(capability, &payload, true).is_none(), "{capability}");
        }
        for service in ["network.http", "matrix.room_info", "matrix.search_room", "clipboard.write", "permissions.request"] {
            assert!(from_bridge(&request(service, payload.clone())).is_none(), "{service}");
        }
        assert!(from_flow_payload("matrix.room.message.send", &json!({"body":42}), false).is_none());
        assert!(from_bridge(&request("matrix.send_message", json!({"body":"  "}))).is_none());
        assert!(from_bridge(&request("matrix.send_message", json!({"body":"x".repeat(4097)}))).is_none());
        assert!(from_flow_payload("matrix.room.message.send", &json!({"text":"not an agent event"}), false).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn agent_markdown_preview_uses_exactly_the_final_host_formatter() {
        let body = "# Report\n\n**Bold** and *emphasis*\n\n- first\n- second\n- third\n\n[Room](https://matrix.to/#/!target:preview-test)";
        let (answer, _) = std::sync::mpsc::channel();
        let parked = ParkedRequest::AiTool { room_id: "!origin:preview-test".try_into().unwrap(),
            job: SessionJob::PostRoomMessage { room_id: "!target:preview-test".into(), text: body.into(), answer } };
        with_state(|state| {
            let preview = from_parked(state, &[parked]).unwrap();
            assert_eq!(preview.body, body);
            assert_eq!(preview.formatted_html, crate::a2app::ai_room_events::agent_reply_formatted_html(body));
            assert!(preview.formatted_html.as_deref().unwrap().contains("<strong>Bold</strong>"));
        });
        let captured = from_flow_payload("matrix.rooms.message.send", &json!({"text":body, "formatted":"<p>The exact captured rendered body</p>"}), true).unwrap();
        assert_eq!(captured.body, body);
        assert_eq!(captured.formatted_html.as_deref(), Some("<p>The exact captured rendered body</p>"));
        let canonical_plain = from_flow_payload("matrix.rooms.message.send", &json!({"msgtype":"m.text", "body":"**literal**"}), true).unwrap();
        assert!(canonical_plain.formatted_html.is_none(), "an agent's explicitly plain native content stays plain");
    }

    #[test]
    fn media_preview_exposes_only_a_nonempty_actual_caption() {
        let caption = "Caption with **literal** markup\nand another line";
        let media = json!({"filename":"picture.png", "mime_type":"image/png", "caption":caption,
            "sha256":"digest", "size":100});
        for capability in ["matrix.media.send", "matrix.media.upload"] {
            let preview = from_flow_payload(capability, &json!({"room_id":"!target:preview-test", "media":media}), false).unwrap();
            assert_eq!(preview.body, caption);
            assert!(preview.formatted_html.is_none());
            for absent in [json!(null), json!(""), json!(42)] {
                let mut media = media.clone();
                media["caption"] = absent;
                assert!(from_flow_payload(capability, &json!({"media":media}), false).is_none(), "filename remains metadata, never message text");
            }
        }
        let preview = from_bridge(&request("matrix.send_media", json!({"filename":"picture.png", "mime_type":"image/png",
            "data_base64":"unshown encoded bytes", "caption":caption}))).unwrap();
        assert_eq!(preview.body, caption);
    }

    #[test]
    fn ordinary_and_flow_prompt_population_keep_content_out_of_action_and_write_notice() {
        let body = "**literal mini-app body**\nsecond\nthird\nfourth";
        let request = request("matrix.send_message", json!({"body":body}));
        let review = a2app_core::information_flow::EffectReview {
            id: 1, context: a2app_core::information_flow::ContextId::App { account: "@preview:test".into(),
                app: "roll-call".into(), room: Some("!origin:preview-test".into()) }, epoch: 1,
            recipient: Some(a2app_core::information_flow::Recipient::MatrixRoom { account: "@preview:test".into(), room:"!origin:preview-test".into() }),
            action: Some(a2app_core::information_flow::SensitiveAction { kind:"matrix.room.message.send".into(), target:"!origin:preview-test".into() }),
            sources: Default::default(), denied_sources: Default::default(), influences: Default::default(),
            payload: json!({"msgtype":"m.text", "body":body}).to_string().into(), allowed: false,
        };
        with_state(|state| {
            let info = prompt_info_for(state, None, "roll-call", Permission::MatrixRoomSend,
                &[ParkedRequest::Bridge(Some(request.clone()))], None);
            assert_eq!(info.capability.as_deref(), Some("Post this message:"));
            let preview = info.message_preview.unwrap();
            assert_eq!(preview.body, body);
            assert!(preview.formatted_html.is_none());
            let flow = FlowContinuation::Bridge { request, review, allow_once: false, permission: Some(Permission::MatrixRoomSend) };
            let info = flow_prompt_info(state, None, &flow);
            assert_eq!(info.action, "Post this message:");
            assert_eq!(info.write_warning.as_deref(), Some("Approving also turns on room changes for mini-apps. Each app still needs its own permission."),
                "the existing global switch notice is separate from the title and message preview");
            let preview = info.message_preview.unwrap();
            assert_eq!(preview.body, body);
            assert!(!preview.body.contains("turns on room changes"));
        });
    }
}
