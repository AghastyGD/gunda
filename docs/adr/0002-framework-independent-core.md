# Keep the Core Independent of Client Frameworks

Status: Accepted

## Context

The first user-facing application is expected to use Tauri and Svelte. Active
downloads may initially run in that desktop process. Later clients may include a
CLI, a browser native messaging host, and a background daemon.

## Decision

The Rust core owns the download domain and application orchestration without
depending on Tauri, Svelte, browser APIs, or presentation types. Clients express
intent through commands and observe state or runtime events. They do not mutate
jobs directly.

Concrete storage, protocol, filesystem, and presentation adapters are assembled
outside the core. The initial desktop application may host this assembly
in-process. A daemon is deferred, but the application boundary must remain usable
across a future process boundary.

## Alternatives considered

- Putting domain state and orchestration behind Tauri commands would make the
  desktop framework the application boundary and make later extraction costly.
- Starting with a daemon would require an IPC and service-lifecycle design before
  the download model has been proven.

## Consequences

- Tauri commands remain adapters around application operations.
- Core tests run without a desktop runtime.
- Application requests, results, and events need explicit ownership and error
  semantics.
- UI convenience types cannot become domain types.
- Moving orchestration into a daemon should primarily replace the in-process
  adapter with local IPC.
- The local IPC protocol, daemon lifecycle, and event transport remain undecided
  until daemon work begins.
