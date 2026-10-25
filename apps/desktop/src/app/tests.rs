use std::collections::BTreeSet;

use super::widgets::display_total;
use super::*;

fn desktop(snapshot: Snapshot) -> Desktop {
    Desktop {
        backend: Backend::with_snapshot(snapshot),
        snapshot: Snapshot::default(),
        selected: None,
        transfer: None,
        filter: Filter::All,
        search: String::new(),
        dark: true,
        capture_open: false,
        add_open: false,
        url: String::new(),
        destination: None,
        picker: None,
        pending: false,
        error: None,
        measurement: None,
        rate: 0,
    }
}

#[test]
fn stopped_transfer_dialog_keeps_restart_separate_from_resume_and_retry() {
    for (state, retryable, continuation) in [
        (DownloadState::Paused, false, Some("Resume")),
        (DownloadState::Interrupted, false, Some("Resume")),
        (DownloadState::Failed, true, Some("Retry")),
        (DownloadState::Cancelled, false, None),
        (DownloadState::Completed, false, None),
    ] {
        for dark in [false, true] {
            let ctx = egui::Context::default();
            let mut view = crate::view::tests::sample(state);
            view.retryable = retryable;
            let id = view.id;
            let mut desktop = desktop(Snapshot {
                ready: true,
                jobs: vec![view],
                ..Default::default()
            });
            desktop.update();
            desktop.transfer = Some(id);
            desktop.dark = dark;
            desktop.theme(&ctx);
            for frame in 0..2 {
                let mut output = ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(900.0, 600.0),
                        )),
                        ..Default::default()
                    },
                    |ui| desktop.transfer_dialog(ui.ctx()),
                );
                if frame == 1 {
                    let has_label = |label: &str| {
                        output.shapes.iter().any(|shape|
                        matches!(&shape.shape, egui::Shape::Text(text) if text.galley.job.text == label))
                    };
                    assert!(has_label("Restart"));
                    assert_eq!(has_label("Resume"), continuation == Some("Resume"));
                    assert_eq!(has_label("Retry"), continuation == Some("Retry"));
                }
                output.textures_delta.clear();
            }
        }
    }
}

#[test]
fn published_output_offers_save_status_instead_of_transfer_actions() {
    for dark in [false, true] {
        let ctx = egui::Context::default();
        let mut view = crate::view::tests::sample(DownloadState::Finalizing);
        view.completion_pending = true;
        view.output = Some(std::path::PathBuf::from("Downloads/file.bin"));
        let id = view.id;
        let mut desktop = desktop(Snapshot {
            ready: true,
            jobs: vec![view],
            ..Default::default()
        });
        desktop.update();
        desktop.transfer = Some(id);
        desktop.dark = dark;
        desktop.theme(&ctx);
        for frame in 0..2 {
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(900.0, 600.0),
                    )),
                    ..Default::default()
                },
                |ui| desktop.transfer_dialog(ui.ctx()),
            );
            if frame == 1 {
                let has_label = |label: &str| {
                    output.shapes.iter().any(|shape|
                    matches!(&shape.shape, egui::Shape::Text(text) if text.galley.job.text == label))
                };
                assert!(has_label("Save status"));
                assert!(has_label("File saved"));
                assert!(!has_label("Resume"));
                assert!(!has_label("Retry"));
                assert!(!has_label("Restart"));
            }
            output.textures_delta.clear();
        }
    }
}

