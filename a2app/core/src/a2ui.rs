//! A2UI payloads as Splash mini-apps.
//!
//! [A2UI](https://a2ui.org) lets an agent describe a UI as JSON messages: a flat
//! component list, a data model and a root. [`to_splash`] translates one complete
//! payload (v0.8, v0.9.x or v1.0 messages) into a static Splash script. Bindings
//! are resolved at translation time, inputs write back into script variables, and
//! button actions are posted to the room as the A2UI client-to-server message,
//! so the agent that sent the UI receives them. Later updates are not live.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;

use serde_json::{Map, Value};

/// The built-in app every inline timeline surface acts as: its declarations
/// and the user's grants decide what any A2UI message may do.
pub const APP_ID: &str = "a2ui";

/// File extension of an A2UI payload saved as a file.
pub const FILE_EXT: &str = "a2ui";

/// `application/a2ui+json` (v0.9.1+) or the older `application/json+a2ui`.
pub fn is_a2ui_mime(mime: &str) -> bool {
    let mime = mime.split(';').next().unwrap_or_default().trim();
    mime.eq_ignore_ascii_case("application/a2ui+json") || mime.eq_ignore_ascii_case("application/json+a2ui")
}

const COMPONENT_KEYS: [&str; 3] = ["surfaceUpdate", "updateComponents", "createSurface"];
const MESSAGE_KEYS: [&str; 7] = [
    "surfaceUpdate", "dataModelUpdate", "beginRendering", "deleteSurface",
    "createSurface", "updateComponents", "updateDataModel",
];
/// Bounds a hostile payload: no more elements than this are emitted, which
/// also keeps the app's first run inside Splash's instruction and time budget.
const MAX_NODES: usize = 400;
const MAX_DEPTH: usize = 40;

/// Whether `text` is an A2UI payload with at least one component update.
pub fn is_a2ui(text: &str) -> bool {
    let text = strip_fence(text.trim());
    if !COMPONENT_KEYS.iter().any(|key| text.contains(key)) {
        return false;
    }
    parse_payload(text).is_ok_and(|payload| payload.messages.iter().any(|m|
        COMPONENT_KEYS.iter().any(|key| m.get(*key)
            .and_then(|body| body.get("components")).is_some_and(Value::is_array))))
}

/// The A2UI payload carried by a chat message, if any: its `<a2ui-json>` blocks
/// (what the A2UI agent SDK prompts for), its fenced blocks, or the whole body.
pub fn find_block(body: &str) -> Option<String> {
    if !COMPONENT_KEYS.iter().any(|key| body.contains(key)) {
        return None;
    }
    for blocks in [tagged_blocks(body), fenced_blocks(body)] {
        let blocks: Vec<&str> = blocks.into_iter()
            .filter(|block| parse_payload(block).is_ok_and(|p| !p.messages.is_empty()))
            .collect();
        if !blocks.is_empty() {
            let joined = blocks.join("\n");
            return is_a2ui(&joined).then_some(joined);
        }
    }
    is_a2ui(body).then(|| body.trim().to_string())
}

/// The contents of every `<a2ui-json>…</a2ui-json>` block in `text`.
fn tagged_blocks(text: &str) -> Vec<&str> {
    let mut blocks = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find("<a2ui-json>") {
        let body = &rest[open + "<a2ui-json>".len()..];
        let Some(close) = body.find("</a2ui-json>") else { break };
        blocks.push(strip_fence(body[..close].trim()));
        rest = &body[close..];
    }
    blocks
}

/// The contents of every ``` fenced block in `text`.
fn fenced_blocks(text: &str) -> Vec<&str> {
    let mut blocks = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find("```") {
        let after = &rest[open + 3..];
        let Some(newline) = after.find('\n') else { break };
        let body = &after[newline + 1..];
        let Some(close) = body.find("```") else { break };
        blocks.push(body[..close].trim());
        rest = &body[close + 3..];
    }
    blocks
}

/// The body of a lone fenced block, or `text` unchanged.
fn strip_fence(text: &str) -> &str {
    if text.starts_with("```") && let [block] = fenced_blocks(text)[..] {
        return block;
    }
    text
}

/// The messages of a payload, plus the gallery format's `name`/`description`.
#[derive(Default)]
struct Payload {
    messages: Vec<Map<String, Value>>,
    /// For each message, its entry: the timeline hands over a surface as an
    /// array of message arrays (the surface's own, then each update's).
    entries: Vec<usize>,
    entry: usize,
    name: Option<String>,
    description: Option<String>,
}

impl Payload {
    /// How many entries the payload has (1 unless it is an array of message arrays).
    fn entry_count(&self) -> usize {
        self.entries.iter().max().map_or(1, |max| max + 1)
    }
}

/// Every A2UI message in a JSON value stream (JSONL, concatenated objects or
/// arrays), unwrapping A2A data parts and `{"messages": [...]}` wrappers.
fn parse_payload(text: &str) -> Result<Payload, String> {
    let mut payload = Payload::default();
    for value in serde_json::Deserializer::from_str(text).into_iter::<Value>() {
        let value = value.map_err(|e| format!("not valid A2UI JSON: {e}"))?;
        match value {
            // The timeline's form: the surface's message array, then each update's messages.
            Value::Array(items) if items.first().is_some_and(Value::is_array) => {
                for (entry, item) in items.into_iter().enumerate() {
                    payload.entry = entry;
                    collect_messages(item, &mut payload, 1);
                }
            }
            value => collect_messages(value, &mut payload, 0),
        }
    }
    Ok(payload)
}

fn collect_messages(value: Value, payload: &mut Payload, depth: usize) {
    if depth > 4 { return; }
    match value {
        Value::Array(items) => for item in items { collect_messages(item, payload, depth + 1) },
        Value::Object(mut map) => {
            if MESSAGE_KEYS.iter().any(|key| map.contains_key(*key)) {
                payload.messages.push(map);
                payload.entries.push(payload.entry);
                return;
            }
            if map.contains_key("messages") {
                let text = |v: Option<&Value>| v.and_then(Value::as_str).map(str::to_string).filter(|s| !s.trim().is_empty());
                if payload.name.is_none() { payload.name = text(map.get("name")); }
                if payload.description.is_none() { payload.description = text(map.get("description")); }
            }
            // A2A data parts (`{"kind": "data", "data": ...}`) and other wrappers.
            for key in ["data", "messages", "parts"] {
                if let Some(inner) = map.remove(key) {
                    collect_messages(inner, payload, depth + 1);
                }
            }
        }
        _ => {}
    }
}

/// One component, normalized across protocol versions.
#[derive(Debug, Clone)]
struct Comp {
    kind: String,
    props: Map<String, Value>,
    weight: Option<f64>,
}

#[derive(Debug, Default)]
struct Surface {
    id: String,
    root: Option<String>,
    components: HashMap<String, Comp>,
    order: Vec<String>,
    data: Value,
    primary_color: Option<u32>,
    /// `None` for v0.8, which has no `version` field.
    version: Option<String>,
    /// Actions carry the whole data model (`createSurface.sendDataModel`).
    sends_data_model: bool,
}

impl Surface {
    fn new(id: &str) -> Self {
        Surface { id: id.to_string(), data: Value::Object(Map::new()), ..Default::default() }
    }

    fn is_v08(&self) -> bool { self.version.is_none() }

    fn add_components(&mut self, body: &Value) {
        for item in body.get("components").and_then(Value::as_array).into_iter().flatten() {
            let Some(id) = item.get("id").and_then(Value::as_str) else { continue };
            let Some(comp) = normalize_component(item) else { continue };
            if !self.components.contains_key(id) { self.order.push(id.to_string()); }
            self.components.insert(id.to_string(), comp);
        }
    }
}

/// Replays the messages into their final surfaces, in creation order.
/// The surfaces the messages build. Data writes by messages from entries at or
/// after `newer_than` are listed as (surface id, path): those win over edits.
fn build_surfaces(messages: Vec<Map<String, Value>>, entries: &[usize], newer_than: usize) -> (Vec<Surface>, Vec<(String, String)>) {
    let mut surfaces: Vec<Surface> = Vec::new();
    let mut fresh = Vec::new();
    fn surface<'a>(surfaces: &'a mut Vec<Surface>, body: &Value) -> &'a mut Surface {
        let id = body.get("surfaceId").and_then(Value::as_str).unwrap_or("default");
        if let Some(i) = surfaces.iter().position(|s| s.id == id) {
            return &mut surfaces[i];
        }
        surfaces.push(Surface::new(id));
        surfaces.last_mut().unwrap()
    }
    for (index, message) in messages.into_iter().enumerate() {
        let version = message.get("version").and_then(Value::as_str).map(str::to_string);
        let is_fresh = entries.get(index).is_some_and(|entry| *entry >= newer_than);
        for (key, body) in &message {
            match key.as_str() {
                "surfaceUpdate" | "updateComponents" => {
                    let s = surface(&mut surfaces, body);
                    if key == "updateComponents" && s.version.is_none() {
                        s.version = version.clone().or_else(|| Some("v0.9".into()));
                    }
                    s.add_components(body);
                }
                "beginRendering" => {
                    let s = surface(&mut surfaces, body);
                    s.root = body.get("root").and_then(Value::as_str).map(str::to_string);
                    s.primary_color = body.get("styles").and_then(|st| st.get("primaryColor"))
                        .and_then(Value::as_str).and_then(crate::header::parse_hex_color);
                }
                "createSurface" => {
                    let s = surface(&mut surfaces, body);
                    s.version = version.clone().or_else(|| Some("v0.9".into()));
                    s.sends_data_model = body.get("sendDataModel").and_then(Value::as_bool).unwrap_or(false);
                    s.primary_color = body.get("theme").and_then(|t| t.get("primaryColor"))
                        .and_then(Value::as_str).and_then(crate::header::parse_hex_color);
                    // v1.0 can carry the whole UI and its model inline.
                    s.add_components(body);
                    if let Some(model) = body.get("dataModel") {
                        s.data = model.clone();
                        if is_fresh { fresh.push((s.id.clone(), "/".to_string())); }
                    }
                }
                "dataModelUpdate" => {
                    // The resulting map replaces whatever is at `path`.
                    let s = surface(&mut surfaces, body);
                    let path = normalize_path(body.get("path").and_then(Value::as_str).unwrap_or("/"));
                    let value = match body.get("contents") {
                        Some(Value::Array(entries)) => match &entries[..] {
                            [only] if only.get("key").and_then(Value::as_str) == Some(".") => entry_value(only),
                            _ => contents_to_value(entries),
                        },
                        Some(other) => other.clone(),
                        None => continue,
                    };
                    set_path(&mut s.data, &path, value);
                    if is_fresh { fresh.push((s.id.clone(), path)); }
                }
                "updateDataModel" => {
                    let s = surface(&mut surfaces, body);
                    let path = normalize_path(body.get("path").and_then(Value::as_str).unwrap_or("/"));
                    match body.get("value").filter(|v| !v.is_null()) {
                        Some(value) => set_path(&mut s.data, &path, value.clone()),
                        None => remove_path(&mut s.data, &path),
                    }
                    if is_fresh { fresh.push((s.id.clone(), path)); }
                }
                "deleteSurface" => {
                    let id = body.get("surfaceId").and_then(Value::as_str).unwrap_or("default");
                    surfaces.retain(|s| s.id != id);
                }
                _ => {}
            }
        }
    }
    surfaces.retain(|s| !s.components.is_empty());
    (surfaces, fresh)
}

/// v0.8 wraps props as `{"component": {"Text": {...}}}`; v0.9+ is flat:
/// `{"id", "component": "Text", ...props}`.
fn normalize_component(item: &Value) -> Option<Comp> {
    let weight = item.get("weight").and_then(Value::as_f64);
    match item.get("component")? {
        Value::Object(wrapper) => {
            let (kind, props) = wrapper.iter().next()?;
            Some(Comp { kind: kind.clone(), props: props.as_object().cloned().unwrap_or_default(), weight })
        }
        Value::String(kind) => {
            let mut props = item.as_object()?.clone();
            props.remove("id");
            props.remove("component");
            props.remove("weight");
            Some(Comp { kind: kind.clone(), props, weight })
        }
        _ => None,
    }
}

/// v0.8 `contents`: `[{key, valueString | valueNumber | valueBoolean | valueMap}]`.
/// Keys may be paths; index-keyed maps are arrays (v0.8 has no array type).
fn contents_to_value(entries: &[Value]) -> Value {
    let mut out = Value::Object(Map::new());
    for entry in entries {
        let Some(key) = entry.get("key").and_then(Value::as_str) else { continue };
        set_path(&mut out, &normalize_path(key), entry_value(entry));
    }
    arrayify(out)
}

fn entry_value(entry: &Value) -> Value {
    if let Some(Value::String(s)) = entry.get("valueString") {
        // The reference renderer also accepts JSON smuggled in a string.
        let trimmed = s.trim();
        if (trimmed.starts_with('{') || trimmed.starts_with('['))
            && let Ok(parsed) = serde_json::from_str::<Value>(trimmed)
        {
            return parsed;
        }
        return Value::String(s.clone());
    }
    if let Some(v) = entry.get("valueNumber").or(entry.get("valueBoolean")) { return v.clone(); }
    if let Some(v) = entry.get("valueMap").and_then(Value::as_array) { return contents_to_value(v); }
    entry.get("valueArray").cloned().unwrap_or(Value::Null)
}

fn arrayify(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let is_list = !map.is_empty() && (0..map.len()).all(|i| map.contains_key(&i.to_string()));
            if is_list {
                let mut map = map;
                Value::Array((0..map.len()).map(|i| arrayify(map.remove(&i.to_string()).unwrap())).collect())
            } else {
                Value::Object(map.into_iter().map(|(k, v)| (k, arrayify(v))).collect())
            }
        }
        other => other,
    }
}

/// JSON Pointer form of the dot (`a.b.0`) and bracket (`a[0].b`) paths some agents send.
fn normalize_path(path: &str) -> String {
    let path = path.trim();
    let path = path.strip_prefix("./").unwrap_or(path);
    if path == "." { return String::new(); }
    let path = path.replace('[', "/").replace(']', "");
    if path.contains('/') || !path.contains('.') { path } else { path.replace('.', "/") }
}

fn path_parts(path: &str) -> impl Iterator<Item = &str> {
    path.split('/').filter(|p| !p.is_empty() && *p != ".")
}

/// Joins a relative path onto `base`; absolute paths stand alone.
fn join_path(base: &str, path: &str) -> String {
    let path = normalize_path(path);
    let base = if path.starts_with('/') { "" } else { base };
    let mut out = String::new();
    for part in path_parts(base).chain(path_parts(&path)) {
        out.push('/');
        out.push_str(part);
    }
    if out.is_empty() { out.push('/'); }
    out
}

fn get_path<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    path_parts(path).try_fold(root, |value, part| match value {
        Value::Object(map) => map.get(part),
        Value::Array(items) => part.parse::<usize>().ok().and_then(|i| items.get(i)),
        _ => None,
    })
}

