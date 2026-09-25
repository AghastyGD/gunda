# Gunda

> download manager for the stubborn

Gunda is a cross-platform download manager written in Rust and developed Linux-first.

I'm building it around reliable downloads first, with browser integration and native support for streaming protocols such as HLS and DASH planned as the project grows.

## Status

Gunda is still under active development.

The desktop can currently download files over HTTP/HTTPS, choose a destination, show live progress, cancel an active transfer, and keep download records in SQLite.

Only one download can run at a time. Resume, interrupted-download recovery, browser integration, HLS, and DASH are not available yet.

## Running the desktop

You'll need:

- Rust
- Node.js
- pnpm
- the [Tauri 2 prerequisites](https://v2.tauri.app/start/prerequisites/) for
  your operating system

Then:

```console
cd apps/desktop
pnpm install
pnpm tauri dev
```

Gunda is still early, so expect things to change.

If you'd like to contribute, see [CONTRIBUTING.md](CONTRIBUTING)
## License

Gunda is licensed under the [GNU General Public License v3.0 only](LICENSE).
