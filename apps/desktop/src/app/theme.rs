use eframe::egui::{self, Color32};

use gunda_core::download::DownloadState;

use crate::view::DownloadView;

use super::Desktop;

pub(super) const ACCENT: Color32 = Color32::from_rgb(46, 119, 224);

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
                background: Color32::from_rgb(17, 21, 29),
                surface: Color32::from_rgb(23, 29, 39),
                elevated: Color32::from_rgb(30, 38, 50),
                border: Color32::from_rgb(43, 53, 69),
                text: Color32::from_rgb(224, 232, 244),
                muted: Color32::from_rgb(145, 158, 180),
                selection: Color32::from_rgb(31, 64, 105),
            }
        } else {
            Self {
                background: Color32::from_rgb(241, 244, 249),
                surface: Color32::WHITE,
                elevated: Color32::from_rgb(233, 239, 247),
                border: Color32::from_rgb(215, 224, 237),
                text: Color32::from_rgb(35, 48, 69),
                muted: Color32::from_rgb(96, 112, 137),
                selection: Color32::from_rgb(217, 232, 253),
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
        visuals.window_corner_radius = 4.into();
        visuals.window_stroke = egui::Stroke::new(1.0, colors.border);
        visuals.selection.bg_fill = colors.selection;
        visuals.selection.stroke = egui::Stroke::new(1.0, ACCENT);
        visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, colors.border);
        visuals.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, colors.muted);
        visuals.widgets.inactive.bg_fill = colors.elevated;
        visuals.widgets.inactive.weak_bg_fill = colors.elevated;
        visuals.widgets.inactive.bg_stroke = egui::Stroke::NONE;
        visuals.widgets.hovered.bg_fill = colors.selection;
        visuals.widgets.hovered.weak_bg_fill = colors.selection;
        visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, colors.border);
        visuals.widgets.active.bg_fill = colors.selection;
        for widget in [
            &mut visuals.widgets.inactive,
            &mut visuals.widgets.hovered,
            &mut visuals.widgets.active,
        ] {
            widget.corner_radius = 3.into();
        }
        ctx.set_theme(theme);
        ctx.set_visuals(visuals);
        ctx.style_mut_of(theme, |style| {
            style.spacing.item_spacing = egui::vec2(8.0, 5.0);
            style.spacing.button_padding = egui::vec2(10.0, 5.0);
            style.spacing.interact_size.y = 26.0;
            style.spacing.window_margin = egui::Margin::same(14);
            style.spacing.scroll.bar_width = 6.0;
            style
                .text_styles
                .insert(egui::TextStyle::Body, egui::FontId::proportional(13.0));
            style
                .text_styles
                .insert(egui::TextStyle::Button, egui::FontId::proportional(13.0));
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
                    Color32::from_rgb(22, 126, 73)
                }
            }
            DownloadState::Failed => self.error_color(),
            _ => Color32::from_gray(if self.dark { 175 } else { 90 }),
        }
    }
}
