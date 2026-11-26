mod capture;
mod dialogs;
mod downloads;
mod fonts;
mod theme;
mod widgets;

#[cfg(test)]
mod tests;

use std::{
    collections::BTreeSet,
    fs,
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant},
};

use eframe::egui::{self, RichText};
use gunda_core::download::{DownloadId, DownloadState};

use crate::{
    backend::{Backend, Command, Snapshot, TrackChoice},
    system,
    view::{DownloadView, bytes},
};

/// Shared with download windows, which have no menu of their own.
const THEME_FILE: &str = "ui-theme";

/// A second request for the same window within this time is ignored.
const WINDOW_LAUNCH_INTERVAL: Duration = Duration::from_secs(2);

use downloads::Filter;
use theme::Palette;
use widgets::{Icon, action_button, primary_action};

/// A capture the user asked to download, waiting for confirmation.
#[derive(Clone)]
struct CaptureStart {
    id: gunda_capture::CandidateId,
    track: Option<TrackChoice>,
    label: String,
    source: String,
    type_label: String,
    size_bytes: Option<u64>,
}

pub struct Desktop {
    backend: Backend,
    snapshot: Snapshot,
    selected: Option<DownloadId>,
    transfer: Option<DownloadId>,
    filter: Filter,
    search: String,
    dark: bool,
    capture_open: bool,
    capture_start: Option<CaptureStart>,
    add_open: bool,
    url: String,
    destination: Option<PathBuf>,
    picker: Option<mpsc::Receiver<Option<PathBuf>>>,
    pending: bool,
    error: Option<String>,
    measurement: Option<(DownloadId, u64, Instant)>,
    rate: u64,
    titlebar_sent: std::cell::Cell<bool>,
    directory: PathBuf,
    /// The download this window follows, when it is not the main window.
    standalone: Option<DownloadId>,
    known_jobs: Option<BTreeSet<DownloadId>>,
    last_launch: Option<(DownloadId, Instant)>,
    window_title: String,
}

