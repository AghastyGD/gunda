
# Contributing to Gunda

Thanks for taking the time to contribute.

Gunda is still under active development, so some parts of the project are changing quickly. If you're unsure about a larger change, opening an issue first is usually a good idea.

Small fixes, tests, documentation improvements and focused bug fixes don't need an architecture proposal.

For bigger changes such as a new protocol, changes to persisted state, new crates, or changes to existing architectural decisions, please open an issue before starting.

Security issues should be reported through [SECURITY.md](SECURITY.md), not in a public issue.

## Development

You'll need Rust and the dependencies required by the part of Gunda you're working on.

The desktop uses egui/eframe and requires OpenGL and your platform's window-system libraries. On Linux, folder selection needs a file-picker portal backend or Zenity.

Before opening a pull request, run:

```console
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
cargo build --workspace --all-features --locked
```

For desktop changes, also launch the native app with an isolated data folder and check the affected interactions:

```console
cargo run -p gunda-desktop --locked -- --data-dir /path/to/test-data
```

## Tests

New behavior should normally come with tests.

Please keep tests deterministic and avoid depending on public websites or services. Network tests should use local servers or fixtures.

Changes involving persistence should test what happens after reopening the repository where that matters, rather than only testing the in-memory state.

Gunda supports native filesystem paths, so avoid assuming paths are always valid UTF-8.

## Rust style

Keep new code consistent with the module around it. A change should look like it belongs in the file, not like it was appended in a different style.

- Put imports at the top, grouped as standard library, external crates, workspace crates, and local modules.
- Use imported names in impl blocks and bodies: `impl fmt::Display`, `impl Error`, `io::ErrorKind`, and `fs::read` rather than fully qualified `std::...` paths when the module is already imported.
- Avoid `unwrap` and `expect` in production code. Prefer `?`, `let-else`, or explicit branches. Tests may use them when the fixture is expected to be valid.
- Prefer early returns over deep nesting, and keep helpers close to their only caller.
- Mark pure constructors and getters `#[must_use]`; make trivial ones `const fn`.
- Keep comments focused on why something is done, invariants, ordering, or security boundaries. Do not restate what a name or operation already says.
- Name tests as behavior statements and keep them deterministic with local fixtures.

## A few project rules

The Rust core should stay independent of egui and presentation code.

Protocol-specific behavior belongs in the relevant protocol implementation, and database-specific types should stay outside the core.

Treat URLs, headers, remote filenames, manifests and other network input as untrusted.

Do not log credentials, cookies, authorization headers, sensitive request data or other secrets.

If a change affects an architectural contract, update the relevant document under `docs/`. If it changes an architectural decision itself, an ADR may be appropriate.

And please don't add future functionality to the README until it actually works.

## Pull requests

Try to keep pull requests focused on one problem.

Explain what changed and why, include tests where appropriate, and mention anything that still needs follow-up.

That's it. 🙂
