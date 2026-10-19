//! Runs the capture listener and prints what the browser reports.
//!
//! This is the harness for working on the channel by hand: start it, pair the
//! extension with the printed endpoint and token, and play something.
//!
//! ```text
//! cargo run -p gunda-capture --example listen
//! ```

use std::error::Error;

use gunda_capture::{CaptureServer, load_or_create_token, token_path};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let directory = std::env::temp_dir().join("gunda-capture-example");
    let token = load_or_create_token(&token_path(&directory))?;

    let (server, mut handle) = CaptureServer::bind(token).await?;

    println!("endpoint: {}", handle.endpoint());
    println!("token:    {}", handle.token().as_str());
    println!("waiting for the extension...");

    tokio::spawn(server.run());

    while handle.changed().await.is_ok() {
        for candidate in handle.view().candidates() {
            println!(
                "id={} kind={:?} seen={} url={} page={} title={:?}",
                candidate.id(),
                candidate.kind(),
                candidate.seen(),
                candidate.url(),
                candidate.page_url(),
                candidate.page_title(),
            );
        }
    }

    Ok(())
}