fn set_path(root: &mut Value, path: &str, new: Value) {
    let parts: Vec<&str> = path_parts(path).collect();
    let Some((last, init)) = parts.split_last() else {
        *root = new;
        return;
    };
    let mut value = root;
    for part in init {
        if !value.is_object() && !value.is_array() { *value = Value::Object(Map::new()); }
        value = match value {
            Value::Array(items) => match part.parse::<usize>().ok().filter(|i| *i < items.len()) {
                Some(i) => &mut items[i],
                None => return,
            },
            Value::Object(map) => map.entry(part.to_string()).or_insert_with(|| Value::Object(Map::new())),
            _ => return,
        };
    }
    match value {
        Value::Array(items) => match last.parse::<usize>() {
            Ok(i) if i < items.len() => items[i] = new,
            Ok(i) if i == items.len() => items.push(new),
            _ => {}
        },
        Value::Object(map) => { map.insert(last.to_string(), new); }
        other => {
            let mut map = Map::new();
            map.insert(last.to_string(), new);
            *other = Value::Object(map);
        }
    }
}

fn remove_path(root: &mut Value, path: &str) {
    let parts: Vec<&str> = path_parts(path).collect();
    let Some((last, init)) = parts.split_last() else {
        *root = Value::Object(Map::new());
        return;
    };
    let mut value = root;
    for part in init {
        value = match value {
            Value::Object(map) => match map.get_mut(*part) { Some(v) => v, None => return },
            Value::Array(items) => match part.parse::<usize>().ok().and_then(|i| items.get_mut(i)) { Some(v) => v, None => return },
            _ => return,
        };
    }
    match value {
        Value::Object(map) => { map.remove(*last); }
        Value::Array(items) => if let Ok(i) = last.parse::<usize>() && i < items.len() { items[i] = Value::Null; },
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// formatString expressions
// ---------------------------------------------------------------------------

/// A `${...}` expression: a data path, a literal, or a function call with
/// named arguments, e.g. `${formatDate(value: ${/when}, format: 'MMM d')}`.
#[derive(Clone, Debug, PartialEq)]
enum Expr {
    Path(String),
    Lit(Value),
    Call(String, Vec<(String, Expr)>),
}

/// A piece of a formatString template: literal text or an expression.
#[derive(Clone, Debug, PartialEq)]
enum Part {
    Text(String),
    Expr(Expr),
}

/// Splits a template into text and `${...}` expressions; `\${` stays literal.
fn parse_template(template: &str) -> Vec<Part> {
    let chars: Vec<char> = template.chars().collect();
    let (mut parts, mut text, mut i) = (Vec::new(), String::new(), 0);
    while i < chars.len() {
        if chars[i] == '\\' && chars.get(i + 1) == Some(&'$') && chars.get(i + 2) == Some(&'{') {
            text.push_str("${");
            i += 3;
            continue;
        }
        if chars[i] == '$' && chars.get(i + 1) == Some(&'{')
            && let Some(end) = closing_brace(&chars, i + 2)
        {
            if !text.is_empty() { parts.push(Part::Text(std::mem::take(&mut text))); }
            let inner: String = chars[i + 2..end].iter().collect();
            parts.push(Part::Expr(parse_expr(inner.trim()).unwrap_or(Expr::Lit(Value::Null))));
            i = end + 1;
            continue;
        }
        text.push(chars[i]);
        i += 1;
    }
    if !text.is_empty() { parts.push(Part::Text(text)); }
    parts
}

/// The index of the `}` closing a `${` whose body starts at `start`.
fn closing_brace(chars: &[char], start: usize) -> Option<usize> {
    let (mut depth, mut quote, mut i) = (1, None, start);
    while i < chars.len() {
        let c = chars[i];
        match quote {
            Some(q) if c == '\\' => { let _ = q; i += 1; }
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None => match c {
                '\'' | '"' => quote = Some(c),
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 { return Some(i); }
                }
                _ => {}
            },
        }
        i += 1;
    }
    None
}

fn parse_expr(source: &str) -> Option<Expr> {
    let mut parser = ExprParser { src: source, pos: 0 };
    let expr = parser.expr(0)?;
    parser.skip_ws();
    (parser.pos == source.len()).then_some(expr)
}

struct ExprParser<'a> {
    src: &'a str,
    pos: usize,
}

