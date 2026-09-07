# Contributing to Gunda

Gunda is in early development. Contributions should keep the implemented surface focused while preserving the accepted boundaries documented under
`docs/`.

## Before starting

Use a GitHub issue to coordinate changes that add a crate, introduce a protocol, change durable state, alter a security boundary, or revise an accepted architecture decision. Small fixes do not require an architecture proposal.

Report suspected vulnerabilities according to [SECURITY.md](SECURITY.md), not in a public issue.

## Repository layout

- `crates/gunda-core/`: download domain, lifecycle rules, application commands and events repository contract, and download manager.
- `crates/gunda-storage/`: SQLx SQLite adapter, migrations, native path encoding,
  and persistence integration tests.
- `docs/`: architecture, focused design documents, and architectural decisions.
- `.github/workflows/`: continuous integration.

There is no executable application or protocol engine yet. The
[architecture overview](docs/architecture/overview.md) describes the accepted
boundaries for those components.

Do not create empty crates or placeholder applications for future roadmap items.

## Local checks

Run these commands from the workspace root before opening a pull request:

```console
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo build --workspace --all-features
```

To apply formatting locally:

```console
cargo fmt --all
```

CI runs formatting and Clippy on Linux, and builds and tests on both Linux and
Windows. Changes involving native paths or conditional compilation must account
for both platforms; a successful local Linux run does not validate Windows-only
code.

## Testing expectations

Tests must be deterministic and independent of public services.

- Use controlled local servers or fixtures for network behavior.
- Use temporary SQLite databases and directories for storage tests.
- Cover persistence across repository reopen, not only in-memory behavior.
- Check that failed persistence operations do not produce successful
  application outcomes.
- Preserve native path representations without assuming UTF-8.
- When recovery is introduced, test deliberate disagreement between database
  checkpoints and partial files.

Tests should exercise observable behavior and invariants rather than depend on
incidental implementation details.

## Structured diagnostics

Libraries emit tracing spans and events but must not install a global
subscriber. Subscriber configuration belongs to an executable composition root.

Instrumentation uses an explicit allowlist of safe fields:

- Prefer `#[tracing::instrument(skip_all, ...)]` and record only reviewed fields.
- Useful fields include download IDs, lifecycle states, operation names, counts,
  and safe error categories.
- Do not record request URLs, header values, destination paths, browser context,
  SQL statements, query parameters, or complete error values.
- Public header classification permits persistence; it does not automatically
  make a header value appropriate for logs.
- Keep SQLx statement logging disabled when configuring storage connections.
- Record successful durable operations only after their transaction commits.

For asynchronous work, use async-aware instrumentation such as
`#[tracing::instrument]` or `Instrument`. Do not hold a `Span::enter()` guard
across an `.await`.

Diagnostic tests should attach a subscriber locally to the tested future,
assert that useful safe context is present, and verify that sensitive marker
values are absent. They must not install a process-wide subscriber.

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