impl Desktop {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        backend: Backend,
        directory: PathBuf,
        standalone: Option<DownloadId>,
    ) -> Self {
        let theme = if standalone.is_some() {
            fs::read_to_string(directory.join(THEME_FILE)).ok()
        } else {
            cc.storage.and_then(|storage| storage.get_string("theme"))
        };
        let dark = theme.as_deref().map(str::trim) != Some("light");
        let desktop = Self {
            backend,
            snapshot: Snapshot::default(),
            selected: None,
            transfer: None,
            filter: Filter::All,
            search: String::new(),
            dark,
            capture_open: false,
            capture_start: None,
            add_open: false,
            url: String::new(),
            destination: directories::UserDirs::new()
                .and_then(|dirs| dirs.download_dir().map(PathBuf::from)),
            picker: None,
            pending: false,
            error: None,
            measurement: None,
            rate: 0,
            titlebar_sent: std::cell::Cell::new(false),
            directory,
            standalone,
            known_jobs: None,
            last_launch: None,
            window_title: String::new(),
        };
        desktop.theme(&cc.egui_ctx);
        if standalone.is_none() {
            desktop.share_theme();
        }
        desktop
    }

    /// Lets download windows open in the theme chosen here.
    fn share_theme(&self) {
        let _ = fs::write(
            self.directory.join(THEME_FILE),
            if self.dark { "dark" } else { "light" },
        );
    }

    /// Shows a download: its own window while it is unfinished, details once it is done.
    fn show_download(&mut self, id: DownloadId, state: DownloadState) {
        if self.standalone.is_some() || state == DownloadState::Completed {
            self.transfer = Some(id);
            self.capture_start = None;
            return;
        }
        if self
            .last_launch
            .is_some_and(|(last, at)| last == id && at.elapsed() < WINDOW_LAUNCH_INTERVAL)
        {
            return;
        }
        self.last_launch = Some((id, Instant::now()));
        if system::launch_download_window(&self.directory, id).is_err() {
            self.error = Some("Could not open the download window.".into());
        }
    }

    fn show_selected(&mut self) {
        if let Some(view) = self.selected_view() {
            self.show_download(view.id, view.state);
        }
    }

    fn update(&mut self) {
        if self.backend.snapshots.has_changed().unwrap_or(true) {
            let next = self.backend.snapshots.borrow_and_update().clone();
            if let Some(id) = self.standalone {
                self.selected = Some(id);
            } else {
                if self.selected.is_none()
                    && let Some(id) = next.active.iter().next().copied()
                {
                    self.selected = Some(id);
                    self.filter = Filter::All;
                    self.search.clear();
                }
                if next.ready {
                    // A download that appears while the app is open gets its window,
                    // whichever way it was started.
                    let started: Vec<_> = self.known_jobs.as_ref().map_or_else(Vec::new, |known| {
                        next.jobs
                            .iter()
                            .filter(|view| !known.contains(&view.id))
                            .map(|view| (view.id, view.state))
                            .collect()
                    });
                    self.known_jobs = Some(next.jobs.iter().map(|view| view.id).collect());
                    for (id, state) in started {
                        self.show_download(id, state);
                    }
                }
            }
            self.pending = false;
            self.snapshot = next;
        }
        if let Some(receiver) = &self.picker {
            match receiver.try_recv() {
                Ok(path) => {
                    if path.is_some() {
                        self.destination = path;
                    }
                    self.picker = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.error = Some("Could not open the folder picker. Try again.".into());
                    self.picker = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Some(view) = self.snapshot.jobs.iter().find(|view| {
            self.selected == Some(view.id)
                && self.snapshot.active.contains(&view.id)
                && view.state == DownloadState::Downloading
        }) {
            match self.measurement {
                Some((id, initial, started)) if id == view.id && view.written >= initial => {
                    let elapsed = started.elapsed().as_secs_f64();
                    if elapsed >= 1.0 {
                        self.rate = ((view.written - initial) as f64 / elapsed) as u64;
                    }
                }
                _ => {
                    self.measurement = Some((view.id, view.written, Instant::now()));
                    self.rate = 0;
                }
            }
        } else {
            self.measurement = None;
            self.rate = 0;
        }
    }

    fn selected_view(&self) -> Option<&DownloadView> {
        self.snapshot
            .jobs
            .iter()
            .find(|view| Some(view.id) == self.selected)
    }

    fn busy(&self) -> bool {
        self.snapshot.busy || self.pending
    }

    fn send(&mut self, command: Command) {
        match self.backend.send(command) {
            Ok(()) => {
                self.pending = true;
                self.error = None;
            }
            Err(message) => self.error = Some(message.into()),
        }
    }

    fn menu(&mut self, ui: &mut egui::Ui) {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui
                    .add_enabled(
                        self.snapshot.ready && !self.busy(),
                        egui::Button::new("Add download").shortcut_text("Ctrl+N"),
                    )
                    .clicked()
                {
                    self.error = None;
                    self.add_open = true;
                    ui.close();
                }
                ui.separator();
                if ui.button("Exit").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.menu_button("Downloads", |ui| {
                if ui
                    .add_enabled(self.selected.is_some(), egui::Button::new("Details"))
                    .clicked()
                {
                    self.show_selected();
                    ui.close();
                }
            });
            ui.menu_button("View", |ui| {
                ui.checkbox(&mut self.capture_open, "Browser capture");
                ui.separator();
                let previous = self.dark;
                ui.selectable_value(&mut self.dark, false, "Light mode");
                ui.selectable_value(&mut self.dark, true, "Dark mode");
                if previous != self.dark {
                    self.theme(ui.ctx());
                    self.share_theme();
                    ui.close();
                }
            });
        });
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        let colors = Palette::new(self.dark);
        ui.horizontal(|ui| {
            if primary_action(
                ui,
                Icon::Add,
                "Add download",
                self.snapshot.ready && !self.busy(),
                colors,
            )
            .on_hover_text("Add a download · Ctrl+N")
            .clicked()
            {
                self.error = None;
                self.add_open = true;
            }
            ui.separator();
            let selected = self.selected_view().cloned();
            let can_resume = selected.as_ref().is_some_and(DownloadView::can_resume);
            let can_retry = selected.as_ref().is_some_and(DownloadView::can_retry);
            let can_restart = selected.as_ref().is_some_and(DownloadView::can_restart);
            let active_selected = self
                .selected
                .is_some_and(|id| self.snapshot.active.contains(&id));
            let save_status = selected
                .as_ref()
                .is_some_and(|view| view.completion_pending);
            let resume_action = action_button(
                ui,
                Icon::Resume,
                if save_status {
                    "Save status"
                } else if can_retry {
                    "Retry"
                } else {
                    "Resume"
                },
                !self.busy() && (save_status || can_resume || can_retry),
            );
            if resume_action.clicked()
                && let Some(view) = &selected
            {
                self.send(if save_status {
                    Command::SaveStatus(view.id)
                } else if can_retry {
                    Command::Retry(view.id)
                } else {
                    Command::Resume(view.id)
                });
                if !save_status {
                    self.show_download(view.id, DownloadState::Queued);
                }
            }
            if action_button(
                ui,
                Icon::Resume,
                "Restart",
                !self.busy() && !active_selected && can_restart,
            )
            .on_hover_text(
                "Discard saved progress and download again from zero. Existing files are kept.",
            )
            .clicked()
                && let Some(view) = &selected
            {
                self.send(Command::Restart(view.id));
                self.show_download(view.id, DownloadState::Queued);
            }
            let pause_supported = self
                .selected_view()
                .is_some_and(|view| view.pause_supported);
            let mut pause =
                action_button(ui, Icon::Pause, "Pause", active_selected && pause_supported);
            if active_selected && !pause_supported {
                pause = pause
                    .on_disabled_hover_text("This server does not support resuming this download.");
            }
            if pause.clicked()
                && let Some(id) = self.selected
                && !self.backend.pause(id)
            {
                self.error = Some("This download cannot be paused safely.".into());
            }
            if action_button(
                ui,
                Icon::Cancel,
                "Cancel",
                self.selected
                    .is_some_and(|id| self.snapshot.active.contains(&id)),
            )
            .clicked()
                && let Some(id) = self.selected
                && !self.backend.cancel(id)
            {
                self.error = Some("This download is no longer running.".into());
            }
            ui.separator();
            if action_button(ui, Icon::Inspect, "Details", selected.is_some()).clicked() {
                self.show_selected();
            }
            if self.busy() {
                ui.spinner();
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.search)
                        .hint_text("Search")
                        .desired_width(220.0)
                        .margin(egui::vec2(9.0, 6.0)),
                );
            });
        });
    }

    fn remaining(&self, view: &DownloadView) -> String {
        if !self.snapshot.active.contains(&view.id) || self.rate == 0 {
            return "-".into();
        }
        view.total
            .map(|total| {
                let seconds = total.saturating_sub(view.written).div_ceil(self.rate);
                format!("About {}m {:02}s", seconds / 60, seconds % 60)
            })
            .unwrap_or_else(|| "Unknown".into())
    }
}

