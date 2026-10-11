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
fn filters_separate_running_interrupted_and_completed_rows() {
    let view = crate::view::tests::sample(DownloadState::Interrupted);
    assert!(Filter::Interrupted.matches(&view, None));
    assert!(!Filter::Active.matches(&view, None));
    assert!(!Filter::Completed.matches(&view, None));
    assert!(Filter::Active.matches(&view, Some(view.id)));
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
            active: (!opens_add).then_some(id),
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
            active: Some(id),
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
