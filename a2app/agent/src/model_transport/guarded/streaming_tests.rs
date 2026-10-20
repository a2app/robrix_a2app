//! Each provider's own stream shapes, served by local mock services.
use super::*;
use super::tests::{approval, fixture, joined, message, previewing, read_request};
use std::io::Write;
use std::net::TcpListener;

/// SSE as services send it: a keep-alive comment, Anthropic's `event:` names, and `newline` line endings.
fn framed(events: &[Value], newline: &str) -> Vec<u8> {
    let mut stream = format!(": OPENROUTER PROCESSING{newline}{newline}");
    for event in events {
        if let Some(name) = event["type"].as_str() { stream.push_str(&format!("event: {name}{newline}")); }
        stream.push_str(&format!("data: {event}{newline}{newline}"));
    }
    stream.into_bytes()
}

fn done(mut stream: Vec<u8>, newline: &str) -> Vec<u8> {
    stream.extend(format!("data: [DONE]{newline}{newline}").into_bytes());
    stream
}

/// Rebuilds `stream` whole, split at every byte, and byte by byte, which must all agree.
fn rebuilt(protocol: Protocol, provider: &str, stream: &[u8]) -> (Vec<(bool, String)>, Value) {
    let rebuild = |pieces: Vec<&[u8]>| {
        let mut reply = StreamedReply::new(protocol, provider);
        let mut shown = Vec::new();
        for piece in pieces { shown.extend(reply.feed(piece).unwrap()); }
        (joined(&shown), reply.finish().unwrap())
    };
    let whole = rebuild(vec![stream]);
    for at in 0..=stream.len() {
        assert_eq!(rebuild(vec![&stream[..at], &stream[at..]]), whole, "{provider} split at {at}");
    }
    assert_eq!(rebuild(stream.chunks(1).collect()), whole, "{provider} byte by byte");
    whole
}

enum Reply { Stream(Vec<u8>), Json(u16, Value) }

