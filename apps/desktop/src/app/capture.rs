use eframe::egui::{self, RichText};
use gunda_capture::CandidateId;

use crate::backend::{CaptureStatus, CaptureTracks, CaptureView, Command, TrackChoice};

use super::Desktop;
use super::theme::Palette;
use super::widgets::notice;

/// What the panel asks the backend to do once the frame is built.
#[derive(Default)]
struct Actions {
    /// A captured presentation downloaded whole, in the quality the plan picks.
    download: Option<CandidateId>,
    /// A capture whose qualities the user asked to see.
    inspect: Option<CandidateId>,
    /// One chosen quality or track of a capture.
    choose: Option<(CandidateId, TrackChoice)>,
    dismiss: Option<CandidateId>,
    clear: bool,
}

impl Desktop {
    /// The browser capture panel: how to pair, and what the browser found.
    pub(super) fn capture(&mut self, ui: &mut egui::Ui) {
        let colors = Palette::new(self.dark);

        ui.label(
            RichText::new("BROWSER CAPTURE")
                .strong()
                .color(colors.muted),
        );
        ui.separator();

        if let Some(problem) = self.snapshot.capture_problem {
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

        ui.add_space(6.0);
        ui.label(RichText::new("CAPTURED").strong().color(colors.muted));
        ui.separator();

        if self.snapshot.captures.is_empty() {
            ui.label(
                RichText::new("Nothing yet. Play a video in the browser.")
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

        let chosen = actions
            .choose
            .map(|(id, track)| (id, Some(track)))
            .or_else(|| actions.download.map(|id| (id, None)));

        if let Some((id, track)) = chosen {
            match self.destination.clone() {
                Some(directory) => self.send(Command::Capture {
                    id,
                    directory,
                    track,
                }),
                None => {
                    self.error =
                        Some("Choose a download folder before downloading a capture.".into());
                }
            }
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

/// One captured resource: what it is, and what can be done with it.
fn row(
    ui: &mut egui::Ui,
    capture: &CaptureView,
    tracks: Option<&CaptureTracks>,
    colors: Palette,
    actions: &mut Actions,
) {
    ui.add(egui::Label::new(RichText::new(&capture.url).size(12.0)).truncate())
        .on_hover_text(&capture.url);

    let mut details = format!("{} · seen {}×", capture.kind, capture.seen);

    if let Some(content_type) = &capture.content_type {
        details = format!("{content_type} · {details}");
    }

    if let Some(title) = &capture.page_title {
        details = format!("{title} · {details}");
    }

    ui.label(
        RichText::new(details.clone())
            .size(11.0)
            .color(colors.muted),
    )
    .on_hover_text(details);

    ui.horizontal(|ui| {
        if capture.segmented {
            if ui.button("Qualities").clicked() {
                actions.inspect = Some(capture.id);
            }
            if ui.button("Best quality").clicked() {
                actions.download = Some(capture.id);
            }
        } else if ui.button("Download").clicked() {
            actions.download = Some(capture.id);
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
    if let Some(problem) = tracks.problem {
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
        for track in &tracks.tracks {
            ui.horizontal(|ui| {
                if ui.button("Download").clicked() {
                    actions.choose = Some((
                        capture.id,
                        TrackChoice {
                            url: track.url.clone(),
                            filename: track.filename.clone(),
                            selection: track.selection.clone(),
                        },
                    ));
                }

                ui.label(RichText::new(&track.label).size(12.0));
            });

            ui.label(RichText::new(&track.details).size(11.0).color(colors.muted));

            ui.add_space(4.0);
        }
    });
}
