use super::*;
use super::tests::{approval, fixture, message, request, ok_response};
use crate::tool_output::{ModelAttachment, ToolOutput, ToolOutputStore};
use std::net::TcpListener;
use base64::{Engine, engine::general_purpose::STANDARD};

struct Context(ContextId);
impl Context {
    fn new(name: &str) -> Self {
        let context = ContextId::Agent { account: "@multimodal:test".into(), room: format!("!{name}:test") };
        information_flow::register_context(&context).unwrap();
        Self(context)
    }
}
impl Drop for Context { fn drop(&mut self) { let _ = information_flow::remove_context(&self.0); } }

fn attachments() -> Vec<ModelAttachment> {
    [
        (include_bytes!("../../../tests/fixtures/vision-colors.png").as_slice(), "image/png", "quadrants.png"),
        (include_bytes!("../../../tests/fixtures/report.pdf").as_slice(), "application/pdf", "report.pdf"),
        (include_bytes!("../../../tests/fixtures/tone.wav").as_slice(), "audio/wav", "recording.wav"),
        (include_bytes!("../../../tests/fixtures/blue.mp4").as_slice(), "video/mp4", "movie.mp4"),
        ("Real UTF-8 report content: 猫, 42".as_bytes(), "text/plain", "report.txt"),
        (include_bytes!("../../../tests/fixtures/vision-colors.heic").as_slice(), "image/heic", "photo.heic"),
        (include_bytes!("../../../tests/fixtures/vision-colors.heic").as_slice(), "image/heif", "photo.heif"),
        (include_bytes!("../../../tests/fixtures/vision-colors.gif").as_slice(), "image/gif", "quadrants.gif"),
    ].into_iter().map(|(bytes, mime, name)| ModelAttachment::new(bytes.to_vec(), mime.into(), name.into()).unwrap()).collect()
}

fn transcript(store: &ToolOutputStore, context: &ContextId, attachments: Vec<ModelAttachment>) -> Vec<Message> {
    let receipt = store.register(context, information_flow::context_epoch(context).unwrap(), ToolOutput { text:"Authorized attachment loaded".into(), attachments }).unwrap();
    let mut assistant = message(MessageRole::Assistant, "");
    assistant.tool_calls = Some(vec![
        ToolCall { id:"first".into(), name:"inspect_media".into(), arguments:json!({"draft_id":"draft"}), metadata:None },
        ToolCall { id:"second".into(), name:"room_info".into(), arguments:json!({}), metadata:None },
    ]);
    let mut first = message(MessageRole::Tool, &receipt); first.tool_call_id = Some("first".into());
    let mut second = message(MessageRole::Tool, "Second tool result"); second.tool_call_id = Some("second".into());
    vec![assistant, first, second]
}

#[tokio::test]
async fn actual_http_openai_media_is_user_content_after_all_tool_acknowledgements() {
    let context = Context::new("openai-wire");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let server = request(listener.try_clone().unwrap(), ok_response(), None);
    let mut provider = GuardedProvider::new(fixture(&format!("http://{}/v1",listener.local_addr().unwrap()), Protocol::OpenAi), approval(Arc::new(AtomicBool::new(true)))).unwrap();
    provider.audit_context = Some(context.0.clone());
    let source = attachments();
    let history = transcript(&provider.outputs, &context.0, source.clone());
    provider.chat(&history, &[], &ChatConfig::default()).await.unwrap();
    let received = server.join().unwrap();
    let body: Value = serde_json::from_str(received.split_once("\r\n\r\n").unwrap().1).unwrap();
    let turns = body["messages"].as_array().unwrap();
    assert_eq!(turns.iter().map(|turn| turn["role"].as_str().unwrap()).collect::<Vec<_>>(), ["assistant","tool","tool","user"]);
    assert_eq!(turns[1]["tool_call_id"], "first"); assert_eq!(turns[2]["tool_call_id"], "second");
    assert!(turns[1]["content"].is_string() && turns[2]["content"].is_string(), "OpenAI tool acknowledgements only accept text");
    let parts = turns[3]["content"].as_array().unwrap();
    assert!(parts.iter().any(|part| part["image_url"]["url"] == format!("data:image/png;base64,{}",STANDARD.encode(source[0].bytes()))));
    assert!(parts.iter().any(|part| part["image_url"]["url"] == format!("data:image/gif;base64,{}",STANDARD.encode(source[7].bytes()))));
    assert!(parts.iter().any(|part| part["file"]["file_data"] == format!("data:application/pdf;base64,{}",STANDARD.encode(source[1].bytes()))));
    assert!(parts.iter().any(|part| part["input_audio"]["data"] == STANDARD.encode(source[2].bytes())));
    assert!(parts.iter().any(|part| part["text"].as_str().is_some_and(|text| text.contains("Real UTF-8 report content: 猫, 42"))));
    assert!(parts.iter().any(|part| part["text"].as_str().is_some_and(|text| text.contains("capability error") && text.contains("video/mp4"))));
    for mime in ["image/heic","image/heif"] { assert!(parts.iter().any(|part| part["text"].as_str().is_some_and(|text| text.contains("capability error") && text.contains(mime)))); }
    assert!(!received.contains(&STANDARD.encode(source[5].bytes())), "unsupported HEIC/HEIF bytes must not reach OpenAI");
    assert!(!received.contains("_robrix_media_receipt"));
    assert!(!parts.iter().filter_map(|part| part["text"].as_str()).any(|text| text.contains(&STANDARD.encode(source[0].bytes()))));
}

