//! A2UI payloads translate into Splash apps the real parser accepts.

use makepad_widgets::*;

const FIXTURES: [&str; 3] = ["booking_v08.jsonl", "list_v08.json", "form_v09.jsonl"];

fn fixture(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/a2ui").join(name);
    std::fs::read_to_string(path).unwrap()
}

/// Splash's theme is process-wide, so the tests that set one take turns.
static THEME: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn theme_turn() -> std::sync::MutexGuard<'static, ()> {
    THEME.lock().unwrap_or_else(|e| e.into_inner())
}

#[test]
fn a2ui_fixtures_pass_real_splash_validation() {
    let mut cx = Cx::new(Box::new(|_, _| {}));
    cx.with_vm(makepad_widgets::script_mod);
    let _theme = theme_turn();
    a2app_core::palette::use_light_theme();
    for name in FIXTURES {
        let payload = fixture(name);
        assert!(a2app_core::a2ui::is_a2ui(&payload), "{name} not detected");
        let source = a2app_core::a2ui::to_splash(&payload).unwrap();
        if std::env::var("A2UI_DUMP").is_ok() {
            println!("===== {name}\n{source}");
        }
        let errors = makepad_widgets::splash::validate_splash_body_with_host_io(&mut cx, &source);
        assert!(errors.is_empty(), "{name} failed validation: {errors:#?}\n{source}");
    }
}

/// The `messages` of an `io.element.a2ui` room message, as the timeline reads them.
fn event_messages(name: &str) -> String {
    let event: serde_json::Value = serde_json::from_str(&fixture(name)).unwrap();
    event["content"]["messages"].to_string()
}

#[test]
fn matrix_a2ui_events_translate_for_panes_and_the_timeline() {
    let mut cx = Cx::new(Box::new(|_, _| {}));
    cx.with_vm(makepad_widgets::script_mod);
    let _theme = theme_turn();
    a2app_core::palette::use_light_theme();
    for event in ["contact_event.json", "flight_event.json"] {
        let payload = event_messages(event);
        assert!(a2app_core::a2ui::is_a2ui(&payload));
        for source in [a2app_core::a2ui::to_splash(&payload).unwrap(), a2app_core::a2ui::to_inline_splash(&payload, "$surface:test", None).unwrap().source] {
            if std::env::var("A2UI_DUMP").is_ok() {
                println!("===== {event}\n{source}");
            }
            let errors = makepad_widgets::splash::validate_splash_body_with_host_io(&mut cx, &source);
            assert!(errors.is_empty(), "{event}: {errors:#?}\n{source}");
        }
    }
    let payload = event_messages("contact_event.json");
    let manifest = a2app_core::bundle::parse(&payload).unwrap();
    assert_eq!(manifest.name, "Contact us");
}

#[test]
fn a2ui_imports_as_a_bare_app_with_send_permission() {
    let manifest = a2app_core::bundle::parse(&fixture("booking_v08.jsonl")).unwrap();
    assert_eq!(manifest.name, "Book a table");
    assert!(manifest.permissions.iter().any(|p| p == "matrix-room-send"), "{:?}", manifest.permissions);
    assert_eq!(manifest.tint, 0xE05D2F);
}

#[test]
fn chat_messages_yield_their_fenced_a2ui_block() {
    let body = format!("Here is the form:\n```a2ui\n{}\n```\nThanks!", fixture("form_v09.jsonl"));
    let block = a2app_core::a2ui::find_block(&body).unwrap();
    assert!(block.starts_with("{\"version\""));
    assert!(a2app_core::a2ui::find_block("just talking about surfaceUpdate messages").is_none());
    assert!(a2app_core::a2ui::find_block("```json\n{\"surfaceUpdate\": 1}\n```").is_none());
}

