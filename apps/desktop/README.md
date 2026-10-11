# Rust-native desktop

```sh
cargo run -p gunda-desktop --locked
```

This workspace member connects egui to the existing HTTP executor, SQLite store,
and application manager.

The manager runs on a background Tokio runtime. The UI sends commands and receives
coalesced snapshots; active cancellation has a separate signal so it does not
wait for execution to release the manager. Folder selection retains a native
`PathBuf`; strings are display-only. Source labels omit URL credentials, paths,
and query parameters.

The shared data folder is the platform-local data directory followed by
`io.github.aghastygd.gunda`. A runtime lease prevents two clients from owning it.
Use `--data-dir PATH` for isolated manual tests. The egui window/theme settings
are stored under `ui-egui` inside that folder.

Ctrl+N opens Add download. Arrow keys select rows when a text field or dialog
does not have keyboard focus; Enter opens details. Escape or Hide dismisses a
dialog without cancelling a transfer.

## Validation

```sh
cargo test -p gunda-desktop --locked
cargo clippy -p gunda-desktop --all-targets --locked -- -D warnings
```

Integration tests use local HTTP fixtures and temporary data/output folders.
Native picker, keyboard, and platform rendering still require manual checks.
The transfer panel is an egui window within the main native window, not a separate
OS window. No simulated downloads or segmented-transfer graphics are included.
