# Gunda

> download manager for the stubborn

Gunda is a Linux-first download manager written primarily in Rust. Its focus is
reliable ordinary file downloads and native support for web streaming protocols.

## Project status

Gunda is at the bootstrap stage. The repository currently contains a minimal
Rust binary, not a working download manager. HTTP transfers, SQLite persistence,
the desktop application, HLS support, browser integration, a daemon, and a CLI
are not implemented yet.

The accepted design starts with persistent HTTP download jobs. SQLite will hold
durable job state, while a Rust application layer will remain independent of
Tauri and other clients. HLS is planned after the direct HTTP path has established
the engine boundary.

## Build the current bootstrap

Install a current stable Rust toolchain, then run:

```console
cargo build
cargo test
```

The current binary can be started with `cargo run`, but it has no download
functionality yet.

## Documentation

- [Architecture overview](docs/architecture/overview.md)
- [Download lifecycle](docs/design/download-lifecycle.md)
- [Persistence and recovery](docs/design/persistence-and-recovery.md)
- [Contributing](CONTRIBUTING.md)
- [Security policy](SECURITY.md)

Accepted architectural decisions are recorded under [`docs/adr/`](docs/adr/).

## License

Gunda is licensed under the [GNU General Public License, version 3](LICENSE).
