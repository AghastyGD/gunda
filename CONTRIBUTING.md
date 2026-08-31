# Contributing to Gunda

Gunda is in its bootstrap stage. Contributions should keep the implemented
surface small while establishing the accepted boundaries documented under
`docs/`.

## Before starting

Use a GitHub issue to coordinate changes that add a crate, introduce a protocol,
change durable state, alter a security boundary, or revise an accepted
architecture decision. Small fixes do not require an architecture proposal.

Report suspected vulnerabilities according to [SECURITY.md](SECURITY.md), not in
a public issue.

## Repository layout

The repository currently contains one Rust binary in `src/`. The proposed core,
storage, HTTP, and desktop components do not exist yet. The
[architecture overview](docs/architecture/overview.md) defines the boundaries
they must follow when introduced.

Do not create empty crates or placeholder applications for future roadmap items.

## Local checks

Run the checks that apply to a change before opening a pull request:

```console
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

Tests for network behavior must use a controlled local server or local fixtures.
They must not depend on public websites. Storage tests should use temporary
SQLite databases and recovery tests should cover deliberate disagreement between
database checkpoints and partial files.

## Change expectations

- Keep the Rust core independent of Tauri, browser APIs, and presentation code.
- Keep SQL and database-library types outside the core.
- Keep protocol-specific state inside the relevant engine.
- Persist a download job before making it eligible to run.
- Treat remote names, URLs, manifests, headers, and paths as untrusted input.
- Never log cookies, authorization values, or other sensitive request data.
- Add user-visible capabilities to the README only after they work.

Changes should include tests for new behavior and update the focused document
whose contract changed. Use an ADR when changing a decision whose rationale will
matter to future contributors. Implementation details that are local to one API
or module should remain close to the code.