impl Desktop {
    fn render(&mut self, ui: &mut egui::Ui) {
        self.update();
        let ctx = ui.ctx().clone();
        if !self.titlebar_sent.replace(true) {
            self.apply_titlebar_theme(&ctx);
        }
        if let Some(id) = self.standalone {
            if self.busy() {
                ctx.request_repaint_after(Duration::from_millis(250));
            }
            self.download_window(ui, id);
            return;
        }
        if self.snapshot.ready
            && !self.busy()
            && ctx.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::N))
        {
            self.error = None;
            self.add_open = true;
        }
        if self.busy() {
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }
        if ctx.input(|input| input.key_pressed(egui::Key::Escape)) {
            self.add_open = false;
            self.transfer = None;
            self.capture_start = None;
        }
        if !self.add_open
            && self.transfer.is_none()
            && self.capture_start.is_none()
            && !ctx.egui_wants_keyboard_input()
        {
            let down = ctx.input(|input| input.key_pressed(egui::Key::ArrowDown));
            let up = ctx.input(|input| input.key_pressed(egui::Key::ArrowUp));
            if down || up {
                let query = self.search.to_lowercase();
                let ids: Vec<_> = self
                    .snapshot
                    .jobs
                    .iter()
                    .filter(|view| {
                        self.filter.matches(view, &self.snapshot.active)
                            && view.name.to_lowercase().contains(&query)
                    })
                    .map(|view| view.id)
                    .collect();
                if !ids.is_empty() {
                    let current = ids.iter().position(|id| Some(*id) == self.selected);
                    let index = match current {
                        None => 0,
                        Some(index) if down => (index + 1).min(ids.len() - 1),
                        Some(index) => index.saturating_sub(1),
                    };
                    self.selected = Some(ids[index]);
                }
            }
            if ctx.input(|input| input.key_pressed(egui::Key::Enter)) {
                self.show_selected();
            }
        }
        let colors = Palette::new(self.dark);
        let border = egui::Stroke::new(1.0, colors.border);
        egui::Panel::top("menu")
            .frame(
                egui::Frame::new()
                    .fill(colors.surface)
                    .inner_margin(egui::Margin::symmetric(12, 4)),
            )
            .show(ui, |ui| self.menu(ui));
        egui::Panel::top("toolbar")
            .frame(
                egui::Frame::new()
                    .fill(colors.surface)
                    .stroke(border)
                    .inner_margin(egui::Margin::symmetric(12, 8)),
            )
            .show(ui, |ui| self.toolbar(ui));
        egui::Panel::bottom("status")
            .frame(
                egui::Frame::new()
                    .fill(colors.surface)
                    .stroke(border)
                    .inner_margin(egui::Margin::symmetric(12, 8)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!("{} downloads", self.snapshot.jobs.len()))
                            .size(11.0)
                            .color(colors.muted),
                    );
                    ui.add_space(4.0);
                    let speed = if self.rate > 0 {
                        format!("{}/s", bytes(self.rate))
                    } else if self.busy() {
                        "Working".into()
                    } else {
                        "Idle".into()
                    };
                    ui.label(
                        RichText::new(speed)
                            .size(11.0)
                            .monospace()
                            .color(colors.muted),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if self.snapshot.capture.is_some() {
                            let found = self.snapshot.captures.len();
                            let text = if found == 0 {
                                "Browser capture".to_owned()
                            } else {
                                format!("Browser capture · {found}")
                            };
                            ui.label(RichText::new(text).size(11.0).color(colors.muted));
                        } else if self.snapshot.capture_problem.is_some() {
                            ui.label(
                                RichText::new("Browser capture unavailable")
                                    .size(11.0)
                                    .color(colors.muted),
                            );
                        }
                    });
                });
            });
        egui::Panel::left("library")
            .resizable(true)
            .default_size(200.0)
            .size_range(168.0..=280.0)
            .frame(
                egui::Frame::new()
                    .fill(colors.surface)
                    .stroke(border)
                    .inner_margin(12),
            )
            .show(ui, |ui| self.sidebar(ui));
        if self.capture_open {
            egui::Panel::right("capture")
                .resizable(true)
                .default_size(300.0)
                .size_range(240.0..=460.0)
                .frame(
                    egui::Frame::new()
                        .fill(colors.surface)
                        .stroke(border)
                        .inner_margin(12),
                )
                .show(ui, |ui| {
                    egui::ScrollArea::vertical().show(ui, |ui| self.capture(ui));
                });
        }
        egui::Panel::bottom("inspector")
            .resizable(true)
            .default_size(150.0)
            .size_range(100.0..=240.0)
            .frame(
                egui::Frame::new()
                    .fill(colors.surface)
                    .stroke(border)
                    .inner_margin(12),
            )
            .show(ui, |ui| self.inspector(ui));
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(colors.background).inner_margin(8))
            .show(ui, |ui| self.table(ui));
        self.add_dialog(&ctx);
        if self.capture_start.is_some() {
            self.capture_dialog(&ctx);
        } else {
            self.transfer_dialog(&ctx);
        }
    }
}

impl eframe::App for Desktop {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.render(ui);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        storage.set_string("theme", if self.dark { "dark" } else { "light" }.into());
    }
}
