//! A fake ACP agent for offline tests of the create-app pipeline.
//!
//! Speaks just enough newline-delimited JSON-RPC to stand in for `octos acp`:
//! `initialize` → `session/new` → `session/prompt` with streamed
//! `session/update` chunks and a final `stopReason` reply. Deterministic and
//! LLM-free; the scenario is chosen by what the USER'S REQUEST contains —
//! the tail after the prompt's "User request:" / "User's change request:"
//! marker, never the whole prompt. A refine prompt embeds the app's current
//! source, so scanning everything let an app's own comments hijack the
//! scenario (a mini-app that mentioned refusing a request made every refine
//! come back as a refusal):
//!
//! - contains `"refuse"` → replies with `stopReason: "refusal"`.
//! - contains `"broken"` → first prompt streams an invalid Splash script (the
//!   validator must reject it); the repair prompt (recognized by the phrase
//!   "failed to compile") then streams a valid one.
//! - contains `"slow"` → publishes a plan, streams thinking, fires a burst of
//!   tool calls, then idles (the in-flight console state).
//! - contains `"hopeless"` → streams an invalid script EVERY turn, so the
//!   repair budget runs out and the generation fails (the case Retry
//!   exists for).
//! - contains `"trickle"` → streams thinking, then the valid app line by line
//!   over a few seconds, like a real streaming provider.
//! - contains `"hosted"` → asks the host broker for the model's reply, as
//!   `octos acp --host-managed` does, and relays it. Robrix then streams it from
//!   its configured model service.
//! - anything else → streams a valid pomodoro-ish app, split across two
//!   chunks mid-fence to exercise accumulation.
//!
//! Point the app at it with
//! `ROBRIX_AGENT_CMD=$PWD/target/debug/fake_acp`.

use std::io::{BufRead, Write};

use serde_json::{json, Value};

/// The user's own words: everything after the last request marker the prompt
/// builders emit. Falls back to the whole prompt when neither appears (the
/// repair prompt, which carries no user text and is matched separately).
fn user_request(prompt: &str) -> &str {
    for marker in ["User's change request:", "User request:"] {
        if let Some(at) = prompt.rfind(marker) {
            return &prompt[at + marker.len()..];
        }
    }
    prompt
}

// Explicit indentation: a `\` line continuation would strip it.
const GOOD_APP: &str = "// name: Pomodoro\n// icon: 🍅\n// tint: #E84D3D\n\
let secs = 1500\n\
fn show(){ ui.time.set_text(\"\" + secs) }\n\
View{\n\
\u{20}   width: Fill height: Fit flow: Down spacing: 12 padding: 16\n\
\u{20}   align: Align{x: 0.5}\n\
\u{20}   glass.H1{text: \"Pomodoro\"}\n\
\u{20}   time := Label{text: \"1500\"}\n\
\u{20}   glass.GlassButton{text: \"Tick\" width: 90 height: 44 on_click: || { secs -= 1 show() }}\n\
}";

const BROKEN_APP: &str = "// name: Pomodoro\n// icon: 🍅\n// tint: #E84D3D\n\
DefinitelyNotAWidget{ &&& this does not parse }";

/// What a refine turn returns: the good app with a renamed header + title, so
/// tests can watch the installed app's identity change in place.
const REFINED_APP: &str = "// name: Pomodoro Pro\n// icon: 🍅\n// tint: #E84D3D\n\
// permissions: files\n\
// why-files: Saves your session log where you choose.\n\
let secs = 1500\n\
fn show(){ ui.time.set_text(\"\" + secs) }\n\
View{\n\
\u{20}   width: Fill height: Fit flow: Down spacing: 12 padding: 16\n\
\u{20}   align: Align{x: 0.5}\n\
\u{20}   glass.H1{text: \"Pomodoro Pro\"}\n\
\u{20}   time := Label{text: \"1500\"}\n\
\u{20}   glass.GlassButton{text: \"Tick\" width: 90 height: 44 on_click: || { secs -= 1 show() }}\n\
\u{20}   glass.GlassButton{text: \"Reset\" width: 90 height: 44 on_click: || { secs = 1500 show() }}\n\
}";

fn send(out: &mut impl Write, v: Value) {
    let mut line = v.to_string();
    line.push('\n');
    out.write_all(line.as_bytes()).unwrap();
    out.flush().unwrap();
}

fn chunk(out: &mut impl Write, session_id: &str, text: &str) {
    update(out, session_id, "agent_message_chunk", text);
}