impl ExprParser<'_> {
    fn peek(&self) -> Option<char> { self.src[self.pos..].chars().next() }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn skip_ws(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) { self.bump(); }
    }

    fn expr(&mut self, depth: usize) -> Option<Expr> {
        if depth > 16 { return None; }
        self.skip_ws();
        if self.src[self.pos..].starts_with("${") {
            self.pos += 2;
            let inner = self.expr(depth + 1)?;
            self.skip_ws();
            return (self.bump()? == '}').then_some(inner);
        }
        match self.peek()? {
            quote @ ('\'' | '"') => {
                self.bump();
                let mut text = String::new();
                loop {
                    match self.bump()? {
                        '\\' => text.push(self.bump()?),
                        c if c == quote => break,
                        c => text.push(c),
                    }
                }
                Some(Expr::Lit(Value::String(text)))
            }
            c if c.is_ascii_digit() || c == '-' => {
                let start = self.pos;
                while self.peek().is_some_and(|c| c.is_ascii_digit() || ".-+eE".contains(c)) { self.bump(); }
                let number: f64 = self.src[start..self.pos].parse().ok()?;
                Some(Expr::Lit(serde_json::json!(number)))
            }
            _ => {
                let start = self.pos;
                while self.peek().is_some_and(|c| !c.is_whitespace() && !"(),:}".contains(c)) { self.bump(); }
                let word = self.src[start..self.pos].to_string();
                self.skip_ws();
                if self.peek() != Some('(') || word.contains('/') {
                    return Some(match word.as_str() {
                        "true" => Expr::Lit(Value::Bool(true)),
                        "false" => Expr::Lit(Value::Bool(false)),
                        "null" => Expr::Lit(Value::Null),
                        _ => Expr::Path(word),
                    });
                }
                self.bump();
                let mut args = Vec::new();
                loop {
                    self.skip_ws();
                    if self.peek() == Some(')') {
                        self.bump();
                        break;
                    }
                    let name_start = self.pos;
                    while self.peek().is_some_and(|c| !c.is_whitespace() && c != ':') { self.bump(); }
                    let name = self.src[name_start..self.pos].to_string();
                    self.skip_ws();
                    if self.bump()? != ':' { return None; }
                    args.push((name, self.expr(depth + 1)?));
                    self.skip_ws();
                    match self.bump()? {
                        ',' => continue,
                        ')' => break,
                        _ => return None,
                    }
                }
                Some(Expr::Call(word, args))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Splash emission
// ---------------------------------------------------------------------------

const TEXT_COLOR: &str = "#x1C274C";
const MUTED_COLOR: &str = "#x66708F";
const ERROR_COLOR: &str = "#xDC0005";
const DEFAULT_TINT: u32 = 0x4A90D9;

/// A resolved value, and the absolute data path it is bound to, if any.
#[derive(Clone, Debug)]
struct Bound {
    path: Option<String>,
    value: Value,
    /// A formatString's pieces, so an answer reports the text inputs make now.
    template: Option<Vec<TextPiece>>,
}

impl Bound {
    fn lit(value: Value) -> Self { Bound { path: None, value, template: None } }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum VarKind { Text, Number, Bool, List }

/// A piece of rendered text: fixed text, or a data path with its value now.
#[derive(Clone, Debug)]
enum TextPiece {
    Static(String),
    Path(String, String),
}

/// A label showing data paths an input may write; it re-renders on each write.
struct LiveText {
    widget: String,
    surface: usize,
    pieces: Vec<TextPiece>,
}

/// A script variable backing one bound data path that an input writes.
struct Var {
    name: String,
    kind: VarKind,
    initial: Value,
}

/// What a function-call button does locally.
#[derive(Clone, Debug)]
enum LocalAction {
    /// Opens the link these pieces make at click time.
    Open(Vec<TextPiece>),
    Unsupported,
}

/// A value a check reads: a data path (its input's variable at click time,
/// if it has one) and its value at translation time.
#[derive(Clone, Debug)]
struct Operand {
    path: Option<String>,
    now: Value,
}

/// A check's condition: the basic catalog's validation functions.
#[derive(Clone, Debug)]
enum Cond {
    Lit(bool),
    Present(Operand),
    Regex(Operand, String),
    Length(Operand, Option<f64>, Option<f64>),
    Numeric(Operand, Option<f64>, Option<f64>),
    /// An ISO 8601 value within optional bounds, compared as text.
    IsoRange(Operand, Option<String>, Option<String>),
    And(Vec<Cond>),
    Or(Vec<Cond>),
    Not(Box<Cond>),
}

/// An input's checks, shown under it while failing.
struct Validator {
    fn_name: String,
    error_widget: String,
    surface: usize,
    checks: Vec<(Cond, String)>,
}

/// A button's action, emitted once every input variable is known.
struct Action {
    fn_name: String,
    name: String,
    surface_id: String,
    source_id: String,
    context: Vec<(String, Bound)>,
    surface: usize,
    /// The surface's protocol version: `None` posts a v0.8 `userAction`.
    version: Option<String>,
    /// A `functionCall` action, which runs here and posts nothing.
    local: Option<LocalAction>,
    /// The button's own checks; any failing one blocks the press.
    checks: Vec<(Cond, String)>,
}

/// How a choice picker shows its options.
#[derive(Clone, Copy, PartialEq)]
enum ChoiceStyle {
    /// Chips; the selected ones tinted (an on and an off widget per option).
    Chips,
    /// Real checkboxes: multiple selection without a limit.
    Checks,
    /// Rows marked with a radio or check glyph.
    Marks,
}

/// One option of a choice picker and the value it toggles.
struct ChoiceOption {
    widget: String,
    label: String,
    value: String,
}

struct Choice {
    fn_name: String,
    var: usize,
    max: Option<usize>,
    style: ChoiceStyle,
    is_filterable: bool,
    validator: Option<usize>,
    options: Vec<ChoiceOption>,
}

#[derive(Default)]
struct Emitter {
    vars: Vec<Var>,
    /// (surface, absolute path) → index into `vars`.
    var_index: HashMap<(usize, String), usize>,
    /// Labels that re-render when an input writes one of their paths.
    live_texts: Vec<LiveText>,
    /// Sliders have no change callback: (var, widget) are read before each action.
    sliders: Vec<(usize, String)>,
    choices: Vec<Choice>,
    actions: Vec<Action>,
    /// Tab sets: the select fn and how many tabs it switches between.
    tabs: Vec<(String, usize)>,
    validators: Vec<Validator>,
    /// Text inputs that pass through a handler: (fn, setter, validator, is a date-time).
    input_handlers: Vec<(String, String, Option<usize>, bool)>,
    /// `mxc` images the host loads into inline surfaces.
    images: Vec<InlineImage>,
    /// Variables of obscured fields, never exported where the host keeps edits.
    secrets: Vec<usize>,
    next_id: usize,
    nodes: usize,
    title: Option<String>,
    is_truncated: bool,
    /// Whether any control opens a link (`url.open`).
    opens_links: bool,
    /// Fit the content (a timeline message) instead of filling a pane.
    is_inline: bool,
    /// The timeline message an inline surface's actions answer.
    inline_event: Option<String>,
}

/// Where a component's parent puts it.
#[derive(Clone, Copy, Default)]
struct Place {
    /// Laid out left to right.
    in_row: bool,
    /// The parent sizes to its content, so a Fill child would collapse to nothing.
    parent_fit: bool,
    /// The one flexible child of a row, which takes the leftover width so text wraps.
    grows: bool,
    /// Where text sits across its width, from the parent column's `align`.
    text_x: f64,
}

/// One surface being emitted: its components, and its data model with the
/// path+literal initializers applied.
struct Ctx<'a> {
    index: usize,
    surface: &'a Surface,
    data: Value,
    visiting: Vec<String>,
}

/// Translates an A2UI payload into a Splash mini-app script, header included.
pub fn to_splash(text: &str) -> Result<String, String> {
    translate(text, None, None).map(|(source, ..)| source)
}

/// An inline surface: its Splash source and the `mxc` images the host loads
/// into it, since a host-I/O-only isolate can't fetch media itself.
#[derive(Debug, Clone)]
pub struct InlineApp {
    pub source: String,
    pub images: Vec<InlineImage>,
    /// The payload's entries (its own messages, then each update's).
    pub entries: usize,
}

/// What a user entered in an inline surface, carried over a rebuild so an
/// agent's update doesn't wipe it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Edits {
    /// (surface id, data path, value) of each input.
    pub values: Vec<(String, String, Value)>,
    /// The payload's entries when these were read: data written by later
    /// entries (the agent's newer updates) wins over them.
    pub entries: usize,
}

impl Edits {
    /// What a surface exported into its hidden [`STATE_WIDGET`] label.
    pub fn parse(exported: &str, entries: usize) -> Option<Edits> {
        let Ok(Value::Array(rows)) = serde_json::from_str::<Value>(exported) else { return None };
        let values = rows.into_iter().filter_map(|row| match row {
            Value::Array(mut row) if row.len() == 3 => {
                let value = row.pop()?;
                let path = row.pop()?.as_str()?.to_string();
                let surface = row.pop()?.as_str()?.to_string();
                Some((surface, path, value))
            }
            _ => None,
        }).collect();
        Some(Edits { values, entries })
    }
}

/// The hidden label an inline surface writes its inputs' values to.
pub const STATE_WIDGET: &str = "a2ui_state";

/// An `Image` widget (by id) in an inline surface, and the `mxc://` URL to show.
#[derive(Debug, Clone, PartialEq)]
pub struct InlineImage {
    pub widget: String,
    pub url: String,
}

/// The same surface laid out to fit its content, drawn inside the timeline
/// message `event_id`. It runs as [`APP_ID`], so its buttons answer that
/// message (`matrix.a2ui_action`) only as that app's permissions allow.
pub fn to_inline_splash(text: &str, event_id: &str, edits: Option<&Edits>) -> Result<InlineApp, String> {
    translate(text, Some(event_id), edits).map(|(source, images, entries)| InlineApp { source, images, entries })
}

fn translate(text: &str, inline_event: Option<&str>, edits: Option<&Edits>) -> Result<(String, Vec<InlineImage>, usize), String> {
    let is_inline = inline_event.is_some();
    let text = strip_fence(text.trim());
    let payload = parse_payload(text)?;
    let (name, description) = (payload.name.clone(), payload.description.clone());
    let entries = payload.entry_count();
    let (mut surfaces, fresh) = build_surfaces(payload.messages, &payload.entries, edits.map_or(usize::MAX, |e| e.entries));
    // The user's values, except where the agent wrote the same data since.
    for (surface_id, path, value) in edits.map(|e| e.values.as_slice()).unwrap_or_default() {
        let edited: Vec<&str> = path_parts(path).collect();
        let is_overwritten = fresh.iter().any(|(fresh_surface, fresh_path)| fresh_surface == surface_id && {
            let written: Vec<&str> = path_parts(fresh_path).collect();
            let shared = written.len().min(edited.len());
            written[..shared] == edited[..shared]
        });
        if !is_overwritten && let Some(surface) = surfaces.iter_mut().find(|s| &s.id == surface_id) {
            set_path(&mut surface.data, path, value.clone());
        }
    }
    if surfaces.is_empty() {
        return Err("the A2UI payload has no components to show".into());
    }
    let mut emitter = Emitter { title: name, is_inline, ..Default::default() };
    emitter.inline_event = inline_event.map(str::to_string);
    let mut body = String::new();
    let mut drawn = 0;
    for (index, surface) in surfaces.iter().enumerate() {
        let Some(root) = root_of(surface) else { continue };
        if drawn > 0 { body.push_str("        SolidView{ width: Fill height: 1 draw_bg +: { color: #x00000018 } }\n"); }
        drawn += 1;
        let mut ctx = Ctx { index, surface, data: surface.data.clone(), visiting: Vec::new() };
        emitter.node(&mut ctx, &root, "/", 2, &mut body, Place::default());
    }
    if drawn == 0 {
        return Err("no A2UI surface has a root component to show yet".into());
    }
    let source = emitter.finish(&surfaces, &body, description.as_deref());
    Ok((source, emitter.images, entries))
}

/// The root a renderer would start from: v0.8's `beginRendering.root`, v0.9's
/// `root` id, else the first component nothing else references.
fn root_of(surface: &Surface) -> Option<String> {
    if let Some(root) = surface.root.as_ref().filter(|r| surface.components.contains_key(*r)) {
        return Some(root.clone());
    }
    if surface.components.contains_key("root") {
        return Some("root".into());
    }
    // v0.9+ surfaces render once a `root` exists; only v0.8 needs a guess.
    if !surface.is_v08() {
        return None;
    }
    let mut referenced = HashSet::new();
    for comp in surface.components.values() {
        collect_refs(&comp.props, &mut referenced);
    }
    surface.order.iter().find(|id| !referenced.contains(id.as_str())).cloned()
        .or_else(|| surface.order.first().cloned())
}

fn collect_refs(props: &Map<String, Value>, out: &mut HashSet<String>) {
    for (key, value) in props {
        match (key.as_str(), value) {
            ("child" | "entryPointChild" | "contentChild" | "trigger" | "content", Value::String(id)) => { out.insert(id.clone()); }
            ("children", Value::Array(ids)) => out.extend(ids.iter().filter_map(Value::as_str).map(str::to_string)),
            ("children", Value::Object(children)) => {
                if let Some(ids) = children.get("explicitList").and_then(Value::as_array) {
                    out.extend(ids.iter().filter_map(Value::as_str).map(str::to_string));
                }
                if let Some(id) = children.get("template").and_then(|t| t.get("componentId")).or(children.get("componentId")).and_then(Value::as_str) {
                    out.insert(id.to_string());
                }
            }
            ("tabItems" | "tabs", Value::Array(items)) => out.extend(items.iter()
                .filter_map(|item| item.get("child").and_then(Value::as_str)).map(str::to_string)),
            _ => {}
        }
    }
}

impl Emitter {
    fn fresh(&mut self, prefix: &str) -> String {
        self.next_id += 1;
        format!("a2ui_{prefix}{}", self.next_id)
    }

    /// An absolute data path. Inside a v0.8 template every path is item-relative,
    /// as in the reference renderer (the official samples depend on it).
    fn resolve(&self, ctx: &Ctx, scope: &str, path: &str) -> String {
        let mut path = path.trim();
        if ctx.surface.is_v08() && scope != "/" {
            for prefix in ["/item", "./item", "/text", "/label"] {
                if let Some(rest) = path.strip_prefix(prefix) && (rest.is_empty() || rest.starts_with('/')) {
                    path = rest;
                    break;
                }
            }
            path = path.trim_start_matches("./").trim_start_matches('/');
        }
        join_path(scope, path)
    }

    /// Resolves a v0.8 BoundValue or a v0.9/v1.0 dynamic value against `scope`.
    /// A v0.8 path with a literal initializes the data model, as the spec says.
    fn bound(&self, ctx: &mut Ctx, value: Option<&Value>, scope: &str) -> Bound {
        let Some(value) = value else { return Bound::lit(Value::Null) };
        // v1.0 binds only with `@path`/`@call`; a plain `path` key there is data.
        let is_v1 = ctx.surface.version.as_deref().is_some_and(|v| v.starts_with("v1"));
        let Value::Object(map) = value else {
            return Bound::lit(if is_v1 { unescape_v1(value) } else { value.clone() });
        };
        let key = |name: &str| map.get(&format!("@{name}")).or_else(|| if is_v1 { None } else { map.get(name) });
        let literal = ["literalString", "literalNumber", "literalBoolean", "literalArray"]
            .iter().find_map(|key| map.get(*key)).cloned();
        let call = key("call").and_then(Value::as_str);
        if let Some(path) = key("path").and_then(Value::as_str) {
            let path = self.resolve(ctx, scope, path);
            let current = get_path(&ctx.data, &path).cloned();
            let value = match (current, literal) {
                (Some(current), _) if !current.is_null() => current,
                (_, Some(literal)) => {
                    set_path(&mut ctx.data, &path, literal.clone());
                    literal
                }
                _ => Value::Null,
            };
            return Bound { path: Some(path), value, template: None };
        }
        if let Some(call) = call {
            return Bound::lit(self.call(ctx, call, map.get("args"), scope));
        }
        Bound::lit(literal.unwrap_or_else(|| if is_v1 { unescape_v1(value) } else { value.clone() }))
    }

    /// The function a dynamic value calls (v1.0 only through `@call`).
    fn call_name<'v>(&self, ctx: &Ctx, value: &'v Value) -> Option<&'v str> {
        let is_v1 = ctx.surface.version.as_deref().is_some_and(|v| v.starts_with("v1"));
        value.get("@call").or_else(|| if is_v1 { None } else { value.get("call") }).and_then(Value::as_str)
    }

    fn arg(&self, ctx: &mut Ctx, args: Option<&Value>, key: &str, scope: &str) -> Value {
        self.bound(ctx, args.and_then(|a| a.get(key)), scope).value
    }

    /// The basic catalog's formatting functions, evaluated once at translation
    /// time. Validation functions are compiled separately (see `cond_expr`).
    fn call(&self, ctx: &mut Ctx, call: &str, args: Option<&Value>, scope: &str) -> Value {
        match call {
            "formatString" => {
                let template = display(&self.arg(ctx, args, "value", scope));
                Value::String(parse_template(&template).into_iter().map(|part| match part {
                    Part::Text(text) => text,
                    Part::Expr(expr) => display(&self.eval(ctx, &expr, scope)),
                }).collect())
            }
            "formatNumber" | "formatCurrency" => {
                let value = self.arg(ctx, args, "value", scope);
                let number = value.as_f64().or_else(|| display(&value).trim().parse().ok()).unwrap_or(0.0);
                let decimals = self.arg(ctx, args, "decimals", scope).as_f64()
                    .map_or(if call == "formatCurrency" { 2 } else { 0 }, |d| d.clamp(0.0, 10.0) as usize);
                let grouping = self.arg(ctx, args, "grouping", scope).as_bool().unwrap_or(true);
                let formatted = format_number(number, decimals, grouping);
                Value::String(if call == "formatCurrency" {
                    currency_text(&display(&self.arg(ctx, args, "currency", scope)), &formatted)
                } else { formatted })
            }
            "formatDate" => {
                let value = display(&self.arg(ctx, args, "value", scope));
                let pattern = display(&self.arg(ctx, args, "format", scope));
                Value::String(format_date(&value, &pattern).unwrap_or(value))
            }
            "pluralize" => {
                let count = self.arg(ctx, args, "value", scope).as_f64().unwrap_or(0.0);
                let key = match count { 0.0 => "zero", 1.0 => "one", 2.0 => "two", _ => "other" };
                match self.arg(ctx, args, key, scope) {
                    Value::Null => self.arg(ctx, args, "other", scope),
                    form => form,
                }
            }
            // v1.0: the index of the current template item, plus `offset`.
            "@index" => {
                let index = scope.rsplit('/').next().and_then(|i| i.parse::<f64>().ok()).unwrap_or(0.0);
                serde_json::json!(index + self.arg(ctx, args, "offset", scope).as_f64().unwrap_or(0.0))
            }
            _ => Value::Null,
        }
    }

    fn eval(&self, ctx: &mut Ctx, expr: &Expr, scope: &str) -> Value {
        match expr {
            Expr::Lit(value) => value.clone(),
            Expr::Path(path) => {
                let path = self.resolve(ctx, scope, path);
                get_path(&ctx.data, &path).cloned().unwrap_or(Value::Null)
            }
            Expr::Call(name, args) => {
                let args: Map<String, Value> = args.iter().map(|(key, expr)| (key.clone(), self.eval(ctx, expr, scope))).collect();
                self.call(ctx, name, Some(&Value::Object(args)), scope)
            }
        }
    }

    /// What a text value renders: fixed text and the data paths it shows, so
    /// the label can follow inputs that write those paths.
    fn pieces(&self, ctx: &mut Ctx, value: Option<&Value>, scope: &str) -> Vec<TextPiece> {
        let call = value.and_then(|v| self.call_name(ctx, v));
        let template = if call == Some("formatString") {
            display(&self.arg(ctx, value.and_then(|v| v.get("args")), "value", scope))
        } else {
            let bound = self.bound(ctx, value, scope);
            if let Some(path) = bound.path {
                return vec![TextPiece::Path(path, display(&bound.value))];
            }
            let text = display(&bound.value);
            // v0.9 interpolates only inside formatString, but agents (and the
            // spec's own openUrl example) put `${/path}` in plain strings too:
            // honor that only when every expression is a path the model has.
            let is_template = text.contains("${") && parse_template(&text).iter().all(|part| match part {
                Part::Text(_) => true,
                Part::Expr(Expr::Path(path)) => path.starts_with('/') && get_path(&ctx.data, &self.resolve(ctx, scope, path)).is_some(),
                Part::Expr(_) => false,
            });
            if !is_template {
                return vec![TextPiece::Static(text)];
            }
            text
        };
        parse_template(&template).into_iter().map(|part| match part {
            Part::Text(text) => TextPiece::Static(text),
            Part::Expr(Expr::Path(path)) => {
                let path = self.resolve(ctx, scope, &path);
                let now = get_path(&ctx.data, &path).map(display).unwrap_or_default();
                TextPiece::Path(path, now)
            }
            Part::Expr(expr) => TextPiece::Static(display(&self.eval(ctx, &expr, scope))),
        }).collect()
    }

    fn text(&self, ctx: &mut Ctx, value: Option<&Value>, scope: &str) -> String {
        self.pieces(ctx, value, scope).into_iter()
            .map(|piece| match piece { TextPiece::Static(text) | TextPiece::Path(_, text) => text })
            .collect()
    }

    fn var_for(&mut self, ctx: &Ctx, path: &str, kind: VarKind, initial: Value) -> usize {
        let key = (ctx.index, path.to_string());
        if let Some(&var) = self.var_index.get(&key) {
            return var;
        }
        let name = format!("a2ui_v{}", self.vars.len());
        self.vars.push(Var { name, kind, initial });
        self.var_index.insert(key, self.vars.len() - 1);
        self.vars.len() - 1
    }

    fn children(&self, ctx: &mut Ctx, comp: &Comp, scope: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let template = |out: &mut Vec<(String, String)>, ctx: &mut Ctx, id: &str, binding: &str| {
            let path = self.resolve(ctx, scope, binding);
            match get_path(&ctx.data, &path) {
                Some(Value::Array(items)) => for i in 0..items.len() {
                    out.push((id.to_string(), format!("{}/{i}", path.trim_end_matches('/'))));
                },
                Some(Value::Object(map)) => for key in map.keys() {
                    out.push((id.to_string(), format!("{}/{key}", path.trim_end_matches('/'))));
                },
                _ => {}
            }
        };
        match comp.props.get("children") {
            Some(Value::Array(ids)) => out.extend(ids.iter().filter_map(Value::as_str).map(|id| (id.to_string(), scope.to_string()))),
            Some(Value::Object(children)) => {
                if let Some(ids) = children.get("explicitList").and_then(Value::as_array) {
                    out.extend(ids.iter().filter_map(Value::as_str).map(|id| (id.to_string(), scope.to_string())));
                } else {
                    let template_spec = children.get("template").unwrap_or(&Value::Null);
                    let id = template_spec.get("componentId").or(children.get("componentId")).and_then(Value::as_str);
                    let binding = template_spec.get("dataBinding").or(template_spec.get("path"))
                        .or(children.get("path")).or(children.get("dataBinding")).and_then(Value::as_str);
                    if let (Some(id), Some(binding)) = (id, binding) {
                        template(&mut out, ctx, id, binding);
                    }
                }
            }
            _ => {}
        }
        if out.is_empty() && let Some(Value::String(id)) = comp.props.get("child") {
            out.push((id.clone(), scope.to_string()));
        }
        out
    }

    fn node(&mut self, ctx: &mut Ctx, id: &str, scope: &str, depth: usize, out: &mut String, place: Place) {
        let Some(comp) = ctx.surface.components.get(id).cloned() else { return };
        if ctx.visiting.iter().any(|v| v == id) || depth > MAX_DEPTH {
            return;
        }
        if self.nodes >= MAX_NODES {
            self.is_truncated = true;
            return;
        }
        self.nodes += 1;
        ctx.visiting.push(id.to_string());
        let pad = "    ".repeat(depth);
        let width = match comp.weight.filter(|_| place.in_row) {
            _ if place.parent_fit => "Fit".to_string(),
            Some(weight) => format!("Fill{{weight: {}}}", num(weight.clamp(0.01, 100.0) * 100.0)),
            None if place.in_row && !place.grows => "Fit".to_string(),
            None => "Fill".to_string(),
        };
        let is_fit = width == "Fit";
        let p = &comp.props;
        match comp.kind.as_str() {
            "Text" | "Heading" | "Paragraph" => {
                let pieces = self.pieces(ctx, p.get("text"), scope);
                let text: String = pieces.iter()
                    .map(|piece| match piece { TextPiece::Static(text) | TextPiece::Path(_, text) => text.as_str() })
                    .collect();
                // A Markdown heading stands in for a heading variant.
                let (text, heading) = markdown_heading(&text);
                let hint = match str_prop(p, &["usageHint", "variant", "level"]) {
                    "" | "body" => heading.unwrap_or(""),
                    hint => hint,
                };
                let (size, is_bold, color) = match hint {
                    "h1" | "1" => (20.0, true, TEXT_COLOR),
                    "h2" | "2" => (17.0, true, TEXT_COLOR),
                    "h3" | "3" => (15.0, true, TEXT_COLOR),
                    "h4" | "4" => (13.0, true, TEXT_COLOR),
                    "h5" | "5" => (12.0, true, TEXT_COLOR),
                    "caption" => (9.0, false, MUTED_COLOR),
                    _ => (11.0, false, TEXT_COLOR),
                };
                if self.title.is_none() && is_bold && !text.trim().is_empty() {
                    self.title = Some(text.trim().to_string());
                }
                let name = pieces.iter().any(|piece| matches!(piece, TextPiece::Path(..))).then(|| {
                    let name = self.fresh("t");
                    self.live_texts.push(LiveText { widget: name.clone(), surface: ctx.index, pieces });
                    name
                });
                write_aligned_label(out, &pad, name.as_deref(), &width, &strip_markdown(&text), size, is_bold, color, place.text_x);
            }
            "Image" | "Video" | "AudioPlayer" => {
                let url = self.text(ctx, p.get("url").or(p.get("src")), scope);
                let about = self.text(ctx, p.get("altText").or(p.get("description")).or(p.get("alt")), scope);
                let variant = str_prop(p, &["usageHint", "variant"]);
                if comp.kind == "Image" && self.is_inline && is_mxc_url(&url) {
                    // The host loads the bytes; a host-I/O-only isolate can't.
                    let widget = self.fresh("img");
                    let fill = |fixed: &'static str| if is_fit { fixed } else { "Fill" };
                    let (w, h) = match variant {
                        "icon" => ("24", "24"),
                        "avatar" => ("40", "40"),
                        "smallFeature" => ("100", "100"),
                        "largeFeature" => (fill("320"), "300"),
                        "header" => (fill("320"), "200"),
                        _ => (fill("240"), "200"),
                    };
                    let fit = match str_prop(p, &["fit"]) {
                        "cover" => "Biggest", "fill" => "Stretch", "none" => "Size", _ => "Smallest",
                    };
                    let _ = writeln!(out, "{pad}{widget} := Image{{ width: {w} height: {h} fit: ImageFit.{fit} }}");
                    self.images.push(InlineImage { widget, url });
                } else {
                    let glyph = match (comp.kind.as_str(), variant) {
                        ("Video", _) => "🎬", ("AudioPlayer", _) => "🔊", (_, "avatar") => "👤", _ => "🖼",
                    };
                    let caption = format!("{glyph} {}", if about.is_empty() { short_url(&url) } else { about });
                    if is_web_url(&url) {
                        self.opens_links = true;
                        let _ = writeln!(out, "{pad}A2uiFlat{{ text: {} on_click: || a2ui_open({}) }}", lit(&caption), lit(&url));
                    } else {
                        write_label(out, &pad, None, &width, &caption, 10.0, false, MUTED_COLOR);
                    }
                }
            }
            "Icon" => {
                let svg_path = p.get("name").and_then(|name| name.get("svgPath")).and_then(Value::as_str)
                    .filter(|d| is_svg_path(d));
                match svg_path {
                    Some(d) => {
                        let _ = writeln!(out, "{pad}Vector{{ width: 20 height: 20 viewbox: vec4(0 0 24 24) Path{{ d: {} fill: {TEXT_COLOR} }} }}", lit(d));
                    }
                    None => {
                        let name = self.text(ctx, p.get("name").or(p.get("icon")), scope);
                        write_label(out, &pad, None, "Fit", icon_emoji(&name), 14.0, false, TEXT_COLOR);
                    }
                }
            }
            "Divider" => {
                if str_prop(p, &["axis"]) == "vertical" {
                    let _ = writeln!(out, "{pad}SolidView{{ width: 1 height: 24 draw_bg +: {{ color: #x00000018 }} }}");
                } else {
                    let _ = writeln!(out, "{pad}SolidView{{ width: Fill height: 1 draw_bg +: {{ color: #x00000018 }} }}");
                }
            }
            "Row" | "Column" | "List" => {
                let is_row = comp.kind == "Row" || (comp.kind == "List" && str_prop(p, &["direction"]) == "horizontal");
                let main = str_prop(p, &["distribution", "justify"]);
                let cross = str_prop(p, &["alignment", "align"]);
                let main_pos = match main { "center" => 0.5, "end" => 1.0, _ => 0.0 };
                let cross_pos = match cross { "center" => 0.5, "end" => 1.0, _ => 0.0 };
                let (ax, ay) = if is_row { (main_pos, cross_pos) } else { (cross_pos, main_pos) };
                let container = if comp.kind == "List" && is_row { "ScrollXView" } else { "View" };
                let flow = if is_row { "Right" } else { "Down" };
                let _ = writeln!(out, "{pad}{container}{{ width: {width} height: Fit flow: {flow} spacing: 8 align: Align{{x: {ax} y: {ay}}}");
                let kids = self.children(ctx, &comp, scope);
                let spread = is_row && matches!(main, "spaceBetween" | "spaceAround" | "spaceEvenly");
                // A row's one unweighted text-like child takes the leftover width so it wraps.
                let flexible: Vec<usize> = kids.iter().enumerate().filter(|(_, (kid, _))| {
                    ctx.surface.components.get(kid).is_some_and(|c| c.weight.is_none()
                        && matches!(c.kind.as_str(), "Text" | "Heading" | "Paragraph" | "Column" | "Card" | "List"))
                }).map(|(i, _)| i).collect();
                let is_weighted = kids.iter().any(|(kid, _)| ctx.surface.components.get(kid).is_some_and(|c| c.weight.is_some()));
                let grows = (is_row && !spread && !is_weighted && flexible.len() == 1).then(|| flexible[0]);
                for (i, (kid, kid_scope)) in kids.iter().enumerate() {
                    if spread && (i > 0 || main != "spaceBetween") {
                        let _ = writeln!(out, "{pad}    Filler{{}}");
                    }
                    let text_x = if is_row { 0.0 } else { cross_pos };
                    self.node(ctx, kid, kid_scope, depth + 1, out, Place { in_row: is_row, parent_fit: is_fit, grows: grows == Some(i), text_x });
                }
                if spread && main != "spaceBetween" && !kids.is_empty() {
                    let _ = writeln!(out, "{pad}    Filler{{}}");
                }
                let _ = writeln!(out, "{pad}}}");
            }
            "Card" => {
                if self.is_inline && depth == 2 {
                    let _ = writeln!(out, "{pad}View{{ width: {width} height: Fit flow: Down spacing: 8 padding: 4");
                } else {
                    let _ = writeln!(out, "{pad}RoundedView{{ width: {width} height: Fit flow: Down spacing: 8 padding: 12 show_bg: true new_batch: true");
                    let _ = writeln!(out, "{pad}    draw_bg +: {{ color: #xFFFFFF border_radius: 8.0 border_size: 1.0 border_color: #xDDE1EA }}");
                }
                for (kid, kid_scope) in self.children(ctx, &comp, scope) {
                    self.node(ctx, &kid, &kid_scope, depth + 1, out, Place { parent_fit: is_fit, ..Place::default() });
                }
                let _ = writeln!(out, "{pad}}}");
            }
            "Tabs" => {
                let mut items = p.get("tabItems").or(p.get("tabs")).and_then(Value::as_array).cloned().unwrap_or_default();
                items.truncate(MAX_NODES.saturating_sub(self.nodes));
                self.nodes += items.len();
                let fn_name = self.fresh("tab");
                let _ = writeln!(out, "{pad}View{{ width: {width} height: Fit flow: Down spacing: 8");
                let _ = writeln!(out, "{pad}    View{{ width: {width} height: Fit flow: Flow.Right{{wrap: true}} spacing: 6");
                for (i, item) in items.iter().enumerate() {
                    let title = lit(&self.text(ctx, item.get("title"), scope));
                    let _ = writeln!(out, "{pad}        {fn_name}_on_{i} := A2uiChipOn{{ visible: {} text: {title} on_click: || {fn_name}({i}) }}", i == 0);
                    let _ = writeln!(out, "{pad}        {fn_name}_off_{i} := A2uiChip{{ visible: {} text: {title} on_click: || {fn_name}({i}) }}", i != 0);
                }
                let _ = writeln!(out, "{pad}    }}");
                for (i, item) in items.iter().enumerate() {
                    let _ = writeln!(out, "{pad}    {fn_name}_pane_{i} := View{{ width: {width} height: Fit flow: Down visible: {}", i == 0);
                    if let Some(child) = item.get("child").and_then(Value::as_str) {
                        self.node(ctx, child, scope, depth + 2, out, Place { parent_fit: is_fit, ..Place::default() });
                    }
                    let _ = writeln!(out, "{pad}    }}");
                }
                let _ = writeln!(out, "{pad}}}");
                self.tabs.push((fn_name, items.len()));
            }
            "Modal" => {
                let trigger = p.get("entryPointChild").or(p.get("trigger")).and_then(Value::as_str).map(str::to_string);
                let content = p.get("contentChild").or(p.get("content")).and_then(Value::as_str).map(str::to_string);
                let pane = self.fresh("modal");
                let label = trigger.as_deref().map(|t| self.label_of(ctx, t, scope)).filter(|l| !l.is_empty())
                    .unwrap_or_else(|| "Open".into());
                let _ = writeln!(out, "{pad}View{{ width: {width} height: Fit flow: Down spacing: 8");
                let _ = writeln!(out, "{pad}    A2uiButton{{ text: {} on_click: || ui.{pane}.set_visible(true) }}", lit(&label));
                let _ = writeln!(out, "{pad}    {pane} := RoundedView{{ visible: false width: {width} height: Fit flow: Down spacing: 8 padding: 12 show_bg: true new_batch: true");
                let _ = writeln!(out, "{pad}        draw_bg +: {{ color: #xF4F6FB border_radius: 8.0 border_size: 1.0 border_color: #xC9CFDC }}");
                if let Some(content) = content {
                    self.node(ctx, &content, scope, depth + 2, out, Place { parent_fit: is_fit, ..Place::default() });
                }
                let _ = writeln!(out, "{pad}        A2uiFlat{{ text: \"Close\" on_click: || ui.{pane}.set_visible(false) }}");
                let _ = writeln!(out, "{pad}    }}");
                let _ = writeln!(out, "{pad}}}");
            }
            "Button" => {
                let label = match p.get("child").and_then(Value::as_str) {
                    Some(child) => self.label_of(ctx, child, scope),
                    None => self.text(ctx, p.get("label").or(p.get("text")), scope),
                };
                let label = if label.trim().is_empty() {
                    let described = self.text(ctx, p.get("accessibility").and_then(|a| a.get("label")), scope);
                    if described.trim().is_empty() { "Submit".to_string() } else { described }
                } else { label };
                let variant = str_prop(p, &["variant"]);
                let is_primary = p.get("primary").and_then(Value::as_bool).unwrap_or(false) || variant == "primary";
                let fn_name = self.fresh("act");
                let action = p.get("action").cloned().unwrap_or(Value::Null);
                let (name, context) = self.action_parts(ctx, &action, scope);
                // A function call runs here and never reaches the agent.
                let local = action.get("functionCall").map(|call| {
                    let is_open = self.call_name(ctx, call) == Some("openUrl");
                    let url = is_open.then(|| self.pieces(ctx, call.get("args").and_then(|a| a.get("url")), scope));
                    // The link as it reads now must be openable; the host checks it again on click.
                    let now = url.as_ref().map(|pieces| pieces_text(pieces)).unwrap_or_default();
                    url.filter(|_| is_openable_url(&now)).map_or(LocalAction::Unsupported, LocalAction::Open)
                });
                self.opens_links |= matches!(local, Some(LocalAction::Open(_)));
                let checks = self.checks_of(ctx, p, scope);
                self.actions.push(Action {
                    fn_name: fn_name.clone(), name, surface_id: ctx.surface.id.clone(), source_id: id.to_string(),
                    context, surface: ctx.index, version: ctx.surface.version.clone(), local, checks,
                });
                let widget = if is_primary { "A2uiPrimary" } else if variant == "borderless" { "A2uiFlat" } else { "A2uiButton" };
                let _ = writeln!(out, "{pad}{widget}{{ text: {} on_click: || {fn_name}() }}", lit(&label));
            }
            "CheckBox" | "Checkbox" | "Switch" | "Toggle" => {
                let label = self.text(ctx, p.get("label").or(p.get("text")), scope);
                let bound = self.bound(ctx, p.get("value").or(p.get("checked")), scope);
                let checked = bound.value.as_bool().unwrap_or(false);
                let path = bound.path.unwrap_or_else(|| format!("/_local/{id}"));
                let var = self.var_for(ctx, &path, VarKind::Bool, Value::Bool(checked));
                let checks = self.checks_of(ctx, p, scope);
                let (handler, validator) = self.input_handler(ctx, var, checks, false);
                let _ = writeln!(out, "{pad}View{{ width: {width} height: Fit flow: Down spacing: 2");
                let _ = writeln!(out, "{pad}    A2uiCheck{{ text: {} active: {checked} on_click: |c| {handler}(c) }}", lit(&label));
                if let Some(validator) = validator { write_error_label(out, &pad, &width, validator); }
                let _ = writeln!(out, "{pad}}}");
            }
            "TextField" | "DateTimeInput" => {
                let label = self.text(ctx, p.get("label"), scope);
                let bound = self.bound(ctx, p.get("text").or(p.get("value")), scope);
                let text = display(&bound.value);
                let path = bound.path.unwrap_or_else(|| format!("/_local/{id}"));
                let var = self.var_for(ctx, &path, VarKind::Text, Value::String(text.clone()));
                let mut checks = self.checks_of(ctx, p, scope);
                let operand = Operand { path: Some(path.clone()), now: Value::String(text.clone()) };
                let kind = str_prop(p, &["textFieldType", "variant", "type"]);
                let (placeholder, is_date_time) = if comp.kind == "DateTimeInput" {
                    let has_date = p.get("enableDate").and_then(Value::as_bool).unwrap_or(false);
                    let has_time = p.get("enableTime").and_then(Value::as_bool).unwrap_or(false);
                    let (placeholder, example) = match (has_date, has_time) {
                        (true, true) => ("YYYY-MM-DDTHH:MM", "2026-10-23T14:30"),
                        (false, true) => ("HH:MM", "14:30"),
                        _ => ("YYYY-MM-DD", "2026-10-23"),
                    };
                    // An empty value is for `required` to judge; a typed one must be
                    // ISO 8601 in any of its forms (agents send `Z`, offsets, fractions).
                    let is_empty = || Cond::Not(Box::new(Cond::Present(operand.clone())));
                    checks.push((Cond::Or(vec![is_empty(), Cond::Regex(operand.clone(), ISO_8601_PATTERN.into())]),
                        format!("Use the format {placeholder}, e.g. {example}.")));
                    let mut bound_text = |key: &str| p.get(key).map(|v| self.text(ctx, Some(v), scope)).filter(|t| !t.is_empty());
                    let (min, max) = (bound_text("min"), bound_text("max"));
                    if min.is_some() || max.is_some() {
                        let message = match (&min, &max) {
                            (Some(min), Some(max)) => format!("Pick a value from {min} to {max}."),
                            (Some(min), None) => format!("Pick {min} or later."),
                            (None, Some(max)) => format!("Pick {max} or earlier."),
                            (None, None) => unreachable!(),
                        };
                        checks.push((Cond::Or(vec![is_empty(), Cond::IsoRange(operand.clone(), min, max)]), message));
                    }
                    (placeholder.to_string(), has_date && has_time)
                } else {
                    let placeholder = self.text(ctx, p.get("placeholder"), scope);
                    (if placeholder.is_empty() { label.clone() } else { placeholder }, false)
                };
                if let Some(pattern) = p.get("validationRegexp").and_then(Value::as_str).filter(|pattern| is_valid_regex(pattern)) {
                    checks.push((Cond::Regex(operand, pattern.into()), "Enter a valid value.".into()));
                }
                let (handler, validator) = self.input_handler(ctx, var, checks, is_date_time);
                let _ = writeln!(out, "{pad}View{{ width: {width} height: Fit flow: Down spacing: 4");
                if !label.is_empty() {
                    write_label(out, &format!("{pad}    "), None, &width, &label, 10.0, false, MUTED_COLOR);
                }
                let mut flags = String::new();
                if is_fit { flags.push_str(" width: 220"); }
                match kind {
                    "obscured" | "password" => {
                        flags.push_str(" is_password: true");
                        self.secrets.push(var);
                    }
                    "number" => flags.push_str(" is_numeric_only: true"),
                    "longText" | "multiline" => flags.push_str(" is_multiline: true"),
                    _ => {}
                }
                let _ = writeln!(out, "{pad}    A2uiField{{ text: {} empty_text: {}{flags} on_change: |t| {handler}(t) }}",
                    lit(&text), lit(&placeholder));
                if let Some(validator) = validator { write_error_label(out, &pad, &width, validator); }
                let _ = writeln!(out, "{pad}}}");
            }
            "Slider" => {
                let label = self.text(ctx, p.get("label"), scope);
                let bound = self.bound(ctx, p.get("value"), scope);
                let min = num_prop(p, &["minValue", "min"]).unwrap_or(0.0);
                let max = num_prop(p, &["maxValue", "max"]).unwrap_or(100.0).max(min);
                let value = bound.value.as_f64().unwrap_or(min).clamp(min, max);
                let path = bound.path.unwrap_or_else(|| format!("/_local/{id}"));
                let var = self.var_for(ctx, &path, VarKind::Number, serde_json::json!(value));
                // A slider has no change callback: its checks run when a button is pressed.
                let checks = self.checks_of(ctx, p, scope);
                let validator = (!checks.is_empty()).then(|| self.add_validator(ctx.index, checks));
                // v1.0 `steps` divides the range; whole steps show no decimals.
                let step = p.get("steps").and_then(Value::as_f64).filter(|steps| *steps >= 1.0)
                    .map(|steps| (max - min) / steps.floor());
                let mut extra = String::new();
                if let Some(step) = step.filter(|step| *step > 0.0) {
                    let _ = write!(extra, " step: {}", num(step));
                    if step.fract() == 0.0 && min.fract() == 0.0 { extra.push_str(" precision: 0"); }
                }
                let widget = self.fresh("slider");
                let _ = writeln!(out, "{pad}View{{ width: {width} height: Fit flow: Down spacing: 2");
                let _ = writeln!(out, "{pad}    {widget} := A2uiSlider{{ width: {} text: {} min: {} max: {} default: {}{extra} }}",
                    if is_fit { "200" } else { "Fill" }, lit(&label), num(min), num(max), num(value));
                if let Some(validator) = validator { write_error_label(out, &pad, &width, validator); }
                let _ = writeln!(out, "{pad}}}");
                self.sliders.push((var, widget));
            }
            "MultipleChoice" | "ChoicePicker" => {
                let label = self.text(ctx, p.get("label").or(p.get("description")), scope);
                let bound = self.bound(ctx, p.get("selections").or(p.get("value")), scope);
                // v0.8 data models have no arrays: a list arrives as a `valueMap`.
                let selected: Vec<String> = match &bound.value {
                    Value::Array(items) => items.iter().map(display).collect(),
                    Value::Object(map) => map.values().map(display).collect(),
                    Value::Null => Vec::new(),
                    other => vec![display(other)],
                };
                let variant = str_prop(p, &["variant"]);
                let is_single = variant == "mutuallyExclusive" || (comp.kind == "ChoicePicker" && variant != "multipleSelection");
                let max = if is_single { Some(1) } else { p.get("maxAllowedSelections").and_then(Value::as_u64).map(|m| m as usize) };
                // v0.9 `displayStyle`, v0.8 `variant`; both default to checkboxes.
                let style = match str_prop(p, &["displayStyle"]) {
                    "chips" => ChoiceStyle::Chips,
                    _ if variant == "chips" => ChoiceStyle::Chips,
                    _ if max.is_none() => ChoiceStyle::Checks,
                    _ => ChoiceStyle::Marks,
                };
                let is_filterable = p.get("filterable").and_then(Value::as_bool).unwrap_or(false);
                let path = bound.path.unwrap_or_else(|| format!("/_local/{id}"));
                let initial = Value::Array(selected.iter().cloned().map(Value::String).collect());
                let var = self.var_for(ctx, &path, VarKind::List, initial);
                let checks = self.checks_of(ctx, p, scope);
                let validator = (!checks.is_empty()).then(|| self.add_validator(ctx.index, checks));
                let fn_name = self.fresh("pick");
                let options = p.get("options").and_then(Value::as_array).cloned().unwrap_or_default();
                let _ = writeln!(out, "{pad}View{{ width: {width} height: Fit flow: Down spacing: 4");
                if !label.is_empty() {
                    write_label(out, &format!("{pad}    "), None, &width, &label, 10.0, false, MUTED_COLOR);
                }
                if is_filterable {
                    let field_width = if is_fit { " width: 220" } else { "" };
                    let _ = writeln!(out, "{pad}    A2uiField{{ empty_text: \"Filter\"{field_width} on_change: |t| {fn_name}_filter(t) }}");
                }
                let flow = if style == ChoiceStyle::Chips { "Flow.Right{wrap: true}" } else { "Down" };
                let _ = writeln!(out, "{pad}    View{{ width: {width} height: Fit flow: {flow} spacing: 6");
                let mut choice_options = Vec::new();
                for (i, option) in options.iter().enumerate().take(MAX_NODES.saturating_sub(self.nodes)) {
                    self.nodes += 1;
                    let text = self.text(ctx, option.get("label"), scope);
                    let value = option.get("value").map(display).unwrap_or_else(|| text.clone());
                    let widget = format!("{fn_name}_{i}");
                    let is_on = selected.contains(&value);
                    let label = lit(&text);
                    match style {
                        ChoiceStyle::Chips => {
                            let _ = writeln!(out, "{pad}        {widget}_on := A2uiChipOn{{ visible: {is_on} text: {label} on_click: || {fn_name}({i}) }}");
                            let _ = writeln!(out, "{pad}        {widget}_off := A2uiChip{{ visible: {} text: {label} on_click: || {fn_name}({i}) }}", !is_on);
                        }
                        ChoiceStyle::Checks => {
                            let _ = writeln!(out, "{pad}        {widget} := A2uiCheck{{ text: {label} active: {is_on} on_click: |c| {fn_name}_set({i}, c) }}");
                        }
                        ChoiceStyle::Marks => {
                            let mark = choice_mark(is_on, max == Some(1), &text);
                            let _ = writeln!(out, "{pad}        {widget} := A2uiOption{{ text: {} on_click: || {fn_name}({i}) }}", lit(&mark));
                        }
                    }
                    choice_options.push(ChoiceOption { widget, label: text, value });
                }
                let _ = writeln!(out, "{pad}    }}");
                if let Some(validator) = validator { write_error_label(out, &pad, &width, validator); }
                let _ = writeln!(out, "{pad}}}");
                self.choices.push(Choice { fn_name, var, max, style, is_filterable, validator, options: choice_options });
            }
            other => {
                write_label(out, &pad, None, &width, &format!("Unsupported component: {other}"), 9.0, false, MUTED_COLOR);
            }
        }
        ctx.visiting.pop();
    }

    /// An input's or button's `checks`, in v0.9's `{condition, message}` form or
    /// the docs' inline `{call, args, message}` form.
    fn checks_of(&self, ctx: &mut Ctx, p: &Map<String, Value>, scope: &str) -> Vec<(Cond, String)> {
        let mut out = Vec::new();
        for check in p.get("checks").and_then(Value::as_array).into_iter().flatten().take(16) {
            let condition = check.get("condition").unwrap_or(check);
            let message = self.text(ctx, check.get("message"), scope);
            let message = if message.trim().is_empty() { "This value isn't valid.".to_string() } else { message };
            out.push((self.cond(ctx, condition, scope, 0), message));
        }
        out
    }

    fn operand(&self, ctx: &mut Ctx, value: Option<&Value>, scope: &str) -> Operand {
        let bound = self.bound(ctx, value, scope);
        Operand { path: bound.path, now: bound.value }
    }

    /// A check condition: a boolean, a bound value's truthiness, or one of the
    /// basic catalog's validation functions.
    fn cond(&self, ctx: &mut Ctx, value: &Value, scope: &str, depth: usize) -> Cond {
        if depth > 8 { return Cond::Lit(true); }
        let Some(call) = value.get("@call").or(value.get("call")).and_then(Value::as_str) else {
            return match value {
                Value::Bool(b) => Cond::Lit(*b),
                _ => Cond::Present(self.operand(ctx, Some(value), scope)),
            };
        };
        let args = value.get("args");
        let arg = |key: &str| args.and_then(|args| args.get(key));
        let number = |key: &str| arg(key).and_then(Value::as_f64);
        match call {
            "required" => Cond::Present(self.operand(ctx, arg("value"), scope)),
            "regex" => match arg("pattern").and_then(Value::as_str).filter(|pattern| is_valid_regex(pattern)) {
                Some(pattern) => Cond::Regex(self.operand(ctx, arg("value"), scope), pattern.into()),
                None => Cond::Lit(true),
            },
            "length" => Cond::Length(self.operand(ctx, arg("value"), scope), number("min"), number("max")),
            "numeric" => Cond::Numeric(self.operand(ctx, arg("value"), scope), number("min"), number("max")),
            "email" => Cond::Regex(self.operand(ctx, arg("value"), scope), EMAIL_PATTERN.into()),
            "and" | "or" => {
                let parts = arg("values").and_then(Value::as_array).into_iter().flatten().take(16)
                    .map(|value| self.cond(ctx, value, scope, depth + 1)).collect();
                if call == "and" { Cond::And(parts) } else { Cond::Or(parts) }
            }
            "not" => Cond::Not(Box::new(arg("value").map_or(Cond::Lit(true), |value| self.cond(ctx, value, scope, depth + 1)))),
            // Functions this client can't evaluate never block the user.
            _ => Cond::Lit(true),
        }
    }

    fn add_validator(&mut self, surface: usize, checks: Vec<(Cond, String)>) -> usize {
        let index = self.validators.len();
        self.validators.push(Validator {
            fn_name: format!("a2ui_valid{index}"), error_widget: format!("a2ui_err{index}"), surface, checks,
        });
        index
    }

    /// What an input calls with its new value: its variable's setter, or a
    /// handler that also checks the input (and normalizes a date-time).
    fn input_handler(&mut self, ctx: &Ctx, var: usize, checks: Vec<(Cond, String)>, is_date_time: bool) -> (String, Option<usize>) {
        let setter = format!("{}_set", self.vars[var].name);
        let validator = (!checks.is_empty()).then(|| self.add_validator(ctx.index, checks));
        if validator.is_none() && !is_date_time {
            return (setter, None);
        }
        let handler = self.fresh("in");
        self.input_handlers.push((handler.clone(), setter, validator, is_date_time));
        (handler, validator)
    }

    fn operand_expr(&self, surface: usize, operand: &Operand) -> String {
        match operand.path.as_ref().and_then(|path| self.var_index.get(&(surface, path.clone()))) {
            Some(&var) => self.vars[var].name.clone(),
            None => splash_literal(&operand.now),
        }
    }

    /// A condition as a Splash boolean expression over the input variables.
    fn cond_expr(&self, surface: usize, cond: &Cond) -> String {
        let join = |parts: &[Cond], op: &str| if parts.is_empty() { "true".to_string() } else {
            format!("({})", parts.iter().map(|part| self.cond_expr(surface, part)).collect::<Vec<_>>().join(op))
        };
        match cond {
            Cond::Lit(value) => value.to_string(),
            Cond::Present(value) => format!("a2ui_present({})", self.operand_expr(surface, value)),
            Cond::Regex(value, pattern) => format!("a2ui_re({}, {})", lit(pattern), self.operand_expr(surface, value)),
            Cond::Length(value, min, max) => format!("a2ui_length_ok({}, {}, {})",
                self.operand_expr(surface, value), opt_num(*min), opt_num(*max)),
            Cond::Numeric(value, min, max) => format!("a2ui_numeric_ok({}, {}, {})",
                self.operand_expr(surface, value), opt_num(*min), opt_num(*max)),
            Cond::IsoRange(value, min, max) => format!("a2ui_iso_in({}, {}, {})", self.operand_expr(surface, value),
                min.as_deref().map_or_else(|| "nil".into(), lit), max.as_deref().map_or_else(|| "nil".into(), lit)),
            Cond::And(parts) => join(parts, " && "),
            Cond::Or(parts) => join(parts, " || "),
            Cond::Not(inner) => format!("!({})", self.cond_expr(surface, inner)),
        }
    }

    /// The text a button-like component shows: its own text, or its first Text descendant's.
    /// The text a button-like component shows: its icons and texts in order,
    /// e.g. "📤 Send message" for a Row of an Icon and a Text.
    fn label_of(&self, ctx: &mut Ctx, id: &str, scope: &str) -> String {
        let mut stack = vec![id.to_string()];
        let mut seen = HashSet::new();
        let mut parts = Vec::new();
        while let Some(id) = stack.pop() {
            if seen.len() > 50 { break; }
            seen.insert(id.clone());
            let Some(comp) = ctx.surface.components.get(&id).cloned() else { continue };
            match comp.kind.as_str() {
                "Text" | "Heading" | "Paragraph" => {
                    let text = strip_markdown(&markdown_heading(&self.text(ctx, comp.props.get("text"), scope)).0);
                    if !text.trim().is_empty() { parts.push(text.trim().to_string()); }
                }
                "Icon" => {
                    let name = self.text(ctx, comp.props.get("name"), scope);
                    parts.push(icon_emoji(&name).to_string());
                }
                _ => for (kid, _) in self.children(ctx, &comp, scope).into_iter().rev() {
                    stack.push(kid);
                },
            }
        }
        parts.join(" ")
    }

    /// An action's name and context entries, for v0.8 (`{name, context: [{key, value}]}`)
    /// and v0.9 (`{event: {name, context: {...}}}` or `{name, context: {...}}`).
    fn action_parts(&self, ctx: &mut Ctx, action: &Value, scope: &str) -> (String, Vec<(String, Bound)>) {
        let action = action.get("event").unwrap_or(action);
        let name = action.get("name").and_then(Value::as_str).unwrap_or("action").to_string();
        let mut context = Vec::new();
        match action.get("context") {
            Some(Value::Array(entries)) => for entry in entries {
                if let Some(key) = entry.get("key").and_then(Value::as_str) {
                    context.push((key.to_string(), self.context_value(ctx, entry.get("value"), scope)));
                }
            },
            Some(Value::Object(map)) => for (key, value) in map {
                context.push((key.clone(), self.context_value(ctx, Some(value), scope)));
            },
            _ => {}
        }
        (name, context)
    }

    /// An action context value; a formatString keeps its pieces so the answer
    /// carries the text the inputs make at click time.
    fn context_value(&self, ctx: &mut Ctx, value: Option<&Value>, scope: &str) -> Bound {
        let call = value.and_then(|v| self.call_name(ctx, v));
        if call != Some("formatString") {
            return self.bound(ctx, value, scope);
        }
        let pieces = self.pieces(ctx, value, scope);
        let now: String = pieces.iter().map(|piece| match piece { TextPiece::Static(t) | TextPiece::Path(_, t) => t.as_str() }).collect();
        Bound { path: None, value: Value::String(now), template: Some(pieces) }
    }

    /// A Splash expression evaluating to the JSON text of `value` at `path`,
    /// reading input variables at click time. Inputs below `path` are included
    /// even where the agent's model had no key for them yet.
    fn json_expr(&self, surface: usize, path: Option<&str>, value: &Value, out: &mut Vec<Piece>) {
        if let Some(path) = path {
            if let Some(&var) = self.var_index.get(&(surface, path.to_string())) {
                let name = &self.vars[var].name;
                out.push(Piece::Expr(format!("{name}.to_json()")));
                return;
            }
            let prefix = format!("{}/", path.trim_end_matches('/'));
            // The next path segment of every input variable below `path`.
            let inner: Vec<String> = self.var_index.keys()
                .filter(|(s, p)| *s == surface && p.starts_with(&prefix) && !p.starts_with("/_local/"))
                .filter_map(|(_, p)| p[prefix.len()..].split('/').next().map(str::to_string))
                .fold(Vec::new(), |mut keys, key| { if !keys.contains(&key) { keys.push(key); } keys });
            if !inner.is_empty() {
                let indices: Option<Vec<usize>> = inner.iter().map(|key| key.parse().ok()).collect();
                match (value, indices) {
                    (Value::Array(items), Some(indices)) => {
                        let len = items.len().max(indices.iter().max().map_or(0, |max| max + 1));
                        out.push(Piece::Lit("[".into()));
                        for i in 0..len {
                            if i > 0 { out.push(Piece::Lit(",".into())); }
                            self.json_expr(surface, Some(&format!("{prefix}{i}")), items.get(i).unwrap_or(&Value::Null), out);
                        }
                        out.push(Piece::Lit("]".into()));
                    }
                    _ => {
                        let empty = Map::new();
                        let map = value.as_object().unwrap_or(&empty);
                        let keys: Vec<&String> = map.keys().chain(inner.iter().filter(|key| !map.contains_key(*key))).collect();
                        out.push(Piece::Lit("{".into()));
                        for (i, key) in keys.into_iter().enumerate() {
                            if i > 0 { out.push(Piece::Lit(",".into())); }
                            out.push(Piece::Lit(format!("{}:", Value::String(key.clone()))));
                            self.json_expr(surface, Some(&format!("{prefix}{key}")), map.get(key).unwrap_or(&Value::Null), out);
                        }
                        out.push(Piece::Lit("}".into()));
                    }
                }
                return;
            }
        }
        out.push(Piece::Lit(value.to_string()));
    }

    fn finish(&self, surfaces: &[Surface], body: &str, description: Option<&str>) -> String {
        let mut s = String::new();
        let title = self.title.clone()
            .or_else(|| surfaces.first().map(|s| s.id.clone()).filter(|id| id != "default"))
            .unwrap_or_else(|| "A2UI App".into());
        let title: String = title.chars().filter(|c| !c.is_control()).take(18).collect();
        let tint = surfaces.iter().find_map(|s| s.primary_color).unwrap_or(DEFAULT_TINT);
        let _ = writeln!(s, "// name: {}", title.trim());
        let _ = writeln!(s, "// icon: 🧩");
        let _ = writeln!(s, "// tint: #{tint:06X}");
        let description: String = description.unwrap_or("An agent's A2UI surface, translated into a Splash mini-app.")
            .chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
        let _ = writeln!(s, "// description: {}", description.trim());
        let sends = self.actions.iter().any(|a| a.local.is_none());
        let mut permissions = Vec::new();
        if sends { permissions.extend(["matrix-room-send", "matrix.room.message.send"]); }
        if self.opens_links { permissions.extend(["open-url", "device.url.open"]); }
        if !permissions.is_empty() {
            let _ = writeln!(s, "// permissions: {}", permissions.join(", "));
        }
        if sends {
            let _ = writeln!(s, "// why-matrix-room-send: Sends your button presses back to the agent as A2UI action messages.");
        }
        if self.opens_links {
            let _ = writeln!(s, "// why-open-url: Opens the agent's links and media in your browser.");
        }
        s.push('\n');
        let shade = |by: u32| {
            let channel = |shift: u32| ((tint >> shift) & 0xFF) * (100 - by) / 100;
            format!("#x{:02X}{:02X}{:02X}", channel(16), channel(8), channel(0))
        };
        let (base, hover, down) = (shade(0), shade(12), shade(24));
        s.push_str(&STYLES.replace("$TINT", &base).replace("$HOVER", &hover).replace("$DOWN", &down));
        s.push('\n');
        let _ = writeln!(s, "let a2ui_sending = false");
        // Inline surfaces answer their message; panes post the action as text.
        match &self.inline_event {
            Some(event_id) => {
                let _ = writeln!(s, "fn a2ui_post(body, done){{ host.request(\"matrix.a2ui_action\", {{event_id: {}, content: body}}, done) }}", lit(event_id));
            }
            None => {
                let _ = writeln!(s, "fn a2ui_post(body, done){{ host.request(\"matrix.send_message\", {{body: body}}, done) }}");
            }
        }
        for var in &self.vars {
            let _ = writeln!(s, "let {} = {}", var.name, splash_value(&var.initial, var.kind));
        }
        s.push_str(RUNTIME);
        // Input setters refresh the labels showing their path.
        for (index, var) in self.vars.iter().enumerate() {
            if var.kind == VarKind::List { continue; }
            let _ = writeln!(s, "fn {}_set(v){{", var.name);
            let _ = writeln!(s, "    {} = v", var.name);
            self.write_refresh(&mut s, index);
            let _ = writeln!(s, "    a2ui_export()");
            let _ = writeln!(s, "}}");
        }
        // Inline surfaces keep their inputs' values where the host can read them
        // before a rebuild (see `Edits`).
        let _ = writeln!(s, "fn a2ui_export(){{");
        if self.is_inline {
            let rows: Vec<String> = self.var_index.iter()
                .filter(|((_, path), var)| !path.starts_with("/_local/") && !self.secrets.contains(var))
                .map(|((surface, path), &var)| {
                    let value = match self.sliders.iter().find(|(slider_var, _)| *slider_var == var) {
                        Some((_, widget)) => format!("ui.{widget}.value()"),
                        None => self.vars[var].name.clone(),
                    };
                    format!("[{}, {}, {value}]", lit(&surfaces[*surface].id), lit(path))
                }).collect();
            let _ = writeln!(s, "    ui.{STATE_WIDGET}.set_text([{}].to_json())", rows.join(", "));
        }
        let _ = writeln!(s, "}}");
        let _ = writeln!(s, "fn a2ui_sync(){{");
        for (var, widget) in &self.sliders {
            let _ = writeln!(s, "    {}_set(ui.{widget}.value())", self.vars[*var].name);
        }
        let _ = writeln!(s, "    a2ui_export()");
        let _ = writeln!(s, "}}");
        for validator in &self.validators {
            let _ = writeln!(s, "fn {}(){{", validator.fn_name);
            let _ = writeln!(s, "    let msg = \"\"");
            for (i, (cond, message)) in validator.checks.iter().enumerate() {
                let keyword = if i == 0 { "if" } else { "elif" };
                let _ = writeln!(s, "    {keyword} !({}) {{ msg = {} }}", self.cond_expr(validator.surface, cond), lit(message));
            }
            let _ = writeln!(s, "    ui.{}.set_text(msg)", validator.error_widget);
            let _ = writeln!(s, "    ui.{}.set_visible(msg != \"\")", validator.error_widget);
            let _ = writeln!(s, "    return msg == \"\"");
            let _ = writeln!(s, "}}");
        }
        // Every input check of a surface; a press is blocked while any fails.
        for surface in 0..surfaces.len() {
            let _ = writeln!(s, "fn a2ui_valid_s{surface}(){{");
            let _ = writeln!(s, "    let passed = true");
            for validator in self.validators.iter().filter(|validator| validator.surface == surface) {
                let _ = writeln!(s, "    if !{}() {{ passed = false }}", validator.fn_name);
            }
            let _ = writeln!(s, "    return passed");
            let _ = writeln!(s, "}}");
        }
        for (handler, setter, validator, is_date_time) in &self.input_handlers {
            let _ = writeln!(s, "fn {handler}(v){{");
            // ISO 8601 joins date and time with `T`; people type a space.
            let value = if *is_date_time { "v.replace(\" \", \"T\")" } else { "v" };
            let _ = writeln!(s, "    {setter}({value})");
            if let Some(validator) = validator {
                let _ = writeln!(s, "    {}()", self.validators[*validator].fn_name);
            }
            let _ = writeln!(s, "}}");
        }
        for choice in &self.choices {
            let name = &self.vars[choice.var].name;
            let f = &choice.fn_name;
            let values: Vec<String> = choice.options.iter().map(|option| lit(&option.value)).collect();
            if choice.is_filterable {
                let _ = writeln!(s, "let {f}_query = \"\"");
                let _ = writeln!(s, "fn {f}_filter(t){{");
                let _ = writeln!(s, "    {f}_query = a2ui_lower(t)");
                let _ = writeln!(s, "    {f}_show()");
                let _ = writeln!(s, "}}");
            }
            let _ = writeln!(s, "fn {f}(i){{");
            let _ = writeln!(s, "    let values = [{}]", values.join(", "));
            let _ = writeln!(s, "    {name} = a2ui_toggle_pick({name}, values[i], {})", choice.max.unwrap_or(0));
            let _ = writeln!(s, "    {f}_changed()");
            let _ = writeln!(s, "}}");
            if choice.style == ChoiceStyle::Checks {
                let _ = writeln!(s, "fn {f}_set(i, c){{");
                let _ = writeln!(s, "    let values = [{}]", values.join(", "));
                let _ = writeln!(s, "    if c != a2ui_has({name}, values[i]) {{ {name} = a2ui_toggle_pick({name}, values[i], 0) }}");
                let _ = writeln!(s, "    {f}_changed()");
                let _ = writeln!(s, "}}");
            }
            let _ = writeln!(s, "fn {f}_changed(){{");
            let _ = writeln!(s, "    {f}_show()");
            let _ = writeln!(s, "    a2ui_export()");
            self.write_refresh(&mut s, choice.var);
            if let Some(validator) = choice.validator {
                let _ = writeln!(s, "    {}()", self.validators[validator].fn_name);
            }
            let _ = writeln!(s, "}}");
            // Each option's state, and only the options matching the filter.
            let _ = writeln!(s, "fn {f}_show(){{");
            for option in &choice.options {
                let has = format!("a2ui_has({name}, {})", lit(&option.value));
                let shown = if choice.is_filterable {
                    format!("{}.search({f}_query) >= 0", lit(&option.label.to_lowercase()))
                } else { "true".into() };
                let w = &option.widget;
                match choice.style {
                    ChoiceStyle::Chips => {
                        let _ = writeln!(s, "    ui.{w}_on.set_visible({has} && {shown})");
                        let _ = writeln!(s, "    ui.{w}_off.set_visible(!{has} && {shown})");
                    }
                    ChoiceStyle::Checks => {
                        let _ = writeln!(s, "    ui.{w}.set_visible({shown})");
                    }
                    ChoiceStyle::Marks => {
                        let is_single = choice.max == Some(1);
                        let _ = writeln!(s, "    ui.{w}.set_text(if {has} {{{}}} else {{{}}})",
                            lit(&choice_mark(true, is_single, &option.label)), lit(&choice_mark(false, is_single, &option.label)));
                        let _ = writeln!(s, "    ui.{w}.set_visible({shown})");
                    }
                }
            }
            let _ = writeln!(s, "}}");
        }
        for (fn_name, count) in &self.tabs {
            let _ = writeln!(s, "fn {fn_name}(i){{");
            for i in 0..*count {
                let _ = writeln!(s, "    ui.{fn_name}_pane_{i}.set_visible(i == {i})");
                let _ = writeln!(s, "    ui.{fn_name}_on_{i}.set_visible(i == {i})");
                let _ = writeln!(s, "    ui.{fn_name}_off_{i}.set_visible(i != {i})");
            }
            let _ = writeln!(s, "}}");
        }
        for action in &self.actions {
            let _ = writeln!(s, "fn {}(){{", action.fn_name);
            let _ = writeln!(s, "    a2ui_sync()");
            if action.local.is_none() && self.validators.iter().any(|validator| validator.surface == action.surface) {
                let _ = writeln!(s, "    if !a2ui_valid_s{}() {{", action.surface);
                let _ = writeln!(s, "        a2ui_note(\"Check the highlighted fields first.\", true)");
                let _ = writeln!(s, "        return nil");
                let _ = writeln!(s, "    }}");
            }
            // A failing check would disable the button; here it blocks the press.
            for (cond, message) in &action.checks {
                let _ = writeln!(s, "    if !({}) {{", self.cond_expr(action.surface, cond));
                let _ = writeln!(s, "        a2ui_note({}, true)", lit(message));
                let _ = writeln!(s, "        return nil");
                let _ = writeln!(s, "    }}");
            }
            match &action.local {
                Some(LocalAction::Open(url)) => {
                    let live = LiveText { widget: String::new(), surface: action.surface, pieces: url.clone() };
                    let _ = writeln!(s, "    a2ui_open({})", self.live_expr(&live));
                    let _ = writeln!(s, "}}");
                    continue;
                }
                Some(LocalAction::Unsupported) => {
                    let _ = writeln!(s, "    a2ui_note(\"This button runs a function Robrix doesn't support.\", true)");
                    let _ = writeln!(s, "}}");
                    continue;
                }
                None => {}
            }
            let mut pieces = Vec::new();
            // Answers to a timeline message always carry a version (the Matrix
            // mapping's shape); a pane post of a v0.8 surface keeps `userAction`.
            let version = action.version.clone().or_else(|| self.inline_event.is_some().then(|| "v0.8".to_string()));
            let envelope = match version {
                Some(version) => format!("{{\"version\":{},\"action\":{{", Value::String(version)),
                None => "{\"userAction\":{".to_string(),
            };
            pieces.push(Piece::Lit(format!(
                "{envelope}\"name\":{},\"surfaceId\":{},\"sourceComponentId\":{},\"timestamp\":",
                Value::String(action.name.clone()), Value::String(action.surface_id.clone()),
                Value::String(action.source_id.clone()),
            )));
            pieces.push(Piece::Expr("a2ui_now().to_json()".into()));
            pieces.push(Piece::Lit(",\"context\":{".into()));
            for (i, (key, bound)) in action.context.iter().enumerate() {
                if i > 0 { pieces.push(Piece::Lit(",".into())); }
                pieces.push(Piece::Lit(format!("{}:", Value::String(key.clone()))));
                match &bound.template {
                    Some(template) => {
                        let live = LiveText { widget: String::new(), surface: action.surface, pieces: template.clone() };
                        pieces.push(Piece::Expr(format!("({}).to_json()", self.live_expr(&live))));
                    }
                    None => self.json_expr(action.surface, bound.path.as_deref(), &bound.value, &mut pieces),
                }
            }
            pieces.push(Piece::Lit("}}".into()));
            let surface = &surfaces[action.surface];
            if action.version.is_some() && surface.sends_data_model {
                pieces.push(Piece::Lit(",\"dataModel\":".into()));
                self.json_expr(action.surface, Some("/"), &surface.data, &mut pieces);
            }
            pieces.push(Piece::Lit("}".into()));
            let _ = writeln!(s, "    a2ui_send({}, {})", join_pieces(&pieces), lit(&action.name));
            let _ = writeln!(s, "}}");
        }
        s.push('\n');
        if self.is_inline {
            let _ = writeln!(s, "View{{");
            let _ = writeln!(s, "    width: Fill height: Fit flow: Down");
            let _ = writeln!(s, "    View{{");
            let _ = writeln!(s, "        width: Fill height: Fit flow: Down spacing: 10 padding: 10");
        } else {
            let _ = writeln!(s, "SolidView{{");
            let _ = writeln!(s, "    width: Fill height: Fill flow: Down new_batch: true draw_bg +: {{ color: #xFFFFFF }}");
            let _ = writeln!(s, "    ScrollYView{{");
            let _ = writeln!(s, "        width: Fill height: Fill flow: Down spacing: 10 padding: 14");
        }
        s.push_str(body);
        if self.is_truncated {
            write_label(&mut s, "        ", None, "Fill", "This A2UI surface was too large to show in full.", 9.0, false, MUTED_COLOR);
        }
        if self.is_inline {
            let _ = writeln!(s, "        {STATE_WIDGET} := Label{{ visible: false text: \"\" }}");
        }
        let _ = writeln!(s, "        a2ui_status_wrap := View{{ width: Fill height: Fit visible: false");
        let _ = writeln!(s, "            a2ui_status := Label{{ width: Fill padding: 0 margin: 0 text: \"\" draw_text +: {{ color: {MUTED_COLOR} text_style: theme.font_regular{{font_size: 9}} }} }}");
        let _ = writeln!(s, "        }}");
        let _ = writeln!(s, "        a2ui_error_wrap := View{{ width: Fill height: Fit visible: false");
        let _ = writeln!(s, "            a2ui_error := Label{{ width: Fill padding: 0 margin: 0 text: \"\" draw_text +: {{ color: {ERROR_COLOR} text_style: theme.font_regular{{font_size: 9}} }} }}");
        let _ = writeln!(s, "        }}");
        let _ = writeln!(s, "    }}");
        let _ = writeln!(s, "}}");
        s
    }

    /// Re-renders every live label showing the variable's path.
    fn write_refresh(&self, s: &mut String, var: usize) {
        let Some(((surface, path), _)) = self.var_index.iter().find(|(_, v)| **v == var) else { return };
        let shows = |live: &&LiveText| live.surface == *surface
            && live.pieces.iter().any(|piece| matches!(piece, TextPiece::Path(shown, _) if shown == path));
        for live in self.live_texts.iter().filter(shows) {
            let _ = writeln!(s, "    ui.{}.set_text({})", live.widget, self.live_expr(live));
        }
    }

    /// A Splash expression for a live label's text, reading input variables.
    fn live_expr(&self, live: &LiveText) -> String {
        let parts: Vec<String> = live.pieces.iter().map(|piece| match piece {
            TextPiece::Static(text) => lit(text),
            TextPiece::Path(path, now) => match self.var_index.get(&(live.surface, path.clone())) {
                Some(&var) => self.display_expr(var),
                None => lit(now),
            },
        }).collect();
        if parts.is_empty() { lit("") } else { parts.join(" + ") }
    }

    /// A variable's value as display text.
    fn display_expr(&self, var: usize) -> String {
        let name = &self.vars[var].name;
        match self.vars[var].kind {
            VarKind::Text | VarKind::Number => format!("(\"\" + {name})"),
            VarKind::Bool => format!("(if {name} {{\"true\"}} else {{\"false\"}})"),
            VarKind::List => format!("a2ui_join({name})"),
        }
    }
}

