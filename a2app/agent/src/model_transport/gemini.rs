//! Google's native content API preserves image, document, audio and video
//! bytes, plus function signatures across tool-feedback requests.
use octos_core::{MessageRole, ToolCall};
use octos_llm::{ChatConfig, ChatResponse, StopReason, ToolChoice, ToolSpec, TokenUsage};
use serde_json::{Value, json};
use super::multimodal::{Turn, Format, parts};

pub(super) fn body(turns: &[Turn], tools: &[ToolSpec], config: &ChatConfig) -> Value {
    let mut system = Vec::new();
    let mut contents: Vec<Value> = Vec::new();
    let mut names = std::collections::HashMap::new();
    for turn in turns {
        let message = &turn.message;
        if message.role == MessageRole::System { system.push(message.content.clone()); continue; }
        let role = if message.role == MessageRole::Assistant { "model" } else { "user" };
        let mut content = Vec::new();
        if message.role == MessageRole::Tool {
            let id = message.tool_call_id.as_deref().unwrap_or("");
            let (name, provider_id) = names.get(id).copied().unwrap_or(("host_tool",None));
            let mut response = json!({"functionResponse":{"name":name,"response":{"result":message.content}}});
            if let Some(id) = provider_id { response["functionResponse"]["id"] = json!(id); }
            content.push(response);
        } else {
            if !message.content.is_empty() { content.push(json!({"text":message.content})); }
            for call in message.tool_calls.iter().flatten() {
                let provider_id = call.metadata.as_ref().and_then(|metadata| metadata["gemini_call_id"].as_str());
                names.insert(call.id.as_str(), (call.name.as_str(),provider_id));
                let mut part = json!({"functionCall":{"name":call.name,"args":call.arguments}});
                if let Some(id) = provider_id { part["functionCall"]["id"] = json!(id); }
                if let Some(signature) = call.metadata.as_ref().and_then(|metadata| metadata["thought_signature"].as_str()) {
                    part["thoughtSignature"] = json!(signature);
                }
                content.push(part);
            }
        }
        content.extend(parts(&turn.attachments, Format::Gemini, "Gemini"));
        if let Some(previous) = contents.last_mut().filter(|previous| previous["role"] == role) {
            previous["parts"].as_array_mut().unwrap().extend(content);
        } else { contents.push(json!({"role":role,"parts":content})); }
    }
    let mut value = json!({"contents":contents});
    if !system.is_empty() { value["systemInstruction"] = json!({"parts":[{"text":system.join("\n\n")}]}); }
    let mut generation = json!({});
    if let Some(max) = config.max_tokens { generation["maxOutputTokens"] = json!(max); }
    if let Some(temperature) = config.temperature { generation["temperature"] = json!(temperature); }
    if !config.stop_sequences.is_empty() { generation["stopSequences"] = json!(config.stop_sequences); }
    value["generationConfig"] = generation;
    if !tools.is_empty() {
        value["tools"] = json!([{"functionDeclarations":tools.iter().map(|tool| json!({"name":tool.name,"description":tool.description,"parametersJsonSchema":tool.input_schema})).collect::<Vec<_>>()}]);
        value["toolConfig"] = json!({"functionCallingConfig":match &config.tool_choice {
            ToolChoice::Auto => json!({"mode":"AUTO"}), ToolChoice::None => json!({"mode":"NONE"}),
            ToolChoice::Required => json!({"mode":"ANY"}), ToolChoice::Specific { name } => json!({"mode":"ANY","allowedFunctionNames":[name]}),
        }});
    }
    value
}