/// Answers each request in turn, streams in small writes, and returns the request bodies and paths.
fn serve(listener: TcpListener, replies: Vec<Reply>) -> std::thread::JoinHandle<Vec<(String, Value)>> {
    std::thread::spawn(move || replies.into_iter().map(|reply| {
        let (mut stream, _) = listener.accept().unwrap();
        let received = read_request(&mut stream);
        match reply {
            Reply::Stream(bytes) => {
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n");
                for piece in bytes.chunks(7) {
                    let _ = stream.write_all(piece);
                    let _ = stream.flush();
                    std::thread::sleep(Duration::from_micros(300));
                }
            }
            Reply::Json(status, body) => {
                let body = body.to_string();
                let _ = stream.write_all(format!("HTTP/1.1 {status} Reply\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes());
            }
        }
        let (head, body) = received.split_once("\r\n\r\n").unwrap();
        (head.lines().next().unwrap().to_string(), serde_json::from_str(body).unwrap())
    }).collect())
}

fn delta(delta: Value) -> Value { json!({"id":"chunk","object":"chat.completion.chunk","choices":[{"index":0,"delta":delta,"finish_reason":null}]}) }
fn finished(reason: &str) -> Value { json!({"choices":[{"index":0,"delta":{},"finish_reason":reason}],"usage":{"prompt_tokens":9,"completion_tokens":4}}) }

const APP: &str = "```splash\nView{}\n```";

#[test]
fn openai_style_reasoning_variants_rebuild_at_any_split() {
    // Groq's gpt-oss and OpenRouter stream `reasoning`; OpenRouter adds `reasoning_details`.
    for provider in ["groq", "openrouter", "ollama"] {
        let stream = done(framed(&[
            delta(json!({"role":"assistant","content":"","reasoning":"Plan the ",
                "reasoning_details":[{"type":"reasoning.text","text":"Plan the ","format":"unknown","index":0}]})),
            delta(json!({"reasoning":"timer.","reasoning_details":[{"type":"reasoning.text","text":"timer.","index":0}]})),
            delta(json!({"content":"```splash\n","reasoning":null})),
            delta(json!({"tool_calls":[{"index":0,"id":"call-1","type":"function","function":{"name":"room_read","arguments":"{\"room\""}}]})),
            delta(json!({"content":"View{}\n```","tool_calls":[{"index":0,"function":{"arguments":":\"a\"}"}}]})),
            finished("tool_calls"),
        ], "\r\n"), "\r\n");
        let (shown, value) = rebuilt(Protocol::OpenAi, provider, &stream);
        assert_eq!(shown, vec![(true, "Plan the timer.".into()), (false, APP.into())], "{provider}");
        let response = parse_openai(value, provider).unwrap();
        assert_eq!((response.reasoning_content.as_deref(), response.content.as_deref()), (Some("Plan the timer."), Some(APP)));
        assert_eq!(response.tool_calls[0].arguments, json!({"room":"a"}));
        assert_eq!(response.stop_reason, StopReason::ToolUse);
    }
    // DeepSeek and Kimi name it `reasoning_content`; a copy under `reasoning` is not shown twice.
    for provider in ["deepseek", "moonshot", "moonshot-coding", "groq"] {
        let stream = done(framed(&[
            delta(json!({"role":"assistant","content":null,"reasoning_content":"Plan.","reasoning":"Plan."})),
            delta(json!({"content":APP,"reasoning_content":null})),
            finished("stop"),
        ], "\n"), "\n");
        let (shown, value) = rebuilt(Protocol::OpenAi, provider, &stream);
        assert_eq!(shown, vec![(true, "Plan.".into()), (false, APP.into())], "{provider}");
        assert_eq!(parse_openai(value, provider).unwrap().reasoning_content.as_deref(), Some("Plan."));
    }
    // Other services keep sending reasoning back, so a field they did not name is not taken as theirs.
    let stream = done(framed(&[delta(json!({"reasoning":"Unkept","content":APP})), finished("stop")], "\n"), "\n");
    let (shown, value) = rebuilt(Protocol::OpenAi, "custom", &stream);
    assert_eq!(shown, vec![(false, APP.into())]);
    assert_eq!(parse_openai(value, "custom").unwrap().reasoning_content, None);
}

#[test]
fn streamed_usage_is_kept_where_each_service_reports_it() {
    // Unstreamed replies report usage at the top; a stream must not read as free.
    let usage = json!({"prompt_tokens":9,"completion_tokens":4,"total_tokens":13});
    let kimi = json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop","usage":usage}]});
    let groq = json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"x_groq":{"id":"req-1","usage":usage}});
    let openai = json!({"choices":[],"usage":usage});
    for (provider, last) in [("moonshot", vec![kimi.clone()]), ("moonshot-coding", vec![kimi]), ("groq", vec![groq]),
        ("openai", vec![finished("stop"), openai])] {
        let stream = done(framed(&[vec![delta(json!({"content":APP}))], last].concat(), "\r\n"), "\r\n");
        let (_, value) = rebuilt(Protocol::OpenAi, provider, &stream);
        let response = parse_openai(value, provider).unwrap();
        assert_eq!((response.usage.input_tokens, response.usage.output_tokens), (9, 4), "{provider}");
    }
}

#[test]
fn think_tags_preview_as_thinking_the_way_the_agent_loop_sorts_them() {
    // Groq's qwen3 puts its reasoning in the reply text by default.
    let stream = done(framed(&[
        delta(json!({"role":"assistant","content":"<think>\nPlan the "})),
        delta(json!({"content":"timer.</th"})),
        delta(json!({"content":"ink>\n\n```splash\nlet a = 1 < 2\nView{}\n```"})),
        finished("stop"),
    ], "\r\n"), "\r\n");
    let (shown, value) = rebuilt(Protocol::OpenAi, "groq", &stream);
    assert_eq!(shown, vec![(true, "\nPlan the timer.".into()), (false, "\n\n```splash\nlet a = 1 < 2\nView{}\n```".into())]);
    // The reply itself stays as the service sent it; the agent loop sorts it the same way.
    let content = parse_openai(value, "groq").unwrap().content.unwrap();
    assert!(content.starts_with("<think>\nPlan the timer.</think>"));
    let (text, thinking) = octos_llm::strip_think_tags(&content);
    assert_eq!((text.as_str(), thinking.as_deref()), ("```splash\nlet a = 1 < 2\nView{}\n```", Some("Plan the timer.")));
    // Anthropic and Gemini reply text is sorted the same way.
    let (shown, _) = rebuilt(Protocol::Anthropic, "anthropic", &framed(&[
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"<think>Plan"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"</think>Done"}}),
        json!({"type":"message_stop"}),
    ], "\n"));
    assert_eq!(shown, vec![(true, "Plan".into()), (false, "Done".into())]);
    let (shown, _) = rebuilt(Protocol::Gemini, "gemini", &framed(&[
        json!({"candidates":[{"content":{"parts":[{"text":"<think>Plan</think>Done"}]},"finishReason":"STOP"}]}),
    ], "\n"));
    assert_eq!(shown, vec![(true, "Plan".into()), (false, "Done".into())]);
}

