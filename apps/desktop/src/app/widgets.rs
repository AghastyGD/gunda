use eframe::egui::{self, Color32, RichText};
use gunda_core::download::DownloadState;

use crate::view::{DownloadView, bytes};

use super::theme::Palette;

#[derive(Clone, Copy)]
pub(super) enum Icon {
    Add,
    Pause,
    Resume,
    Cancel,
    Inspect,
}

/// A neutral action: the icon takes the text role, never its own color.
pub(super) fn action_button(
    ui: &mut egui::Ui,
    icon: Icon,
    label: &str,
    enabled: bool,
) -> egui::Response {
    let ink = if enabled {
        ui.visuals().text_color().gamma_multiply(0.82)
    } else {
        ui.visuals().weak_text_color().gamma_multiply(0.72)
    };
    button(ui, icon, label, enabled, None, Some(ink), ink)
}

/// The one accent-filled action on a screen.
pub(super) fn primary_action(
    ui: &mut egui::Ui,
    icon: Icon,
    label: &str,
    enabled: bool,
    colors: Palette,
) -> egui::Response {
    let fill = if enabled {
        colors.accent
    } else {
        colors.elevated
    };
    let ink = if enabled {
        colors.accent_ink
    } else {
        colors.muted
    };
    button(ui, icon, label, enabled, Some(fill), Some(ink), ink)
}

fn button(
    ui: &mut egui::Ui,
    icon: Icon,
    label: &str,
    enabled: bool,
    fill: Option<Color32>,
    text: Option<Color32>,
    icon_color: Color32,
) -> egui::Response {
    let id = ui.make_persistent_id(("action_icon", label));
    let text = match text {
        Some(color) => RichText::new(label).color(color),
        None => RichText::new(label),
    };
    let mut widget = egui::Button::new((egui::Atom::custom(id, egui::vec2(18.0, 18.0)), text))
        .frame(true)
        .min_size(egui::vec2(96.0, 32.0));
    if let Some(fill) = fill {
        widget = widget.fill(fill);
    }
    let button = ui.add_enabled_ui(enabled, |ui| widget.atom_ui(ui)).inner;
    if let Some(rect) = button.rect(id) {
        let color = if enabled {
            icon_color
        } else {
            ui.visuals().weak_text_color()
        };
        paint_icon(ui.painter(), rect, icon, color);
    }
    button.response
}

fn paint_icon(painter: &egui::Painter, rect: egui::Rect, icon: Icon, color: Color32) {
    let center = rect.center();
    let scale = (rect.width() / 22.0).max(0.5);
    let stroke = egui::Stroke::new(1.6, color);
    let at = |x: f32, y: f32| center + egui::vec2(x * scale, y * scale);
    match icon {
        Icon::Add => {
            painter.line_segment([at(-7.0, 0.0), at(7.0, 0.0)], stroke);
            painter.line_segment([at(0.0, -7.0), at(0.0, 7.0)], stroke);
        }
        Icon::Resume => {
            painter.add(egui::Shape::convex_polygon(
                vec![at(-5.0, -7.0), at(7.0, 0.0), at(-5.0, 7.0)],
                color,
                egui::Stroke::NONE,
            ));
        }
        Icon::Pause => {
            for offset in [-3.5, 3.5] {
                painter.line_segment([at(offset, -7.0), at(offset, 7.0)], stroke);
            }
        }
        Icon::Cancel => {
            painter.line_segment([at(-6.0, -6.0), at(6.0, 6.0)], stroke);
            painter.line_segment([at(-6.0, 6.0), at(6.0, -6.0)], stroke);
        }
        Icon::Inspect => {
            painter.rect_stroke(
                rect.shrink2(egui::vec2(3.0 * scale, 1.0)),
                2,
                stroke,
                egui::StrokeKind::Inside,
            );
            for offset in [-4.0, 0.0, 4.0] {
                painter.line_segment([at(-4.0, offset), at(4.0, offset)], stroke);
            }
        }
    }
}

pub(super) fn dialog_heading(ui: &mut egui::Ui, title: &str, subtitle: &str, colors: Palette) {
    ui.add(egui::Label::new(RichText::new(title).size(15.0).strong()).truncate())
        .on_hover_text(title);
    ui.label(RichText::new(subtitle).size(12.0).color(colors.muted));
}

pub(super) fn stat(ui: &mut egui::Ui, label: &str, value: &str, colors: Palette) {
    ui.label(RichText::new(label).size(11.0).color(colors.muted));
    ui.label(RichText::new(value).size(13.0).monospace());
}

pub(super) fn notice(ui: &mut egui::Ui, text: &str, color: Color32) {
    egui::Frame::new()
        .fill(color.gamma_multiply(0.10))
        .stroke(egui::Stroke::new(1.0, color.gamma_multiply(0.38)))
        .inner_margin(10)
        .corner_radius(6)
        .show(ui, |ui| {
            ui.colored_label(color, text);
        });
}

pub(super) fn metric(ui: &mut egui::Ui, label: &str, value: &str, colors: Palette) {
    ui.label(RichText::new(label).size(11.0).color(colors.muted));
    ui.allocate_ui_with_layout(
        egui::vec2(220.0, 18.0),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.add(egui::Label::new(RichText::new(value).size(12.0).monospace()).truncate())
                .on_hover_text(value);
        },
    );
    ui.end_row();
}

pub(super) fn display_total(view: &DownloadView) -> String {
    view.total
        .or_else(|| (view.state == DownloadState::Completed).then_some(view.written))
        .map(bytes)
        .unwrap_or_else(|| "Unknown".into())
}

/// A thin rule with its figure in mono, so progress reads like an instrument.
pub(super) fn progress(ui: &mut egui::Ui, view: &DownloadView, width: f32, colors: Palette) {
    let Some(fraction) = view.progress() else {
        ui.label(
            RichText::new(bytes(view.written))
                .size(11.0)
                .monospace()
                .color(colors.muted),
        );
        return;
    };

    ui.horizontal(|ui| {
        let bar_width = (width - 46.0).max(40.0);
        let (rect, _) = ui.allocate_exact_size(egui::vec2(bar_width, 6.0), egui::Sense::hover());
        let fill = if view.state == DownloadState::Completed {
            colors.success
        } else {
            colors.accent
        };
        let painter = ui.painter();
        painter.rect_filled(rect, 3.0, colors.track);
        if fraction > 0.0 {
            let mut filled = rect;
            filled.set_width(rect.width() * fraction);
            painter.rect_filled(filled, 3.0, fill);
        }
        ui.label(
            RichText::new(format!("{:.0}%", fraction * 100.0))
                .size(11.0)
                .monospace()
                .color(colors.muted),
        );
    });
}
