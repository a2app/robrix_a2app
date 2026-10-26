//! Gives mini-app isolates Robrix's own theme: makepad's light or dark theme,
//! Robrix's colors under `theme.palette` (see `a2app_core::palette`), and the
//! look `helpers.rs` gives Robrix's own scroll bars and toggles.

use a2app_core::palette;
use makepad_widgets::*;

use crate::settings::app_preferences::Appearance;

/// The Robrix color behind each palette color, in `palette::NAMES` order.
/// `transparent` takes the surface's color at no opacity.
const PALETTE: [(&str, &str); palette::NAMES.len()] = [
    ("title", "COLOR_TEXT"),
    ("body", "MESSAGE_TEXT_COLOR"),
    ("muted", "COLOR_MINI_APP_TEXT_MUTED"),
    ("on_accent", "COLOR_TEXT_ON_ACCENT"),
    ("placeholder", "COLOR_TEXT_INPUT_PLACEHOLDER"),
    ("placeholder_hover", "COLOR_TEXT_INPUT_PLACEHOLDER_HOVER"),
    ("surface", "COLOR_PRIMARY"),
    ("card", "COLOR_BG_PREVIEW"),
    ("card_subtle", "COLOR_MINI_APP_CARD_SUBTLE"),
    ("pressed", "COLOR_MINI_APP_PRESSED"),
    ("border", "COLOR_SECONDARY_DARKER"),
    ("card_border", "COLOR_MINI_APP_CARD_BORDER"),
    ("divider", "COLOR_DIVIDER"),
    ("accent", "COLOR_ACTIVE_PRIMARY"),
    ("accent_hover", "COLOR_ACTIVE_PRIMARY_HOVER"),
    ("accent_down", "COLOR_ACTIVE_PRIMARY_DOWN"),
    ("accent_text", "COLOR_MINI_APP_ACCENT_TEXT"),
    ("focus", "COLOR_ACTIVE_PRIMARY_DARKER"),
    ("neutral", "COLOR_BG_NEUTRAL"),
    ("neutral_hover", "COLOR_BG_NEUTRAL_HOVER"),
    ("neutral_down", "COLOR_BG_NEUTRAL_DOWN"),
    ("neutral_border", "COLOR_BG_DISABLED"),
    ("success", "COLOR_FG_ACCEPT_GREEN"),
    ("success_bg", "COLOR_BG_ACCEPT_GREEN"),
    ("success_bg_hover", "COLOR_BG_ACCEPT_GREEN_HOVER"),
    ("success_bg_down", "COLOR_BG_ACCEPT_GREEN_DOWN"),
    ("danger", "COLOR_FG_DANGER_RED"),
    ("danger_bg", "COLOR_BG_DANGER_RED"),
    ("danger_bg_hover", "COLOR_BG_DANGER_RED_HOVER"),
    ("danger_bg_down", "COLOR_BG_DANGER_RED_DOWN"),
    ("warning", "COLOR_TEXT_WARNING_NOT_FOUND"),
    ("avatar", "COLOR_AVATAR_BG"),
    ("selection", "COLOR_SELECT_TEXT"),
    ("transparent", "COLOR_PRIMARY"),
    ("shadow", "COLOR_SHADOW_SOFT"),
];

/// The stock widget colors `helpers.rs` restyles for Robrix, which isolates copy.
const WIDGET_COLORS: [(&str, &[&str]); 5] = [
    ("ScrollBar.draw_bg", &["color", "color_hover", "color_drag"]),
    ("CheckBoxFlat.draw_bg", &TOGGLE_COLORS),
    ("ToggleFlat.draw_bg", &["mark_color", "mark_color_hover", "mark_color_active"]),
    ("RadioButtonFlat.draw_bg", &TOGGLE_COLORS),
    ("DrawCheckLabel", &["color", "color_hover", "color_down", "color_focus", "color_active", "color_disabled"]),
];

const TOGGLE_COLORS: [&str; 13] = [
    "color", "color_hover", "color_down", "color_focus", "color_active", "color_disabled",
    "border_color", "border_color_hover", "border_color_down", "border_color_focus",
    "border_color_active", "border_color_disabled", "mark_color_active",
];

/// Puts mini-app isolates on Robrix's current theme. Running ones restyle to it.
///
/// Must run after `helpers.rs`, whose widget colors it copies.
pub fn set(vm: &mut ScriptVm, appearance: Appearance) {
    makepad_widgets::set_splash_theme(match appearance {
        Appearance::Light => SplashTheme::Light,
        Appearance::Dark => SplashTheme::Dark,
    });
    let widgets = vm.module(id!(widgets));
    let colors = std::array::from_fn(|i| {
        let (name, host) = PALETTE[i];
        let color = color_at(vm, widgets, &[host]).unwrap_or_else(|| {
            error!("Mini-app palette color {name} has no Robrix color {host}");
            palette::LIGHT[i]
        });
        if name == "transparent" { color & 0xFFFFFF00 } else { color }
    });
    let mut widget_script = String::new();
    for (proto, fields) in WIDGET_COLORS {
        let path: Vec<&str> = proto.split('.').collect();
        for field in fields {
            let mut field_path = path.clone();
            field_path.push(field);
            if let Some(color) = color_at(vm, widgets, &field_path) {
                widget_script.push_str(&format!("mod.widgets.{proto}.{field} = #x{color:08X};\n"));
            }
        }
    }
    makepad_widgets::set_splash_theme_script(SplashThemeScript {
        theme: palette::theme_script(&colors, appearance == Appearance::Dark),
        widgets: widget_script,
    });
}

/// The color at `path` below `obj`, if that's where one is.
fn color_at(vm: &mut ScriptVm, mut obj: ScriptObject, path: &[&str]) -> Option<u32> {
    let (last, parents) = path.split_last()?;
    for key in parents {
        obj = vm.bx.heap.value(obj, LiveId::from_str(key).into(), NoTrap).as_object()?;
    }
    vm.bx.heap.value(obj, LiveId::from_str(last).into(), NoTrap).as_color()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_follows_the_core_names() {
        let names: Vec<&str> = PALETTE.iter().map(|(name, _)| *name).collect();
        assert_eq!(names, palette::NAMES);
    }

    /// The fallback palette for hosts without a theme must stay Robrix's light one.
    #[test]
    fn core_light_palette_is_robrix_light() {
        let mut cx = Cx::new(Box::new(|_, _| {}));
        cx.with_vm(|vm| {
            makepad_widgets::script_mod(vm);
            makepad_code_editor::script_mod(vm);
            crate::shared::script_mod(vm, Appearance::Light);
            let widgets = vm.module(id!(widgets));
            for (i, (name, host)) in PALETTE.iter().enumerate() {
                let color = color_at(vm, widgets, &[host]).unwrap();
                let color = if *name == "transparent" { color & 0xFFFFFF00 } else { color };
                assert_eq!(color, palette::LIGHT[i], "{name} ({host})");
            }
        });
    }
}