#[test]
fn one_huge_delta_of_think_tags_sorts_in_linear_time() {
    // Sorting it whole moved the rest of the delta per tag: minutes for a hostile 16 MiB one.
    let tags = "<think>t</think>".repeat(2 * 1024 * 1024 / 16);
    let stream = done(framed(&[delta(json!({"content":format!("{tags}<think>Plan</think>Done")})), finished("stop")], "\n"), "\n");
    let started = std::time::Instant::now();
    let shown = joined(&StreamedReply::new(Protocol::OpenAi, "groq").feed(&stream).unwrap());
    assert!(started.elapsed() < Duration::from_secs(2), "took {:?}", started.elapsed());
    assert_eq!(shown, vec![(true, format!("{}Plan", "t".repeat(2 * 1024 * 1024 / 16))), (false, "Done".into())]);
}

/// Kimi's Coding Plan and Claude stream thinking blocks with signatures before the text.
fn anthropic_events(thinking: &[&str], text: &[&str]) -> Vec<Value> {
    let mut events = vec![
        json!({"type":"message_start","message":{"id":"msg-1","type":"message","role":"assistant","content":[],"usage":{"input_tokens":12,"output_tokens":1}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
    ];
    events.extend(thinking.iter().map(|piece| json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":piece}})));
    events.extend([
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"EqQBCgIYAhIM"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
        json!({"type":"ping"}),
    ]);
    events.extend(text.iter().map(|piece| json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":piece}})));
    events.extend([
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":30}}),
        json!({"type":"message_stop"}),
    ]);
    events
}

#[test]
fn anthropic_thinking_and_tool_input_rebuild_at_any_split() {
    let mut events = anthropic_events(&["Plan the ", "timer."], &["```splash\n", "View{}\n```"]);
    let stop = events.split_off(events.len() - 2);
    events.extend([
        json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu-1","name":"room_read","input":{}}}),
        json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"room\":"}}),
        json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"\"a\"}"}}),
        json!({"type":"content_block_stop","index":2}),
    ]);
    events.extend(stop);
    let (shown, value) = rebuilt(Protocol::Anthropic, "moonshot-coding", &framed(&events, "\r\n"));
    assert_eq!(shown, vec![(true, "Plan the timer.".into()), (false, APP.into())]);
    let response = parse_anthropic(value).unwrap();
    assert_eq!((response.reasoning_content.as_deref(), response.content.as_deref()), (Some("Plan the timer."), Some(APP)));
    assert_eq!(response.tool_calls[0].arguments, json!({"room":"a"}));
    assert_eq!((response.usage.input_tokens, response.usage.output_tokens), (12, 30));
}

fn gemini_events() -> Vec<Value> {
    let part = |part: Value, extra: Value| {
        let mut event = json!({"candidates":[{"content":{"parts":[part],"role":"model"},"index":0}],"modelVersion":"gemini-3.8-flash","responseId":"r-1"});
        if let Some(extra) = extra.as_object() { for (key, value) in extra { event["candidates"][0][key] = value.clone(); } }
        event
    };
    vec![
        part(json!({"text":"**Planning**\nA timer","thought":true}), Value::Null),
        part(json!({"text":" with one button.","thought":true}), Value::Null),
        part(json!({"text":"```splash\nView{}","thoughtSignature":"c2lnbmVk"}), Value::Null),
        part(json!({"text":"\n```"}), Value::Null),
        part(json!({"functionCall":{"name":"room_info","args":{}},"thoughtSignature":"c2lnbmVk"}), json!({"finishReason":"STOP"})),
        json!({"candidates":[],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":7,"thoughtsTokenCount":40}}),
    ]
}

#[tokio::test]
async fn reasoning_goes_back_only_to_services_that_take_it() {
    for (provider, field, is_returned) in [("groq", "reasoning", false), ("openrouter", "reasoning", false), ("ollama", "reasoning", false),
        ("deepseek", "reasoning_content", true), ("moonshot", "reasoning_content", true), ("moonshot-coding", "reasoning_content", true)] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let stream = done(framed(&[delta(json!({field:"Plan the timer."})), delta(json!({"content":APP})), finished("stop")], "\r\n"), "\r\n");
        let server = serve(listener, vec![Reply::Stream(stream.clone()), Reply::Stream(stream)]);
        let mut config = fixture(&format!("http://{address}/v1"), Protocol::OpenAi);
        config.provider = provider.into();
        let (model, shown) = previewing(config, Arc::new(AtomicBool::new(true)));
        let response = model.chat(&[message(MessageRole::User, "Make a timer.")], &[], &ChatConfig::default()).await.unwrap();
        assert_eq!(joined(&shown.lock().unwrap()), vec![(true, "Plan the timer.".into()), (false, APP.into())], "{provider}");
        assert_eq!(response.reasoning_content.as_deref(), Some("Plan the timer."), "{provider}");
        // A repair turn sends the reply back; Groq rejects a `reasoning_content` field.
        let mut reply = message(MessageRole::Assistant, APP);
        reply.reasoning_content = response.reasoning_content;
        model.chat(&[message(MessageRole::User, "Make a timer."), reply, message(MessageRole::User, "Fix it.")], &[], &ChatConfig::default()).await.unwrap();
        let requests = server.join().unwrap();
        let returned = &requests[1].1["messages"][1];
        assert_eq!(returned.get("reasoning_content").is_some(), is_returned, "{provider}: {returned}");
        assert!(returned.get("reasoning").is_none());
    }
}

#[test]
fn unstreamed_replies_keep_the_same_reasoning() {
    // Room agents and refused streams parse whole replies, which must keep what a stream would show.
    let reply = |message: Value| json!({"choices":[{"message":message,"finish_reason":"stop"}],"usage":{}});
    for provider in ["groq", "openrouter", "ollama"] {
        let response = parse_openai(reply(json!({"content":"Done","reasoning":"Plan."})), provider).unwrap();
        assert_eq!(response.reasoning_content.as_deref(), Some("Plan."), "{provider}");
    }
    let both = reply(json!({"content":"Done","reasoning_content":"Kept.","reasoning":"Copy."}));
    assert_eq!(parse_openai(both.clone(), "openrouter").unwrap().reasoning_content.as_deref(), Some("Kept."));
    assert_eq!(parse_openai(both, "deepseek").unwrap().reasoning_content.as_deref(), Some("Kept."));
    assert_eq!(parse_openai(reply(json!({"content":"Done","reasoning":"Plan."})), "openai").unwrap().reasoning_content, None);
    assert_eq!(parse_openai(reply(json!({"content":"Done","reasoning":""})), "groq").unwrap().reasoning_content, None);
    let response = parse_anthropic(json!({"content":[
        {"type":"thinking","thinking":"Plan ","signature":"opaque"},{"type":"redacted_thinking","data":"opaque"},
        {"type":"thinking","thinking":"first.","signature":"opaque"},{"type":"text","text":"Done"},
    ],"stop_reason":"end_turn","usage":{}})).unwrap();
    assert_eq!((response.reasoning_content.as_deref(), response.content.as_deref()), (Some("Plan first."), Some("Done")));
    // Claude 5 thinks unasked, and its thinking reads as empty unless a summary was requested.
    let response = parse_anthropic(json!({"content":[{"type":"thinking","thinking":"","signature":"opaque"},{"type":"text","text":"Done"}],
        "stop_reason":"end_turn","usage":{}})).unwrap();
    assert_eq!(response.reasoning_content, None);
}

#[tokio::test]
async fn claude_and_kimi_stream_thinking_without_sampling_or_extra_requests() {
    let anthropic_reply = |text: &str| json!({"content":[{"type":"thinking","thinking":"","signature":"opaque"},{"type":"text","text":text}],
        "stop_reason":"end_turn","usage":{"input_tokens":3,"output_tokens":2}});
    for (provider, model, is_summary_requested) in [("anthropic", "claude-opus-5", true), ("anthropic", "claude-sonnet-5", true),
        ("anthropic", "claude-haiku-4-5", false), ("moonshot-coding", "k3", false)] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let stream = framed(&anthropic_events(&["Plan the ", "timer."], &["```splash\n", "View{}\n```"]), "\r\n");
        let server = serve(listener.try_clone().unwrap(), vec![Reply::Stream(stream)]);
        let mut config = fixture(&format!("http://{address}"), Protocol::Anthropic);
        (config.provider, config.model) = (provider.into(), model.into());
        let (streaming, shown) = previewing(config, Arc::new(AtomicBool::new(true)));
        let response = streaming.chat(&[message(MessageRole::User, "Make a timer.")], &[], &ChatConfig::default()).await.unwrap();
        assert_eq!(joined(&shown.lock().unwrap()), vec![(true, "Plan the timer.".into()), (false, APP.into())], "{model}");
        assert_eq!((response.reasoning_content.as_deref(), response.content.as_deref()), (Some("Plan the timer."), Some(APP)));
        let (line, body) = server.join().unwrap().remove(0);
        assert!(line.starts_with("POST /v1/messages "), "{line}");
        assert_eq!(body["stream"], true);
        // Claude 4.7 and later reject any sampling setting; Kimi's K3 fixes its own.
        assert!(body.get("temperature").is_none(), "{model}: {body}");
        assert_eq!(body.get("thinking").cloned(), is_summary_requested.then(|| json!({"type":"adaptive","display":"summarized"})), "{model}");

        // Room agents get completed replies and no change to what they ask for.
        let server = serve(listener, vec![Reply::Json(200, anthropic_reply("Done"))]);
        let mut config = fixture(&format!("http://{address}"), Protocol::Anthropic);
        (config.provider, config.model) = (provider.into(), model.into());
        let room = GuardedProvider::new(config, approval(Arc::new(AtomicBool::new(true)))).unwrap();
        assert_eq!(room.chat(&[message(MessageRole::User, "Hi")], &[], &ChatConfig::default()).await.unwrap().content.as_deref(), Some("Done"));
        let body = server.join().unwrap().remove(0).1;
        assert!(body.get("thinking").is_none() && body.get("temperature").is_none() && body["stream"] == false, "{model}: {body}");
    }
}

#[test]
fn anthropic_requests_send_sampling_only_where_octos_does() {
    // The agent loop asks for 0.0 by default, which Claude 4.7 and later reject with HTTP 400.
    let body = |model: &str, config: &ChatConfig| {
        let mut resolved = fixture("https://api.anthropic.com/v1", Protocol::Anthropic);
        resolved.model = model.into();
        anthropic_body(&resolved, &[message(MessageRole::User, "Hi")], &[], config)
    };
    let set = ChatConfig { temperature: Some(0.7), ..Default::default() };
    for model in ["claude-opus-5", "claude-sonnet-5", "claude-haiku-4-5", "k3"] {
        assert!(body(model, &ChatConfig::default()).get("temperature").is_none(), "{model}");
        assert!(body(model, &set).get("temperature").is_none(), "{model}");
    }
    // GLM's Anthropic endpoint takes sampling settings, but not the agent's unset 0.0 default.
    assert_eq!(body("glm-5.2", &set)["temperature"].as_f64().map(|value| (value * 10.0).round()), Some(7.0));
    assert!(body("glm-5.2", &ChatConfig::default()).get("temperature").is_none());
}

#[test]
fn only_models_that_think_anyway_are_asked_to_show_it() {
    for (model, is_asked) in [("claude-opus-5", true), ("claude-sonnet-5", true), ("claude-opus-5-5", true), ("claude-haiku-5-5", true),
        ("claude-fable-5-1", true), ("claude-opus-4-8", false), ("claude-sonnet-4-20250514", false), ("claude-haiku-4-5", false),
        ("claude-3-7-sonnet-latest", false), ("k3", false), ("glm-5.2", false)] {
        assert_eq!(is_claude_thinking_by_default(model), is_asked, "{model}");
    }
    for (model, is_asked) in [("gemini-3.8-flash", true), ("models/gemini-2.5-flash-lite", true), ("gemini-3-pro-preview", true),
        ("gemini-2.0-flash", false), ("gemini-1.5-pro", false), ("gemini-flash-latest", false), ("gemma-3-27b-it", false)] {
        assert_eq!(is_gemini_thinking_model(model), is_asked, "{model}");
    }
}

#[tokio::test]
async fn gemini_thinking_models_ask_for_thought_summaries_only_while_streaming() {
    // Thought parts and signed calls rebuild at any split.
    let (shown, value) = rebuilt(Protocol::Gemini, "gemini", &framed(&gemini_events(), "\r\n"));
    assert_eq!(shown, vec![(true, "**Planning**\nA timer with one button.".into()), (false, APP.into())]);
    let response = super::super::gemini::parse(value).unwrap();
    assert_eq!(response.reasoning_content.as_deref(), Some("**Planning**\nA timer with one button."));
    assert_eq!(response.content.as_deref(), Some(APP));
    assert_eq!(response.tool_calls[0].metadata.as_ref().unwrap()["thought_signature"], "c2lnbmVk");

    let plain = json!({"candidates":[{"content":{"parts":[{"text":"Done"}]},"finishReason":"STOP"}],"usageMetadata":{}});
    let provider_for = |address: std::net::SocketAddr, model: &str| {
        let mut config = fixture(&format!("http://{address}/v1beta"), Protocol::Gemini);
        (config.provider, config.model) = ("gemini".into(), model.into());
        config.recipient.endpoint = format!("http://{address}/v1beta/models/{model}:generateContent");
        config
    };
    let thoughts = |body: &Value| body["generationConfig"]["thinkingConfig"]["includeThoughts"].clone();
    for (model, is_asked) in [("gemini-3.8-flash", true), ("models/gemini-2.5-pro", true), ("gemini-2.0-flash", false), ("gemini-flash-latest", false)] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = serve(listener.try_clone().unwrap(), vec![Reply::Stream(framed(&gemini_events(), "\r\n"))]);
        let (streaming, shown) = previewing(provider_for(address, model), Arc::new(AtomicBool::new(true)));
        let response = streaming.chat(&[message(MessageRole::User, "Make a timer.")], &[], &ChatConfig::default()).await.unwrap();
        assert_eq!(joined(&shown.lock().unwrap()), vec![(true, "**Planning**\nA timer with one button.".into()), (false, APP.into())]);
        assert_eq!(response.reasoning_content.as_deref(), Some("**Planning**\nA timer with one button."));
        let (line, body) = server.join().unwrap().remove(0);
        assert!(line.contains(":streamGenerateContent?alt=sse "), "{line}");
        assert_eq!(thoughts(&body), if is_asked { json!(true) } else { Value::Null }, "{model}");

        // A room agent's completed reply asks for nothing more.
        let server = serve(listener, vec![Reply::Json(200, plain.clone())]);
        let room = GuardedProvider::new(provider_for(address, model), approval(Arc::new(AtomicBool::new(true)))).unwrap();
        room.chat(&[message(MessageRole::User, "Hi")], &[], &ChatConfig::default()).await.unwrap();
        assert_eq!(thoughts(&server.join().unwrap()[0].1), Value::Null, "{model}");
    }
    // When the service refuses the streamed request, the plain one is the original request.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = serve(listener, vec![Reply::Json(400, json!({"error":{"message":"Thinking is not supported.","status":"INVALID_ARGUMENT"}})), Reply::Json(200, plain)]);
    let (streaming, shown) = previewing(provider_for(address, "gemini-3.8-flash"), Arc::new(AtomicBool::new(true)));
    assert_eq!(streaming.chat(&[message(MessageRole::User, "Hi")], &[], &ChatConfig::default()).await.unwrap().content.as_deref(), Some("Done"));
    let requests = server.join().unwrap();
    assert_eq!((thoughts(&requests[0].1), thoughts(&requests[1].1)), (json!(true), Value::Null));
    assert!(shown.lock().unwrap().is_empty());
}