pub(super) fn parse(value: Value) -> eyre::Result<ChatResponse> {
    let candidate = value["candidates"].get(0).ok_or_else(|| eyre::eyre!("Gemini returned no response candidate. Check model input capabilities and safety restrictions."))?;
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut calls = Vec::new();
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).map_err(|_| eyre::eyre!("Cannot identify Gemini tool requests."))?;
    let prefix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    for part in candidate["content"]["parts"].as_array().into_iter().flatten() {
        if let Some(chunk) = part["text"].as_str() {
            if part["thought"].as_bool() == Some(true) { reasoning.push_str(chunk); } else { text.push_str(chunk); }
        }
        if let Some(name) = part["functionCall"]["name"].as_str() {
            let arguments = part["functionCall"].get("args").cloned().unwrap_or_else(|| json!({}));
            if !arguments.is_object() { return Err(eyre::eyre!("Gemini returned malformed host tool arguments.")); }
            let mut metadata = serde_json::Map::new();
            if let Some(signature) = part["thoughtSignature"].as_str() { metadata.insert("thought_signature".into(),json!(signature)); }
            if let Some(id) = part["functionCall"]["id"].as_str() { metadata.insert("gemini_call_id".into(),json!(id)); }
            calls.push(ToolCall { id: format!("gemini-{prefix}-{}", calls.len()), name: name.into(), arguments,
                metadata: (!metadata.is_empty()).then_some(Value::Object(metadata)) });
        }
    }
    let usage = &value["usageMetadata"];
    Ok(ChatResponse { content: (!text.is_empty()).then_some(text), reasoning_content: (!reasoning.is_empty()).then_some(reasoning),
        stop_reason: if !calls.is_empty() { StopReason::ToolUse } else { match candidate["finishReason"].as_str() {
            Some("MAX_TOKENS") => StopReason::MaxTokens, Some("SAFETY" | "RECITATION" | "PROHIBITED_CONTENT") => StopReason::ContentFiltered, _ => StopReason::EndTurn,
        } }, tool_calls: calls, provider_index: None,
        usage: TokenUsage { input_tokens: usage["promptTokenCount"].as_u64().unwrap_or(0) as u32,
            output_tokens: usage["candidatesTokenCount"].as_u64().unwrap_or(0) as u32,
            cache_read_tokens: usage["cachedContentTokenCount"].as_u64().unwrap_or(0) as u32, ..Default::default() } })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn no_argument_calls_and_native_ids_round_trip_without_reusing_local_ids() {
        let response = parse(json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"room_info","id":"provider-call"},"thoughtSignature":"signed"}]}}]})).unwrap();
        assert_eq!(response.tool_calls[0].arguments,json!({}));
        let id = response.tool_calls[0].id.clone();
        assert_ne!(id,"provider-call");
        let mut assistant = octos_core::Message::assistant(""); assistant.tool_calls = Some(response.tool_calls);
        let mut feedback = octos_core::Message::user("Actual host result"); feedback.role=MessageRole::Tool; feedback.tool_call_id=Some(id);
        let value = body(&[Turn {message:assistant,attachments:vec![]},Turn {message:feedback,attachments:vec![]}],&[],&ChatConfig::default());
        assert_eq!(value["contents"][0]["parts"][0]["functionCall"]["id"],"provider-call");
        assert_eq!(value["contents"][0]["parts"][0]["thoughtSignature"],"signed");
        assert_eq!(value["contents"][1]["parts"][0]["functionResponse"]["id"],"provider-call");
        for args in [Value::Null,json!([]),json!("bad")] {
            assert!(parse(json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"room_info","args":args}}]}}]})).is_err());
        }
    }

    #[test]
    fn full_host_json_schema_uses_native_json_schema_field() {
        let schema = json!({"type":"object","properties":{"draft_id":{"type":"string"},"event_id":{"type":"string"}},
            "oneOf":[{"required":["draft_id"]},{"required":["event_id"]}],"additionalProperties":false});
        let tool = ToolSpec { name:"inspect_media".into(), description:"Inspect authorized content".into(), input_schema:schema.clone() };
        let body = body(&[], &[tool], &ChatConfig::default());
        let declaration = &body["tools"][0]["functionDeclarations"][0];
        assert_eq!(declaration["parametersJsonSchema"], schema);
        assert!(declaration.get("parameters").is_none(), "typed OpenAPI parameters cannot carry our oneOf and additionalProperties constraints");
    }
}