#[test]
fn filename_hover_has_one_tooltip_and_rows_use_the_arrow_cursor() {
    for dark in [false, true] {
        let ctx = egui::Context::default();
        let mut view = crate::view::tests::sample(DownloadState::Completed);
        view.name = "a-very-long-download-filename-that-does-not-fit-inside-the-file-name-column-and-must-be-truncated.mp4".into();
        let name = view.name.clone();
        let id = view.id;
        let mut desktop = desktop(Snapshot {
            ready: true,
            jobs: vec![view],
            ..Default::default()
        });
        desktop.update();
        desktop.dark = dark;
        desktop.theme(&ctx);
        let theme = if dark {
            egui::Theme::Dark
        } else {
            egui::Theme::Light
        };
        ctx.style_mut_of(theme, |style| style.interaction.tooltip_delay = 0.0);
        {
            let mut render = |time: f64, events: Vec<egui::Event>| {
                ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(900.0, 300.0),
                        )),
                        time: Some(time),
                        events,
                        ..Default::default()
                    },
                    |ui| desktop.table(ui),
                )
            };
            let mut initial = render(0.0, Vec::new());
            let filename = initial
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) if text.galley.job.text == name => {
                        assert!(text.galley.elided);
                        Some(text.pos + egui::vec2(10.0, 5.0))
                    }
                    _ => None,
                })
                .expect("filename must be visible");
            let status = initial
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) if text.galley.job.text == "Completed" => {
                        Some(text.pos + egui::vec2(10.0, 5.0))
                    }
                    _ => None,
                })
                .expect("status must be visible");
            initial.textures_delta.clear();
            let mut hovered = render(1.0, vec![egui::Event::PointerMoved(filename)]);
            assert_eq!(
                hovered.platform_output.cursor_icon,
                egui::CursorIcon::Default
            );
            hovered.textures_delta.clear();
            let mut tooltip = render(2.0, Vec::new());
            let full_names = tooltip.shapes.iter().filter(|shape|
            matches!(&shape.shape, egui::Shape::Text(text) if text.galley.job.text == name && !text.galley.elided)).count();
            assert_eq!(
                full_names, 1,
                "truncated filename must have exactly one tooltip"
            );
            tooltip.textures_delta.clear();
            let mut status_hover = render(3.0, vec![egui::Event::PointerMoved(status)]);
            assert_eq!(
                status_hover.platform_output.cursor_icon,
                egui::CursorIcon::Default
            );
            status_hover.textures_delta.clear();
            for (time, pressed) in [(4.0, true), (4.1, false), (4.2, true), (4.3, false)] {
                let mut clicked = render(
                    time,
                    vec![
                        egui::Event::PointerMoved(filename),
                        egui::Event::PointerButton {
                            pos: filename,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                );
                clicked.textures_delta.clear();
            }
        }
        assert_eq!(
            desktop.selected,
            Some(id),
            "filename clicks must still select the row"
        );
        assert_eq!(
            desktop.transfer,
            Some(id),
            "filename double-click must still open details"
        );
        assert!(
            ctx.style_of(theme).interaction.selectable_labels,
            "table settings must not change other panels"
        );
    }
}

#[test]
fn filters_separate_running_interrupted_and_completed_rows() {
    let view = crate::view::tests::sample(DownloadState::Interrupted);
    assert!(Filter::Interrupted.matches(&view, &BTreeSet::new()));
    assert!(!Filter::Active.matches(&view, &BTreeSet::new()));
    assert!(!Filter::Completed.matches(&view, &BTreeSet::new()));
    assert!(Filter::Active.matches(&view, &BTreeSet::from([view.id])));
}

#[test]
fn completed_unknown_size_uses_final_bytes_only_after_completion() {
    let mut view = crate::view::tests::sample(DownloadState::Downloading);
    view.total = None;
    assert_eq!(display_total(&view), "Unknown");
    view.state = DownloadState::Completed;
    assert_eq!(display_total(&view), "25 B");
}

#[test]
fn closed_backend_still_delivers_its_last_failure_snapshot() {
    let mut desktop = desktop(Snapshot {
        notice: Some("Could not open saved downloads.".into()),
        ..Default::default()
    });
    desktop.update();
    assert!(desktop.snapshot.notice.is_some());
    assert!(!desktop.snapshot.ready);
}

#[test]
fn keyboard_shortcut_opens_add_dialog_and_escape_hides_transfer_details() {
    for (key, modifiers, opens_add) in [
        (egui::Key::N, egui::Modifiers::COMMAND, true),
        (egui::Key::Escape, egui::Modifiers::NONE, false),
    ] {
        let ctx = egui::Context::default();
        let view = crate::view::tests::sample(DownloadState::Downloading);
        let id = view.id;
        let mut desktop = desktop(Snapshot {
            ready: true,
            busy: !opens_add,
            active: if opens_add {
                BTreeSet::new()
            } else {
                BTreeSet::from([id])
            },
            jobs: vec![view],
            ..Default::default()
        });
        desktop.theme(&ctx);
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1180.0, 780.0),
                )),
                events: vec![
                    egui::Event::ModifiersChanged(modifiers),
                    egui::Event::Key {
                        key,
                        physical_key: Some(key),
                        pressed: true,
                        repeat: false,
                        modifiers,
                    },
                ],
                ..Default::default()
            },
            |ui| desktop.render(ui),
        );
        assert_eq!(desktop.add_open, opens_add);
        if !opens_add {
            assert!(desktop.transfer.is_none());
            assert!(desktop.snapshot.busy);
        }
        output.textures_delta.clear();
    }
}

