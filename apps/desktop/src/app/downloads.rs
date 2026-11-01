use std::collections::BTreeSet;

use eframe::egui::{self, Color32, RichText};
use egui_extras::{Column, TableBuilder};
use gunda_core::download::{DownloadId, DownloadState};

use crate::view::{DownloadView, bytes};

use super::{
    Desktop,
    theme::Palette,
    widgets::{display_total, metric, notice, progress},
};

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Filter {
    All,
    Active,
    Interrupted,
    Completed,
    Failed,
    Cancelled,
}

impl Filter {
    const ALL: [Self; 6] = [
        Self::All,
        Self::Active,
        Self::Interrupted,
        Self::Completed,
        Self::Failed,
        Self::Cancelled,
    ];
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::All => "All downloads",
            Self::Active => "Active",
            Self::Interrupted => "Interrupted",
            Self::Completed => "Completed",
            Self::Failed => "Failed",
            Self::Cancelled => "Cancelled",
        }
    }
    pub(super) fn matches(self, view: &DownloadView, active: &BTreeSet<DownloadId>) -> bool {
        match self {
            Self::All => true,
            Self::Active => active.contains(&view.id),
            Self::Interrupted => matches!(
                view.state,
                DownloadState::Paused | DownloadState::Interrupted
            ),
            Self::Completed => view.state == DownloadState::Completed,
            Self::Failed => view.state == DownloadState::Failed,
            Self::Cancelled => view.state == DownloadState::Cancelled,
        }
    }
}

impl Desktop {
    pub(super) fn sidebar(&mut self, ui: &mut egui::Ui) {
        let colors = Palette::new(self.dark);
        ui.label(
            RichText::new("Status")
                .size(11.0)
                .strong()
                .color(colors.muted),
        );
        ui.add_space(4.0);
        for filter in Filter::ALL {
            let count = self
                .snapshot
                .jobs
                .iter()
                .filter(|view| filter.matches(view, &self.snapshot.active))
                .count();
            let icon_id = ui.make_persistent_id(("category_icon", filter.label()));
            let response = egui::Button::selectable(
                self.filter == filter,
                (
                    egui::Atom::custom(icon_id, egui::vec2(14.0, 18.0)),
                    filter.label(),
                ),
            )
            .right_text(
                RichText::new(format!("{count}"))
                    .size(11.0)
                    .color(colors.muted),
            )
            .min_size(egui::vec2(ui.available_width(), 32.0))
            .corner_radius(6)
            .atom_ui(ui);
            if let Some(rect) = response.rect(icon_id) {
                let color = if self.filter == filter {
                    colors.accent
                } else {
                    colors.muted
                };
                paint_filter_icon(ui, rect, filter, color);
            }
            if response.clicked() {
                self.filter = filter;
                if self
                    .selected_view()
                    .is_some_and(|view| !filter.matches(view, &self.snapshot.active))
                {
                    self.selected = None;
                }
            }
        }
    }

