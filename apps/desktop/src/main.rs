mod app;
mod backend;
mod native_host;
mod view;

use std::env;
use std::error::Error;
use std::io;
use std::path::PathBuf;

enum Launch {
    Desktop(PathBuf),
    Daemon(PathBuf),
    NativeHost(PathBuf),
}

fn invocation() -> Result<Launch, &'static str> {
    let mut args = env::args_os().skip(1);

    if let Some(argument) = args.next() {
        if argument == "--daemon" {
            let Some(argument) = args.next() else {
                return Ok(Launch::Daemon(default_data_directory()?));
            };
            if argument != "--data-dir" {
                return Err("Usage: gunda-desktop [--daemon|--native-host] [--data-dir PATH]");
            }
            let path = args
                .next()
                .filter(|path| !path.is_empty())
                .ok_or("Provide a path after --data-dir.")?;
            if args.next().is_some() {
                return Err("Usage: gunda-desktop [--daemon|--native-host] [--data-dir PATH]");
            }
            return Ok(Launch::Daemon(path.into()));
        }
        if argument == "--native-host" {
            let Some(argument) = args.next() else {
                return Ok(Launch::NativeHost(default_data_directory()?));
            };
            if argument != "--data-dir" {
                return Err("Usage: gunda-desktop [--daemon|--native-host] [--data-dir PATH]");
            }
            let path = args
                .next()
                .filter(|path| !path.is_empty())
                .ok_or("Provide a path after --data-dir.")?;
            if args.next().is_some() {
                return Err("Usage: gunda-desktop [--daemon|--native-host] [--data-dir PATH]");
            }
            return Ok(Launch::NativeHost(path.into()));
        }
        if argument != "--data-dir" {
            return Err("Usage: gunda-desktop [--daemon|--native-host] [--data-dir PATH]");
        }
        let path = args
            .next()
            .filter(|path| !path.is_empty())
            .ok_or("Provide a path after --data-dir.")?;
        if args.next().is_some() {
            return Err("Usage: gunda-desktop [--daemon|--native-host] [--data-dir PATH]");
        }
        return Ok(Launch::Desktop(path.into()));
    }

    Ok(Launch::Desktop(default_data_directory()?))
}

fn default_data_directory() -> Result<PathBuf, &'static str> {
    directories::BaseDirs::new()
        .map(|dirs| dirs.data_local_dir().join("io.github.aghastygd.gunda"))
        .ok_or("Could not find the application data folder.")
}

fn main() -> Result<(), Box<dyn Error>> {
    let directory = match invocation().map_err(io::Error::other)? {
        Launch::Daemon(directory) => {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(gundad::run(directory))?;
            return Ok(());
        }
        Launch::NativeHost(directory) => {
            native_host::run(directory)?;
            return Ok(());
        }
        Launch::Desktop(directory) => directory,
    };
    let _ = native_host::install(&directory);
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 780.0])
            .with_min_inner_size([900.0, 600.0])
            .with_app_id("io.github.aghastygd.gunda"),
        persistence_path: Some(directory.join("ui-egui")),
        run_and_return: false,
        ..Default::default()
    };
    eframe::run_native(
        "Gunda",
        options,
        Box::new(move |cc| {
            let backend = backend::Backend::start(directory, cc.egui_ctx.clone())
                .map_err(|_| io::Error::other("Could not start Gunda."))?;
            Ok(Box::new(app::Desktop::new(cc, backend)))
        }),
    )?;
    Ok(())
}
