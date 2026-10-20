//! Actual Claude tool selection, using public web data and synthetic room data.
//! Native composer/Matrix effects are captured here; SDK round trips are tested
//! separately in Robrix, without using a signed-in Matrix account.
use super::*;
use std::{collections::HashMap, sync::Mutex, time::{Duration, Instant}};
use crate::mcp::{McpServer, Tool};
use crate::tool_output::{ModelAttachment, ToolOutput};
use a2app_core::information_flow as flow;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

const EVENT: &str = "$earlier-public-photo";
const ROOM: &str = "!public-photo-workflow:example.invalid";

#[derive(Default)]
struct State {
    calls: Vec<String>,
    found: Vec<Value>,
    drafts: HashMap<String, ModelAttachment>,
    originals: Vec<[u8; 32]>,
    requested_capabilities: Vec<String>,
    inspected: Vec<[u8; 32]>,
    staged: Vec<[u8; 32]>,
    posted: Vec<[u8; 32]>,
    allow_post: bool,
}

struct Fixture { context: flow::ContextId, epoch: u64, state: Mutex<State> }
impl Fixture {
    fn new(name: &str) -> Arc<Self> {
        let context = flow::ContextId::Agent { account: format!("@{name}:example.invalid"), room: ROOM.into() };
        flow::register_context(&context).unwrap();
        Arc::new(Self { epoch: flow::context_epoch(&context).unwrap(), context, state: Mutex::new(State::default()) })
    }
    fn registry(self: &Arc<Self>) -> McpServer {
        let registry = McpServer::new();
        for name in ["search_media", "read_room_messages", "room_info", "list_rooms", "request_task_permissions", "draft_media", "inspect_media", "attach_media", "post_room_media"] {
            registry.add_tool(FixtureTool { name, host: self.clone() });
        }
        registry
    }
    fn call(&self, name: &str, args: &Map<String, Value>) -> Result<ToolOutput, String> {
        flow::ensure_context_epoch(&self.context, self.epoch)?;
        self.state.lock().unwrap().calls.push(name.into());
        eprintln!("Actual Claude workflow selected host tool: {name}");
        let text = |value: Value| Ok(ToolOutput { text:value.to_string(),attachments:vec![] });
        match name {
            "room_info" => text(json!({"room_id":ROOM,"name":"Public photo test","encrypted":true})),
            "list_rooms" => text(json!({"rooms":[{"room_id":ROOM,"name":"Public photo test"}]})),
            "request_task_permissions" => {
                let allow_post=self.state.lock().unwrap().allow_post;
                let needs=args.get("needs").and_then(Value::as_array).ok_or("Task needs must be an array.")?;
                let mut granted=Vec::new(); let mut not_granted=Vec::new();
                for need in needs {
                    let id=need["id"].as_str().ok_or("Each need requires an id.")?;
                    if let Some(capability)=need["capability"].as_str() { self.state.lock().unwrap().requested_capabilities.push(capability.into()); }
                    let sends=matches!(need["capability"].as_str(),Some("matrix.media.upload"|"matrix.media.send"|"matrix.rooms.message.send"));
                    if sends && !allow_post { not_granted.push(json!({"id":id,"reason":"declined"})); }
                    else { granted.push(id); }
                }
                text(json!({"task_id":"public-fixture-task","status":if not_granted.is_empty(){"granted"}else if granted.is_empty(){"declined"}else{"partial"},
                    "granted":granted,"not_granted":not_granted,"flow_rules":{"applied":0,"skipped":0},"lasts":"until this turn ends"}))
            }
            "read_room_messages" => text(json!({"messages":[{
                "event_id":EVENT,"room_id":ROOM,"sender":"Public fixture","sender_id":"@fixture:example.invalid",
                "body":"Earlier test image","ts":1700000000000u64,"unread":false,"msgtype":"m.image",
                "media":{"uri":"mxc://example.invalid/public-fixture","filename":"earlier.jpg","mime_type":"image/jpeg",
                    "size":include_bytes!("../../tests/fixtures/vision-colors.jpg").len(),"width":256,"height":256,"duration_ms":null,"encrypted":true}
            }],"unread_count":0})),
            "search_media" => {
                let query = required(args,"query")?;
                let found = public_search(query)?;
                self.state.lock().unwrap().found = found.clone();
                text(json!({"query":query,"kind":"image","provider":"Wikimedia Commons","results":found,
                    "next":"Choose a result; draft_media downloads its url or its smaller preview_url. Inspect its actual pixels before describing it. Keep source attribution."}))
            }
            "draft_media" => {
                let filename = required(args,"filename")?;
                let mime = required(args,"mime_type")?;
                let (bytes, original) = if let Some(event) = args.get("event_id") {
                    if event != EVENT || args.get("source_room").is_some_and(|room| room != ROOM) { return Err("Unknown fixture attachment event.".into()); }
                    if mime != "image/jpeg" { return Err("Existing event has trusted image/jpeg content.".into()); }
                    (include_bytes!("../../tests/fixtures/vision-colors.jpg").to_vec(),false)
                } else {
                    let url = required(args,"url")?;
                    let found = self.state.lock().unwrap().found.clone();
                    if !found.iter().any(|entry| (entry["url"] == url || entry["preview_url"] == url) && entry["mime_type"] == mime) {
                        return Err("Only actual URLs/MIME types returned by this search can be downloaded.".into());
                    }
                    (public_download(url)?,found.iter().any(|entry|entry["url"]==url))
                };
                let media = ModelAttachment::new(bytes, mime.into(), filename.into())?;
                let id = format!("media_{:x}", u128::from_le_bytes(random_id()));
                let metadata = metadata(&media);
                let mut state=self.state.lock().unwrap();
                if original { state.originals.push(digest(&media)); }
                state.drafts.insert(id.clone(), media);
                text(json!({"draft_id":id,"status":"drafted","media":metadata}))
            }
            "inspect_media" => {
                let media = if let Some(event) = args.get("event_id") {
                    if event != EVENT || args.get("source_room").is_some_and(|room| room != ROOM) { return Err("Unknown fixture attachment event.".into()); }
                    ModelAttachment::new(include_bytes!("../../tests/fixtures/vision-colors.jpg").to_vec(),"image/jpeg".into(),"earlier.jpg".into())?
                } else { self.draft(required(args,"draft_id")?)? };
                self.state.lock().unwrap().inspected.push(digest(&media));
                Ok(ToolOutput { text:json!({"status":"inspected","media":metadata(&media)}).to_string(), attachments:vec![media] })
            }
            "attach_media" | "post_room_media" => {
                let media = self.draft(required(args,"draft_id")?)?;
                if args.get("room").is_some_and(|room| room != ROOM) { return Err("Only the synthetic test room is available.".into()); }
                let mut state = self.state.lock().unwrap();
                if name == "attach_media" {
                    state.staged.push(digest(&media));
                    text(json!({"status":"attached","media":metadata(&media)}))
                } else {
                    if !state.allow_post { return Err("Sending is denied during the draft-only turn.".into()); }
                    state.posted.push(digest(&media));
                    text(json!({"status":"posted","event_id":"$fixture-native-post","media":metadata(&media)}))
                }
            }
            _ => Err("Unknown fixture tool.".into()),
        }
    }
    fn draft(&self, id: &str) -> Result<ModelAttachment,String> {
        self.state.lock().unwrap().drafts.get(id).cloned().ok_or_else(|| "Unknown draft id; use the host's returned id.".into())
    }
}
impl Drop for Fixture { fn drop(&mut self) { let _ = flow::remove_context_for_activation(&self.context,self.epoch); } }

