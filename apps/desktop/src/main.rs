mod app;
mod backend;
mod native_host;
mod system;
mod view;

use std::env;
use std::error::Error;
use std::io;
use std::path::PathBuf;

use gunda_core::download::DownloadId;

use backend::WindowClaim;

const USAGE: &str =
    "Usage: gunda-desktop [--daemon|--native-host|--download-window ID] [--data-dir PATH]";

enum Launch {
    Desktop(PathBuf),
    DownloadWindow(PathBuf, DownloadId),
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
                return Err(USAGE);
            }
            let path = args
                .next()
                .filter(|path| !path.is_empty())
                .ok_or("Provide a path after --data-dir.")?;
            if args.next().is_some() {
                return Err(USAGE);
            }
            return Ok(Launch::Daemon(path.into()));
        }
        if argument == "--download-window" {
            let id = args
                .next()
                .and_then(|id| id.to_str()?.parse::<i64>().ok())
                .and_then(|id| DownloadId::new(id).ok())
                .ok_or("Provide a download number after --download-window.")?;
            let Some(argument) = args.next() else {
                return Ok(Launch::DownloadWindow(default_data_directory()?, id));
            };
            if argument != "--data-dir" {
                return Err(USAGE);
            }
            let path = args
                .next()
                .filter(|path| !path.is_empty())
                .ok_or("Provide a path after --data-dir.")?;
            if args.next().is_some() {
                return Err(USAGE);
            }
            return Ok(Launch::DownloadWindow(path.into(), id));
        }
        if argument == "--native-host" {
            let Some(argument) = args.next() else {
                return Ok(Launch::NativeHost(default_data_directory()?));
            };
            if argument != "--data-dir" {
                return Err(USAGE);
            }
            let path = args
                .next()
                .filter(|path| !path.is_empty())
                .ok_or("Provide a path after --data-dir.")?;
            if args.next().is_some() {
                return Err(USAGE);
            }
            return Ok(Launch::NativeHost(path.into()));
        }
        if argument != "--data-dir" {
            return Err(USAGE);
        }
        let path = args
            .next()
            .filter(|path| !path.is_empty())
            .ok_or("Provide a path after --data-dir.")?;
        if args.next().is_some() {
            return Err(USAGE);
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
    let (directory, download) = match invocation().map_err(io::Error::other)? {
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
        Launch::Desktop(directory) => (directory, None),
        Launch::DownloadWindow(directory, id) => (directory, Some(id)),
    };
    if download.is_none()
        && let Err(error) = native_host::install(&directory)
    {
        eprintln!("Could not register the browser capture host: {error}");
    }
    let lease = match backend::claim_window(directory.clone(), download) {
        WindowClaim::Owned(lease) => lease,
        WindowClaim::Shown => return Ok(()),
    };
    let viewport =
        eframe::egui::ViewportBuilder::default().with_app_id("io.github.aghastygd.gunda");
    let (viewport, persistence) = if download.is_some() {
        (
            viewport
                .with_inner_size([600.0, 430.0])
                .with_min_inner_size([520.0, 380.0]),
            "ui-egui-download",
        )
    } else {
        (
            viewport
                .with_inner_size([1180.0, 780.0])
                .with_min_inner_size([900.0, 600.0]),
            "ui-egui",
        )
    };
    let options = eframe::NativeOptions {
        viewport,
        persistence_path: Some(directory.join(persistence)),
        run_and_return: false,
        ..Default::default()
    };
    eframe::run_native(
        "Gunda",
        options,
        Box::new(move |cc| {
            lease.attach(&cc.egui_ctx);
            let backend = backend::Backend::start(directory.clone(), cc.egui_ctx.clone())
                .map_err(|_| io::Error::other("Could not start Gunda."))?;
            Ok(Box::new(app::Desktop::new(
                cc, backend, directory, download,
            )))
        }),
    )?;
    Ok(())
}
