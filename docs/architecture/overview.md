# Architecture Overview

Status: Accepted architecture with current implementation notes

## Current state

The Rust workspace contains:

- `gunda-core`, providing the download domain, lifecycle rules, application
  command and event types, the repository contract, and a download manager;
- `gunda-storage`, providing SQLite migrations and transactional creation,
  lookup, and listing of initial queued jobs through SQLx.

The manager loads persisted jobs before startup succeeds, exposes read-only
snapshots, and persists newly created jobs before inserting them into its
runtime registry.

Manager and storage operations emit structured tracing with explicitly selected
safe fields. Libraries do not configure a global subscriber.

Protocol engines, executable clients, persistent lifecycle updates, scheduling,
and interrupted-download recovery are not implemented yet.

The remaining sections describe accepted responsibilities and boundaries.
Planned behavior must not be interpreted as an existing capability.

## Component ownership

Components have the following responsibilities and implementation status:

| Component | Responsibility | Status |
| --- | --- | --- |
| `gunda-core` | Download domain, lifecycle rules, command and event types, repository contract, and application orchestration | Initial domain and manager implemented; execution orchestration remains planned |
| `gunda-storage` | SQLite schema, migrations, and implementation of the core repository contract | Initial queued-job creation, lookup, and listing implemented |
| `gunda-http` | Shared HTTP transport and the direct HTTP file engine | Planned |
| Desktop application | Tauri composition root and Svelte presentation client | Planned |

The first implementation may run the application layer inside the desktop
process. This is a deployment choice, not permission to couple the core to
Tauri.

HLS, browser integration, a daemon, a CLI, DASH, and Chromium support are future
work. Their directories and crates should be created only when their
responsibilities are being implemented.

## Dependency rules

The core owns domain and application policy. It must not depend on Tauri,
Svelte, browser APIs, SQL, a database library, or protocol-specific manifest
models.

Storage and protocol crates implement interfaces required by the core. A
composition root selects concrete adapters and supplies them to the application
layer. Presentation clients send commands and observe snapshots or events. They
do not receive mutable access to downloader internals.

The accepted dependency direction, including planned components, is:

```text
desktop composition root
    |---> gunda-core
    |---> gunda-storage ---> gunda-core interfaces
    `---> gunda-http -----> gunda-core interfaces
```

A future HLS engine may reuse the HTTP transport. It must not reuse the direct
file engine's download algorithm or put playlist and segment types into the
core.

## Download flow

The implemented creation flow is:

1. The caller supplies a new download to the manager.
2. The manager requests creation through the repository contract.
3. The SQLite adapter commits the job and its public request headers.
4. The manager adds the persisted job to its runtime registry.
5. The manager returns a `Created` event to the caller.

A failed repository operation does not add a job to the manager. Returning an
event does not imply an event bus or subscription mechanism exists.

At startup, the manager loads the repository snapshot before returning a usable
instance. Its current registry is ordered by download ID; this is not a
scheduling policy.

The planned execution flow extends these foundations:

```text
client command
     |
     v
download manager ---> persistence interface ---> SQLite adapter
     |
     v
engine interface ---> HTTP or future streaming engine
     |
     v
filesystem and network adapters
```

Engines will own protocol behavior: inspection, transfer execution, progress,
outcomes, and protocol-specific resume information. They must not change
persistent jobs directly.

The manager will validate lifecycle transitions, persist their results, and
publish application events. These execution paths are not implemented yet.

The exact Rust engine trait and registration mechanism remain open until direct
HTTP and HLS requirements provide enough evidence for a stable interface.

## Durable and runtime state

`DownloadJob` is the primary aggregate. Its domain model includes request and
destination intent, origin, optional resolved resource and destination
information, lifecycle state, progress checkpoints, failures, and timestamps.

The current SQLite adapter persists only the initial queued-job representation.
Persisting subsequent lifecycle changes, inspection results, resolved
destinations, failures, and progress checkpoints requires additional repository
operations and schema changes.

SQLite is the durable store, but a stored checkpoint is not proof that bytes
exist on disk. Future recovery must reconcile database state with protocol
metadata and partial output.

Transfer speed, ETA, worker handles, open files, in-flight requests, and emitted
events belong to runtime state as execution is introduced. Events are
notifications, not an event-sourced durable model.

See [Download lifecycle](../design/download-lifecycle.md) and
[Persistence and recovery](../design/persistence-and-recovery.md).

## Planned protocol boundaries

Direct HTTP files, HLS, and future DASH resources are separate engines behind an
application-facing boundary. Low-level HTTP behavior such as request headers,
redirects, byte ranges, and streamed bodies belongs in a reusable transport.
Scheduling a direct file and scheduling HLS segments are separate algorithms.

The initial implementation should establish correct single-stream HTTP downloads
before adding range acceleration. HLS work begins after the engine boundary has
been exercised by direct HTTP. DASH does not receive a design until those
interfaces have been tested by HLS.

## Planned client and process boundaries

The desktop application is a client of the application layer. A future browser
extension is a sensor and browser-facing UI: it may observe candidate requests
and provide request context, but native code remains authoritative for protocol
parsing, persistence, scheduling, transfer, and files.

A future native messaging host is a narrow bridge, not another download manager.
A daemon may later own active jobs so that transfers outlive a desktop window.
The current design preserves that option through commands, events, and adapter
interfaces. It does not define the daemon IPC protocol in advance.

## Security boundaries

Network responses, redirects, remote filenames, manifests, browser-supplied
headers, destination paths, native messages, and future local IPC requests cross
trust boundaries.

Implementation must preserve these constraints:

- Sensitive headers such as `Cookie` and `Authorization` are classified and are
  never written to logs, errors, telemetry, or ordinary UI events in plaintext.
- Browser credentials are not persisted until a separate credential-storage
  design is accepted. The persistence mechanism is currently unresolved.
- Remote filenames are sanitized and cannot escape the selected destination.
- Existing destination files are not overwritten without an explicit conflict
  policy. Incomplete output does not replace a final file.
- Manifest parsing and scheduling apply explicit resource limits once streaming
  protocol work begins.
- Native messaging and local IPC expose only the minimum required local
  interface. Authentication and authorization details remain open until those
  components are designed.
- If an external program is introduced for finalization, arguments are passed
  directly. Remote input is never interpolated into a shell command.
- Supporting authenticated requests and standard encrypted streams does not
  include circumventing DRM systems.

These constraints apply as each component is implemented. Current protections
include rejection of browser-originated jobs and explicitly sensitive headers
by the SQLite adapter, plus restricted diagnostic fields. Network, filesystem
finalization, and client-boundary protections remain requirements for future
components.

## Decisions

- [ADR 0001: Use SQLite for durable job state](../adr/0001-sqlite-for-durable-job-state.md)
- [ADR 0002: Keep the core independent of client frameworks](../adr/0002-framework-independent-core.md)
- [ADR 0003: Implement native protocol engines over shared HTTP transport](../adr/0003-native-protocol-engines.md)