#[test]
fn empty_loading_and_error_views_render_in_both_themes() {
    for dark in [false, true] {
        for (ready, notice, title) in [
            (
                true,
                None,
                "No downloads. Use File > Add download or Ctrl+N.",
            ),
            (false, None, "Loading downloads…"),
            (
                false,
                Some("Could not load saved downloads."),
                "Saved downloads unavailable",
            ),
        ] {
            let ctx = egui::Context::default();
            let mut desktop = desktop(Snapshot {
                ready,
                notice: notice.map(str::to_owned),
                ..Default::default()
            });
            desktop.dark = dark;
            desktop.theme(&ctx);
            for size in [[1180.0, 780.0], [900.0, 600.0]] {
                let mut output = ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size.into())),
                        ..Default::default()
                    },
                    |ui| desktop.render(ui),
                );
                assert!(output.shapes.iter().any(|shape| {
                    matches!(&shape.shape, egui::Shape::Text(text) if text.galley.job.text == title)
                }), "missing empty-state heading: {title}");
                assert!(desktop.snapshot.jobs.is_empty());
                for heading in ["File name", "Status", "Progress", "Size", "Rate", "Origin"] {
                    assert!(output.shapes.iter().any(|shape| {
                        matches!(&shape.shape, egui::Shape::Text(text) if text.galley.job.text == heading)
                    }), "missing table heading: {heading}");
                }
                output.textures_delta.clear();
            }
        }
    }
}

#[test]
fn unknown_size_transfer_dialog_does_not_show_a_percentage() {
    for dark in [false, true] {
        let ctx = egui::Context::default();
        let mut view = crate::view::tests::sample(DownloadState::Downloading);
        view.total = None;
        let id = view.id;
        let mut desktop = desktop(Snapshot {
            ready: true,
            busy: true,
            active: BTreeSet::from([id]),
            jobs: vec![view],
            ..Default::default()
        });
        desktop.transfer = Some(id);
        desktop.dark = dark;
        desktop.theme(&ctx);
        for frame in 0..2 {
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(900.0, 600.0),
                    )),
                    ..Default::default()
                },
                |ui| desktop.render(ui),
            );
            if frame == 1 {
                assert!(output.shapes.iter().any(|shape| {
                    matches!(&shape.shape, egui::Shape::Text(text) if text.galley.job.text == "The final size will appear when the download finishes.")
                }));
                assert!(output.shapes.iter().any(|shape| {
                    matches!(&shape.shape, egui::Shape::Text(text) if text.galley.job.text == "This server does not support resuming this download. Cancelling will stop it permanently.")
                }));
                assert!(!output.shapes.iter().any(|shape| {
                    matches!(&shape.shape, egui::Shape::Text(text) if text.galley.job.text.contains('%'))
                }));
            }
            output.textures_delta.clear();
        }
    }
}

#[test]
fn both_themes_render_table_inspector_and_dialogs_at_supported_sizes() {
    let ctx = egui::Context::default();
    let view = crate::view::tests::sample(DownloadState::Interrupted);
    let id = view.id;
    let mut desktop = desktop(Snapshot {
        ready: true,
        jobs: vec![view],
        ..Default::default()
    });
    desktop.selected = Some(id);
    desktop.transfer = Some(id);
    desktop.add_open = true;
    for dark in [false, true] {
        desktop.dark = dark;
        desktop.theme(&ctx);
        for size in [[1180.0, 780.0], [900.0, 600.0]] {
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size.into())),
                    ..Default::default()
                },
                |ui| desktop.render(ui),
            );
            assert!(!output.shapes.is_empty());
            output.textures_delta.clear();
        }
    }
}

#[test]
fn selected_rows_and_progress_keep_readable_text_in_both_themes() {
    for dark in [false, true] {
        let ctx = egui::Context::default();
        let mut view = crate::view::tests::sample(DownloadState::Completed);
        view.total = Some(view.written);
        let name = view.name.clone();
        let id = view.id;
        let mut desktop = desktop(Snapshot {
            ready: true,
            jobs: vec![view],
            ..Default::default()
        });
        desktop.dark = dark;
        desktop.selected = Some(id);
        desktop.theme(&ctx);
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1180.0, 780.0),
                )),
                ..Default::default()
            },
            |ui| desktop.render(ui),
        );
        output.textures_delta.clear();
        let selection_text = ctx
            .style_of(if dark {
                egui::Theme::Dark
            } else {
                egui::Theme::Light
            })
            .visuals
            .selection
            .stroke
            .color;
        assert_ne!(selection_text, ACCENT);
        for (text, expected) in [(name.as_str(), selection_text), ("100.0%", Color32::WHITE)] {
            assert!(output.shapes.iter().any(|shape| {
                matches!(&shape.shape, egui::Shape::Text(shape)
                    if shape.galley.job.text == text
                        && shape.galley.job.sections.iter().all(|section| section.format.color == expected))
            }), "missing readable text: {text}, dark={dark}");
        }
    }
}

