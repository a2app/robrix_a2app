//! The colors mini-apps style themselves with, so they follow the host's light
//! and dark themes. Scripts read them as `theme.palette.<name>`; the host fills
//! them from its own theme whenever it picks one.

/// Each palette color's name, in the order [`theme_script`] takes their values.
pub const NAMES: [&str; 35] = [
    // Text.
    "title",
    "body",
    "muted",
    "on_accent",
    "placeholder",
    "placeholder_hover",
    // Surfaces and lines.
    "surface",
    "card",
    "card_subtle",
    "pressed",
    "border",
    // The outline of a card or framed box on the surface.
    "card_border",
    "divider",
    // The accent, for main buttons, selected chips, links and spinners.
    "accent",
    "accent_hover",
    "accent_down",
    "accent_text",
    "focus",
    // Neutral, positive and destructive buttons.
    "neutral",
    "neutral_hover",
    "neutral_down",
    "neutral_border",
    "success",
    "success_bg",
    "success_bg_hover",
    "success_bg_down",
    "danger",
    "danger_bg",
    "danger_bg_hover",
    "danger_bg_down",
    "warning",
    "avatar",
    // A selection or highlight laid over the surface.
    "selection",
    // The surface's color at no opacity, for a fill that fades in from nothing.
    "transparent",
    // A shadow under a card or popup.
    "shadow",
];

/// Robrix's light palette, for hosts and tests that run apps without a theme of their own.
pub const LIGHT: [u32; NAMES.len()] = [
    0x1C274CFF, 0x333333FF, 0x66708FFF, 0xFFFFFFFF, 0xBBBBBBFF, 0x999999FF,
    0xFFFFFFFF, 0xF0F5FFFF, 0xF7F9FCFF, 0xE4EEFFFF, 0xC8C8C8FF, 0xE0E0E0FF, 0x00000018,
    0x0F88FEFF, 0x106FCCFF, 0x0C5DAAFF, 0x0F88FEFF, 0x106FCCFF,
    0xE3E3E3FF, 0xD0D0D0FF, 0xC0C0C0FF, 0xE0E0E0FF,
    0x138808FF, 0xF0FFF0FF, 0xD4EED4FF, 0xB8E0B8FF,
    0xDC0005FF, 0xFFF0F0FF, 0xF0D4D4FF, 0xE0B8B8FF,
    0x953800FF, 0x52B2ACFF,
    0x087DFC33, 0xFFFFFF00, 0x00000033,
];

/// The Splash code that puts `colors` (an RGBA value for each of [`NAMES`])
/// under `theme.palette`, with `theme.palette.is_dark` saying which theme they're from.
pub fn theme_script(colors: &[u32; NAMES.len()], is_dark: bool) -> String {
    let mut palette = String::new();
    for (name, color) in NAMES.iter().zip(colors) {
        palette.push_str(&format!("{name}: #x{color:08X} "));
    }
    format!("mod.theme.palette = {{{palette}is_dark: {is_dark}}};\n")
}

/// Puts Splash isolates on Robrix's light theme and palette, for a host
/// (or test) that runs mini-apps without a theme of its own.
pub fn use_light_theme() {
    makepad_widgets::set_splash_theme(makepad_widgets::SplashTheme::Light);
    makepad_widgets::set_splash_theme_script(makepad_widgets::SplashThemeScript {
        theme: theme_script(&LIGHT, false),
        widgets: String::new(),
    });
}

/// Whether a script suits either theme: its code takes colors from the palette,
/// or it names no colors of its own for a dark theme to clash with.
pub fn is_theme_aware(source: &str) -> bool {
    code_only(source).contains("theme.palette") || bare_colors(source).is_empty()
}