fn required<'a>(args: &'a Map<String, Value>, name: &str) -> Result<&'a str,String> {
    args.get(name).and_then(Value::as_str).filter(|value|!value.is_empty()).ok_or_else(||format!("Missing {name}."))
}
fn random_id() -> [u8;16] { let mut bytes=[0;16]; getrandom::fill(&mut bytes).unwrap(); bytes }
fn digest(media: &ModelAttachment) -> [u8;32] { Sha256::digest(media.bytes()).into() }
fn metadata(media: &ModelAttachment) -> Value { json!({"filename":media.filename(),"mime_type":media.mime_type(),"size":media.bytes().len(),"sha256":format!("{:x}",Sha256::digest(media.bytes()))}) }

struct FixtureTool { name: &'static str, host: Arc<Fixture> }
impl Tool for FixtureTool {
    fn name(&self) -> &str { self.name }
    fn description(&self) -> &str {
        match self.name {
            "search_media" => "Find actual public photos on Wikimedia Commons. Returns source pages, direct download URLs, smaller preview URLs, filenames, MIME types and attribution. Use draft_media to download, inspect_media to see the actual pixels, then attach_media for user review or post_room_media with separate send permission. Never guess URLs.",
            "read_room_messages" => "Read recent messages in this room, with attachment metadata and event ids. Use inspect_media with event_id to see actual contents, or draft_media to prepare an existing attachment for attaching or posting.",
            "room_info" => "Read this room's name and id.",
            "list_rooms" => "List joined rooms and their exact ids.",
            "request_task_permissions" => "Request known task permissions with task, explanation and needs (capability, website or app_tool). The fixture user approves only these isolated synthetic/public operations.",
            "draft_media" => "Prepare bytes locally from exactly one complete HTTP(S) url or an existing attachment event_id, plus filename and mime_type; optional source_room/caption. Returns an opaque draft_id; nothing uploaded or posted. Use inspect_media to see contents, attach_media to stage or post_room_media to send.",
            "inspect_media" => "Show actual authorized contents to the configured model. Use a draft_id or event_id with optional source_room. Images arrive as native model inputs. Does not attach/upload/post.",
            "attach_media" => "Put a returned draft_id in this room's composer for the user to review and press Send. Requires composer permission independently of uploading/sending. Does not upload or post.",
            "post_room_media" => "Upload and post a prepared draft_id as a native Matrix attachment. Omit room for here. Upload/send/sharing approvals are separate from staging; only explicit posting is permitted.",
            _ => unreachable!(),
        }
    }
    fn input_schema(&self) -> Value {
        let (properties, required) = match self.name {
            "search_media" => (json!({"query":{"type":"string"},"kind":{"type":"string","enum":["image","audio","video","file"]},"limit":{"type":"integer","minimum":1,"maximum":10}}),vec!["query"]),
            "read_room_messages" => (json!({"limit":{"type":"integer","description":"How many messages to return (default 15, max 50)."}}),vec![]),
            "request_task_permissions" => (json!({"task":{"type":"string"},"explanation":{"type":"string"},"needs":{"type":"array","items":{"type":"object","properties":{"id":{"type":"string"},"kind":{"type":"string"},"capability":{"type":"string"},"targets":{"type":"array","items":{"type":"string"}},"url":{"type":"string"},"tool":{"type":"string"},"why":{"type":"string"}},"required":["id","kind"],"additionalProperties":false}}}),vec!["task","needs"]),
            "draft_media" => (json!({"url":{"type":"string"},"event_id":{"type":"string"},"source_room":{"type":"string"},"filename":{"type":"string"},"mime_type":{"type":"string"},"caption":{"type":"string"}}),vec!["filename","mime_type"]),
            "inspect_media" => (json!({"draft_id":{"type":"string"},"event_id":{"type":"string"},"source_room":{"type":"string"}}),vec![]),
            "attach_media" => (json!({"draft_id":{"type":"string"}}),vec!["draft_id"]),
            "post_room_media" => (json!({"draft_id":{"type":"string"},"room":{"type":"string"}}),vec!["draft_id"]),
            _ => (json!({}),vec![]),
        };
        let mut schema=json!({"type":"object","properties":properties,"required":required,"additionalProperties":false});
        if self.name=="draft_media" { schema["oneOf"]=json!([{"required":["url"]},{"required":["event_id"]}]); }
        if self.name=="inspect_media" { schema["oneOf"]=json!([{"required":["draft_id"]},{"required":["event_id"]}]); }
        schema
    }
    fn call(&self,args:&Map<String,Value>)->Result<String,String> { self.host.call(self.name,args).map(|output|output.text) }
    fn call_output(&self,args:&Map<String,Value>)->Result<ToolOutput,String> { self.host.call(self.name,args) }
}

fn public_bytes(url: &str) -> Result<Vec<u8>,String> {
    let url=url::Url::parse(url).map_err(|_|"Invalid public URL.")?;
    if url.scheme()!="https" || !matches!(url.host_str(),Some("commons.wikimedia.org"|"upload.wikimedia.org"|"thumb.wikimedia.org"))
        || !url.username().is_empty() || url.password().is_some() { return Err("Public fixture permits only Wikimedia HTTPS URLs.".into()); }
    tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|error|error.to_string())?.block_on(async {
        let client=reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).no_proxy()
            .user_agent("Robrix public attachment test (https://github.com/project-robius/robrix)")
            .timeout(Duration::from_secs(30)).build().map_err(|error|error.to_string())?;
        let mut response=client.get(url).send().await.map_err(|error|error.to_string())?.error_for_status().map_err(|error|error.to_string())?;
        let mut bytes=Vec::new();
        while let Some(chunk)=response.chunk().await.map_err(|error|error.to_string())? {
            if bytes.len()+chunk.len()>16*1024*1024 { return Err("Public fixture response exceeds 16 MiB.".into()); }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    })
}
fn public_download(url: &str) -> Result<Vec<u8>,String> { public_bytes(url) }
fn public_search(query: &str) -> Result<Vec<Value>,String> {
    let mut url=url::Url::parse("https://commons.wikimedia.org/w/api.php").unwrap();
    url.query_pairs_mut().extend_pairs([("action","query"),("format","json"),("formatversion","2"),("generator","search"),("gsrnamespace","6"),
        ("gsrsearch",&format!("{query} filetype:bitmap")),("gsrlimit","3"),("prop","imageinfo"),("iiprop","url|mime|size|extmetadata"),("iiurlwidth","1024")]);
    let value:Value=serde_json::from_slice(&public_bytes(url.as_str())?).map_err(|error|error.to_string())?;
    let pages=value["query"]["pages"].as_array().ok_or("Public search returned no photos.")?;
    let mut results:Vec<_>=pages.iter().filter_map(|page| {
        let info=page["imageinfo"].as_array()?.first()?;
        Some(json!({"index":page["index"],"title":page["title"],"filename":page["title"].as_str()?.strip_prefix("File:")?,"url":info["url"],
            "preview_url":info["thumburl"],"mime_type":info["mime"],"size":info["size"],"source_page":info["descriptionurl"],
            "license":info["extmetadata"]["LicenseShortName"]["value"],"artist_html":info["extmetadata"]["Artist"]["value"]}))
    }).collect();
    results.sort_by_key(|result|result["index"].as_u64().unwrap_or(u64::MAX));
    if results.is_empty() { return Err("Public search returned no photos.".into()); }
    Ok(results)
}