    pub(super) fn table(&mut self, ui: &mut egui::Ui) {
        let colors = Palette::new(self.dark);
        ui.style_mut().interaction.selectable_labels = false;
        ui.spacing_mut().scroll = egui::style::ScrollStyle::solid();

        if let Some(message) = self.error.as_ref().or(self.snapshot.notice.as_ref()) {
            notice(ui, message, self.error_color());
            ui.add_space(8.0);
        }
        let query = self.search.to_lowercase();
        let visible: Vec<_> = self
            .snapshot
            .jobs
            .iter()
            .filter(|view| {
                self.filter.matches(view, &self.snapshot.active)
                    && view.name.to_lowercase().contains(&query)
            })
            .cloned()
            .collect();
        if visible.is_empty() {
            egui::ScrollArea::horizontal().show(ui, |ui| {
                ui.set_min_width(600.0);
                TableBuilder::new(ui)
                    .column(Column::remainder().at_least(160.0))
                    .column(Column::initial(96.0))
                    .column(Column::initial(128.0))
                    .column(Column::initial(78.0))
                    .column(Column::initial(72.0))
                    .column(Column::initial(78.0))
                    .min_scrolled_height(0.0)
                    .max_scroll_height(0.0)
                    .header(32.0, |mut header| {
                        for label in COLUMNS {
                            header.col(|ui| {
                                header_label(ui, label, colors);
                            });
                        }
                    })
                    .body(|_| {});
            });
            let message = if !self.snapshot.ready && self.snapshot.notice.is_some() {
                "Saved downloads unavailable"
            } else if !self.snapshot.ready {
                "Loading downloads…"
            } else if self.snapshot.jobs.is_empty() {
                "No downloads yet. Add one with Ctrl+N."
            } else {
                "No matching downloads"
            };
            ui.add_space(12.0);
            ui.label(RichText::new(message).size(12.0).color(colors.muted));
            return;
        }
        egui::ScrollArea::horizontal().show(ui, |ui| {
            ui.set_min_width(600.0);
            TableBuilder::new(ui)
                .striped(true)
                .resizable(true)
                .sense(egui::Sense::click())
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(Column::remainder().at_least(160.0))
                .column(Column::initial(96.0))
                .column(Column::initial(128.0))
                .column(Column::initial(78.0))
                .column(Column::initial(72.0))
                .column(Column::initial(78.0))
                .header(32.0, |mut header| {
                    for label in COLUMNS {
                        header.col(|ui| {
                            header_label(ui, label, colors);
                        });
                    }
                })
                .body(|body| {
                    body.rows(30.0, visible.len(), |mut row| {
                        let view = &visible[row.index()];
                        let active = self.snapshot.active.contains(&view.id);
                        row.set_selected(self.selected == Some(view.id));
                        row.col(|ui| {
                            ui.horizontal(|ui| {
                                file_icon(ui, colors);
                                ui.add(
                                    egui::Label::new(RichText::new(&view.name).size(12.0))
                                        .truncate(),
                                );
                            });
                        });
                        row.col(|ui| {
                            let color = self.state_color(view, active);
                            let response =
                                ui.label(RichText::new(view.label(active)).size(12.0).color(color));
                            if let Some(error) = &view.error {
                                response.on_hover_text(error);
                            }
                        });
                        row.col(|ui| {
                            progress(ui, view, 128.0, colors);
                        });
                        row.col(|ui| {
                            ui.label(RichText::new(display_total(view)).size(12.0).monospace());
                        });
                        row.col(|ui| {
                            let rate = if active && self.rate > 0 {
                                format!("{}/s", bytes(self.rate))
                            } else {
                                "-".into()
                            };
                            ui.label(
                                RichText::new(rate)
                                    .size(11.0)
                                    .monospace()
                                    .color(colors.muted),
                            );
                        });
                        row.col(|ui| {
                            ui.label(RichText::new(view.origin).size(11.0).color(colors.muted));
                        });
                        let response = row.response();
                        if response.clicked() {
                            self.selected = Some(view.id);
                        }
                        if response.double_clicked() {
                            self.selected = Some(view.id);
                            self.transfer = Some(view.id);
                        }
                    });
                });
        });
    }

    pub(super) fn inspector(&self, ui: &mut egui::Ui) {
        let colors = Palette::new(self.dark);
        let Some(view) = self.selected_view() else {
            ui.add_space(4.0);
            ui.label(RichText::new("Download details").size(13.0).strong());
            ui.add_space(6.0);
            ui.label(
                RichText::new("Select a download to see its progress, source, and folder.")
                    .size(12.0)
                    .color(colors.muted),
            );
            return;
        };
        let active = self.snapshot.active.contains(&view.id);
        ui.horizontal(|ui| {
            ui.add(egui::Label::new(RichText::new(&view.name).size(13.0).strong()).truncate())
                .on_hover_text(&view.name);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    RichText::new(view.label(active))
                        .size(11.0)
                        .color(self.state_color(view, active)),
                );
            });
        });
        ui.add_space(8.0);
        egui::ScrollArea::vertical().show(ui, |ui| {
            let finished = view.state == DownloadState::Completed;
            let remaining = self.remaining(view);
            let rate = (active && self.rate > 0).then(|| format!("{}/s", bytes(self.rate)));
            ui.columns(2, |columns| {
                egui::Grid::new("overview_metrics")
                    .spacing([16.0, 6.0])
                    .min_col_width(76.0)
                    .show(&mut columns[0], |ui| {
                        if finished {
                            metric(ui, "Size", &display_total(view), colors);
                        } else {
                            metric(ui, "Downloaded", &bytes(view.written), colors);
                            metric(ui, "Total size", &display_total(view), colors);
                            if remaining != "—" {
                                metric(ui, "Remaining", &remaining, colors);
                            }
                            if let Some(rate) = &rate {
                                metric(ui, "Speed", rate, colors);
                            }
                        }
                    });
                egui::Grid::new("overview_source")
                    .spacing([16.0, 6.0])
                    .min_col_width(76.0)
                    .show(&mut columns[1], |ui| {
                        metric(ui, "Site", &view.source, colors);
                        match &view.output {
                            Some(output) => {
                                metric(ui, "Saved file", &output.to_string_lossy(), colors)
                            }
                            None => {
                                metric(ui, "Folder", &view.destination.to_string_lossy(), colors)
                            }
                        }
                    });
            });
            if let Some(error) = &view.error {
                ui.add_space(8.0);
                notice(ui, error, self.error_color());
            }
        });
            if let Some(error) = &view.error {
                ui.add_space(8.0);
                notice(ui, error, self.error_color());
            }
        }
    }

