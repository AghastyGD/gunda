use eframe::egui::{self, RichText};
use gunda_capture::CandidateId;
use url::Url;

use crate::backend::{CaptureStatus, CaptureTracks, CaptureView, Command, TrackChoice, TrackView};
use crate::view::bytes;

use super::theme::Palette;
use super::widgets::notice;
use super::{CaptureStart, Desktop, fonts};

/// What the panel asks the backend to do once the frame is built.
#[derive(Default)]
struct Actions {
    /// A download the user asked to start; it waits for confirmation.
    start: Option<CaptureStart>,
    /// A capture whose qualities the user asked to see.
    inspect: Option<CandidateId>,
    dismiss: Option<CandidateId>,
    clear: bool,
}

impl Desktop {
    /// The browser capture panel: how to pair, and what the browser found.
    pub(super) fn capture(&mut self, ui: &mut egui::Ui) {
        let colors = Palette::new(self.dark);

        ui.label(
            RichText::new("Browser capture")
                .size(11.0)
                .family(fonts::semibold())
                .color(colors.muted),
        );
        ui.add_space(4.0);
        ui.separator();

        if let Some(problem) = self.snapshot.capture_problem.as_deref() {
            notice(ui, problem, self.error_color());

            return;
        }

        match &self.snapshot.capture {
            Some(status) => pairing(ui, status, colors),
            None => notice(
                ui,
                "The capture channel is not running.",
                self.error_color(),
            ),
        }

        ui.add_space(8.0);
        ui.label(
            RichText::new("Captured")
                .size(11.0)
                .family(fonts::semibold())
                .color(colors.muted),
        );
        ui.add_space(4.0);
        ui.separator();

        if self.snapshot.captures.is_empty() {
            ui.label(
                RichText::new("Nothing yet. Download a file or play a video in the browser.")
                    .size(11.0)
                    .color(colors.muted),
            );

            return;
        }

        let mut actions = Actions::default();

        for capture in &self.snapshot.captures {
            let tracks = self
                .snapshot
                .capture_tracks
                .as_ref()
                .filter(|tracks| tracks.id == capture.id);

            row(ui, capture, tracks, colors, &mut actions);
        }

        ui.horizontal(|ui| {
            if ui.button("Clear list").clicked() {
                actions.clear = true;
            }
            ui.label(
                RichText::new(format!("{} captured", self.snapshot.captures.len()))
                    .size(11.0)
                    .color(colors.muted),
            );
        });

        self.act(actions);
    }

    /// Turns the panel's actions into commands.
    fn act(&mut self, actions: Actions) {
        if let Some(id) = actions.inspect {
            self.send(Command::Inspect(id));
        }
        if let Some(id) = actions.dismiss {
            self.send(Command::DismissCapture(id));
        }
        if actions.clear {
            self.send(Command::ClearCaptures);
        }
        if let Some(start) = actions.start {
            self.error = None;
            self.capture_start = Some(start);
            self.transfer = None;
        }
    }
}

/// Shows the two values the extension is paired with.
fn pairing(ui: &mut egui::Ui, status: &CaptureStatus, colors: Palette) {
    ui.label(
        RichText::new("Paste these into the extension's options page.")
            .size(11.0)
            .color(colors.muted),
    );

    field(ui, "Endpoint", &status.endpoint, colors);
    field(ui, "Token", &status.token, colors);

    ui.horizontal(|ui| {
        if ui.button("Copy endpoint").clicked() {
            ui.ctx().copy_text(status.endpoint.clone());
        }
        if ui.button("Copy token").clicked() {
            ui.ctx().copy_text(status.token.clone());
        }
    });
}

/// One labelled pairing value.
fn field(ui: &mut egui::Ui, label: &str, value: &str, colors: Palette) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).size(11.0).color(colors.muted));
        ui.add(egui::Label::new(RichText::new(value).monospace().size(11.0)).truncate())
            .on_hover_text(value);
    });
}

/// A short, plain description of what was captured: its type and page.
fn capture_summary(capture: &CaptureView) -> String {
    let mut parts = vec![capture_type_label(capture).to_owned()];

    if let Some(title) = capture
        .page_title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty())
    {
        parts.push(title.to_owned());
    }

    parts.join(" · ")
}

/// The origin shown in the download dialog, without a long path or query.
fn capture_source(value: &str) -> String {
    Url::parse(value)
        .ok()
        .map(|url| {
            let origin = url.origin().ascii_serialization();
            if origin == "null" {
                value.to_owned()
            } else {
                origin
            }
        })
        .unwrap_or_else(|| value.to_owned())
}

/// The kind shown in the download dialog for a chosen track.
fn track_type_label(details: &str) -> &str {
    details
        .split(" · ")
        .next()
        .unwrap_or(details)
        .split(" - ")
        .next()
        .unwrap_or(details)
}

