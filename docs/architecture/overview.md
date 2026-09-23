# Architecture

Gunda is split into a small set of Rust crates and client applications.

The core owns download state and lifecycle rules. Storage, protocol implementations, and presentation code live outside the core and connect through explicit application boundaries.

The goal is to keep the download engine independent from any particular UI or client.

## Components

### `gunda-core`

Owns the download domain and application-level behavior.

This includes:

* download jobs and lifecycle states
* commands and events
* progress and failure models
* repository interfaces
* download manager orchestration
* execution and cancellation contracts

The core does not depend on Tauri, Svelte, SQLx, SQLite, or protocol-specific implementations.

### `gunda-storage`

Provides durable storage using SQLite.

It implements the repository contracts defined by `gunda-core` and is responsible for:

* migrations
* serialization and validation of persisted state
* loading downloads at startup
* persisting lifecycle changes, progress, failures, and resolved metadata

SQLite stores application state, but it is not the authority for bytes already written to disk.

### `gunda-http`

Implements the current direct HTTP download path.

It owns HTTP-specific behavior such as:

* request execution
* response inspection
* streamed response bodies
* HTTP validation
* partial file writing
* transfer progress
* final file publication

HTTP errors are converted into domain-level failures before they reach the rest of the application.

Future protocol engines may reuse the shared HTTP transport without depending on the direct-file transfer algorithm itself.

### Desktop

The Tauri desktop application is currently the composition root.

It connects:

* `gunda-core`
* `gunda-storage`
* `gunda-http`
* the Svelte desktop interface

Tauri commands and events form the boundary between the Rust application layer and the frontend.

The frontend owns presentation and local UI state. Download lifecycle decisions remain in Rust.

## Dependency direction

The main dependency direction is:

```text
desktop
   |
   +----> gunda-core
   |
   +----> gunda-storage ----> gunda-core
   |
   `----> gunda-http -------> gunda-core
```

Infrastructure depends on the core contracts, not the other way around.

The core should remain usable without knowing whether the caller is the desktop, a CLI, a browser integration, or another client.

## Download lifecycle

`DownloadJob` is the main download aggregate.

It contains the durable information needed to describe a download, including its request, origin, destination, resolved resource information, state, progress, failures, and timestamps.

The download manager owns application-level lifecycle changes.

Protocol engines perform transfer-specific work, but they do not directly mutate persisted download jobs.

A simplified execution path looks like this:

```text
client
  |
  v
download manager
  |
  +----> persistence
  |
  v
download executor
  |
  +----> network
  |
  `----> filesystem
```

The manager coordinates state transitions around execution and persists the resulting state.

See [Download lifecycle](../design/download-lifecycle.md).

## Persistence and filesystem state

SQLite stores Gunda's durable application state.

Downloaded bytes live on the filesystem.

These two sources can disagree after interruption or failure, so persisted progress should not be treated as proof that the same number of bytes can safely be resumed from disk.

Recovery and resume must reconcile both sides before continuing a transfer.

Partial downloads are written separately from final files and are only published as completed files after successful finalization.

See [Persistence and recovery](../design/persistence-and-recovery.md).

## Clients

The desktop is the first client of Gunda, but it is not intended to be the only way downloads enter the application.

A download may eventually originate from:

```text
Desktop ───────────┐
Browser extension ─┼──> Gunda
CLI ───────────────┤
Other clients ─────┘
```

The desktop should therefore primarily act as a place to view, inspect, and manage downloads known by Gunda rather than assuming every download starts with a pasted URL.

Browser extensions are expected to discover resources during normal browser navigation and pass useful context to Gunda.

The native application remains responsible for persistence, protocol handling, transfer execution, and filesystem access.

## Protocols

Direct HTTP is the first implemented download path.

HLS and DASH are separate protocol concerns and should remain outside the core domain.

A streaming engine may reuse shared networking infrastructure, but playlist parsing, segment scheduling, stream selection, and protocol-specific recovery belong to that engine.

This keeps the core focused on application-level download behavior instead of protocol details.

## Process model

Downloads currently execute inside the desktop process.

A future daemon may take ownership of active jobs so transfers can continue independently of the desktop window.

The current architecture keeps this possible by separating clients from the core application and execution boundaries rather than coupling download behavior directly to the UI.

The daemon protocol itself does not need to be defined before that work begins.

## Security boundaries

Network input, remote filenames, request context, destination paths, browser-provided data, and future local IPC messages should be treated as untrusted.

Important constraints include:

* sensitive headers and credentials must not appear in logs or ordinary UI events
* sensitive browser request context must not be persisted without an appropriate storage design
* remote filenames must not escape the selected destination
* partial output must not unexpectedly replace completed files
* existing files must follow an explicit conflict policy
* remote input must never be interpolated into shell commands
* future browser and local IPC interfaces should expose only what they need

Supporting authenticated requests or encrypted media transport does not imply support for DRM circumvention.

See [SECURITY.md](../../SECURITY.md).

## Architectural decisions

Major decisions whose rationale should survive beyond the code are recorded in [`docs/adr/`](../adr/).

Current ADRs cover:

* SQLite for durable download state
* keeping the core independent of client frameworks
* native protocol engines over shared HTTP transport