/// Controls styled for the white surface: the theme defaults draw light text.
const STYLES: &str = r#"let A2uiButton = Button{
    padding: Inset{left: 12 right: 12 top: 8 bottom: 8}
    draw_bg +: {
        border_size: 1.0 border_radius: 4.0
        color: #xFFFFFF color_hover: #xF0F5FF color_down: #xE4EEFF color_focus: #xFFFFFF
        border_color: #xC8C8C8 border_color_hover: $TINT border_color_down: $TINT border_color_focus: #xC8C8C8
        color_2: vec4(-1.0, -1.0, -1.0, -1.0) border_color_2: vec4(-1.0, -1.0, -1.0, -1.0)
    }
    draw_text +: { color: #x1C274C color_hover: #x1C274C color_down: #x1C274C color_focus: #x1C274C text_style: theme.font_regular{font_size: 10} }
}
let A2uiPrimary = A2uiButton{
    draw_bg +: {
        border_size: 0.0
        color: $TINT color_hover: $HOVER color_down: $DOWN color_focus: $TINT
    }
    draw_text +: { color: #xFFFFFF color_hover: #xFFFFFF color_down: #xFFFFFF color_focus: #xFFFFFF }
}
let A2uiFlat = A2uiButton{
    draw_bg +: { border_size: 0.0 color: #xFFFFFF color_hover: #xF0F5FF color_down: #xE4EEFF color_focus: #xFFFFFF }
    draw_text +: { color: $TINT color_hover: $HOVER color_down: $DOWN color_focus: $TINT }
}
let A2uiChip = A2uiButton{
    padding: Inset{left: 10 right: 10 top: 5 bottom: 5}
    draw_bg +: { border_radius: 12.0 }
}
let A2uiChipOn = A2uiChip{
    draw_bg +: {
        color: $TINT color_hover: $HOVER color_down: $DOWN color_focus: $TINT
        border_color: $TINT border_color_hover: $HOVER border_color_down: $DOWN border_color_focus: $TINT
    }
    draw_text +: { color: #xFFFFFF color_hover: #xFFFFFF color_down: #xFFFFFF color_focus: #xFFFFFF }
}
let A2uiOption = A2uiFlat{
    padding: Inset{left: 4 right: 8 top: 4 bottom: 4}
    draw_text +: { color: #x1C274C color_hover: #x1C274C color_down: #x1C274C color_focus: #x1C274C }
}
let A2uiSlider = Slider{
    draw_text +: { color: #x66708F color_hover: #x1C274C color_drag: #x1C274C color_focus: #x1C274C color_disabled: #x66708F }
}
let A2uiField = TextInput{
    width: Fill height: Fit padding: 10
    draw_bg +: {
        border_radius: 4.0 border_size: 1.0
        color: #xFFFFFF color_hover: #xFFFFFF color_focus: #xFFFFFF color_down: #xFFFFFF color_empty: #xFFFFFF
        border_color: #xC8C8C8 border_color_hover: $TINT border_color_focus: $TINT border_color_down: $TINT border_color_empty: #xC8C8C8
        color_2: vec4(-1.0, -1.0, -1.0, -1.0) border_color_2: vec4(-1.0, -1.0, -1.0, -1.0)
    }
    draw_text +: { color: #x333333 color_hover: #x333333 color_focus: #x333333 color_down: #x333333 color_empty: #xBBBBBB color_empty_hover: #x999999 color_empty_focus: #x999999 text_style: theme.font_regular{font_size: 11} }
}
let A2uiCheck = CheckBox{
    draw_text +: { color: #x1C274C color_hover: #x1C274C color_down: #x1C274C color_focus: #x1C274C color_active: #x1C274C color_disabled: #x66708F text_style: theme.font_regular{font_size: 10} }
}
"#;

/// Runtime helpers every translated app carries.
const RUNTIME: &str = r#"
fn a2ui_note(text, is_error){
    ui.a2ui_status.set_text(if is_error {""} else {text})
    ui.a2ui_error.set_text(if is_error {text} else {""})
    ui.a2ui_status_wrap.set_visible(!is_error && text != "")
    ui.a2ui_error_wrap.set_visible(is_error && text != "")
}

fn a2ui_send(body, name){
    if a2ui_sending { return nil }
    a2ui_sending = true
    a2ui_note("Sending " + name + "…", false)
    a2ui_post(body, fn(r){
        a2ui_sending = false
        if r.is_ok {
            a2ui_note("Sent " + name + " to the room.", false)
            return nil
        }
        a2ui_fail(r.error)
    })
}

fn a2ui_fail(error){
    if error.search("not available here") >= 0 { a2ui_note("This interactive message can't answer from here.", true) }
    elif error.search("needs {event_id}") >= 0 { a2ui_note("This message hasn't finished sending yet.", true) }
    elif error.search("not attached") >= 0 { a2ui_note("Open this app from a room to send its actions.", true) }
    else { a2ui_note(error, true) }
}

fn a2ui_open(url){
    host.request("url.open", {url: url}, fn(r){
        if !r.is_ok { a2ui_fail(r.error) }
    })
}

fn a2ui_pad(n){ return if n < 10 {"0" + n} else {"" + n} }

// local_time() applies one fixed UTC offset, so epoch 0 reveals it.
fn a2ui_now(){
    let e = local_time(0)
    let offset = e.hour * 3600 + e.minute * 60 + e.second
    if e.year < 1970 { offset = offset - 86400 }
    let u = local_time(time_now() - offset)
    return "" + u.year + "-" + a2ui_pad(u.month) + "-" + a2ui_pad(u.day) + "T" + a2ui_pad(u.hour) + ":" + a2ui_pad(u.minute) + ":" + a2ui_pad(u.second) + "Z"
}

fn a2ui_present(v){
    if v == nil { return false }
    if v.is_string() { return v.trim() != "" }
    if v.is_array() { return v.len() > 0 }
    if v.is_bool() { return v }
    return true
}

// Patterns are checked at translation; very long values aren't matched.
fn a2ui_re(pattern, v){
    let text = "" + v
    if text.len() > 4096 { return true }
    return regex(pattern, "").test(text)
}

// ISO 8601 text sorts like time; only the shared prefix is compared.
fn a2ui_iso_cmp(a, b){
    let digits = "0123456789"
    let x = ("" + a).split("")
    let y = ("" + b).split("")
    for i in 1..(min(x.len(), y.len()) - 1) {
        if x[i] != y[i] { return digits.search(x[i]) - digits.search(y[i]) }
    }
    return 0
}

fn a2ui_iso_in(v, lo, hi){
    if lo != nil && a2ui_iso_cmp(v, lo) < 0 { return false }
    if hi != nil && a2ui_iso_cmp(v, hi) > 0 { return false }
    return true
}

fn a2ui_length_ok(v, lo, hi){
    let n = max(("" + v).split("").len() - 2, 0)
    if lo != nil && n < lo { return false }
    if hi != nil && n > hi { return false }
    return true
}

fn a2ui_numeric_ok(v, lo, hi){
    let text = ("" + v).trim()
    if !a2ui_re("^-?[0-9]+([.][0-9]+)?$", text) { return false }
    let n = text.to_f64()
    if lo != nil && n < lo { return false }
    if hi != nil && n > hi { return false }
    return true
}

let a2ui_upper = "ABCDEFGHIJKLMNOPQRSTUVWXYZ"
let a2ui_lower_az = "abcdefghijklmnopqrstuvwxyz".split("")

fn a2ui_lower(text){
    let out = ""
    for c in text.split("") {
        let i = if c.len() == 1 {a2ui_upper.search(c)} else {-1}
        if i >= 0 { out = out + a2ui_lower_az[i + 1] } else { out = out + c }
    }
    return out
}

fn a2ui_join(items){
    let out = ""
    for item in items {
        if out != "" { out = out + ", " }
        out = out + item
    }
    return out
}

fn a2ui_has(items, value){
    for item in items { if item == value { return true } }
    return false
}

fn a2ui_toggle_pick(items, value, max){
    if max == 1 && a2ui_has(items, value) { return items }
    if a2ui_has(items, value) {
        let kept = []
        for item in items { if item != value { kept.push(item) } }
        return kept
    }
    if max == 1 { return [value] }
    if max > 0 && items.len() >= max { return items }
    let next = []
    for item in items { next.push(item) }
    next.push(value)
    return next
}
"#;

enum Piece { Lit(String), Expr(String) }

/// Joins literal JSON fragments and runtime expressions into one Splash expression.
fn join_pieces(pieces: &[Piece]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut pending = String::new();
    for piece in pieces {
        match piece {
            Piece::Lit(text) => pending.push_str(text),
            Piece::Expr(expr) => {
                if !pending.is_empty() { parts.push(lit(&std::mem::take(&mut pending))); }
                parts.push(expr.clone());
            }
        }
    }
    if !pending.is_empty() || parts.is_empty() { parts.push(lit(&pending)); }
    parts.join(" + ")
}

fn write_label(out: &mut String, pad: &str, name: Option<&str>, width: &str, text: &str, size: f64, is_bold: bool, color: &str) {
    write_aligned_label(out, pad, name, width, text, size, is_bold, color, 0.0);
}

fn write_aligned_label(out: &mut String, pad: &str, name: Option<&str>, width: &str, text: &str, size: f64, is_bold: bool, color: &str, x: f64) {
    let font = if is_bold { "theme.font_bold" } else { "theme.font_regular" };
    let name = name.map(|n| format!("{n} := ")).unwrap_or_default();
    let align = if x > 0.0 { format!(" align: Align{{x: {x}}}") } else { String::new() };
    let _ = writeln!(out, "{pad}{name}Label{{ width: {width}{align} padding: 0 margin: 0 text: {} draw_text +: {{ color: {color} text_style: {font}{{font_size: {size:?}}} }} }}", lit(text));
}

/// A Splash string literal.
fn lit(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// v1.0 literal data with its escaped keys restored (`@@key` is `@key`).
fn unescape_v1(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(map.iter()
            .map(|(key, value)| (key.strip_prefix('@').filter(|k| k.starts_with('@')).unwrap_or(key).to_string(), unescape_v1(value)))
            .collect()),
        Value::Array(items) => Value::Array(items.iter().map(unescape_v1).collect()),
        other => other.clone(),
    }
}

/// A Splash float literal: finite, bounded and never in exponent form.
fn num(value: f64) -> String {
    let value = if value.is_finite() { value.clamp(-1e9, 1e9) } else { 0.0 };
    format!("{:?}", (value * 1e4).round() / 1e4)
}

/// A data value's initial Splash form for its variable's kind.
fn splash_value(value: &Value, kind: VarKind) -> String {
    match kind {
        VarKind::Text => lit(&display(value)),
        VarKind::Number => num(value.as_f64().unwrap_or(0.0)),
        VarKind::Bool => value.as_bool().unwrap_or(false).to_string(),
        VarKind::List => {
            let items: Vec<String> = value.as_array().into_iter().flatten().map(|v| lit(&display(v))).collect();
            format!("[{}]", items.join(", "))
        }
    }
}

/// A value as text: strings bare, whole numbers without a fraction.
fn display(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Number(n) => match n.as_f64() {
            Some(f) if f.fract() == 0.0 && f.abs() < 1e15 => format!("{}", f as i64),
            _ => n.to_string(),
        },
        other => other.to_string(),
    }
}

/// `mxc://server/media-id`, bounded and free of path tricks.
fn is_mxc_url(url: &str) -> bool {
    url.len() <= 512 && url.strip_prefix("mxc://").and_then(|rest| rest.split_once('/')).is_some_and(|(server, id)|
        !server.is_empty() && !id.is_empty()
            && server.chars().all(|c| c.is_ascii_alphanumeric() || ".-:[]".contains(c))
            && id.chars().all(|c| c.is_ascii_alphanumeric() || "_-".contains(c)))
}

/// Whether a payload pattern is safe to run: small once its counted
/// repetitions are expanded, free of `(?` constructs the engine misreads
/// (lookaround, named groups, inline flags), and accepted by Splash's own
/// regex engine, where a compile error would be a script error.
fn is_valid_regex(pattern: &str) -> bool {
    use makepad_widgets::makepad_script::regex::{RegexFlags, ScriptRegexData};
    pattern.len() <= 512
        && regex_program_size(pattern).is_some_and(|size| size <= MAX_REGEX_PROGRAM)
        && ScriptRegexData::new(pattern, RegexFlags::default()).is_ok()
}

const MAX_REGEX_PROGRAM: u64 = 2000;

/// An upper bound on a pattern's compiled size, mirroring makepad's regex
/// parser: counted repetitions copy what they repeat (`(a{1000}){1000}` is
/// tiny text but a million ops), and a quantifier binds to the atom before it
/// even right after `(`. `None` for `(?` other than `(?:`, which it misreads.
fn regex_program_size(pattern: &str) -> Option<u64> {
    let chars: Vec<char> = pattern.chars().collect();
    // The size of each open group so far, and of the last atom or group.
    let (mut groups, mut last, mut i) = (vec![0u64], 0u64, 0);
    let add = |groups: &mut Vec<u64>, size: u64| {
        if let Some(top) = groups.last_mut() { *top = top.saturating_add(size); }
    };
    while i < chars.len() {
        match chars[i] {
            '\\' => { i += 1; last = 1; add(&mut groups, 1); }
            '[' => {
                i += 1;
                if chars.get(i) == Some(&'^') { i += 1; }
                if chars.get(i) == Some(&']') { i += 1; }
                while i < chars.len() && chars[i] != ']' {
                    if chars[i] == '\\' { i += 1; }
                    i += 1;
                }
                last = 1;
                add(&mut groups, 1);
            }
            '(' => {
                if chars.get(i + 1) == Some(&'?') {
                    if chars.get(i + 2) != Some(&':') { return None; }
                    i += 2;
                }
                groups.push(0);
            }
            ')' => {
                let inner = groups.pop().unwrap_or(0).max(1);
                if groups.is_empty() { groups.push(0); }
                last = inner;
                add(&mut groups, inner);
            }
            '{' => {
                let end = chars[i..].iter().position(|&c| c == '}').map(|p| i + p);
                let counts = end.map(|end| chars[i + 1..end].iter().collect::<String>());
                let parsed = counts.as_deref().and_then(|counts| {
                    let (low, high) = counts.split_once(',').map_or((counts, Some(counts)), |(l, h)| (l, Some(h)));
                    let low: u64 = low.trim().parse().ok()?;
                    let high = match high.map(str::trim) { Some("") => low + 1, Some(h) => h.parse().ok()?, None => low };
                    Some(low.max(high))
                });
                match (end, parsed) {
                    (Some(_), Some(count)) if count > 1000 => return None,
                    (Some(end), Some(count)) => {
                        let grown = last.max(1).saturating_mul(count.max(1));
                        add(&mut groups, grown.saturating_sub(last));
                        last = grown;
                        i = end;
                    }
                    _ => { last = 1; add(&mut groups, 1); }
                }
            }
            '*' | '+' | '?' | '|' => {}
            _ => { last = 1; add(&mut groups, 1); }
        }
        i += 1;
    }
    Some(groups.iter().fold(0u64, |total, size| total.saturating_add(*size)))
}

const EMAIL_PATTERN: &str = "^[^@ ]+@[^@ ]+[.][^@ ]+$";
const ISO_8601_PATTERN: &str =
    "^([0-9]{4}-[0-9]{2}-[0-9]{2})?([T ]?[0-9]{2}:[0-9]{2}(:[0-9]{2}([.][0-9]+)?)?(Z|[+-][0-9]{2}(:?[0-9]{2})?)?)?$";

/// A value as a Splash literal.
fn splash_literal(value: &Value) -> String {
    match value {
        Value::Null => "nil".into(),
        Value::Bool(value) => value.to_string(),
        Value::Number(n) => num(n.as_f64().unwrap_or(0.0)),
        Value::String(text) => lit(text),
        Value::Array(items) => format!("[{}]", items.iter().take(256).map(splash_literal).collect::<Vec<_>>().join(", ")),
        Value::Object(_) => lit(&value.to_string()),
    }
}

fn opt_num(value: Option<f64>) -> String {
    value.map_or_else(|| "nil".into(), num)
}

/// A choice option's text with its radio or checkbox mark.
fn choice_mark(is_on: bool, is_single: bool, label: &str) -> String {
    let mark = match (is_single, is_on) { (true, true) => "🔘", (true, false) => "⚪", (false, true) => "✅", (false, false) => "⬜" };
    format!("{mark} {label}")
}

/// The label under an input showing its first failing check.
fn write_error_label(out: &mut String, pad: &str, width: &str, validator: usize) {
    let _ = writeln!(out, "{pad}    a2ui_err{validator} := Label{{ visible: false width: {width} padding: 0 margin: 0 text: \"\" draw_text +: {{ color: {ERROR_COLOR} text_style: theme.font_regular{{font_size: 9}} }} }}");
}

/// SVG path data only: commands, numbers and separators, bounded in size.
fn is_svg_path(d: &str) -> bool {
    !d.is_empty() && d.len() <= 4096
        && d.chars().all(|c| c.is_ascii_digit() || " ,.-+eEMmLlHhVvCcSsQqTtAaZz".contains(c))
}

/// A link the host may open for a button: web or mail.
fn is_openable_url(url: &str) -> bool {
    is_web_url(url) || (url.trim().to_ascii_lowercase().starts_with("mailto:") && url.len() <= 2048)
}

fn is_web_url(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    (lower.starts_with("https://") || lower.starts_with("http://")) && url.len() <= 2048
}

fn pieces_text(pieces: &[TextPiece]) -> String {
    pieces.iter().map(|piece| match piece { TextPiece::Static(text) | TextPiece::Path(_, text) => text.as_str() }).collect()
}

/// A leading Markdown heading (`# Title`) as its text and heading variant.
fn markdown_heading(text: &str) -> (String, Option<&'static str>) {
    let hashes = text.chars().take_while(|c| *c == '#').count();
    match (hashes, text[hashes..].strip_prefix(' ')) {
        (1..=5, Some(rest)) if !text.contains('\n') => {
            (rest.trim().to_string(), Some(["h1", "h2", "h3", "h4", "h5"][hashes - 1]))
        }
        _ => (text.to_string(), None),
    }
}

/// Plain text from the simple Markdown A2UI text may carry: emphasis and code
/// markers dropped, `[label](url)` links shown as their label.
fn strip_markdown(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        let Some(close) = rest[open..].find("](").map(|i| open + i) else { break };
        let Some(end) = rest[close..].find(')').map(|i| close + i) else { break };
        out.push_str(&rest[..open]);
        out.push_str(&rest[open + 1..close]);
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    let out = out.replace('`', "");
    let out = strip_emphasis(&strip_emphasis(&out, "**"), "*");
    out.lines().map(|line| {
        let hashes = line.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&hashes) && line[hashes..].starts_with(' ') { line[hashes + 1..].to_string() } else { line.to_string() }
    }).collect::<Vec<_>>().join("\n")
}

/// Drops paired emphasis `marker`s (`*` or `**`): one that opens at a word's
/// start before a non-space and one that closes after a non-space, leaving
/// `5 * 3` and `2**10` alone.
fn strip_emphasis(text: &str, marker: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let m: Vec<char> = marker.chars().collect();
    let at = |i: usize| chars.get(i..i + m.len()).is_some_and(|s| s == m.as_slice());
    let mut keep = vec![true; chars.len()];
    let mut i = 0;
    while i < chars.len() {
        let opens = at(i) && (i == 0 || !chars[i - 1].is_alphanumeric())
            && chars.get(i + m.len()).is_some_and(|c| !c.is_whitespace() && *c != '*');
        if opens && let Some(close) = (i + m.len() + 1..chars.len()).find(|&j| at(j) && !chars[j - 1].is_whitespace()
            && chars.get(j + m.len()).is_none_or(|c| !c.is_alphanumeric()))
        {
            keep[i..i + m.len()].iter_mut().for_each(|k| *k = false);
            keep[close..close + m.len()].iter_mut().for_each(|k| *k = false);
            i = close + m.len();
            continue;
        }
        i += 1;
    }
    chars.into_iter().zip(keep).filter_map(|(c, keep)| keep.then_some(c)).collect()
}

/// `1234567.891` with `decimals` places and, if `grouping`, thousands separators.
fn format_number(value: f64, decimals: usize, grouping: bool) -> String {
    let value = if value.is_finite() { value } else { 0.0 };
    let text = format!("{:.decimals$}", value.abs());
    let (whole, fraction) = text.split_once('.').map_or((text.as_str(), None), |(w, f)| (w, Some(f)));
    let mut grouped = String::new();
    for (i, digit) in whole.chars().enumerate() {
        if grouping && i > 0 && (whole.len() - i) % 3 == 0 { grouped.push(','); }
        grouped.push(digit);
    }
    let sign = if value < 0.0 && text.chars().any(|c| c.is_ascii_digit() && c != '0') { "-" } else { "" };
    match fraction {
        Some(fraction) => format!("{sign}{grouped}.{fraction}"),
        None => format!("{sign}{grouped}"),
    }
}

/// A formatted amount with its currency's symbol, or its ISO code.
fn currency_text(code: &str, amount: &str) -> String {
    let symbol = match code.to_ascii_uppercase().as_str() {
        "USD" | "AUD" | "CAD" | "NZD" | "HKD" | "SGD" | "MXN" => "$",
        "EUR" => "€", "GBP" => "£", "JPY" | "CNY" => "¥", "INR" => "₹", "KRW" => "₩",
        "CHF" => "CHF ", "" => "",
        _ => return format!("{amount} {}", code.to_ascii_uppercase()),
    };
    match amount.strip_prefix('-') {
        Some(positive) => format!("-{symbol}{positive}"),
        None => format!("{symbol}{amount}"),
    }
}

/// An ISO 8601 date, time or date-time in a Unicode TR35 pattern
/// (`yyyy`, `MMM`, `d`, `EEE`, `h`, `mm`, `a`, ...); `None` if unparsable.
fn format_date(value: &str, pattern: &str) -> Option<String> {
    const MONTHS: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July",
        "August", "September", "October", "November", "December"];
    const DAYS: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
    let value = value.trim();
    let (date, time) = match value.split_once(['T', ' ']) {
        Some((date, time)) => (Some(date), Some(time)),
        None if value.contains(':') => (None, Some(value)),
        None => (Some(value), None),
    };
    let num = |text: &str| text.parse::<i64>().ok();
    let (year, month, day) = match date {
        Some(date) => {
            let mut parts = date.split('-');
            (num(parts.next()?)?, num(parts.next()?)?, num(parts.next()?)?)
        }
        None => (1970, 1, 1),
    };
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) { return None; }
    let (hour, minute, second) = match time {
        Some(time) => {
            let time = time.trim_end_matches('Z');
            let time = time.split(['+', '-']).next()?;
            let mut parts = time.split(':');
            let hour = num(parts.next()?)?;
            let minute = num(parts.next().unwrap_or("0"))?;
            let second = parts.next().and_then(|s| s.split('.').next()).and_then(num).unwrap_or(0);
            (hour, minute, second)
        }
        None => (0, 0, 0),
    };
    // Days since 1970-01-01 (Howard Hinnant's days_from_civil), for the weekday.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let days = era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719468;
    let weekday = (days + 4).rem_euclid(7) as usize;
    let mut out = String::new();
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' {
            if chars.get(i + 1) == Some(&'\'') {
                out.push('\'');
                i += 2;
                continue;
            }
            // A quoted literal, where `''` is a single quote.
            i += 1;
            while i < chars.len() {
                if chars[i] == '\'' {
                    if chars.get(i + 1) == Some(&'\'') { out.push('\''); i += 2; continue; }
                    break;
                }
                out.push(chars[i]);
                i += 1;
            }
            i += 1;
            continue;
        }
        let run = chars[i..].iter().take_while(|&&r| r == c).count();
        let h12 = if hour % 12 == 0 { 12 } else { hour % 12 };
        let field = match (c, run) {
            // The week-based year `Y` is the calendar year except in a year's last or first days.
            ('y' | 'Y', 2) => format!("{:02}", year.rem_euclid(100)),
            ('y' | 'Y', _) => year.to_string(),
            ('M', 1) => month.to_string(),
            ('M', 2) => format!("{month:02}"),
            ('M', 3) => MONTHS[month as usize - 1][..3].to_string(),
            ('M', _) => MONTHS[month as usize - 1].to_string(),
            ('d', 1) => day.to_string(),
            ('d', _) => format!("{day:02}"),
            ('E', 1..=3) => DAYS[weekday][..3].to_string(),
            ('E', _) => DAYS[weekday].to_string(),
            ('H', 1) => hour.to_string(),
            ('H', _) => format!("{hour:02}"),
            ('h', 1) => h12.to_string(),
            ('h', _) => format!("{h12:02}"),
            ('m', 1) => minute.to_string(),
            ('m', _) => format!("{minute:02}"),
            ('s', 1) => second.to_string(),
            ('s', _) => format!("{second:02}"),
            ('a', _) => if hour < 12 { "AM" } else { "PM" }.to_string(),
            _ => std::iter::repeat_n(c, run).collect(),
        };
        out.push_str(&field);
        i += run;
    }
    Some(out)
}

fn short_url(url: &str) -> String {
    let rest = url.split("://").nth(1).unwrap_or(url);
    let short: String = rest.chars().take(60).collect();
    if short.is_empty() { "image".into() } else { short }
}

fn str_prop<'a>(props: &'a Map<String, Value>, keys: &[&str]) -> &'a str {
    keys.iter().find_map(|key| props.get(*key).and_then(|v| v.as_str()
        .or_else(|| v.get("literalString").and_then(Value::as_str)))).unwrap_or("")
}

