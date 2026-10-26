//! Every stock app colors itself from the host's palette, so it boots cleanly
//! on either of Robrix's themes.

use a2app_core::palette;
use makepad_widgets::*;

/// A palette unlike the light one in every color, like Robrix's dark one.
fn dark_palette() -> [u32; palette::NAMES.len()] {
    palette::LIGHT.map(|color| (!color & 0xFFFFFF00) | (color & 0xFF))
}

fn use_theme(theme: SplashTheme, colors: &[u32; palette::NAMES.len()]) {
    set_splash_theme(theme);
    set_splash_theme_script(SplashThemeScript {
        theme: palette::theme_script(colors, theme == SplashTheme::Dark),
        widgets: String::new(),
    });
}

#[test]
fn stock_apps_boot_on_both_themes() {
    let mut cx = Cx::new(Box::new(|_, _| {}));
    cx.with_vm(makepad_widgets::script_mod);
    let only = std::env::var("STOCK_APP").ok();
    let mut failures = Vec::new();
    for app in a2app_core::builtin::builtin_apps() {
        if only.as_ref().is_some_and(|only| *only != app.id) {
            continue;
        }
        if !palette::is_theme_aware(&app.source) {
            failures.push(format!("{}: colors itself without `theme.palette`", app.id));
        }
        for color in palette::bare_colors(&app.source) {
            failures.push(format!("{}:{}: a fixed color, {}", app.id, color.line, color.text));
        }
        for (name, theme, colors) in [
            ("light", SplashTheme::Light, palette::LIGHT),
            ("dark", SplashTheme::Dark, dark_palette()),
        ] {
            use_theme(theme, &colors);
            // Splash's 64ms wall budget is about CPU load, not the app; retry a busy machine's overrun.
            let mut errors = Vec::new();
            for _ in 0..5 {
                errors = splash::validate_splash_body_with_host_io(&mut cx, &app.source);
                if !errors.iter().all(|error| error.contains("script time budget exceeded")) || errors.is_empty() {
                    break;
                }
            }
            if !errors.is_empty() {
                failures.push(format!("{} ({name}): {errors:?}", app.id));
            }
        }
    }
    palette::use_light_theme();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