/// The script with its comments and strings (`"..."` or `'...'`) blanked out.
fn code_only(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut code = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        match bytes[i] {
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i < bytes.len() && !(bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/')) {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
            }
            quote @ (b'"' | b'\'') => {
                i += 1;
                while i < bytes.len() && bytes[i] != quote {
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
                i = (i + 1).min(bytes.len());
            }
            byte => {
                code.push(byte);
                i += 1;
                continue;
            }
        }
        code.resize(code.len() + (i - start), b' ');
    }
    String::from_utf8_lossy(&code).into_owned()
}

/// A color a script's code writes as a literal, so it looks the same on either theme.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BareColor {
    /// The 1-based source line it is on.
    pub line: usize,
    /// The literal as written, like `#x1C274C` or `vec4(0.0, 0.0, 0.0, 0.5)`.
    pub text: String,
    /// The palette colors whose light value it is.
    pub light_of: Vec<&'static str>,
}

/// The hex and `vec4` (0 to 1) colors in a script's code, outside comments, strings
/// and `if theme.palette.is_dark {..} else {..}` picks.
pub fn bare_colors(source: &str) -> Vec<BareColor> {
    let code = code_only(source);
    let bytes = code.as_bytes();
    let picks = theme_picks(bytes);
    let (mut colors, mut line, mut counted, mut i) = (Vec::new(), 1, 0, 0);
    while i < bytes.len() {
        let Some((len, rgba)) = color_literal(bytes, i) else {
            i += 1;
            continue;
        };
        if !picks.iter().any(|pick| pick.contains(&i)) {
            // `code_only` keeps byte offsets but not the newlines of comments and strings.
            line += source.as_bytes()[counted..i].iter().filter(|byte| **byte == b'\n').count();
            counted = i;
            let light_of = NAMES.iter().zip(LIGHT)
                .filter(|(_, light)| rgba == Some(*light))
                .map(|(name, _)| *name)
                .collect();
            let text = code[i..i + len].split_whitespace().collect::<Vec<_>>().join(" ");
            colors.push(BareColor { line, text, light_of });
        }
        i += len;
    }
    colors
}

/// The length and RGBA value of the hex or `vec4` color starting at `i`, if any.
fn color_literal(code: &[u8], i: usize) -> Option<(usize, Option<u32>)> {
    let is_ident = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_';
    if code[i] == b'#' {
        let start = i + 1 + usize::from(code.get(i + 1) == Some(&b'x'));
        let digits = code[start..].iter().take(8).take_while(|byte| byte.is_ascii_hexdigit()).count();
        if digits == 0 {
            return None;
        }
        let hex = std::str::from_utf8(&code[start..start + digits]).ok()?;
        let value = u32::from_str_radix(hex, 16).ok()?;
        let nibble = |shift: u32| ((value >> shift) & 0xF) * 0x11;
        // The forms makepad reads: gray `#w`/`#ww`, `#rgb`, `#rgba`, `#rrggbb`, `#rrggbbaa`.
        let rgba = match digits {
            1 => Some((nibble(0) * 0x01010100) | 0xFF),
            2 => Some((value * 0x01010100) | 0xFF),
            3 => Some((nibble(8) << 24) | (nibble(4) << 16) | (nibble(0) << 8) | 0xFF),
            4 => Some((nibble(12) << 24) | (nibble(8) << 16) | (nibble(4) << 8) | nibble(0)),
            6 => Some((value << 8) | 0xFF),
            8 => Some(value),
            _ => None,
        };
        return Some((start + digits - i, rgba));
    }
    if !code[i..].starts_with(b"vec4") || i > 0 && is_ident(code[i - 1]) {
        return None;
    }
    let open = i + 4 + code[i + 4..].iter().take_while(|byte| byte.is_ascii_whitespace()).count();
    if code.get(open) != Some(&b'(') {
        return None;
    }
    let close = open + code[open..].iter().position(|byte| *byte == b')')?;
    let parts = std::str::from_utf8(&code[open + 1..close]).ok()?.split(',')
        .map(|part| part.trim().parse::<f64>().ok().filter(|value| (0.0..=1.0).contains(value)))
        .collect::<Option<Vec<_>>>()?;
    if parts.len() != 4 {
        return None;
    }
    let rgba = parts.iter().fold(0, |rgba, part| (rgba << 8) | (part * 255.0).round() as u32);
    Some((close + 1 - i, Some(rgba)))
}