fn thought(out: &mut impl Write, session_id: &str, text: &str) {
    update(out, session_id, "agent_thought_chunk", text);
}

fn update(out: &mut impl Write, session_id: &str, kind: &str, text: &str) {
    send(
        out,
        json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": session_id,
                "update": {
                    "sessionUpdate": kind,
                    "content": {"type": "text", "text": text},
                },
            },
        }),
    );
}

/// The capability the protected host broker requires, once confinement held.
/// Robrix runs `ROBRIX_AGENT_CMD` as `<cmd> acp --host-managed` for every generation.
fn host_managed_capability() -> Option<Value> {
    if !std::env::args().any(|arg| arg == "--host-managed") {
        return None;
    }
    let sandbox = octos_sandbox::confine_host_managed().ok()?;
    Some(json!({"octos.hostManaged": {
        "version": octos_llm::host::VERSION, "confined": true, "sandbox": sandbox.platform,
    }}))
}

/// Asks the host broker for one model reply to `prompt`, as a confined octos
/// worker does, and returns its `(reasoning, content)`.
fn host_model_reply(
    lines: &mut impl Iterator<Item = String>,
    out: &mut impl Write,
    prompt: &str,
) -> Result<(String, String), String> {
    let request = octos_llm::host::ModelRequest {
        messages: vec![octos_core::Message::user(prompt)],
        tools: Vec::new(),
        config: octos_llm::ChatConfig::default(),
    };
    send(out, json!({"jsonrpc": "2.0", "id": "host-model-1", "method": octos_llm::host::MODEL_METHOD, "params": request}));
    for line in lines {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        if msg.get("id") != Some(&json!("host-model-1")) {
            continue;
        }
        if let Some(error) = msg.get("error") {
            return Err(error["message"].as_str().unwrap_or("host model request failed").to_string());
        }
        let text = |key: &str| msg["result"][key].as_str().unwrap_or_default().to_string();
        return Ok((text("reasoning_content"), text("content")));
    }
    Err("the host closed the connection".into())
}

