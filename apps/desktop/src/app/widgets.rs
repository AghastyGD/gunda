use eframe::egui::{self, Color32, RichText};
use gunda_core::download::DownloadState;

use crate::view::{DownloadView, bytes};

use super::theme::{ACCENT, Palette};

#[derive(Clone, Copy)]
pub(super) enum Icon {
    Add,
    Resume,
    Cancel,
    Inspect,
}

pub(super) fn action_button(
    ui: &mut egui::Ui,
    icon: Icon,
    label: &str,
    enabled: bool,
    color: Color32,
) -> egui::Response {
    let id = ui.make_persistent_id(("action_icon", label));
    let button = ui
        .add_enabled_ui(enabled, |ui| {
            egui::Button::new((egui::Atom::custom(id, egui::vec2(22.0, 22.0)), label))
                .frame(false)
                .min_size(egui::vec2(98.0, 38.0))
                .atom_ui(ui)
        })
        .inner;
    if let Some(rect) = button.rect(id) {
        let color = if enabled {
            color
        } else {
            ui.visuals().weak_text_color()
        };
        paint_icon(ui.painter(), rect, icon, color);
    }
    button.response
}

fn paint_icon(painter: &egui::Painter, rect: egui::Rect, icon: Icon, color: Color32) {
    let center = rect.center();
    let stroke = egui::Stroke::new(2.2, color);
    match icon {
        Icon::Add => {
            painter.line_segment(
                [
                    center + egui::vec2(-8.0, 0.0),
                    center + egui::vec2(8.0, 0.0),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    center + egui::vec2(0.0, -8.0),
                    center + egui::vec2(0.0, 8.0),
                ],
                stroke,
            );
        }
        Icon::Resume => {
            painter.add(egui::Shape::convex_polygon(
                vec![
                    center + egui::vec2(-5.0, -8.0),
                    center + egui::vec2(8.0, 0.0),
                    center + egui::vec2(-5.0, 8.0),
                ],
                color,
                egui::Stroke::NONE,
            ));
        }
        Icon::Cancel => {
            painter.line_segment(
                [
                    center + egui::vec2(-7.0, -7.0),
                    center + egui::vec2(7.0, 7.0),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    center + egui::vec2(-7.0, 7.0),
                    center + egui::vec2(7.0, -7.0),
                ],
                stroke,
            );
        }
        Icon::Inspect => {
            painter.rect_stroke(
                rect.shrink2(egui::vec2(4.0, 1.0)),
                1,
                stroke,
                egui::StrokeKind::Inside,
            );
            for offset in [-4.0, 0.0, 4.0] {
                painter.line_segment(
                    [
                        center + egui::vec2(-3.0, offset),
                        center + egui::vec2(3.0, offset),
                    ],
                    stroke,
                );
            }
        }
    }
}

pub(super) fn dialog_heading(
    ui: &mut egui::Ui,
    icon: Icon,
    title: &str,
    subtitle: &str,
    colors: Palette,
) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(32.0, 36.0), egui::Sense::hover());
        paint_icon(ui.painter(), rect.shrink(5.0), icon, ACCENT);
        ui.vertical(|ui| {
            ui.add(egui::Label::new(RichText::new(title).size(18.0).strong()).truncate())
                .on_hover_text(title);
            ui.label(RichText::new(subtitle).size(12.0).color(colors.muted));
        });
    });
}

pub(super) fn stat(ui: &mut egui::Ui, label: &str, value: &str, colors: Palette) {
    ui.label(RichText::new(label).size(12.0).color(colors.muted));
    ui.label(RichText::new(value).size(17.0).strong());
}

pub(super) fn notice(ui: &mut egui::Ui, text: &str, color: Color32) {
    egui::Frame::new()
        .fill(color.gamma_multiply(0.08))
        .stroke(egui::Stroke::new(1.0, color.gamma_multiply(0.35)))
        .inner_margin(8)
        .corner_radius(3)
        .show(ui, |ui| {
            ui.colored_label(color, text);
        });
}

pub(super) fn metric(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.label(RichText::new(label).weak());
    ui.add_sized([195.0, 20.0], egui::Label::new(value).truncate())
        .on_hover_text(value);
    ui.end_row();
}

pub(super) fn display_total(view: &DownloadView) -> String {
    view.total
        .or_else(|| (view.state == DownloadState::Completed).then_some(view.written))
        .map(bytes)
        .unwrap_or_else(|| "Unknown".into())
}

pub(super) fn progress(ui: &mut egui::Ui, view: &DownloadView, width: f32) {
    if let Some(fraction) = view.progress() {
        ui.add(
            egui::ProgressBar::new(fraction)
                .desired_width(width)
                .desired_height(16.0)
                .fill(if view.state == DownloadState::Completed {
                    Color32::from_rgb(49, 164, 112)
                } else {
                    ACCENT
                })
                .text(format!("{:.1}%", fraction * 100.0)),
        );
    } else {
        ui.label(format!("{} · size unknown", bytes(view.written)));
    }
}
