# Gunda

> download manager for the stubborn

Gunda is a cross-platform download manager developed Linux-first, focused on reliable file downloads and native support for modern web streaming protocols.

## Project status

Gunda is in early development. The repository currently contains the initial project structure and documentation, but no usable download manager yet.

The first implementation milestone is persistent HTTP downloads backed by SQLite. The core application layer is designed to remain independent of the desktop client, with native HLS support planned after the direct HTTP path establishes the engine boundary.

## Build

Install a current stable Rust toolchain, then run:

```sh
cargo build
cargo test
```

The current binary can be started with `cargo run`, but download functionality is not implemented yet.

## Documentation

* [Architecture overview](docs/architecture/overview.md)
* [Download lifecycle](docs/design/download-lifecycle.md)
* [Persistence and recovery](docs/design/persistence-and-recovery.md)
* [Contributing](CONTRIBUTING.md)
* [Security policy](SECURITY.md)

Accepted architectural decisions are recorded in [`docs/adr/`](docs/adr/).

## License

Gunda is licensed under the [GNU General Public License v3.0 only](LICENSE).