/// The branches of each `if theme.palette.is_dark {..} else {..}` in code, braces included.
fn theme_picks(code: &[u8]) -> Vec<std::ops::Range<usize>> {
    const IS_DARK: &[u8] = b"theme.palette.is_dark";
    let is_ident = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_';
    let skip_space = |at: usize| at + code[at..].iter().take_while(|byte| byte.is_ascii_whitespace()).count();
    let block = |open: usize| {
        let mut depth = 0;
        for (at, byte) in code.iter().enumerate().skip(open) {
            match byte {
                b'{' => depth += 1,
                b'}' if depth == 1 => return Some(open..at + 1),
                b'}' => depth -= 1,
                _ => {}
            }
        }
        None
    };
    let mut picks = Vec::new();
    for at in (0..code.len()).filter(|at| code[*at..].starts_with(IS_DARK)) {
        // Only an `if`/`elif` on it picks a color per theme.
        let before = code[..at].iter().rposition(|byte| !byte.is_ascii_whitespace() && !b"!(".contains(byte));
        let is_if = before.is_some_and(|end| [&b"if"[..], b"elif"].iter().any(|word| {
            code[..=end].ends_with(word) && (end + 1 == word.len() || !is_ident(code[end - word.len()]))
        }));
        // Its block opens later on the condition's line.
        let after = at + IS_DARK.len();
        let open = code[after..].iter().position(|byte| b"{}\n".contains(byte)).map(|offset| after + offset);
        let Some(then) = open.filter(|open| is_if && code[*open] == b'{').and_then(block) else {
            continue;
        };
        let rest = skip_space(then.end);
        if code[rest..].starts_with(b"else") && code.get(rest + 4).is_some_and(|byte| !is_ident(*byte)) {
            let open = skip_space(rest + 4);
            if let Some(otherwise) = (code.get(open) == Some(&b'{')).then(|| block(open)).flatten() {
                picks.push(otherwise);
            }
        }
        picks.push(then);
    }
    picks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_lists_every_color() {
        let script = theme_script(&LIGHT, false);
        assert!(script.starts_with("mod.theme.palette = {title: #x1C274CFF "));
        assert!(script.contains("transparent: #xFFFFFF00 "));
        assert!(script.trim_end().ends_with("is_dark: false};"));
    }

    #[test]
    fn theme_awareness() {
        assert!(is_theme_aware("Label{text: \"hi\"}"));
        assert!(is_theme_aware("// tint: #2A9D8F\nLabel{text: \"Issue #1234\"}"));
        assert!(is_theme_aware("/* #fff */ Label{draw_text +: {color: theme.palette.title}}"));
        assert!(is_theme_aware("Label{draw_text +: {color: theme.palette.title}} View{draw_bg +: {color: #x52B2AC}}"));
        assert!(!is_theme_aware("Label{draw_text +: {color: #x1C274C}}"));
        assert!(!is_theme_aware("RoundedView{draw_bg +: {color: #fff}}"));
        assert!(!is_theme_aware("let s = \"a \\\" quote\"\nView{draw_bg +: {color: #F0F5FF}}"));
        assert!(is_theme_aware("let c = theme.palette[name]\nView{draw_bg +: {color: #x52B2AC}}"));
        assert!(!is_theme_aware("// uses theme.palette.title\nView{draw_bg +: {color: #fff}}"));
        assert!(!is_theme_aware("Label{text: 'see theme.palette.title'}\nView{draw_bg +: {color: #fff}}"));
        assert!(!is_theme_aware("View{draw_bg +: {color: vec4(1.0, 1.0, 1.0, 1.0)}}"));
        assert!(is_theme_aware("Button{draw_bg +: {color_2: vec4(-1.0, -1.0, -1.0, -1.0)}}"));
    }

    fn texts(source: &str) -> Vec<(usize, String)> {
        bare_colors(source).into_iter().map(|color| (color.line, color.text)).collect()
    }

    #[test]
    fn bare_colors_are_found_in_code_only() {
        assert!(bare_colors("// tint: #2A9D8F\n/* #fff */\nLabel{text: \"Issue #1234\" draw_text +: {color: theme.palette.title}}").is_empty());
        // Splash strings take either quote; the other quote inside one is just text.
        assert!(bare_colors("Label{text: 'Issue #12'}").is_empty());
        assert!(bare_colors("Label{text: 'it\\'s #dev, \"#a2app:matrix.org\"'}").is_empty());
        assert_eq!(texts("A{text: \"it's\"}\nB{text: 'say \"hi\"' color: #fff}"), vec![(2, "#fff".to_string())]);
        // Multi-line comments and strings still count toward the line number.
        let source = "/* a\nb */\nlet s = \"x\ny\"\nView{\n    draw_bg +: {color: #F0F5FF}\n}";
        assert_eq!(bare_colors(source), vec![BareColor { line: 6, text: "#F0F5FF".into(), light_of: vec!["card"] }]);
        assert_eq!(texts("A{color: #x1C274C}\nB{color: #fff border_color: #xE0E0E0 color_2: #x7B4FD6}"),
            [(1, "#x1C274C"), (2, "#fff"), (2, "#xE0E0E0"), (2, "#x7B4FD6")].map(|(line, text)| (line, text.to_string())));
        let names = |source: &str| bare_colors(source).into_iter().map(|color| color.light_of).collect::<Vec<_>>();
        assert_eq!(names("#x1C274C #fff #xE0E0E0 #x00000018 #x7B4FD6"),
            vec![vec!["title"], vec!["on_accent", "surface"], vec!["card_border", "neutral_border"], vec!["divider"], vec![]]);
        assert_eq!(names("#f #xff #fff0"), vec![vec!["on_accent", "surface"], vec!["on_accent", "surface"], vec!["transparent"]]);
    }

    #[test]
    fn colors_picked_per_theme_are_not_bare() {
        assert!(bare_colors("View{draw_bg +: {color: if theme.palette.is_dark {#x2B3240} else {#xF0F5FF}}}").is_empty());
        assert!(bare_colors("let c = if !theme.palette.is_dark {#x7B4FD6} else {\n    #xB79CFF\n}").is_empty());
        assert!(bare_colors("fn pick(){\n    if theme.palette.is_dark {\n        return {a: #x111111}\n    } elif x {\n        return 1\n    }\n}").is_empty());
        // The else of a pick is a pick too, but code after it is not.
        assert_eq!(texts("A{color: if theme.palette.is_dark {#x111111} else {#x222222}}\nB{color: #x333333}"), vec![(2, "#x333333".to_string())]);
        // A read of `is_dark` outside an `if` exempts nothing.
        assert_eq!(texts("let dark = theme.palette.is_dark\nView{draw_bg +: {color: #fff}}"), vec![(2, "#fff".to_string())]);
        assert_eq!(texts("let dark = theme.palette.is_dark if dark {#x111111} else {#x222222}").len(), 2);
    }

    #[test]
    fn vec4_colors_are_bare_but_other_vec4s_are_not() {
        let source = "B{draw_bg +: {\n    color: vec4(1.0, 1.0, 1.0, 1)\n    color_2: vec4(-1.0, -1.0, -1.0, -1.0)\n    c: vec4(self.color.rgb, 1.0) d: myvec4(0, 0, 0, 0) e: vec4(0.5)\n}}";
        assert_eq!(bare_colors(source), vec![BareColor { line: 2, text: "vec4(1.0, 1.0, 1.0, 1)".into(), light_of: vec!["on_accent", "surface"] }]);
        assert_eq!(texts("x: vec4(0, 0,\n    0, 0.5)"), vec![(1, "vec4(0, 0, 0, 0.5)".to_string())]);
    }
}