#[test]
fn updates_from_later_events_apply_in_order() {
    // How the timeline hands over a surface: its messages, then each update's.
    let update = |name: &str| serde_json::json!([
        {"version": "v0.9", "updateDataModel": {"surfaceId": "main", "path": "/contact/name", "value": name}}
    ]);
    let original: serde_json::Value = serde_json::from_str(&event_messages("contact_event.json")).unwrap();
    let payload = serde_json::json!([original, update("Bob"), update("Alice")]).to_string();
    let source = a2app_core::a2ui::to_inline_splash(&payload, "$surface:test", None).unwrap().source;
    assert!(source.contains(r#"A2uiField{ text: "Alice""#), "{source}");
}

#[test]
fn the_kitchen_sink_translates_every_catalog_feature() {
    let mut cx = Cx::new(Box::new(|_, _| {}));
    cx.with_vm(makepad_widgets::script_mod);
    let _theme = theme_turn();
    a2app_core::palette::use_light_theme();
    let payload = event_messages("kitchen_sink_event.json");
    let pane = a2app_core::a2ui::to_splash(&payload).unwrap();
    let inline = a2app_core::a2ui::to_inline_splash(&payload, "$sink:test", None).unwrap();
    for source in [&pane, &inline.source] {
        if std::env::var("A2UI_DUMP").is_ok() {
            println!("===== kitchen_sink_event.json\n{source}");
        }
        let errors = makepad_widgets::splash::validate_splash_body_with_host_io(&mut cx, source);
        assert!(errors.is_empty(), "{errors:#?}\n{source}");
    }
    let source = &inline.source;
    for text in [
        "\"Kitchen sink\"", "Hello, Ada! You have 3 items.", "Total: €1,234.50", "Ships Sat, Oct 24",
        "many items", "1. First", "2. Second", "🖼 A web photo", "▶️", "⏸️", "step: 1.0 precision: 0",
    ] {
        assert!(source.contains(text), "missing {text:?}\n{source}");
    }
    assert_eq!(inline.images, [a2app_core::a2ui::InlineImage {
        widget: inline.images[0].widget.clone(), url: "mxc://example.org/abcDEF123".into(),
    }]);
    assert!(source.contains(&format!("{} := Image{{", inline.images[0].widget)));
    // Panes can't load host media: there the picture is a label.
    assert!(!pane.contains("Image{"));
}

/// Robrix's dark theme as far as a surface can tell: every palette color changes.
fn use_dark_theme() {
    let dark = a2app_core::palette::LIGHT.map(|color| (!color & 0xFFFFFF00) | (color & 0xFF));
    set_splash_theme(SplashTheme::Dark);
    set_splash_theme_script(SplashThemeScript { theme: a2app_core::palette::theme_script(&dark, true), widgets: String::new() });
}

/// A line of Splash with its string literals emptied.
fn code_of(line: &str) -> String {
    let mut out = String::new();
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        out.push(c);
        if c == '"' {
            while let Some(c) = chars.next() {
                if c == '\\' { chars.next(); } else if c == '"' { out.push(c); break; }
            }
        }
    }
    out
}

#[test]
fn translated_surfaces_take_their_colors_from_the_palette() {
    let mut cx = Cx::new(Box::new(|_, _| {}));
    cx.with_vm(makepad_widgets::script_mod);
    let payloads = FIXTURES.iter().map(|name| (*name, fixture(name)))
        .chain(["contact_event.json", "flight_event.json", "kitchen_sink_event.json"].map(|name| (name, event_messages(name))));
    let mut sources = Vec::new();
    for (name, payload) in payloads {
        sources.push((name, a2app_core::a2ui::to_splash(&payload).unwrap()));
        sources.push((name, a2app_core::a2ui::to_inline_splash(&payload, "$s:test", None).unwrap().source));
    }
    for (name, source) in &sources {
        assert!(a2app_core::palette::is_theme_aware(source), "{name}");
        // A payload's own primaryColor is the only hex color left, in the tint it sets.
        let is_tinted = ["booking_v08.jsonl", "kitchen_sink_event.json"].contains(name);
        for line in source.lines().filter(|line| !line.trim_start().starts_with("//") && code_of(line).contains('#')) {
            assert!(is_tinted && line.starts_with("let a2ui_"), "{name}: a color outside the palette: {line}");
        }
    }
    let _theme = theme_turn();
    use_dark_theme();
    let failures: Vec<String> = sources.iter().filter_map(|(name, source)| {
        let errors = makepad_widgets::splash::validate_splash_body_with_host_io(&mut cx, source);
        (!errors.is_empty()).then(|| format!("{name}: {errors:?}"))
    }).collect();
    a2app_core::palette::use_light_theme();
    assert!(failures.is_empty(), "on the dark theme: {failures:#?}");
}

/// WCAG contrast of two `#xRRGGBB` colors.
fn contrast(a: &str, b: &str) -> f64 {
    let luminance = |hex: &str| {
        let color = u32::from_str_radix(hex.trim_start_matches("#x"), 16).unwrap();
        let channel = |shift: u32| {
            let c = ((color >> shift) & 0xFF) as f64 / 255.0;
            if c <= 0.03928 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * channel(16) + 0.7152 * channel(8) + 0.0722 * channel(0)
    };
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

#[test]
fn a_custom_tint_keeps_its_text_and_borders_readable_on_both_themes() {
    let tinted = |color: &str| inline(&serde_json::json!([
        {"version": "v0.9", "createSurface": {"surfaceId": "s", "theme": {"primaryColor": color}}},
        {"version": "v0.9", "updateComponents": {"surfaceId": "s", "components": [
            {"id": "root", "component": "Button", "variant": "borderless", "child": "go_label", "action": {"event": {"name": "go"}}},
            {"id": "go_label", "component": "Text", "text": "Go"}
        ]}}
    ]).to_string());
    let value = |source: &str, name: &str| source.lines().find_map(|line| line.strip_prefix(&format!("let {name} = "))).unwrap().to_string();
    // A color on Robrix's light and dark panes: `if theme.palette.is_dark {dark} else {light}`, or one for both.
    let by_theme = |source: &str, name: &str| {
        let color = value(source, name);
        let colors: Vec<&str> = color.split(['{', '}', ' ']).filter(|part| part.starts_with("#x")).collect();
        match colors[..] { [dark, light] => (light.to_string(), dark.to_string()), [both] => (both.to_string(), both.to_string()), _ => panic!("{color}") }
    };
    for (tint, on_tint) in [("#7C3AED", "theme.palette.on_accent"), ("#E05D2F", "theme.palette.on_accent"),
        ("#FFD700", "#x1C274C"), ("#1A237E", "theme.palette.on_accent")]
    {
        let source = tinted(tint);
        let hex = format!("#x{}", &tint[1..]);
        assert_eq!(value(&source, "a2ui_tint"), hex, "fills keep the tint");
        assert_eq!(value(&source, "a2ui_on_tint"), on_tint, "{tint}: text on the tint");
        let (light, dark) = by_theme(&source, "a2ui_tint_text");
        assert!(contrast(&light, "#xFFFFFF") >= 3.0 && contrast(&dark, "#x262626") >= 4.5, "{tint}: {light} / {dark}");
        if contrast(&hex, "#xFFFFFF") >= 3.0 { assert_eq!(light, hex, "{tint} reads on the light pane as it is"); }
        // A borderless button's text darkens like the fill when hovered and pressed on light; on dark it keeps its color.
        for (text, fill) in [("a2ui_tint_text_hover", "a2ui_tint_hover"), ("a2ui_tint_text_down", "a2ui_tint_down")] {
            let (text_light, text_dark) = by_theme(&source, text);
            assert!(contrast(&text_light, "#xFFFFFF") > contrast(&light, "#xFFFFFF"), "{tint}: {text} is {text_light} on the light pane");
            if light == hex { assert_eq!(text_light, value(&source, fill), "{tint}: {text} is the shaded tint"); }
            assert_eq!(text_dark, dark, "{tint}: {text} on the dark pane");
        }
        // Hover, focus and selected-chip borders: the fill's own color on light, a line that shows on dark.
        for (line, fill) in [("a2ui_tint_line", "a2ui_tint"), ("a2ui_tint_line_hover", "a2ui_tint_hover"), ("a2ui_tint_line_down", "a2ui_tint_down")] {
            let (light, dark) = by_theme(&source, line);
            assert_eq!(light, value(&source, fill), "{tint}: {line} on the light pane");
            assert!(contrast(&dark, "#x262626") >= 3.0, "{tint}: {line} is {dark} on the dark pane");
            if contrast(&light, "#x262626") >= 3.0 { assert_eq!(dark, light, "{tint}: {line} shows on the dark pane as it is"); }
        }
        assert_eq!(value(&source, "a2ui_focus"), "a2ui_tint_line", "a focused field's border");
    }
    // On light, a tint's hovered and pressed text is the tint 12% and 24% darker.
    let booking = tinted("#E05D2F");
    assert_eq!([by_theme(&booking, "a2ui_tint_text_hover").0, by_theme(&booking, "a2ui_tint_text_down").0], ["#xC55129", "#xAA4623"]);
    let plain = inline(&surface(serde_json::json!([{"id": "root", "component": "Text", "text": "Hi"}]), serde_json::json!({})));
    assert_eq!(value(&plain, "a2ui_tint"), "theme.palette.accent", "no primaryColor: Robrix's accent");
    assert_eq!(value(&plain, "a2ui_tint_text"), "theme.palette.accent_text");
    assert_eq!(value(&plain, "a2ui_tint_text_hover"), "if theme.palette.is_dark {theme.palette.accent_text} else {theme.palette.accent_hover}");
    assert_eq!(value(&plain, "a2ui_tint_text_down"), "if theme.palette.is_dark {theme.palette.accent_text} else {theme.palette.accent_down}");
    assert_eq!(value(&plain, "a2ui_tint_line"), "theme.palette.accent");
    assert_eq!(value(&plain, "a2ui_focus"), "theme.palette.focus");
    assert!(plain.contains("color_hover: a2ui_tint_text_hover color_down: a2ui_tint_text_down"), "a borderless button's text shades");
}

#[test]
fn cards_take_the_card_outline_and_modal_panes_the_border() {
    let source = a2app_core::a2ui::to_splash(&surface(serde_json::json!([
        {"id": "root", "component": "Column", "children": ["card", "modal"]},
        {"id": "card", "component": "Card", "child": "note"},
        {"id": "note", "component": "Text", "text": "In a card"},
        {"id": "modal", "component": "Modal", "trigger": "open", "content": "more"},
        {"id": "open", "component": "Text", "text": "Details"},
        {"id": "more", "component": "Text", "text": "More"}
    ]), serde_json::json!({}))).unwrap();
    // A Card on the surface takes `card_border`; a Modal pane's card_subtle fill keeps the stronger `border`.
    let boxes: Vec<&str> = source.lines().filter(|line| line.contains("border_radius: 8.0")).collect();
    assert_eq!(boxes.len(), 2, "{source}");
    let outline_of = |fill: &str| boxes.iter().find(|line| line.contains(&format!("{{ color: theme.palette.{fill} "))).copied().unwrap_or_default();
    assert!(outline_of("surface").ends_with("border_color: theme.palette.card_border }"), "{boxes:#?}");
    assert!(outline_of("card_subtle").ends_with("border_color: theme.palette.border }"), "{boxes:#?}");
}

#[test]
fn format_functions_follow_the_spec_examples() {
    let text = |template: &str, model: serde_json::Value| {
        let payload = serde_json::json!([
            {"version": "v0.9", "createSurface": {"surfaceId": "s"}},
            {"version": "v0.9", "updateComponents": {"surfaceId": "s", "components": [
                {"id": "root", "component": "Text", "text": {"call": "formatString", "args": {"value": template}}}
            ]}},
            {"version": "v0.9", "updateDataModel": {"surfaceId": "s", "value": model}}
        ]).to_string();
        let source = a2app_core::a2ui::to_inline_splash(&payload, "$s:test", None).unwrap().source;
        let start = source.find("Label{").unwrap();
        let text_start = source[start..].find("text: \"").unwrap() + start + 7;
        source[text_start..].split('"').next().unwrap().to_string()
    };
    assert_eq!(text("${formatNumber(value: ${/n}, decimals: 2)}", serde_json::json!({"n": -1234567.891})), "-1,234,567.89");
    assert_eq!(text("${formatNumber(value: ${/n}, grouping: false)}", serde_json::json!({"n": 1234})), "1234");
    assert_eq!(text("${formatCurrency(value: ${/n}, currency: 'USD')}", serde_json::json!({"n": 5})), "$5.00");
    assert_eq!(text("${formatDate(value: ${/d}, format: 'yyyy-MM-dd h:mm a')}", serde_json::json!({"d": "2026-01-02T15:04:05Z"})), "2026-01-02 3:04 PM");
    assert_eq!(text("${pluralize(value: ${/c}, zero: 'none', one: 'one', other: 'lots')}", serde_json::json!({"c": 0})), "none");
    assert_eq!(text("\\${literal} ${/a/b}", serde_json::json!({"a": {"b": "x"}})), "${literal} x");
}

/// A v0.9 payload for one surface `s` with these components and model.
fn surface(components: serde_json::Value, model: serde_json::Value) -> String {
    serde_json::json!([
        {"version": "v0.9", "createSurface": {"surfaceId": "s"}},
        {"version": "v0.9", "updateComponents": {"surfaceId": "s", "components": components}},
        {"version": "v0.9", "updateDataModel": {"surfaceId": "s", "value": model}}
    ]).to_string()
}

fn inline(payload: &str) -> String {
    a2app_core::a2ui::to_inline_splash(payload, "$s:test", None).unwrap().source
}

#[test]
fn payload_regexes_are_bounded_before_they_compile() {
    let field = |pattern: &str| surface(serde_json::json!([
        {"id": "root", "component": "TextField", "label": "X", "value": {"path": "/x"},
         "checks": [{"condition": {"call": "regex", "args": {"value": {"path": "/x"}, "pattern": pattern}}, "message": "bad"}]}
    ]), serde_json::json!({"x": ""}));
    let started = std::time::Instant::now();
    for pattern in ["(a{1000}){1000}", "^([a-z]{1,100}){1,30}$", "^(?=.*[0-9]).{8,}$", "(?<![a-z])x", "(unclosed"] {
        let source = inline(&field(pattern));
        assert!(!source.contains(&format!("a2ui_re({pattern:?}")), "{pattern} is dropped, not run");
    }
    assert!(started.elapsed() < std::time::Duration::from_secs(1), "rejecting is cheap: {:?}", started.elapsed());
    assert!(inline(&field("^[A-Z]{3}[0-9]{2,4}$")).contains("a2ui_re(\"^[A-Z]{3}[0-9]{2,4}$\""));
    let v08 = serde_json::json!([
        {"surfaceUpdate": {"surfaceId": "s", "components": [
            {"id": "root", "component": {"TextField": {"label": {"literalString": "X"}, "text": {"path": "/x"}, "validationRegexp": "(a{1000}){1000}"}}}
        ]}},
        {"beginRendering": {"surfaceId": "s", "root": "root"}}
    ]).to_string();
    assert!(!inline(&v08).contains("a2ui_re(\"(a{1000}){1000}\""));
}

#[test]
fn layout_and_labels_follow_the_catalog() {
    let source = inline(&surface(serde_json::json!([
        {"id": "root", "component": "Column", "align": "center", "children": ["row", "send", "title"]},
        {"id": "row", "component": "Row", "children": ["icon", "long"]},
        {"id": "icon", "component": "Icon", "name": "info"},
        {"id": "long", "component": "Text", "text": "Free cancellation up to a day before departure."},
        {"id": "send", "component": "Button", "child": "send_row", "action": {"event": {"name": "send"}}},
        {"id": "send_row", "component": "Row", "children": ["send_icon", "send_text"]},
        {"id": "send_icon", "component": "Icon", "name": "send"},
        {"id": "send_text", "component": "Text", "text": "Send message"},
        {"id": "title", "component": "Text", "text": "Centered"}
    ]), serde_json::json!({})));
    assert!(source.contains(r#"Label{ width: Fill padding: 0 margin: 0 text: "Free cancellation"#), "a row's text grows and wraps\n{source}");
    assert!(source.contains(r#"text: "📤 Send message""#), "icon and text both label the button");
    assert!(source.contains(r#"align: Align{x: 0.5} padding: 0 margin: 0 text: "Centered""#), "column alignment reaches text");
}

#[test]
fn v09_needs_a_root_and_plain_strings_stay_literal() {
    let rootless = surface(serde_json::json!([{"id": "title", "component": "Text", "text": "Later"}]), serde_json::json!({}));
    assert!(a2app_core::a2ui::to_inline_splash(&rootless, "$s:test", None).is_err(), "nothing renders before `root`");
    let source = inline(&surface(serde_json::json!([
        {"id": "root", "component": "Column", "children": ["shell", "known"]},
        {"id": "shell", "component": "Text", "text": "Run: export PATH=${HOME}/bin"},
        {"id": "known", "component": "Text", "text": "Hi ${/name}"}
    ]), serde_json::json!({"name": "Ada"})));
    assert!(source.contains("Run: export PATH=${HOME}/bin"), "{source}");
    assert!(source.contains("Hi Ada"));
}

#[test]
fn format_date_handles_week_years_and_quotes() {
    let source = inline(&surface(serde_json::json!([
        {"id": "root", "component": "Text", "text": {"call": "formatString", "args": {"value": "${formatDate(value: ${/d}, format: \"E MMM d, YYYY h 'o''clock' a\")}"}}}
    ]), serde_json::json!({"d": "2026-02-02T15:17:00Z"})));
    assert!(source.contains("Mon Feb 2, 2026 3 o'clock PM"), "{source}");
}

#[test]
fn function_call_buttons_run_locally_and_post_nothing() {
    let source = inline(&surface(serde_json::json!([
        {"id": "root", "component": "Column", "children": ["open", "mail", "other"]},
        {"id": "open", "component": "Button", "child": "l1", "action": {"functionCall": {"call": "openUrl", "args": {"url": "${/url}"}}}},
        {"id": "l1", "component": "Text", "text": "Open"},
        {"id": "mail", "component": "Button", "child": "l2", "action": {"functionCall": {"call": "openUrl", "args": {"url": "mailto:a@example.org"}}}},
        {"id": "l2", "component": "Text", "text": "Mail"},
        {"id": "other", "component": "Button", "child": "l3", "action": {"functionCall": {"call": "copyToClipboard", "args": {}}}},
        {"id": "l3", "component": "Text", "text": "Copy"}
    ]), serde_json::json!({"url": "https://example.com/a"})));
    assert!(source.contains(r#"a2ui_open("https://example.com/a")"#) && source.contains(r#"a2ui_open("mailto:a@example.org")"#));
    assert!(!source.contains("a2ui_send(\""), "no function call posts an answer\n{source}");
}

#[test]
fn user_edits_survive_updates_except_where_the_agent_writes() {
    let original: serde_json::Value = serde_json::from_str(&event_messages("contact_event.json")).unwrap();
    let edits = a2app_core::a2ui::Edits::parse(r#"[["main", "/contact/name", "Alice"], ["main", "/contact/subscribe", true]]"#, 1).unwrap();
    // An update that only retitles the form keeps what the user typed.
    let retitle = serde_json::json!([{"version": "v0.9", "updateComponents": {"surfaceId": "main", "components": [
        {"id": "title", "component": "Text", "text": "Contact the team", "variant": "h2"}]}}]);
    let payload = serde_json::json!([original, retitle]).to_string();
    let app = a2app_core::a2ui::to_inline_splash(&payload, "$s:test", Some(&edits)).unwrap();
    assert_eq!(app.entries, 2);
    assert!(app.source.contains(r#"A2uiField{ text: "Alice""#) && app.source.contains("active: true"), "{}", app.source);
    assert!(app.source.contains("Contact the team"));
    // An update that sets the name itself wins over the user's value.
    let rename = serde_json::json!([{"version": "v0.9", "updateDataModel": {"surfaceId": "main", "path": "/contact/name", "value": "Bob"}}]);
    let payload = serde_json::json!([original, rename]).to_string();
    let source = a2app_core::a2ui::to_inline_splash(&payload, "$s:test", Some(&edits)).unwrap().source;
    assert!(source.contains(r#"A2uiField{ text: "Bob""#) && source.contains("active: true"), "{source}");
}

#[test]
fn regex_sizes_follow_the_engine_parser() {
    let field = |pattern: &str| surface(serde_json::json!([
        {"id": "root", "component": "TextField", "label": "X", "value": {"path": "/x"},
         "checks": [{"condition": {"call": "regex", "args": {"value": {"path": "/x"}, "pattern": pattern}}, "message": "bad"}]}
    ]), serde_json::json!({"x": ""}));
    let started = std::time::Instant::now();
    for pattern in ["(^{1000}){1000}", "(${1000}){1000}", "x({3000})", "^{3000}", "(a{1000}[^](]){5}", "a{4000000000}"] {
        assert!(!inline(&field(pattern)).contains(&format!("a2ui_re({pattern:?}")), "{pattern} is rejected");
    }
    assert!(started.elapsed() < std::time::Duration::from_secs(1), "{:?}", started.elapsed());
    for pattern in [r"^\(?[0-9]{3}\)?[- ]?[0-9]{3}-[0-9]{4}$", "^[(?]x$", "^(?:ab)+$"] {
        assert!(inline(&field(pattern)).contains("a2ui_re("), "{pattern} is kept");
        assert!(inline(&field(pattern)).contains(&format!("a2ui_re({pattern:?}")), "{pattern} is kept");
    }
}

#[test]
fn agent_writes_win_over_edits_whatever_their_path_spelling() {
    let original: serde_json::Value = serde_json::from_str(&event_messages("contact_event.json")).unwrap();
    let edits = a2app_core::a2ui::Edits::parse(r#"[["main", "/contact/name", "Alice"]]"#, 1).unwrap();
    for (path, value) in [("contact/name", serde_json::json!("Bob")), ("contact.name", serde_json::json!("Bob")),
        ("contact", serde_json::json!({"name": "Bob", "subscribe": false}))]
    {
        // An update whose `messages` is one object rather than an array still counts as an entry.
        let update = serde_json::json!({"version": "v0.9", "updateDataModel": {"surfaceId": "main", "path": path, "value": value}});
        let payload = serde_json::json!([original, update]).to_string();
        let source = a2app_core::a2ui::to_inline_splash(&payload, "$s:test", Some(&edits)).unwrap().source;
        assert!(source.contains(r#"A2uiField{ text: "Bob""#), "{path}: the agent's write wins\n{source}");
    }
}

#[test]
fn final_review_translation_fixes() {
    let source = inline(&surface(serde_json::json!([
        {"id": "root", "component": "Column", "children": ["q", "search", "privacy", "rate", "plain", "row", "trail"]},
        {"id": "q", "component": "TextField", "label": "Query", "value": {"path": "/q"},
         "checks": [{"condition": {"call": "required", "args": {"value": {"path": "/q"}}}, "message": "Required."}]},
        {"id": "search", "component": "Button", "child": "search_label", "action": {"functionCall": {"call": "openUrl",
            "args": {"url": {"call": "formatString", "args": {"value": "https://example.com/search?q=${/q}"}}}}}},
        {"id": "search_label", "component": "Text", "text": "Search"},
        {"id": "privacy", "component": "Button", "child": "privacy_label", "action": {"functionCall": {"call": "openUrl", "args": {"url": "https://example.com/privacy"}}}},
        {"id": "privacy_label", "component": "Text", "text": "Privacy"},
        {"id": "rate", "component": "Button", "child": "stars", "action": {"event": {"name": "rate"}}},
        {"id": "stars", "component": "Row", "children": ["star", "rate_text", "star"]},
        {"id": "star", "component": "Icon", "name": "star"},
        {"id": "rate_text", "component": "Text", "text": "Rate"},
        {"id": "plain", "component": "Text", "text": "#1 priority: use 2**10 bytes via snake__case__name(), **really**"},
        {"id": "row", "component": "Row", "children": ["label", "field"]},
        {"id": "label", "component": "Text", "text": "Email"},
        {"id": "field", "component": "TextField", "label": "", "weight": 1, "value": {"path": "/email"}},
        {"id": "trail", "component": "TextField", "label": "Trail", "value": {"path": "/form/email/"}}
    ]), serde_json::json!({"q": "cats"})));
    assert!(source.contains(r#"a2ui_open("https://example.com/search?q=" + ("" + mod.a2ui.v0))"#), "the link follows the input\n{source}");
    let privacy = source.split("fn ").find(|f| f.contains(r#"a2ui_open("https://example.com/privacy")"#)).unwrap();
    assert!(!privacy.contains("a2ui_valid_s"), "a link isn't blocked by the form's checks");
    assert!(source.contains(r#"text: "⭐ Rate ⭐""#), "a repeated icon stays in the label");
    assert!(source.contains("#1 priority: use 2**10 bytes via snake__case__name(), really"), "{source}");
    assert!(source.contains(r#"Label{ width: Fit padding: 0 margin: 0 text: "Email""#), "a weighted sibling takes the row");
    assert!(source.contains(r#"["s", "/form/email", mod.a2ui.v"#), "paths are normalized");
}

#[test]
fn v1_literals_and_v08_template_interpolation() {
    let v1 = serde_json::json!([
        {"version": "v1.0", "createSurface": {"surfaceId": "s", "dataModel": {"x": "VAL"}, "components": [
            {"id": "root", "component": "Button", "child": "go_label", "action": {"event": {"name": "go", "context": {
                "meta": {"call": "formatString", "args": {"value": "lit ${/x}"}},
                "list": [{"@@path": "/x"}]}}}},
            {"id": "go_label", "component": "Text", "text": "Go"}
        ]}}
    ]).to_string();
    let source = inline(&v1);
    assert!(source.contains(r#"\"meta\":{\"args\":{\"value\":\"lit ${/x}\"},\"call\":\"formatString\"}"#), "a plain `call` is data in v1.0\n{source}");
    assert!(source.contains(r#"\"list\":[{\"@path\":\"/x\"}]"#), "escaped keys in arrays are restored");
    let v08 = serde_json::json!([
        {"surfaceUpdate": {"surfaceId": "s", "components": [
            {"id": "root", "component": {"List": {"children": {"template": {"componentId": "row", "dataBinding": "/items"}}}}},
            {"id": "row", "component": {"Text": {"text": {"literalString": "Total ${/total}"}}}}
        ]}},
        {"dataModelUpdate": {"surfaceId": "s", "contents": [
            {"key": "total", "valueNumber": 42},
            {"key": "items", "valueMap": [{"key": "0", "valueMap": [{"key": "name", "valueString": "a"}]}]}
        ]}},
        {"beginRendering": {"surfaceId": "s", "root": "root"}}
    ]).to_string();
    assert!(inline(&v08).contains("Total ${/total}"), "a path that resolves to nothing in the item stays literal");
}
