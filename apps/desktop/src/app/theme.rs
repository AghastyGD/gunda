use eframe::egui::{self, Color32};

use gunda_core::download::DownloadState;

use crate::view::DownloadView;

use super::Desktop;

/// A cool graphite palette with restrained blue and green status accents.
#[derive(Clone, Copy)]
pub(super) struct Palette {
    pub(super) background: Color32,
    pub(super) surface: Color32,
    pub(super) border: Color32,
    pub(super) muted: Color32,
    pub(super) accent: Color32,
    pub(super) accent_ink: Color32,
    pub(super) success: Color32,
    pub(super) track: Color32,
    pub(super) elevated: Color32,
    hover: Color32,
    text: Color32,
    selection: Color32,
}

impl Palette {
    pub(super) fn new(dark: bool) -> Self {
        if dark {
            Self {
                background: Color32::from_rgb(19, 23, 29),
                surface: Color32::from_rgb(25, 31, 40),
                elevated: Color32::from_rgb(35, 43, 54),
                hover: Color32::from_rgb(45, 55, 68),
                border: Color32::from_rgb(55, 67, 82),
                text: Color32::from_rgb(229, 235, 242),
                muted: Color32::from_rgb(155, 168, 184),
                accent: Color32::from_rgb(81, 157, 240),
                accent_ink: Color32::from_rgb(14, 24, 36),
                success: Color32::from_rgb(117, 194, 145),
                track: Color32::from_rgb(43, 53, 66),
                selection: Color32::from_rgb(39, 58, 79),
            }
        } else {
            Self {
                background: Color32::from_rgb(244, 247, 250),
                surface: Color32::from_rgb(250, 251, 253),
                elevated: Color32::from_rgb(255, 255, 255),
                hover: Color32::from_rgb(235, 241, 247),
                border: Color32::from_rgb(213, 222, 231),
                text: Color32::from_rgb(31, 41, 52),
                muted: Color32::from_rgb(92, 108, 124),
                accent: Color32::from_rgb(26, 104, 181),
                accent_ink: Color32::from_rgb(255, 255, 255),
                success: Color32::from_rgb(37, 125, 75),
                track: Color32::from_rgb(222, 230, 238),
                selection: Color32::from_rgb(222, 237, 251),
            }
        }
    }
}

impl Desktop {
    pub(super) fn theme(&self, ctx: &egui::Context) {
        let theme = if self.dark {
            egui::Theme::Dark
        } else {
            egui::Theme::Light
        };
        let mut visuals = if self.dark {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        };
        let colors = Palette::new(self.dark);
        visuals.panel_fill = colors.background;
        visuals.window_fill = colors.surface;
        visuals.extreme_bg_color = colors.background;
        visuals.faint_bg_color = colors.surface;
        visuals.override_text_color = Some(colors.text);
        visuals.window_corner_radius = 8.into();
        visuals.window_stroke = egui::Stroke::new(1.0, colors.border);
        visuals.selection.bg_fill = colors.selection;
        // egui also uses this stroke for text inside selected table rows.
        visuals.selection.stroke = egui::Stroke::new(1.0, colors.text);
        visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, colors.border);
        visuals.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, colors.muted);
        visuals.widgets.inactive.bg_fill = colors.elevated;
        visuals.widgets.inactive.weak_bg_fill = colors.elevated;
        visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, colors.border);
        visuals.widgets.hovered.bg_fill = colors.hover;
        visuals.widgets.hovered.weak_bg_fill = colors.hover;
        visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, colors.border);
        visuals.widgets.active.bg_fill = colors.hover;
        visuals.widgets.active.weak_bg_fill = colors.hover;
        visuals.widgets.active.bg_stroke = egui::Stroke::new(1.0, colors.accent);
        // The `open` role paints egui's window title bar; without it the bar
        // falls back to a light default even in the dark theme.
        visuals.widgets.open.bg_fill = colors.surface;
        visuals.widgets.open.weak_bg_fill = colors.surface;
        visuals.widgets.open.bg_stroke = egui::Stroke::new(1.0, colors.border);
        visuals.widgets.open.fg_stroke = egui::Stroke::new(1.0, colors.muted);
        for widget in [
            &mut visuals.widgets.inactive,
            &mut visuals.widgets.hovered,
            &mut visuals.widgets.active,
            &mut visuals.widgets.open,
        ] {
            widget.corner_radius = 4.into();
        }
        ctx.set_theme(theme);
        ctx.set_visuals(visuals);
        self.apply_titlebar_theme(ctx);
        ctx.style_mut_of(theme, |style| {
            style.spacing.item_spacing = egui::vec2(8.0, 6.0);
            style.spacing.button_padding = egui::vec2(10.0, 6.0);
            style.spacing.interact_size.y = 28.0;
            style.spacing.window_margin = egui::Margin::same(16);
            style.spacing.indent = 16.0;
            style.spacing.scroll.bar_width = 10.0;
            style
                .text_styles
                .insert(egui::TextStyle::Body, egui::FontId::proportional(13.0));
            style
                .text_styles
                .insert(egui::TextStyle::Button, egui::FontId::proportional(13.0));
            style
                .text_styles
                .insert(egui::TextStyle::Small, egui::FontId::proportional(11.0));
            style
                .text_styles
                .insert(egui::TextStyle::Monospace, egui::FontId::monospace(12.0));
            style
                .text_styles
                .insert(egui::TextStyle::Heading, egui::FontId::proportional(17.0));
        });
    }

    /// Keeps the native title bar on the same side as the app theme.
    pub(super) fn apply_titlebar_theme(&self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::SetTheme(if self.dark {
            egui::SystemTheme::Dark
        } else {
            egui::SystemTheme::Light
        }));
    }

    pub(super) fn error_color(&self) -> Color32 {
        if self.dark {
            Color32::from_rgb(224, 145, 132)
        } else {
            Color32::from_rgb(162, 58, 44)
        }
    }

    pub(super) fn state_color(&self, view: &DownloadView, active: bool) -> Color32 {
        if active {
            return Palette::new(self.dark).accent;
        }
        match view.state {
            DownloadState::Completed => Palette::new(self.dark).success,
            DownloadState::Failed => self.error_color(),
            _ => Palette::new(self.dark).muted,
        }
    }
}