fn num_prop(props: &Map<String, Value>, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|key| props.get(*key).and_then(|v| v.as_f64()
        .or_else(|| v.get("literalNumber").and_then(Value::as_f64))))
}

/// The standard catalog's icon names as emoji (a glyph the app font has).
fn icon_emoji(name: &str) -> &'static str {
    let name = name.to_ascii_lowercase().replace(['_', '-', ' '], "");
    let icons: BTreeMap<&str, &str> = [
        ("accountcircle", "👤"), ("person", "👤"), ("add", "➕"), ("arrowback", "⬅️"), ("arrowforward", "➡️"),
        ("attachfile", "📎"), ("calendartoday", "📅"), ("calendar", "📅"), ("call", "📞"), ("phone", "📞"),
        ("camera", "📷"), ("check", "✔️"), ("close", "❌"), ("delete", "🗑️"), ("download", "⬇️"),
        ("edit", "✏️"), ("event", "📅"), ("error", "⛔"), ("favorite", "❤️"), ("favoriteoff", "🤍"),
        ("folder", "📁"), ("help", "❓"), ("home", "🏠"), ("info", "ℹ️"), ("locationon", "📍"),
        ("location", "📍"), ("lock", "🔒"), ("lockopen", "🔓"), ("mail", "✉️"), ("email", "✉️"),
        ("menu", "☰"), ("morevert", "⋮"), ("morehoriz", "⋯"), ("notificationsoff", "🔕"),
        ("notifications", "🔔"), ("payment", "💳"), ("photo", "🖼️"), ("print", "🖨️"), ("refresh", "🔄"),
        ("search", "🔍"), ("send", "📤"), ("settings", "⚙️"), ("share", "🔗"), ("shoppingcart", "🛒"),
        ("star", "⭐"), ("starhalf", "⭐"), ("staroff", "☆"), ("upload", "⬆️"), ("visibility", "👁️"),
        ("visibilityoff", "🙈"), ("warning", "⚠️"), ("restaurant", "🍽️"), ("time", "🕒"), ("schedule", "🕒"),
        ("play", "▶️"), ("pause", "⏸️"), ("stop", "⏹️"), ("skipnext", "⏭️"), ("skipprevious", "⏮️"),
        ("fastforward", "⏩"), ("rewind", "⏪"), ("volumeup", "🔊"), ("volumedown", "🔉"), ("volumemute", "🔇"),
        ("volumeoff", "🔇"), ("flight", "✈️"), ("airplane", "✈️"), ("place", "📍"), ("map", "🗺️"), ("link", "🔗"),
    ].into_iter().collect();
    icons.get(name.as_str()).copied().unwrap_or("🔹")
}
