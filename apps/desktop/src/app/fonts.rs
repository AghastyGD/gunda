#[cfg(windows)]
use std::sync::Arc;

use eframe::egui;

const SEMIBOLD: &str = "semibold";

/// The heading family: the platform's semibold face when it is available,
/// otherwise the same stack as the body text.
pub(super) fn semibold() -> egui::FontFamily {
    egui::FontFamily::Name(SEMIBOLD.into())
}

/// Use the platform's UI faces so the window reads like a native tool instead
/// of a web page. Missing files simply fall back to egui's bundled fonts.
pub(super) fn install(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    #[cfg(windows)]
    {
        load(
            &mut fonts,
            "ui",
            r"C:\Windows\Fonts\segoeui.ttf",
            &[egui::FontFamily::Proportional],
        );
        load(
            &mut fonts,
            "ui-semibold",
            r"C:\Windows\Fonts\seguisb.ttf",
            &[],
        );
        load(
            &mut fonts,
            "mono",
            r"C:\Windows\Fonts\consola.ttf",
            &[egui::FontFamily::Monospace],
        );
    }

    let mut stack = Vec::new();
    if fonts.font_data.contains_key("ui-semibold") {
        stack.push("ui-semibold".to_owned());
    }
    stack.extend(
        fonts
            .families
            .get(&egui::FontFamily::Proportional)
            .cloned()
            .unwrap_or_default(),
    );
    fonts.families.insert(semibold(), stack);

    ctx.set_fonts(fonts);
}

#[cfg(windows)]
fn load(fonts: &mut egui::FontDefinitions, name: &str, path: &str, families: &[egui::FontFamily]) {
    let Ok(bytes) = std::fs::read(path) else {
        return;
    };
    fonts
        .font_data
        .insert(name.to_owned(), Arc::new(egui::FontData::from_owned(bytes)));
    for family in families {
        if let Some(list) = fonts.families.get_mut(family) {
            list.insert(0, name.to_owned());
        }
    }
}