fn main() {
    let host_managed = host_managed_capability();
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let session_id = "fake-session-1";
    // Sticky per-session: once a refine prompt is seen, its REPAIR turns must
    // also answer with the refined identity (a real agent keeps the session's
    // context; answering a refine-repair with the create app would silently
    // pass the wrong manifest through the update path).
    let mut refine_session = false;
    let mut hopeless_session = false;

    let mut lines = stdin.lock().lines().map_while(Result::ok);
    while let Some(line) = lines.next() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        match method {
            "initialize" => {
                let mut capabilities = json!({});
                if let Some(meta) = &host_managed {
                    capabilities["_meta"] = meta.clone();
                }
                send(
                    &mut stdout,
                    json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": {"protocolVersion": 1, "agentCapabilities": capabilities},
                    }),
                );
            }
            "session/new" => send(
                &mut stdout,
                json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {"sessionId": session_id},
                }),
            ),
            "session/prompt" => {
                let text = msg
                    .pointer("/params/prompt/0/text")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                // Scenario keywords come from the user's words only; `text`
                // stays available for structural checks on the whole prompt.
                let ask = user_request(text);
                if ask.contains("hosted") {
                    let (reply, stop) = match host_model_reply(&mut lines, &mut stdout, text) {
                        Ok((reasoning, content)) => {
                            if !reasoning.is_empty() {
                                thought(&mut stdout, session_id, &reasoning);
                            }
                            chunk(&mut stdout, session_id, &content);
                            (json!({"stopReason": "end_turn"}), None)
                        }
                        Err(error) => (Value::Null, Some(error)),
                    };
                    match stop {
                        None => send(&mut stdout, json!({"jsonrpc": "2.0", "id": id, "result": reply})),
                        Some(error) => send(&mut stdout, json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": error}})),
                    }
                    continue;
                }
                if ask.contains("refuse") {
                    send(
                        &mut stdout,
                        json!({
                            "jsonrpc": "2.0", "id": id,
                            "result": {"stopReason": "refusal"},
                        }),
                    );
                    continue;
                }
                // "slow" requests idle first, giving UI tests a window to
                // interact with the in-flight state (the console, Stop). The
                // burst of tool calls first fills the console well past what
                // fits, so tests can check it keeps the whole run rather than
                // the last few lines.
                if ask.contains("slow") {
                    // A plan and some thinking first — the shapes a real
                    // reasoning agent emits during the long opening stretch,
                    // and the ones the console has to render.
                    send(
                        &mut stdout,
                        json!({
                            "jsonrpc": "2.0",
                            "method": "session/update",
                            "params": {
                                "sessionId": session_id,
                                "update": {
                                    "sessionUpdate": "plan",
                                    "entries": [
                                        {"content": "Read the Splash guide", "status": "in_progress", "priority": "medium"},
                                        {"content": "Write the app", "status": "pending", "priority": "medium"},
                                    ],
                                },
                            },
                        }),
                    );
                    for part in ["Let me think about ", "the timer layout."] {
                        thought(&mut stdout, session_id, part);
                    }
                    for i in 1 ..= 20 {
                        send(
                            &mut stdout,
                            json!({
                                "jsonrpc": "2.0",
                                "method": "session/update",
                                "params": {
                                    "sessionId": session_id,
                                    "update": {
                                        "sessionUpdate": "tool_call",
                                        "toolCallId": format!("call-{i}"),
                                        "title": format!("Read splash_guide.md ({i})"),
                                        "status": "completed",
                                    },
                                },
                            }),
                        );
                    }
                    // Long enough for a test to walk the whole console UI
                    // while the run is genuinely in flight — each assertion
                    // costs a widget-snapshot round trip, and those get slow
                    // under a loaded full-suite run. Tests end such a run with
                    // Stop rather than waiting this out.
                    std::thread::sleep(std::time::Duration::from_secs(120));
                }

                // Before answering, fire an agent-originated request with a
                // STRING id (legal JSON-RPC) — the client must refuse it and
                // carry on; a client that drops it would leave a real agent
                // hanging. The reply itself is read below and discarded.
                send(
                    &mut stdout,
                    json!({
                        "jsonrpc": "2.0", "id": "agent-req-1",
                        "method": "session/request_permission",
                        "params": {"sessionId": session_id},
                    }),
                );

                // The repair prompt is recognizable by the pipeline's own
                // copy; a refine prompt by its current-source section.
                let is_repair = text.contains("failed to compile");
                if text.contains("The app's current source") {
                    refine_session = true;
                }
                // "hopeless" never repairs: the repair budget runs out and
                // the generation fails for real. The marker only survives in
                // the FIRST prompt, so the session remembers it.
                if ask.contains("hopeless") {
                    hopeless_session = true;
                }
                // A hopeless run has to be *observably* in flight: it's the
                // Retry test's subject, and a run that starts and fails
                // between two snapshot polls can't be told from one that never
                // started. Three turns at this pace keep the console on screen
                // for a few seconds, which survives a loaded full-suite run.
                if hopeless_session {
                    std::thread::sleep(std::time::Duration::from_millis(1200));
                }
                let source = if hopeless_session {
                    BROKEN_APP
                } else if refine_session {
                    REFINED_APP
                } else if ask.contains("broken") && !is_repair {
                    BROKEN_APP
                } else {
                    GOOD_APP
                };
                let reply = format!("```splash\n{source}\n```");
                if ask.contains("trickle") {
                    // Thinking, then the reply line by line, paced like a real stream.
                    let pause = std::time::Duration::from_millis(150);
                    for part in ["Planning a timer ", "with one button ", "and a big label."] {
                        thought(&mut stdout, session_id, part);
                        std::thread::sleep(pause);
                    }
                    for line in reply.split_inclusive('\n') {
                        chunk(&mut stdout, session_id, line);
                        std::thread::sleep(pause);
                    }
                } else {
                    // Split mid-reply so the client has to accumulate chunks.
                    let mid = reply.len() / 2;
                    let mid = (mid..reply.len())
                        .find(|&i| reply.is_char_boundary(i))
                        .unwrap_or(reply.len());
                    chunk(&mut stdout, session_id, &reply[..mid]);
                    chunk(&mut stdout, session_id, &reply[mid..]);
                }
                send(
                    &mut stdout,
                    json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": {"stopReason": "end_turn"},
                    }),
                );
            }
            // No method: it's the client's RESPONSE to our agent-originated
            // request above — consume silently (replying to a reply is a
            // JSON-RPC violation).
            "" => {}
            // Notifications (session/cancel) and unknown requests: unknown
            // *requests* still get a reply so nothing hangs.
            _ => {
                if !id.is_null() {
                    send(
                        &mut stdout,
                        json!({
                            "jsonrpc": "2.0", "id": id,
                            "error": {"code": -32601, "message": "not supported"},
                        }),
                    );
                }
            }
        }
    }
}
