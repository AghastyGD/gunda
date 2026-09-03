# Gunda

> download manager for the stubborn

Gunda is a cross-platform download manager developed Linux-first, focused on reliable file downloads and native support for modern web streaming protocols.

## Project status

Gunda is in early development and is not yet usable as a download manager.

The repository currently contains the initial core domain and application model, including download lifecycle management, request context, destinations, resource metadata, commands, events, progress tracking, and failure semantics.

The next foundation work focuses on persistent storage with SQLite, followed by the first end-to-end HTTP download path.

## Build

Install a current stable Rust toolchain, then run:

```bash
cargo build --workspace
cargo test --workspace
```
Gunda is not yet runnable as an application. The repository currently contains the core domain and application foundations, while executable clients will be introduced in later milestones.

## Documentation

* [Architecture overview](docs/architecture/overview.md)
* [Download lifecycle](docs/design/download-lifecycle.md)
* [Persistence and recovery](docs/design/persistence-and-recovery.md)
* [Contributing](CONTRIBUTING.md)
* [Security policy](SECURITY.md)

Accepted architectural decisions are recorded in [`docs/adr/`](docs/adr/).

## License

Gunda is licensed under the [GNU General Public License v3.0 only](LICENSE).