#[tokio::test]
async fn actual_http_anthropic_receives_image_pdf_and_file_text_and_precise_audio_video_errors() {
    let context = Context::new("anthropic-wire");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let reply = json!({"content":[{"type":"text","text":"native contents received"}],"stop_reason":"end_turn"}).to_string();
    let server = request(listener.try_clone().unwrap(), format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",reply.len()), None);
    let mut provider = GuardedProvider::new(fixture(&format!("http://{}/v1",listener.local_addr().unwrap()), Protocol::Anthropic), approval(Arc::new(AtomicBool::new(true)))).unwrap();
    provider.audit_context = Some(context.0.clone());
    let source = attachments();
    provider.chat(&transcript(&provider.outputs, &context.0, source.clone()), &[], &ChatConfig::default()).await.unwrap();
    let received = server.join().unwrap();
    let body: Value = serde_json::from_str(received.split_once("\r\n\r\n").unwrap().1).unwrap();
    let content = body["messages"][1]["content"].as_array().unwrap();
    assert_eq!(content[0]["type"],"tool_result"); assert_eq!(content[0]["tool_use_id"],"first");
    assert_eq!(content[1]["type"],"tool_result"); assert_eq!(content[1]["tool_use_id"],"second");
    assert!(content.iter().any(|part| part["type"] == "image" && part["source"]["data"] == STANDARD.encode(source[0].bytes())));
    assert!(content.iter().any(|part| part["type"] == "image" && part["source"]["media_type"] == "image/gif" && part["source"]["data"] == STANDARD.encode(source[7].bytes())));
    assert!(content.iter().any(|part| part["type"] == "document" && part["source"]["data"] == STANDARD.encode(source[1].bytes())));
    for mime in ["audio/wav","video/mp4","image/heic","image/heif"] { assert!(content.iter().any(|part| part["text"].as_str().is_some_and(|text| text.contains("capability error") && text.contains(mime)))); }
    assert!(!received.contains(&STANDARD.encode(source[5].bytes())), "unsupported HEIC/HEIF bytes must not reach Anthropic");
    assert!(!received.contains("_robrix_media_receipt"));
}

#[tokio::test]
async fn actual_http_native_gemini_receives_every_supported_attachment_byte_and_preserves_signatures() {
    let context = Context::new("gemini-wire");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let reply = json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"room_info","args":{}},"thoughtSignature":"opaque-native-signature"}]},"finishReason":"STOP"}]}).to_string();
    let server = request(listener.try_clone().unwrap(),format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",reply.len()),None);
    let mut config = fixture(&format!("http://{}/v1beta",listener.local_addr().unwrap()), Protocol::Gemini);
    config.provider = "gemini".into();
    let mut provider = GuardedProvider::new(config, approval(Arc::new(AtomicBool::new(true)))).unwrap();
    provider.audit_context = Some(context.0.clone());
    let source = attachments();
    let response = provider.chat(&transcript(&provider.outputs,&context.0,source.clone()), &[], &ChatConfig::default()).await.unwrap();
    assert_eq!(response.tool_calls[0].metadata,Some(json!({"thought_signature":"opaque-native-signature"})));
    let received = server.join().unwrap();
    assert!(received.to_lowercase().contains("x-goog-api-key: fixture-credential"));
    let body: Value = serde_json::from_str(received.split_once("\r\n\r\n").unwrap().1).unwrap();
    let parts = body["contents"][1]["parts"].as_array().unwrap();
    assert_eq!(parts[0]["functionResponse"]["name"],"inspect_media");
    assert_eq!(parts[1]["functionResponse"]["name"],"room_info");
    for attachment in source.iter().filter(|attachment| attachment.text().is_none() && attachment.mime_type() != "image/gif") {
        assert!(parts.iter().any(|part| part["inlineData"]["mimeType"] == attachment.mime_type() && part["inlineData"]["data"] == STANDARD.encode(attachment.bytes())), "{} bytes must arrive as native content",attachment.mime_type());
    }
    assert!(parts.iter().any(|part| part["text"].as_str().is_some_and(|text| text.contains("capability error") && text.contains("image/gif"))));
    assert!(!received.contains(&STANDARD.encode(source[7].bytes())), "unsupported GIF bytes must not reach Gemini");
    assert!(!received.contains("_robrix_media_receipt"));
}

