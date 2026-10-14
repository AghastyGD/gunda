use eframe::egui::{self, Color32, RichText};
use egui_extras::{Column, TableBuilder};
use gunda_core::download::{DownloadId, DownloadState};

use crate::view::{DownloadView, bytes};

use super::{
    Desktop,
    theme::{ACCENT, Palette},
    widgets::{display_total, metric, progress},
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
    pub(super) fn matches(self, view: &DownloadView, active: Option<DownloadId>) -> bool {
        match self {
            Self::All => true,
            Self::Active => active == Some(view.id),
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
        ui.label(RichText::new("STATUS").strong().color(colors.muted));
        ui.separator();
        for filter in Filter::ALL {
            let count = self
                .snapshot
                .jobs
                .iter()
                .filter(|view| filter.matches(view, self.snapshot.active))
                .count();
            let icon_id = ui.make_persistent_id(("category_icon", filter.label()));
            let response = egui::Button::selectable(
                self.filter == filter,
                (
                    egui::Atom::custom(icon_id, egui::vec2(12.0, 16.0)),
                    filter.label(),
                ),
            )
            .right_text(RichText::new(format!("({count})")))
            .min_size(egui::vec2(ui.available_width(), 22.0))
            .corner_radius(0)
            .atom_ui(ui);
            if let Some(rect) = response.rect(icon_id) {
                let color = match filter {
                    Filter::Completed => Color32::from_rgb(52, 170, 111),
                    Filter::Failed => self.error_color(),
                    Filter::Active | Filter::All => ACCENT,
                    _ => colors.muted,
                };
                let center = rect.center();
                let stroke = egui::Stroke::new(1.8, color);
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
            if response.clicked() {
                self.filter = filter;
                if self
                    .selected_view()
                    .is_some_and(|view| !filter.matches(view, self.snapshot.active))
                {
                    self.selected = None;
                }
            }
        }
    }

    pub(super) fn table(&mut self, ui: &mut egui::Ui) {
        ui.style_mut().interaction.selectable_labels = false;
        ui.spacing_mut().scroll = egui::style::ScrollStyle::solid();

        if let Some(message) = self.error.as_ref().or(self.snapshot.notice.as_ref()) {
            ui.horizontal_wrapped(|ui| {
                ui.colored_label(self.error_color(), message);
            });
        }
        ui.add_space(4.0);
        let query = self.search.to_lowercase();
        let visible: Vec<_> = self
            .snapshot
            .jobs
            .iter()
            .filter(|view| {
                self.filter.matches(view, self.snapshot.active)
                    && view.name.to_lowercase().contains(&query)
            })
            .cloned()
            .collect();
        if visible.is_empty() {
            egui::ScrollArea::horizontal().show(ui, |ui| {
                ui.set_min_width(690.0);
                TableBuilder::new(ui)
                    .column(Column::remainder().at_least(180.0))
                    .column(Column::initial(102.0))
                    .column(Column::initial(145.0))
                    .column(Column::initial(85.0))
                    .column(Column::initial(90.0))
                    .column(Column::initial(80.0))
                    .min_scrolled_height(0.0)
                    .max_scroll_height(0.0)
                    .header(23.0, |mut header| {
                        for label in ["File name", "Status", "Progress", "Size", "Rate", "Origin"] {
                            header.col(|ui| {
                                ui.label(RichText::new(label).strong().weak());
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
                "No downloads. Use File > Add download or Ctrl+N."
            } else {
                "No matching downloads"
            };
            ui.add_space(6.0);
            ui.label(RichText::new(message).weak());
            return;
        }
        egui::ScrollArea::horizontal().show(ui, |ui| {
            ui.set_min_width(690.0);
            TableBuilder::new(ui)
                .striped(true)
                .resizable(true)
                .sense(egui::Sense::click())
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(Column::remainder().at_least(180.0))
                .column(Column::initial(102.0))
                .column(Column::initial(145.0))
                .column(Column::initial(85.0))
                .column(Column::initial(90.0))
                .column(Column::initial(80.0))
                .header(23.0, |mut header| {
                    for label in ["File name", "Status", "Progress", "Size", "Rate", "Origin"] {
                        header.col(|ui| {
                            ui.label(RichText::new(label).strong().weak());
                        });
                    }
                })
                .body(|body| {
                    body.rows(22.0, visible.len(), |mut row| {
                        let view = &visible[row.index()];
                        let active = self.snapshot.active == Some(view.id);
                        row.set_selected(self.selected == Some(view.id));
                        row.col(|ui| {
                            ui.horizontal(|ui| {
                                let (rect, _) = ui.allocate_exact_size(
                                    egui::vec2(16.0, 18.0),
                                    egui::Sense::hover(),
                                );
                                let stroke = egui::Stroke::new(1.4, Palette::new(self.dark).muted);
                                ui.painter().rect_stroke(
                                    rect.shrink2(egui::vec2(2.0, 0.0)),
                                    2,
                                    stroke,
                                    egui::StrokeKind::Inside,
                                );
                                for offset in [7.0, 11.0] {
                                    ui.painter().line_segment(
                                        [
                                            rect.min + egui::vec2(5.0, offset),
                                            rect.min + egui::vec2(11.0, offset),
                                        ],
                                        stroke,
                                    );
                                }
                                ui.add(egui::Label::new(RichText::new(&view.name)).truncate());
                            });
                        });
                        row.col(|ui| {
                            let color = self.state_color(view, active);
                            let response = ui.colored_label(color, view.label(active));
                            if let Some(error) = &view.error {
                                response.on_hover_text(error);
                            }
                        });
                        row.col(|ui| {
                            progress(ui, view, 135.0);
                        });
                        row.col(|ui| {
                            ui.label(display_total(view));
                        });
                        row.col(|ui| {
                            ui.label(if active && self.rate > 0 {
                                format!("{}/s", bytes(self.rate))
                            } else {
                                "—".into()
                            });
                        });
                        row.col(|ui| {
                            ui.label(view.origin);
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
        let Some(view) = self.selected_view() else {
            ui.add_space(8.0);
            ui.label(RichText::new("Download details").strong());
            ui.add_space(6.0);
            ui.label(
                RichText::new("Select a file to view its progress, source, and destination.")
                    .weak(),
            );
            return;
        };
        ui.horizontal(|ui| {
            ui.label(RichText::new("Overview").strong().color(ACCENT));
            ui.separator();
            ui.label(RichText::new(&view.name));
        });
        ui.separator();
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.columns(2, |columns| {
                egui::Grid::new("overview_metrics")
                    .spacing([12.0, 3.0])
                    .show(&mut columns[0], |ui| {
                        metric(
                            ui,
                            "Status",
                            view.label(self.snapshot.active == Some(view.id)),
                        );
                        metric(ui, "Downloaded", &bytes(view.written));
                        metric(ui, "Total size", &display_total(view));
                        metric(ui, "Remaining", &self.remaining(view));
                    });
                egui::Grid::new("overview_source")
                    .spacing([12.0, 3.0])
                    .show(&mut columns[1], |ui| {
                        metric(ui, "Destination", &view.destination.to_string_lossy());
                        metric(ui, "Source site", &view.source);
                        metric(ui, "Origin", view.origin);
                        if let Some(output) = &view.output {
                            metric(ui, "Saved file", &output.to_string_lossy());
                        }
                    });
            });
            if let Some(error) = &view.error {
                ui.colored_label(self.error_color(), error);
            }
        });
    }
}
