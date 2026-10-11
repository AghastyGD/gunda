# Gunda

> download manager for the stubborn

Gunda is a cross-platform download manager written in Rust and developed Linux-first.

I'm building it around reliable downloads first, with browser integration and native support for streaming protocols such as HLS and DASH planned as the project grows.

## Status

Gunda is still under active development.

The desktop can currently download files over HTTP/HTTPS, choose a destination, show live progress, cancel an active transfer, resume safely checkpointed downloads after a restart, and keep download records in SQLite.

Only one download can run at a time. Downloads without a strong HTTP validator cannot be resumed. Browser integration, HLS, and DASH are not available yet.

## Running the desktop

The desktop is a native Rust application built with egui/eframe.

```console
cargo run -p gunda-desktop --locked
```

It requires Rust, OpenGL support, and your platform's window-system libraries.
On Linux, folder selection uses an XDG Desktop Portal with a file-picker backend
(such as GTK or KDE); Zenity is the fallback. No Node.js or pnpm is needed for
the desktop. Light/dark preference is saved between sessions.

Only one Gunda instance can use an application data folder at a time.
For an isolated development database:

```console
cargo run -p gunda-desktop --locked -- --data-dir /path/to/test-data
```

Closing the desktop ends its downloads; the next launch can recover eligible
interrupted transfers. Closing the transfer details panel only hides that panel.
Active pause, multiple simultaneous downloads, and background daemon execution
are not available yet.

Gunda is still early, so expect things to change.

If you'd like to contribute, see [CONTRIBUTING.md](CONTRIBUTING.md).

## License

Gunda is licensed under the [GNU General Public License v3.0 only](LICENSE).