#[test]
fn capture_panel_shows_the_pairing_and_the_captured_resources() {
    use std::sync::Arc;

    use crate::backend::{CaptureStatus, CaptureView};

    let ctx = egui::Context::default();
    let mut desktop = desktop(Snapshot {
        ready: true,
        capture: Some(Arc::new(CaptureStatus {
            endpoint: "ws://127.0.0.1:48123/capture".into(),
            token: "0123456789abcdef".repeat(4),
        })),
        captures: vec![CaptureView {
            id: 7,
            url: "https://cdn.example.com/movie.mp4".into(),
            kind: "File",
            content_type: Some("video/mp4".into()),
            page_title: Some("A movie page".into()),
            seen: 2,
            segmented: false,
            selectable: false,
        }],
        ..Default::default()
    });
    desktop.update();

    let labels = capture_labels(&ctx, &mut desktop);
    let has = |needle: &str| labels.iter().any(|label| label.contains(needle));

    assert!(has("BROWSER CAPTURE"), "missing panel heading: {labels:?}");
    assert!(has("Endpoint"), "missing endpoint label: {labels:?}");
    assert!(has("Token"), "missing token label: {labels:?}");
    assert!(has("Copy endpoint"), "missing endpoint copy: {labels:?}");
    assert!(has("video/mp4"), "missing content type: {labels:?}");
    assert!(has("A movie page"), "missing page title: {labels:?}");
    assert!(
        has("Download") && has("Dismiss"),
        "missing row actions: {labels:?}"
    );
}

#[test]
fn capture_panel_offers_the_qualities_of_a_presentation() {
    use std::sync::Arc;

    use url::Url;

    use crate::backend::{CaptureStatus, CaptureTracks, CaptureView, TrackView};

    let ctx = egui::Context::default();
    let mut desktop = desktop(Snapshot {
        ready: true,
        capture: Some(Arc::new(CaptureStatus {
            endpoint: "ws://127.0.0.1:48123/capture".into(),
            token: "0123456789abcdef".repeat(4),
        })),
        captures: vec![CaptureView {
            id: 3,
            url: "https://cdn.example.com/master.m3u8".into(),
            kind: "HLS",
            content_type: Some("application/vnd.apple.mpegurl".into()),
            page_title: Some("A film".into()),
            seen: 1,
            segmented: true,
            selectable: true,
        }],
        capture_tracks: Some(CaptureTracks {
            id: 3,
            tracks: vec![
                TrackView {
                    label: "1920x1080 (5.0 Mbps)".into(),
                    details: "video | 1.2 GiB".into(),
                    url: Url::parse("https://cdn.example.com/1080/index.m3u8")
                        .expect("the URL is valid"),
                    filename: Some("master.mp4".into()),
                    selection: None,
                },
                TrackView {
                    label: "English (en)".into(),
                    details: "audio | 128.0 MiB".into(),
                    url: Url::parse("https://cdn.example.com/audio/index.m3u8")
                        .expect("the URL is valid"),
                    filename: Some("master.mp4".into()),
                    selection: None,
                },
            ],
            problem: None,
        }),
        ..Default::default()
    });
    desktop.update();

    let labels = capture_labels(&ctx, &mut desktop);
    let has = |needle: &str| labels.iter().any(|label| label.contains(needle));

    assert!(has("Qualities"), "missing quality action: {labels:?}");
    assert!(
        has("1920x1080 (5.0 Mbps)"),
        "missing video quality: {labels:?}"
    );
    assert!(has("video | 1.2 GiB"), "missing video details: {labels:?}");
    assert!(has("English (en)"), "missing audio track: {labels:?}");
    assert!(
        has("audio | 128.0 MiB"),
        "missing audio details: {labels:?}"
    );
}

#[test]
fn capture_panel_reports_a_channel_that_did_not_start() {
    let ctx = egui::Context::default();
    let mut desktop = desktop(Snapshot {
        ready: true,
        capture_problem: Some("The browser capture channel could not be started.".into()),
        ..Default::default()
    });
    desktop.update();

    let labels = capture_labels(&ctx, &mut desktop);

    assert!(
        labels
            .iter()
            .any(|label| label.contains("could not be started")),
        "missing problem message: {labels:?}"
    );
    assert!(
        !labels.iter().any(|label| label.contains("Copy token")),
        "pairing offered without a channel: {labels:?}"
    );
}

/// Renders the capture panel alone and returns every text it drew.
fn capture_labels(ctx: &egui::Context, desktop: &mut Desktop) -> Vec<String> {
    let mut output = ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(320.0, 600.0),
            )),
            ..Default::default()
        },
        |ui| desktop.capture(ui),
    );
    output.textures_delta.clear();

    output
        .shapes
        .iter()
        .filter_map(|shape| match &shape.shape {
            egui::Shape::Text(text) => Some(text.galley.job.text.clone()),
            _ => None,
        })
        .collect()
}