#[test]
fn receipts_are_activation_bound_and_never_read_untracked_paths_or_user_text() {
    let context = Context::new("receipt-owner");
    let other = Context::new("receipt-other");
    let store = ToolOutputStore::default();
    let receipt = store.register(&context.0,information_flow::context_epoch(&context.0).unwrap(),ToolOutput {text:"loaded".into(),attachments:vec![attachments().remove(0)]}).unwrap();
    assert!(store.resolve(&other.0,&receipt).err().unwrap().contains("another agent session"));
    let user = message(MessageRole::User,&receipt);
    let turns = super::super::multimodal::prepare(&[user],&store,Some(&context.0)).unwrap();
    assert!(turns[0].attachments.is_empty());
    let mut path = message(MessageRole::User,"private file"); path.media.push("/etc/passwd".into());
    assert!(super::super::multimodal::prepare(&[path],&store,Some(&context.0)).is_err());
    information_flow::remove_context(&context.0).unwrap();
    information_flow::register_context(&context.0).unwrap();
    assert!(store.resolve(&context.0,&receipt).is_err(),"replacement activation cannot use an old receipt");
}

#[tokio::test]
#[ignore = "Calls the configured model service with only a synthetic public four-color image"]
async fn live_configured_model_sees_synthetic_image_pixels() {
    let context = Context::new("live-public-vision");
    let prefs = crate::prefs::load_agent_prefs();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let model = loop {
        match crate::model_transport::current_recipient(&prefs) {
            Ok(model) => break model,
            Err(error) if error == "Checking Claude Code sign-in. Try again shortly." && std::time::Instant::now() < deadline => {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
            Err(error) => panic!("configured live model is unavailable: {error}"),
        }
    };
    #[cfg(feature="embedded")]
    let answer=crate::octos_embedded::tests::run_media_tool_loop(&context.0,|outputs|
        crate::model_transport::provider_with_outputs(&prefs,context.0.clone(),Arc::new(AtomicBool::new(false)),outputs).unwrap()).await.to_lowercase();
    #[cfg(not(feature="embedded"))]
    let answer = {
    let outputs = Arc::new(ToolOutputStore::default());
    let provider = crate::model_transport::provider_with_outputs(&prefs,context.0.clone(),Arc::new(AtomicBool::new(false)),outputs.clone()).unwrap();
    let receipt = outputs.register(&context.0,information_flow::context_epoch(&context.0).unwrap(),ToolOutput {text:"Synthetic image, no filename hints".into(),attachments:vec![ModelAttachment::new(include_bytes!("../../../tests/fixtures/vision-colors.png").to_vec(),"image/png".into(),"fixture.png".into()).unwrap()]}).unwrap();
    let mut assistant = message(MessageRole::Assistant,"");
    assistant.tool_calls=Some(vec![ToolCall{id:"vision-probe".into(),name:"inspect_media".into(),arguments:json!({"draft_id":"synthetic"}),metadata:None}]);
    let mut result=message(MessageRole::Tool,&receipt);result.tool_call_id=Some("vision-probe".into());
    let history=[message(MessageRole::User,"Look at the supplied image pixels. Identify the color in each quadrant in this order: top-left, top-right, bottom-left, bottom-right. Reply only four color names separated by commas. Do not infer colors from metadata."),assistant,result];
    let result=provider.chat(&history,&[],&ChatConfig::default()).await.unwrap();
    result.content.unwrap_or_default().to_lowercase()
    };
    let positions=["red","green","blue","yellow"].map(|color|answer.find(color).unwrap_or_else(||panic!("configured model did not identify synthetic colors: {answer}")));
    assert!(positions.windows(2).all(|positions|positions[0]<positions[1]),"configured model did not identify quadrant order: {answer}");
    eprintln!("Live synthetic vision verified: {} returned {}",model.label,answer.trim());
}
