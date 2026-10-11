use eframe::egui::{self, Color32};

use gunda_core::download::DownloadState;

use crate::view::DownloadView;

use super::Desktop;

pub(super) const ACCENT: Color32 = Color32::from_rgb(32, 125, 190);

#[derive(Clone, Copy)]
pub(super) struct Palette {
    pub(super) background: Color32,
    pub(super) surface: Color32,
    elevated: Color32,
    pub(super) border: Color32,
    text: Color32,
    pub(super) muted: Color32,
    selection: Color32,
}

impl Palette {
    pub(super) fn new(dark: bool) -> Self {
        if dark {
            Self {
                background: Color32::from_rgb(27, 37, 44),
                surface: Color32::from_rgb(47, 61, 70),
                elevated: Color32::from_rgb(57, 73, 83),
                border: Color32::from_rgb(76, 92, 101),
                text: Color32::from_rgb(231, 235, 237),
                muted: Color32::from_rgb(171, 184, 191),
                selection: Color32::from_rgb(24, 96, 151),
            }
        } else {
            Self {
                background: Color32::from_rgb(255, 255, 255),
                surface: Color32::from_rgb(237, 237, 237),
                elevated: Color32::from_rgb(224, 224, 224),
                border: Color32::from_rgb(165, 165, 165),
                text: Color32::from_rgb(30, 30, 30),
                muted: Color32::from_rgb(91, 91, 91),
                selection: Color32::from_rgb(174, 209, 239),
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
        visuals.window_corner_radius = 1.into();
        visuals.window_stroke = egui::Stroke::new(1.0, colors.border);
        visuals.selection.bg_fill = colors.selection;
        // egui also uses this stroke for text inside selected table rows.
        visuals.selection.stroke = egui::Stroke::new(1.0, colors.text);
        visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, colors.border);
        visuals.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, colors.muted);
        visuals.widgets.inactive.bg_fill = colors.elevated;
        visuals.widgets.inactive.weak_bg_fill = colors.elevated;
        visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, colors.border);
        visuals.widgets.hovered.bg_fill = colors.selection;
        visuals.widgets.hovered.weak_bg_fill = colors.selection;
        visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, colors.border);
        visuals.widgets.active.bg_fill = colors.selection;
        for widget in [
            &mut visuals.widgets.inactive,
            &mut visuals.widgets.hovered,
            &mut visuals.widgets.active,
        ] {
            widget.corner_radius = 1.into();
        }
        ctx.set_theme(theme);
        ctx.set_visuals(visuals);
        ctx.style_mut_of(theme, |style| {
            style.spacing.item_spacing = egui::vec2(5.0, 2.0);
            style.spacing.button_padding = egui::vec2(6.0, 3.0);
            style.spacing.interact_size.y = 21.0;
            style.spacing.window_margin = egui::Margin::same(8);
            style.spacing.scroll.bar_width = 10.0;
            style
                .text_styles
                .insert(egui::TextStyle::Body, egui::FontId::proportional(12.0));
            style
                .text_styles
                .insert(egui::TextStyle::Button, egui::FontId::proportional(12.0));
        });
    }

    pub(super) fn error_color(&self) -> Color32 {
        if self.dark {
            Color32::from_rgb(239, 133, 135)
        } else {
            Color32::from_rgb(180, 47, 55)
        }
    }

    pub(super) fn state_color(&self, view: &DownloadView, active: bool) -> Color32 {
        if active {
            return if self.dark {
                Color32::from_rgb(100, 179, 255)
            } else {
                Color32::from_rgb(20, 96, 177)
            };
        }
        match view.state {
            DownloadState::Completed => {
                if self.dark {
                    Color32::from_rgb(94, 200, 144)
                } else {
                    Color32::from_rgb(15, 94, 53)
                }
            }
            DownloadState::Failed => self.error_color(),
            _ => Color32::from_gray(if self.dark { 175 } else { 90 }),
        }
    }
}
