
# Contributing to Gunda

Thanks for taking the time to contribute.

Gunda is still under active development, so some parts of the project are changing quickly. If you're unsure about a larger change, opening an issue first is usually a good idea.

Small fixes, tests, documentation improvements and focused bug fixes don't need an architecture proposal.

For bigger changes such as a new protocol, changes to persisted state, new crates, or changes to existing architectural decisions, please open an issue before starting.

Security issues should be reported through [SECURITY.md](SECURITY.md), not in a public issue.

## Development

You'll need Rust and the dependencies required by the part of Gunda you're working on.

For desktop development, you'll also need Node.js, pnpm and the [Tauri 2 prerequisites](https://v2.tauri.app/start/prerequisites/) for your platform.

Before opening a pull request, run:

```console
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo build --workspace --all-features
```

For desktop changes, also run from `apps/desktop`:

```console
pnpm check
pnpm build
```

## Tests

New behavior should normally come with tests.

Please keep tests deterministic and avoid depending on public websites or services. Network tests should use local servers or fixtures.

Changes involving persistence should test what happens after reopening the repository where that matters, rather than only testing the in-memory state.

Gunda supports native filesystem paths, so avoid assuming paths are always valid UTF-8.

## A few project rules

The Rust core should stay independent of Tauri and presentation code.

Protocol-specific behavior belongs in the relevant protocol implementation, and database-specific types should stay outside the core.

Treat URLs, headers, remote filenames, manifests and other network input as untrusted.

Do not log credentials, cookies, authorization headers, sensitive request data or other secrets.

If a change affects an architectural contract, update the relevant document under `docs/`. If it changes an architectural decision itself, an ADR may be appropriate.

And please don't add future functionality to the README until it actually works.

## Pull requests

Try to keep pull requests focused on one problem.

Explain what changed and why, include tests where appropriate, and mention anything that still needs follow-up.

That's it. 🙂