use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};

use tempfile::tempdir;

#[test]
fn native_host_returns_the_existing_capture_pairing() {
    let directory = tempdir().unwrap();
    let daemon = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let daemon_endpoint = daemon.local_addr().unwrap();
    let capture = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let capture_endpoint = format!("ws://{}/capture", capture.local_addr().unwrap());
    let token = "a".repeat(64);
    std::fs::write(
        directory.path().join("gundad.endpoint"),
        format!("{daemon_endpoint}\n"),
    )
    .unwrap();
    std::fs::write(
        directory.path().join("capture.endpoint"),
        format!("{capture_endpoint}\n"),
    )
    .unwrap();
    std::fs::write(directory.path().join("capture-token"), format!("{token}\n")).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_gunda-desktop"))
        .arg("--native-host")
        .arg("--data-dir")
        .arg(directory.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let request = br#"{"kind":"pairing","version":1,"agent":"test"}"#;
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(&u32::try_from(request.len()).unwrap().to_le_bytes())
        .unwrap();
    stdin.write_all(request).unwrap();
    drop(stdin);

    let mut stdout = child.stdout.take().unwrap();
    let mut size = [0_u8; 4];
    stdout.read_exact(&mut size).unwrap();
    let mut body = vec![0_u8; usize::try_from(u32::from_le_bytes(size)).unwrap()];
    stdout.read_exact(&mut body).unwrap();
    child.wait().unwrap();

    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["kind"], "pairing");
    assert_eq!(value["endpoint"], capture_endpoint);
    assert_eq!(value["token"], token);
    drop(capture);
    drop(daemon);
}
