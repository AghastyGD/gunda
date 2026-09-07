# Gunda

> download manager for the stubborn

Gunda is a cross-platform download manager developed Linux-first, focused on reliable file downloads and native support for modern web streaming protocols.

## Project status

Gunda is in early development and is not yet usable as a download manager.

The Rust workspace currently provides:

- A download domain model with lifecycle rules, request context, destinations,
  resource metadata, progress, failures, and application command and event types.
- SQLite persistence for creating, finding, and listing initial queued jobs.
- A download manager that loads persisted jobs at startup, exposes read-only
  snapshots, and persists new jobs before adding them to its runtime registry.
- Structured tracing for manager and storage operations, with tests guarding
  against sensitive data appearing in diagnostics.

Network transfers, persistent lifecycle updates, interrupted-download recovery,
and executable clients are not implemented yet.

## Build

Install a current stable Rust toolchain, then run:

```console
cargo build --workspace --all-features
cargo test --workspace --all-features
```

The workspace currently contains libraries, not a runnable application.

CI builds and tests the workspace on Linux and Windows. Formatting and Clippy
checks run on Linux. See [Contributing](CONTRIBUTING.md) for the complete local
validation commands.

## Documentation

* [Architecture overview](docs/architecture/overview.md)
* [Download lifecycle](docs/design/download-lifecycle.md)
* [Persistence and recovery](docs/design/persistence-and-recovery.md)
* [Contributing](CONTRIBUTING.md)
* [Security policy](SECURITY.md)

Accepted architectural decisions are recorded in [`docs/adr/`](docs/adr/).

## License

Gunda is licensed under the [GNU General Public License v3.0 only](LICENSE).