const COLUMNS: [&str; 6] = ["File name", "Status", "Progress", "Size", "Speed", "Origin"];

fn header_label(ui: &mut egui::Ui, label: &str, colors: Palette) {
    let rect = ui.max_rect();
    ui.painter().hline(
        rect.x_range(),
        rect.bottom(),
        egui::Stroke::new(1.0, colors.border),
    );
    ui.label(RichText::new(label).size(11.0).color(colors.muted));
}

fn file_icon(ui: &mut egui::Ui, colors: Palette) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover());
    let stroke = egui::Stroke::new(1.4, colors.muted);
    ui.painter().rect_stroke(
        rect.shrink2(egui::vec2(2.0, 1.0)),
        2,
        stroke,
        egui::StrokeKind::Inside,
    );
    for offset in [6.0, 10.0] {
        ui.painter().line_segment(
            [
                rect.min + egui::vec2(5.0, offset),
                rect.min + egui::vec2(11.0, offset),
            ],
            stroke,
        );
    }
}

fn paint_filter_icon(ui: &egui::Ui, rect: egui::Rect, filter: Filter, color: Color32) {
    let center = rect.center();
    let stroke = egui::Stroke::new(1.6, color);
    match filter {
        Filter::Completed => {
            ui.painter().line_segment(
                [
                    center + egui::vec2(-4.0, 0.0),
                    center + egui::vec2(-1.0, 3.0),
                ],
                stroke,
            );
            ui.painter().line_segment(
                [
                    center + egui::vec2(-1.0, 3.0),
                    center + egui::vec2(5.0, -3.0),
                ],
                stroke,
            );
        }
        Filter::Active => {
            ui.painter().add(egui::Shape::convex_polygon(
                vec![
                    center + egui::vec2(-3.0, -5.0),
                    center + egui::vec2(4.0, 0.0),
                    center + egui::vec2(-3.0, 5.0),
                ],
                color,
                egui::Stroke::NONE,
            ));
        }
        Filter::Interrupted => {
            for x in [-3.0, 3.0] {
                ui.painter().line_segment(
                    [center + egui::vec2(x, -4.5), center + egui::vec2(x, 4.5)],
                    stroke,
                );
            }
        }
        Filter::Failed => {
            ui.painter().line_segment(
                [
                    center + egui::vec2(0.0, -5.0),
                    center + egui::vec2(0.0, 1.5),
                ],
                stroke,
            );
            ui.painter()
                .circle_filled(center + egui::vec2(0.0, 4.5), 1.2, color);
        }
        Filter::Cancelled => {
            ui.painter().line_segment(
                [
                    center + egui::vec2(-4.0, -4.0),
                    center + egui::vec2(4.0, 4.0),
                ],
                stroke,
            );
            ui.painter().line_segment(
                [
                    center + egui::vec2(-4.0, 4.0),
                    center + egui::vec2(4.0, -4.0),
                ],
                stroke,
            );
        }
        _ => {
            for y in [-4.0, 0.0, 4.0] {
                ui.painter().line_segment(
                    [center + egui::vec2(-4.0, y), center + egui::vec2(4.0, y)],
                    stroke,
                );
            }
        }
    }
}
