# Gunda

> download manager for the stubborn

Gunda is a cross-platform download manager developed Linux-first, with a Rust
engine and a Tauri + SvelteKit desktop interface. Reliable file downloads come
first; native support for web streaming protocols is on the roadmap.

## Project status

The desktop is an early development preview with support for direct HTTP and HTTPS downloads.

You can:

- Paste a link and choose a destination through the native folder picker.
- Download files with streamed writes and live byte progress.
- Cancel the active download.
- View completed downloads, output paths, and failures.
- Reopen the application and load download records from SQLite.

Only one download can run at a time. Downloads currently run inside the desktop process, so Gunda must remain open while a transfer is active. Coordinated shutdown, resume, and interrupted-download recovery are not available yet, and cancelled or interrupted downloads may leave partial files in the destination directory.

Links must currently point directly to a file. Redirects, automatic retries, browser integration, HLS, and DASH are not supported yet.

The planned daemon architecture will eventually separate download execution from the desktop, allowing transfers to continue independently of the desktop window.

## Run the desktop

Install:

- A current stable Rust toolchain.
- Node.js 24 and pnpm 11.3.0, matching the CI setup.
- The [Tauri 2 prerequisites](https://v2.tauri.app/start/prerequisites/) for your
  operating system, including the native development libraries on Linux or the
  C++ build tools and WebView2 on Windows.

From the repository root:

```console
cd apps/desktop
pnpm install --frozen-lockfile
pnpm tauri dev
```

This starts the frontend development server and opens the native application.
Running `pnpm dev` alone starts only the frontend; downloading and folder
selection require the Tauri application.

To build the desktop without generating installers, run from `apps/desktop`:

```console
pnpm tauri build --no-bundle
```

The Tauri build command also builds the frontend.

## Development

From the repository root:

```console
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
cargo build --workspace --all-features --locked
```

The workspace includes the desktop, so these checks also require Tauri's native
development dependencies. To check the frontend, run from `apps/desktop`:

```console
pnpm check
pnpm build
```

CI builds and tests the workspace on Linux and Windows. Formatting and Clippy
checks run on Linux; frontend checks and desktop builds run on both platforms.
These checks do not replace testing the graphical application on each platform.

### Repository layout

- `apps/desktop/`: Tauri application and SvelteKit interface.
- `crates/gunda-core/`: download domain, lifecycle, and application manager.
- `crates/gunda-http/`: direct HTTP transfers and file finalization.
- `crates/gunda-storage/`: SQLite persistence and migrations.
- `docs/`: architecture, design documents, and decisions.

## Documentation

* [Architecture overview](docs/architecture/overview.md)
* [Download lifecycle](docs/design/download-lifecycle.md)
* [Persistence and recovery](docs/design/persistence-and-recovery.md)
* [Contributing](CONTRIBUTING.md)
* [Security policy](SECURITY.md)

Accepted architectural decisions are recorded in [`docs/adr/`](docs/adr/).

## License

Gunda is licensed under the [GNU General Public License v3.0 only](LICENSE).