/// Runs the pinned agent loop over each provider's stream: what was shown live must be what the agent keeps.
#[cfg(feature = "embedded")]
#[tokio::test]
async fn previews_match_what_the_pinned_agent_loop_keeps() {
    #[derive(Default)]
    struct Kept(std::sync::Mutex<(String, String)>);
    impl octos_agent::ProgressReporter for Kept {
        fn report(&self, event: octos_agent::ProgressEvent) {
            let mut kept = self.0.lock().unwrap();
            match event {
                octos_agent::ProgressEvent::ReasoningChunk { text, .. } => kept.0.push_str(&text),
                octos_agent::ProgressEvent::StreamChunk { text, .. } => kept.1.push_str(&text),
                _ => {}
            }
        }
    }
    let openai = |provider: &str, events: Vec<Value>| (Protocol::OpenAi, provider.to_string(), "test-model".to_string(), done(framed(&events, "\r\n"), "\r\n"));
    let variants = [
        openai("deepseek", vec![delta(json!({"reasoning_content":"Plan the timer."})), delta(json!({"content":APP})), finished("stop")]),
        openai("groq", vec![delta(json!({"reasoning":"Plan the timer."})), delta(json!({"content":APP})), finished("stop")]),
        openai("groq", vec![delta(json!({"content":"<think>Plan the "})), delta(json!({"content":"timer.</think>\n"})), delta(json!({"content":APP})), finished("stop")]),
        openai("openrouter", vec![delta(json!({"reasoning":"Plan the timer.","reasoning_details":[]})), delta(json!({"content":APP})), finished("stop")]),
        (Protocol::Anthropic, "moonshot-coding".into(), "k3".into(), framed(&anthropic_events(&["Plan the ", "timer."], &[APP]), "\r\n")),
        (Protocol::Anthropic, "anthropic".into(), "claude-opus-5".into(), framed(&anthropic_events(&["Plan the timer."], &["```splash\n", "View{}\n```"]), "\r\n")),
        // Without the function call, which this agent has no tool for.
        (Protocol::Gemini, "gemini".into(), "gemini-3.8-flash".into(), framed(&[&gemini_events()[..4],
            &[json!({"candidates":[{"content":{"parts":[]},"finishReason":"STOP"}]})]].concat(), "\r\n")),
    ];
    for (protocol, provider, model, stream) in variants {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = serve(listener, vec![Reply::Stream(stream)]);
        let mut config = fixture(&format!("http://{address}/v1"), protocol);
        if protocol == Protocol::Gemini { config.recipient.endpoint = format!("http://{address}/v1beta/models/{model}:generateContent"); }
        (config.provider, config.model) = (provider.clone(), model);
        let (streaming, shown) = previewing(config, Arc::new(AtomicBool::new(true)));
        let agent = octos_agent::Agent::new(octos_core::AgentId::new("preview-parity"), streaming, octos_agent::ToolRegistry::new(),
            Arc::new(octos_memory::EpisodeStore::in_memory().unwrap()))
            .with_config(octos_agent::AgentConfig { max_iterations: 2, save_episodes: false, ..Default::default() });
        let kept = Arc::new(Kept::default());
        agent.set_reporter(kept.clone());
        let response = agent.process_message("Make a timer.", &[], vec![]).await.unwrap();
        assert_eq!(server.join().unwrap().len(), 1);
        let shown = shown.lock().unwrap();
        let of_kind = |is_thought: bool| shown.iter().filter(|(kind, _)| *kind == is_thought).map(|(_, text)| text.as_str()).collect::<String>();
        let kept = kept.0.lock().unwrap();
        assert_eq!((of_kind(true), of_kind(false)), (kept.0.clone(), kept.1.clone()), "{provider}");
        assert!(["Plan the timer.", "**Planning**\nA timer with one button."].contains(&kept.0.as_str()), "{provider}: {}", kept.0);
        assert_eq!(response.content.trim(), APP, "{provider}");
    }
}
