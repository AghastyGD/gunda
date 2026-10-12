mod dialogs;
mod downloads;
mod theme;
mod widgets;

#[cfg(test)]
mod tests;

use std::{path::PathBuf, sync::mpsc, time::Instant};

use eframe::egui::{self, Color32, RichText};
use gunda_core::download::{DownloadId, DownloadState};

use crate::{
    backend::{Backend, Command, Snapshot},
    view::{DownloadView, bytes},
};

use downloads::Filter;
use theme::{ACCENT, Palette};
use widgets::{Icon, action_button};

pub struct Desktop {
    backend: Backend,
    snapshot: Snapshot,
    selected: Option<DownloadId>,
    transfer: Option<DownloadId>,
    filter: Filter,
    search: String,
    dark: bool,
    add_open: bool,
    url: String,
    destination: Option<PathBuf>,
    picker: Option<mpsc::Receiver<Option<PathBuf>>>,
    pending: bool,
    error: Option<String>,
    measurement: Option<(DownloadId, u64, Instant)>,
    rate: u64,
}

impl Desktop {
    pub fn new(cc: &eframe::CreationContext<'_>, backend: Backend) -> Self {
        let dark = cc
            .storage
            .and_then(|storage| storage.get_string("theme"))
            .as_deref()
            != Some("light");
        let desktop = Self {
            backend,
            snapshot: Snapshot::default(),
            selected: None,
            transfer: None,
            filter: Filter::All,
            search: String::new(),
            dark,
            add_open: false,
            url: String::new(),
            destination: directories::UserDirs::new()
                .and_then(|dirs| dirs.download_dir().map(PathBuf::from)),
            picker: None,
            pending: false,
            error: None,
            measurement: None,
            rate: 0,
        };
        desktop.theme(&cc.egui_ctx);
        desktop
    }

    fn update(&mut self) {
        if self.backend.snapshots.has_changed().unwrap_or(true) {
            let next = self.backend.snapshots.borrow_and_update().clone();
            if next.active.is_some() && next.active != self.snapshot.active {
                self.selected = next.active;
                self.transfer = next.active;
                self.filter = Filter::All;
                self.search.clear();
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
            Some(view.id) == self.snapshot.active && view.state == DownloadState::Downloading
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
                    .add_enabled(
                        self.selected.is_some(),
                        egui::Button::new("Transfer details"),
                    )
                    .clicked()
                {
                    self.transfer = self.selected;
                    ui.close();
                }
            });
            ui.menu_button("View", |ui| {
                let previous = self.dark;
                ui.selectable_value(&mut self.dark, false, "Light mode");
                ui.selectable_value(&mut self.dark, true, "Dark mode");
                if previous != self.dark {
                    self.theme(ui.ctx());
                    ui.close();
                }
            });
        });
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if action_button(
                ui,
                Icon::Add,
                "Add download",
                self.snapshot.ready && !self.busy(),
                ACCENT,
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
            let mut restart_action = action_button(
                ui,
                Icon::Resume,
                if can_retry {
                    "Retry"
                } else if can_restart {
                    "Restart"
                } else {
                    "Resume"
                },
                !self.busy() && (can_resume || can_retry || can_restart),
                Color32::from_rgb(52, 170, 111),
            );
            if can_restart {
                restart_action = restart_action
                    .on_hover_text("Discard saved progress and start again from zero.");
            }
            if restart_action.clicked() {
                let id = selected.as_ref().unwrap().id;
                self.send(if can_retry {
                    Command::Retry(id)
                } else if can_restart {
                    Command::Restart(id)
                } else {
                    Command::Resume(id)
                });
            }
            let active_selected = self.selected.is_some() && self.selected == self.snapshot.active;
            let mut pause = action_button(
                ui,
                Icon::Pause,
                "Pause",
                active_selected && self.snapshot.pause_supported,
                Color32::from_rgb(218, 156, 55),
            );
            if active_selected && !self.snapshot.pause_supported {
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
                self.selected.is_some() && self.selected == self.snapshot.active,
                self.error_color(),
            )
            .clicked()
                && let Some(id) = self.selected
                && !self.backend.cancel(id)
            {
                self.error = Some("This download is no longer running.".into());
            }
            ui.separator();
            if action_button(ui, Icon::Inspect, "Details", selected.is_some(), ACCENT).clicked() {
                self.transfer = self.selected;
            }
            if self.busy() {
                ui.spinner();
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.search)
                        .hint_text("Filter downloads…")
                        .desired_width(200.0),
                );
            });
        });
    }

    fn remaining(&self, view: &DownloadView) -> String {
        if self.snapshot.active != Some(view.id) || self.rate == 0 {
            return "—".into();
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
        }
        if !self.add_open && self.transfer.is_none() && !ctx.egui_wants_keyboard_input() {
            let down = ctx.input(|input| input.key_pressed(egui::Key::ArrowDown));
            let up = ctx.input(|input| input.key_pressed(egui::Key::ArrowUp));
            if down || up {
                let query = self.search.to_lowercase();
                let ids: Vec<_> = self
                    .snapshot
                    .jobs
                    .iter()
                    .filter(|view| {
                        self.filter.matches(view, self.snapshot.active)
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
                self.transfer = self.selected;
            }
        }
        let colors = Palette::new(self.dark);
        let border = egui::Stroke::new(1.0, colors.border);
        egui::Panel::top("menu")
            .frame(
                egui::Frame::new()
                    .fill(colors.surface)
                    .inner_margin(egui::Margin::symmetric(8, 3)),
            )
            .show(ui, |ui| self.menu(ui));
        egui::Panel::top("toolbar")
            .frame(
                egui::Frame::new()
                    .fill(colors.surface)
                    .stroke(border)
                    .inner_margin(egui::Margin::symmetric(8, 5)),
            )
            .show(ui, |ui| self.toolbar(ui));
        egui::Panel::bottom("status")
            .frame(
                egui::Frame::new()
                    .fill(colors.surface)
                    .stroke(border)
                    .inner_margin(egui::Margin::symmetric(10, 5)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(format!("{} downloads", self.snapshot.jobs.len()));
                    ui.separator();
                    ui.label(if self.rate > 0 {
                        format!("Download speed: {}/s", bytes(self.rate))
                    } else if self.busy() {
                        "Running".into()
                    } else {
                        "Idle".into()
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(RichText::new("HTTP / HTTPS").small().weak());
                    });
                });
            });
        egui::Panel::left("library")
            .resizable(true)
            .default_size(180.0)
            .size_range(150.0..=260.0)
            .frame(
                egui::Frame::new()
                    .fill(colors.surface)
                    .stroke(border)
                    .inner_margin(8),
            )
            .show(ui, |ui| self.sidebar(ui));
        egui::Panel::bottom("inspector")
            .resizable(true)
            .default_size(160.0)
            .size_range(80.0..=260.0)
            .frame(
                egui::Frame::new()
                    .fill(colors.surface)
                    .stroke(border)
                    .inner_margin(6),
            )
            .show(ui, |ui| self.inspector(ui));
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(colors.background).inner_margin(3))
            .show(ui, |ui| self.table(ui));
        self.add_dialog(&ctx);
        self.transfer_dialog(&ctx);
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
