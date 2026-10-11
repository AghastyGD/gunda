mod app;
mod backend;
mod view;

use std::path::PathBuf;

fn data_directory() -> Result<PathBuf, &'static str> {
    let mut args = std::env::args_os().skip(1);
    if let Some(argument) = args.next() {
        if argument != "--data-dir" {
            return Err("Usage: gunda-desktop [--data-dir PATH]");
        }
        let path = args
            .next()
            .filter(|path| !path.is_empty())
            .ok_or("Provide a path after --data-dir.")?;
        if args.next().is_some() {
            return Err("Usage: gunda-desktop [--data-dir PATH]");
        }
        return Ok(path.into());
    }
    directories::BaseDirs::new()
        .map(|dirs| dirs.data_local_dir().join("io.github.aghastygd.gunda"))
        .ok_or("Could not find the application data folder.")
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let directory = data_directory().map_err(std::io::Error::other)?;
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 780.0])
            .with_min_inner_size([900.0, 600.0])
            .with_app_id("io.github.aghastygd.gunda"),
        persistence_path: Some(directory.join("ui-egui")),
        ..Default::default()
    };
    eframe::run_native(
        "Gunda",
        options,
        Box::new(move |cc| {
            let backend = backend::Backend::start(directory, cc.egui_ctx.clone())
                .map_err(|_| std::io::Error::other("Could not start Gunda."))?;
            Ok(Box::new(app::Desktop::new(cc, backend)))
        }),
    )?;
    Ok(())
}