/// Names for captured content types, never raw MIME types.
fn capture_type_label(capture: &CaptureView) -> &'static str {
    if capture.segmented || matches!(capture.kind, "HLS" | "DASH") {
        return "Stream";
    }

    let essence = capture
        .content_type
        .as_deref()
        .unwrap_or_default()
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();

    if essence.starts_with("video/") {
        "Video"
    } else if essence.starts_with("audio/") {
        "Audio"
    } else if essence.starts_with("image/") {
        "Image"
    } else if essence == "application/pdf" {
        "PDF document"
    } else if essence.starts_with("text/") {
        "Text file"
    } else if essence.contains("zip")
        || essence.contains("compressed")
        || essence.contains("tar")
        || essence.contains("rar")
        || essence.contains("7z")
    {
        "Archive"
    } else if essence.contains("word") || essence.contains("document") {
        "Document"
    } else if essence.contains("sheet") || essence.contains("excel") {
        "Spreadsheet"
    } else if essence.contains("presentation") || essence.contains("powerpoint") {
        "Presentation"
    } else {
        "File"
    }
}

/// One captured resource: what it is, and what can be done with it.
fn row(
    ui: &mut egui::Ui,
    capture: &CaptureView,
    tracks: Option<&CaptureTracks>,
    colors: Palette,
    actions: &mut Actions,
) {
    let label = capture.filename.as_deref().unwrap_or(&capture.url);
    ui.add(egui::Label::new(RichText::new(label).size(12.0)).truncate())
        .on_hover_text(&capture.url);

    let summary = capture_summary(capture);
    let details = capture
        .size_bytes
        .map(|size| format!("{summary} · {}", bytes(size)))
        .unwrap_or_else(|| summary.clone());

    ui.label(
        RichText::new(details.clone())
            .size(11.0)
            .color(colors.muted),
    )
    .on_hover_text(&details);

    ui.horizontal(|ui| {
        if capture.selectable && ui.button("Qualities").clicked() {
            actions.inspect = Some(capture.id);
        }
        let download = if capture.segmented {
            ui.button("Best quality").clicked()
        } else {
            ui.button("Download").clicked()
        };
        if download {
            actions.start = Some(CaptureStart {
                id: capture.id,
                track: None,
                label: label.to_owned(),
                source: capture
                    .page_title
                    .clone()
                    .filter(|title| !title.trim().is_empty())
                    .unwrap_or_else(|| capture_source(&capture.url)),
                type_label: capture_type_label(capture).to_owned(),
                size_bytes: capture.size_bytes,
            });
        }
        if ui.button("Dismiss").clicked() {
            actions.dismiss = Some(capture.id);
        }
    });

    if let Some(tracks) = tracks {
        tracks_of(ui, capture, tracks, colors, actions);
    }

    ui.separator();
}

/// The qualities and tracks of one presentation, once they have been read.
fn tracks_of(
    ui: &mut egui::Ui,
    capture: &CaptureView,
    tracks: &CaptureTracks,
    colors: Palette,
    actions: &mut Actions,
) {
    if let Some(problem) = tracks.problem.as_deref() {
        notice(ui, problem, colors.muted);

        return;
    }

    if tracks.tracks.is_empty() {
        ui.label(
            RichText::new("This presentation offers nothing to choose.")
                .size(11.0)
                .color(colors.muted),
        );

        return;
    }

    ui.indent(&capture.url, |ui| {
        for (group, tracks) in grouped_tracks(&tracks.tracks) {
            if let Some(group) = group {
                ui.label(
                    RichText::new(group)
                        .size(11.0)
                        .family(fonts::semibold())
                        .color(colors.muted),
                );
                ui.add_space(2.0);
            }

            for track in tracks {
                ui.horizontal(|ui| {
                    if ui.button("Download").clicked() {
                        actions.start = Some(CaptureStart {
                            id: capture.id,
                            track: Some(TrackChoice {
                                url: track.url.clone(),
                                filename: track.filename.clone(),
                                selection: track.selection.clone(),
                            }),
                            label: track.label.clone(),
                            source: capture
                                .page_title
                                .clone()
                                .filter(|title| !title.trim().is_empty())
                                .unwrap_or_else(|| capture_source(track.url.as_str())),
                            type_label: track_type_label(&track.details).to_owned(),
                            size_bytes: track.size_bytes.or(capture.size_bytes),
                        });
                    }

                    ui.label(RichText::new(&track.label).size(12.0));
                });

                ui.label(RichText::new(&track.details).size(11.0).color(colors.muted));

                ui.add_space(4.0);
            }
        }
    });
}

/// Keeps qualities of the same audio rendition together, in first-seen order.
fn grouped_tracks(tracks: &[TrackView]) -> Vec<(Option<&str>, Vec<&TrackView>)> {
    let mut groups: Vec<(Option<&str>, Vec<&TrackView>)> = Vec::new();

    for track in tracks {
        let group = track.group.as_deref();

        if let Some((_, grouped)) = groups.iter_mut().find(|(name, _)| *name == group) {
            grouped.push(track);
        } else {
            groups.push((group, vec![track]));
        }
    }

    groups
}