struct ClaudeSelection(Option<String>);
impl ClaudeSelection { fn new()->Self { let old=crate::providers::session_provider(); crate::providers::set_session(crate::claude_code::ID); Self(old) } }
impl Drop for ClaudeSelection { fn drop(&mut self) { match &self.0 { Some(provider)=>crate::providers::set_session(provider),None=>crate::providers::clear_session() } } }

async fn run_workflow(name:&str,prompt:&str,web:bool,original:bool) {
    let _selection=ClaudeSelection::new();
    let prefs=AgentPrefs::default();
    let deadline=Instant::now()+Duration::from_secs(15);
    loop {
        match crate::model_transport::current_recipient(&prefs) {
            Ok(_)=>break,
            Err(error) if error=="Checking Claude Code sign-in. Try again shortly." && Instant::now()<deadline=>tokio::time::sleep(Duration::from_millis(250)).await,
            Err(error)=>panic!("Configure Claude Code before this ignored public test: {error}"),
        }
    }
    let fixture=Fixture::new(name);
    let shutdown=Arc::new(Shutdown::default());
    let generation=shutdown.begin_prompt();
    let (previous,host)=build_protected_agent(&prefs,&shutdown,true,fixture.context.clone(),Some(fixture.registry()),Default::default()).await.unwrap();
    let system=previous.system_prompt_snapshot();
    assert!(system.contains(crate::ROOM_AGENT_GUIDE),"Actual room agent must receive the room role and media guide.");
    assert!(!system.contains(crate::SPLASH_GUIDE),"Mini-app generation instructions must not replace the room agent's role.");
    let host=host.unwrap();
    let agent=host.agent_for_turn(&previous,generation).unwrap();
    let (sender,receiver)=std::sync::mpsc::channel(); let mut history=Vec::new();
    run_turn(&agent,&shutdown,&sender,&mut history,prompt,generation).await;
    let events:Vec<_>=receiver.try_iter().collect();
    assert!(!events.iter().any(|event|matches!(event,AcpEvent::Error(_))),"Claude draft workflow failed: {events:?}");
    let answer=events.iter().find_map(|event|if let AcpEvent::TurnDone{text,..}=event {Some(text)} else {None}).expect("No final draft answer.");
    {
        let state=fixture.state.lock().unwrap();
        assert!(state.calls.iter().any(|name|name==if web {"search_media"} else {"read_room_messages"}),"Claude did not independently discover a photo: {:?}",state.calls);
        for name in ["draft_media","inspect_media","attach_media"] { assert!(state.calls.iter().any(|call|call==name),"Claude omitted {name}: {:?}; requested capabilities: {:?}; final answer: {answer}; reporter events: {events:?}",state.calls,state.requested_capabilities); }
        assert!(!state.calls.iter().any(|name|name=="post_room_media"),"A draft-only request must not try sending.");
        assert_eq!(state.staged.len(),1);
        assert!(state.inspected.contains(&state.staged[0]),"The staged photo must be the bytes Claude inspected.");
        let staged=state.drafts.values().find(|media|digest(media)==state.staged[0]).unwrap();
        if original {
            assert!(state.originals.contains(&state.staged[0]),"Use the actual returned original URL, not a thumbnail.");
            assert!(staged.bytes().len()>7*1024*1024,"The original image must exercise the large-image model path.");
        }
        assert!(state.posted.is_empty());
        if !web {
            let normalized=answer.to_lowercase();
            let words:Vec<_>=normalized.split(|character:char|!character.is_alphabetic()).collect();
            let positions=["red","green","blue","yellow"].map(|color|words.iter().position(|word|*word==color).unwrap_or_else(||panic!("Claude did not identify actual JPEG pixels: {answer}")));
            assert!(positions.windows(2).all(|positions|positions[0]<positions[1]),"Claude reported the wrong quadrant order: {answer}");
        }
    }
    fixture.state.lock().unwrap().allow_post=true;
    let generation=shutdown.begin_prompt();
    let agent=host.agent_for_turn(&agent,generation).unwrap();
    run_turn(&agent,&shutdown,&sender,&mut history,"Now post that prepared photo into this room.",generation).await;
    let events:Vec<_>=receiver.try_iter().collect();
    assert!(!events.iter().any(|event|matches!(event,AcpEvent::Error(_))),"Claude explicit post workflow failed: {events:?}");
    let state=fixture.state.lock().unwrap();
    assert_eq!(state.posted,state.staged,"Explicit post must deliver the very same staged bytes.");
    eprintln!("Actual Claude independently selected tool sequence: {:?}",state.calls);
    let staged=state.drafts.values().find(|media|digest(media)==state.staged[0]).unwrap();
    eprintln!("Actual Claude workflow verified: {} → prepare → inspect → stage; explicit second-turn post preserved {} bytes and SHA-256. Host effects use an isolated fixture, not your Matrix account.",if original {"full-size original public JPEG"} else if web {"public web search/download"} else {"prior synthetic room JPEG"},staged.bytes().len());
}

