# Gunda

> download manager for the stubborn

Gunda is a cross-platform download manager written in Rust and developed Linux-first.

I'm building Gunda with reliable downloads as the priority. It's under active development, with download history and support for resuming interrupted transfers when supported by the server.

## Run from source

With Rust installed:

```console
git clone https://github.com/AghastyGD/gunda.git
cd gunda
cargo run -p gunda-desktop --locked
```

For development setup and contribution guidelines, see [CONTRIBUTING.md](CONTRIBUTING.md).

## Tests

```console
cargo test --workspace --all-features --locked
```

## Feedback

Found a bug? [Open an issue](https://github.com/AghastyGD/gunda/issues). Include your operating system, what happened, and any error message. For download failures, include the link if it's public, without private tokens or credentials.

For security reports, see [SECURITY.md](SECURITY.md).

## License

Gunda is licensed under the [GNU General Public License v3.0 only](LICENSE).
