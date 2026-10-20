use std::error::Error;
use std::io;
use std::path::PathBuf;

fn data_directory() -> Result<PathBuf, &'static str> {
    let mut args = std::env::args_os().skip(1);

    if let Some(argument) = args.next() {
        if argument != "--data-dir" {
            return Err("Usage: gundad [--data-dir PATH]");
        }

        let path = args
            .next()
            .filter(|path| !path.is_empty())
            .ok_or("Provide a path after --data-dir.")?;
        if args.next().is_some() {
            return Err("Usage: gundad [--data-dir PATH]");
        }

        return Ok(path.into());
    }

    directories::BaseDirs::new()
        .map(|dirs| dirs.data_local_dir().join("io.github.aghastygd.gunda"))
        .ok_or("Could not find the application data folder.")
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let directory = data_directory().map_err(io::Error::other)?;

    gundad::run(directory).await?;

    Ok(())
}