#[tokio::test]
#[ignore="Calls configured Claude Code and public Wikimedia; isolated native effects, no Matrix account"]
async fn live_claude_finds_inspects_stages_then_posts_a_public_photo() {
    run_workflow("public-web-photo","Find a real HZMB Golden Bus photo. Use a small preview suitable for chat, look at the actual image to make sure it shows a bus, then put it in my composer for review. Do not send it.",true,false).await;
}

#[tokio::test]
#[ignore="Calls configured Claude Code with a large public original photo; isolated effects, no Matrix account"]
async fn live_claude_finds_inspects_stages_then_posts_a_full_size_original_photo() {
    run_workflow("public-original-photo","Find a real HZMB Golden Bus photo taken at the Hong Kong Port in October 2018. Use the original full-resolution photo, keep its full quality, look at the actual image to make sure it shows a bus, then put that original photo in my composer for review. Do not send it.",true,true).await;
}

#[tokio::test]
#[ignore="Calls configured Claude Code with synthetic earlier room JPEG; no Matrix account"]
async fn live_claude_finds_inspects_stages_then_reposts_an_earlier_room_photo() {
    run_workflow("public-prior-photo","Find the image uploaded earlier in this room. Look at it and tell me its four quadrant colors in reading order, then attach it to my composer for review. Do not send it.",false,false).await;
}
