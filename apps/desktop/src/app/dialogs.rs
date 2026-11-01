use std::sync::mpsc;

use eframe::egui::{self, RichText};
use gunda_core::download::DownloadState;

use crate::{
    backend::{Command, validate_url},
    view::bytes,
};

use super::{
    Desktop,
    theme::Palette,
    widgets::{dialog_heading, display_total, notice, stat},
};

impl Desktop {
    fn browse(&mut self, ctx: &egui::Context) {
        let (sender, receiver) = mpsc::channel();
        let directory = self.destination.clone();
        let ctx = ctx.clone();
        match std::thread::Builder::new()
            .name("gunda-folder-picker".into())
            .spawn(move || {
                let mut dialog = rfd::FileDialog::new().set_title("Choose download folder");
                if let Some(directory) = directory {
                    dialog = dialog.set_directory(directory);
                }
                let _ = sender.send(dialog.pick_folder());
                ctx.request_repaint();
            }) {
            Ok(_) => self.picker = Some(receiver),
            Err(_) => self.error = Some("Could not open the folder picker.".into()),
        }
    }

    pub(super) fn capture_dialog(&mut self, ctx: &egui::Context) {
        let Some(start) = self.capture_start.clone() else {
            return;
        };
        let colors = Palette::new(self.dark);
        let mut open = true;
        let mut cancel = false;
        let mut start_now = false;

        egui::Window::new("Start download")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(560.0)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.add(
                    egui::Label::new(RichText::new(&start.label).size(15.0).strong()).truncate(),
                )
                .on_hover_text(&start.label);
                ui.label(RichText::new(&start.details).size(12.0).color(colors.muted));
                ui.add_space(18.0);
                ui.horizontal(|ui| {
                    stat(
                        ui,
                        "File size",
                        &start
                            .size_bytes
                            .map(bytes)
                            .unwrap_or_else(|| "Unknown".into()),
                        colors,
                    );
                });
                ui.add_space(18.0);
                ui.label(RichText::new("Save to").size(12.0).strong());
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    let label = self
                        .destination
                        .as_ref()
                        .map(|path| path.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "Choose a folder".into());
                    let width = (ui.available_width() - 108.0).max(100.0);
                    egui::Frame::new()
                        .fill(colors.background)
                        .stroke(egui::Stroke::new(1.0, colors.border))
                        .corner_radius(6)
                        .inner_margin(egui::Margin::symmetric(10, 8))
                        .show(ui, |ui| {
                            ui.allocate_ui_with_layout(
                                egui::vec2(width - 20.0, 18.0),
                                egui::Layout::left_to_right(egui::Align::Center),
                                |ui| {
                                    ui.add(egui::Label::new(&label).truncate())
                                        .on_hover_text(&label);
                                },
                            );
                        });
                    if ui
                        .add_enabled(
                            self.picker.is_none(),
                            egui::Button::new(if self.picker.is_some() {
                                "Opening…"
                            } else {
                                "Browse…"
                            })
                            .min_size(egui::vec2(96.0, 36.0)),
                        )
                        .clicked()
                    {
                        self.browse(ctx);
                    }
                });
                if let Some(error) = &self.error {
                    ui.add_space(12.0);
                    notice(ui, error, self.error_color());
                }
                ui.add_space(18.0);
                ui.separator();
                ui.add_space(8.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add_enabled(
                            !self.busy() && self.snapshot.ready && self.picker.is_none(),
                            egui::Button::new(
                                RichText::new("Start download")
                                    .color(colors.accent_ink)
                                    .strong(),
                            )
                            .fill(colors.accent)
                            .min_size(egui::vec2(132.0, 36.0)),
                        )
                        .clicked()
                    {
                        start_now = true;
                    }
                    if ui
                        .add(egui::Button::new("Cancel").min_size(egui::vec2(88.0, 36.0)))
                        .clicked()
                    {
                        cancel = true;
                    }
                });
            });

        open &= !cancel;
        if start_now {
            match self.destination.clone() {
                Some(directory) => {
                    self.send(Command::Capture {
                        id: start.id,
                        directory,
                        track: start.track,
                    });
                    self.capture_start = None;
                    self.error = None;
                }
                None => self.error = Some("Choose a destination folder.".into()),
            }
        } else if !open {
            self.capture_start = None;
        }
    }

    pub(super) fn add_dialog(&mut self, ctx: &egui::Context) {
        if !self.add_open {
            return;
        }
        let colors = Palette::new(self.dark);
        let mut open = true;
        egui::Window::new("Add download")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(560.0)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label(
                    RichText::new("Paste a link and choose where to save it.")
                        .size(12.0)
                        .color(colors.muted),
                );
                ui.add_space(18.0);
                ui.label(RichText::new("Link").size(12.0).strong());
                ui.add_space(4.0);
                ui.add(
                    egui::TextEdit::singleline(&mut self.url)
                        .hint_text("https://example.com/file.zip")
                        .desired_width(f32::INFINITY)
                        .margin(egui::vec2(10.0, 8.0)),
                );
                ui.add_space(18.0);
                ui.label(RichText::new("Save to").size(12.0).strong());
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    let label = self
                        .destination
                        .as_ref()
                        .map(|path| path.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "Choose a folder".into());
                    let width = (ui.available_width() - 108.0).max(100.0);
                    egui::Frame::new()
                        .fill(colors.background)
                        .stroke(egui::Stroke::new(1.0, colors.border))
                        .corner_radius(6)
                        .inner_margin(egui::Margin::symmetric(10, 8))
                        .show(ui, |ui| {
                            ui.allocate_ui_with_layout(
                                egui::vec2(width - 20.0, 18.0),
                                egui::Layout::left_to_right(egui::Align::Center),
                                |ui| {
                                    ui.add(egui::Label::new(&label).truncate())
                                        .on_hover_text(&label);
                                },
                            );
                        });
                    if ui
                        .add_enabled(
                            self.picker.is_none(),
                            egui::Button::new(if self.picker.is_some() {
                                "Opening…"
                            } else {
                                "Browse…"
                            })
                            .min_size(egui::vec2(96.0, 36.0)),
                        )
                        .clicked()
                    {
                        self.browse(ctx);
                    }
                });
                ui.add_space(10.0);
                ui.label(
                    RichText::new("If the file already exists, Gunda keeps both.")
                        .size(12.0)
                        .color(colors.muted),
                );
                if let Some(error) = &self.error {
                    ui.add_space(12.0);
                    notice(ui, error, self.error_color());
                }
                ui.add_space(18.0);
                ui.separator();
                ui.add_space(8.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add_enabled(
                            !self.busy() && self.snapshot.ready && self.picker.is_none(),
                            egui::Button::new(
                                RichText::new("Start download")
                                    .color(colors.accent_ink)
                                    .strong(),
                            )
                            .fill(colors.accent)
                            .min_size(egui::vec2(132.0, 36.0)),
                        )
                        .clicked()
                    {
                        match (validate_url(&self.url), self.destination.clone()) {
                            (Ok(url), Some(directory)) => {
                                self.send(Command::Start { url, directory });
                                if self.pending {
                                    self.url.clear();
                                    self.add_open = false;
                                }
                            }
                            (Err(message), _) => self.error = Some(message.into()),
                            (_, None) => self.error = Some("Choose a destination folder.".into()),
                        }
                    }
                    if ui
                        .add(egui::Button::new("Cancel").min_size(egui::vec2(88.0, 36.0)))
                        .clicked()
                    {
                        self.add_open = false;
                    }
                });
            });
        self.add_open &= open;
    }

    pub(super) fn transfer_dialog(&mut self, ctx: &egui::Context) {
        let Some(view) = self
            .snapshot
            .jobs
            .iter()
            .find(|view| Some(view.id) == self.transfer)
            .cloned()
        else {
            return;
        };
        let active = self.snapshot.active.contains(&view.id);
        let colors = Palette::new(self.dark);
        let color = self.state_color(&view, active);
        let mut open = true;
        egui::Window::new("Download details")
            .id(egui::Id::new("transfer"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(560.0)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                dialog_heading(ui, &view.name, &view.source, colors);
                ui.add_space(16.0);
                egui::Frame::new()
                    .fill(colors.background)
                    .stroke(egui::Stroke::new(1.0, colors.border))
                    .corner_radius(8)
                    .inner_margin(16)
                    .show(ui, |ui| {
                        ui.set_min_width(496.0);
                        ui.horizontal(|ui| {
                            ui.colored_label(color, RichText::new(view.label(active)).strong());
                            if active && view.state == DownloadState::Inspecting {
                                ui.spinner();
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if let Some(fraction) = view.progress() {
                                        ui.label(
                                            RichText::new(format!("{:.0}%", fraction * 100.0))
                                                .size(13.0)
                                                .monospace(),
                                        );
                                    } else {
                                        ui.label(
                                            RichText::new("Size unknown")
                                                .size(12.0)
                                                .color(colors.muted),
                                        );
                                    }
                                },
                            );
                        });
                        ui.add_space(12.0);
                        if let Some(fraction) = view.progress() {
                            let width = ui.available_width();
                            let (rect, _) = ui.allocate_exact_size(
                                egui::vec2(width, 8.0),
                                egui::Sense::hover(),
                            );
                            ui.painter().rect_filled(rect, 4.0, colors.track);
                            if fraction > 0.0 {
                                let mut filled = rect;
                                filled.set_width(rect.width() * fraction);
                                ui.painter().rect_filled(
                                    filled,
                                    4.0,
                                    if view.state == DownloadState::Completed {
                                        colors.success
                                    } else {
                                        colors.accent
                                    },
                                );
                            }
                        } else if active {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label(
                                    RichText::new(
                                        "The final size will appear when the download finishes.",
                                    )
                                    .size(12.0)
                                    .color(colors.muted),
                                );
                            });
                        }
                        ui.add_space(16.0);
                        ui.columns(2, |columns| {
                            stat(&mut columns[0], "Downloaded", &bytes(view.written), colors);
                            stat(&mut columns[1], "File size", &display_total(&view), colors);
                        });
                        ui.add_space(12.0);
                        ui.columns(2, |columns| {
                            stat(
                                &mut columns[0],
                                "Speed",
                                &if active && self.rate > 0 {
                                    format!("{}/s", bytes(self.rate))
                                } else {
                                    "-".into()
                                },
                                colors,
                            );
                            stat(&mut columns[1], "Time left", &self.remaining(&view), colors);
                        });
                    });
                ui.add_space(16.0);
                ui.label(
                    RichText::new(if view.output.is_some() {
                        "Saved file"
                    } else {
                        "Folder"
                    })
                    .size(12.0)
                    .strong(),
                );
                ui.add_space(4.0);
                let destination = view
                    .output
                    .as_ref()
                    .unwrap_or(&view.destination)
                    .to_string_lossy();
                ui.add(egui::Label::new(destination.as_ref()).truncate())
                    .on_hover_text(destination.as_ref());
                if let Some(error) = &view.error {
                    ui.add_space(12.0);
                    notice(ui, error, self.error_color());
                }
                if active && !view.pause_supported {
                    ui.add_space(10.0);
                    ui.label(
                        RichText::new(
                            "This server does not support resuming this download. Cancelling will stop it permanently.",
                        )
                        .size(12.0)
                        .color(colors.muted),
                    );
                }
                ui.add_space(16.0);
                ui.separator();
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if active {
                        ui.label(
                            RichText::new("Hide keeps the download running.")
                                .size(11.0)
                                .color(colors.muted),
                        );
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add(egui::Button::new("Hide").min_size(egui::vec2(80.0, 36.0)))
                            .clicked()
                        {
                            self.transfer = None;
                        }
                        if active {
                            if ui
                                .add(
                                    egui::Button::new("Cancel download")
                                        .min_size(egui::vec2(124.0, 36.0)),
                                )
                                .clicked()
                            {
                                self.backend.cancel(view.id);
                            }
                            if ui
                                .add_enabled(
                                    view.pause_supported,
                                    egui::Button::new("Pause")
                                        .min_size(egui::vec2(92.0, 36.0)),
                                )
                                .on_disabled_hover_text(
                                    "This server does not support resuming this download.",
                                )
                                .clicked()
                                && !self.backend.pause(view.id)
                            {
                                self.error = Some("This download cannot be paused safely.".into());
                            }
                        } else {
                            if view.can_restart()
                                && ui.add_enabled(!self.busy(), egui::Button::new("Restart")
                                    .min_size(egui::vec2(92.0, 36.0)))
                                    .on_hover_text("Discard saved progress and download again from zero. Existing files are kept.")
                                    .clicked()
                            {
                                self.send(Command::Restart(view.id));
                            }
                            if (view.completion_pending || view.can_resume() || view.can_retry())
                                && ui.add_enabled(
                                    !self.busy(),
                                    egui::Button::new(RichText::new(if view.completion_pending {
                                        "Save status"
                                    } else if view.can_retry() {
                                        "Retry"
                                    } else {
                                        "Resume"
                                    }).color(colors.accent_ink))
                                        .fill(colors.accent)
                                        .min_size(egui::vec2(92.0, 36.0)),
                                ).clicked()
                            {
                                self.send(if view.completion_pending {
                                    Command::SaveStatus(view.id)
                                } else if view.can_retry() {
                                    Command::Retry(view.id)
                                } else {
                                    Command::Resume(view.id)
                                });
                            }
                        }
                    });
                });
            });
        if !open {
            self.transfer = None;
        }
    }
}
