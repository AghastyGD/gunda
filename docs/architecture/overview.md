# Architecture Overview

Status: Accepted target constraints, not an implementation inventory

## Current state

The repository contains the initial Rust workspace with:

- `gunda-core`, providing the download domain model, lifecycle rules,
  application commands and events, and the persistence boundary;
- `gunda-storage`, providing the initial SQLite schema, migrations, and support
  for creating and reloading queued download jobs.

Application orchestration, protocol engines, executable clients, lifecycle
persistence beyond initial job creation, and startup recovery are not yet
implemented.

This document defines boundaries that subsequent implementation must preserve.
A component described as planned does not exist merely because it appears here.

## Component ownership

The first useful implementation is expected to introduce these responsibilities
only as they become necessary:

| Component | Responsibility | Status |
| --- | --- | --- |
| `gunda-core` | Download domain, lifecycle rules, application commands and events, and interfaces for engines and persistence | Foundation implemented |
| `gunda-storage` | SQLite schema, migrations, and implementations of core persistence interfaces | Initial queued-job persistence implemented |
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

Dependency direction is therefore:

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

Clients submit intent through an application API. Creation succeeds only after
the job is stored. The download manager can then inspect the request, select an
engine, and schedule execution.

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

Engines own protocol behavior. They inspect and execute transfers, report
progress and outcomes, and maintain protocol-specific resume information. They
do not change persistent jobs directly. The download manager validates state
transitions, writes durable state, and publishes application events.

The exact Rust engine trait and registration mechanism remain open until direct
HTTP and HLS requirements provide enough evidence for a stable interface.

## Durable and runtime state

The `DownloadJob` is the primary durable aggregate. It records request and
destination intent, origin, resolved resource information, lifecycle state,
durable progress checkpoints, failures, and timestamps. SQLite is the durable
store, but it is not proof that bytes exist on disk. Recovery reconciles database
checkpoints with protocol metadata and partial output.

Transfer speed, ETA, worker handles, open files, in-flight requests, and emitted
events are runtime state. Events are notifications, not an event-sourced durable
model.

See [Download lifecycle](../design/download-lifecycle.md) and
[Persistence and recovery](../design/persistence-and-recovery.md).

## Protocol boundaries

Direct HTTP files, HLS, and future DASH resources are separate engines behind an
application-facing boundary. Low-level HTTP behavior such as request headers,
redirects, byte ranges, and streamed bodies belongs in a reusable transport.
Scheduling a direct file and scheduling HLS segments are separate algorithms.

The initial implementation should establish correct single-stream HTTP downloads
before adding range acceleration. HLS work begins after the engine boundary has
been exercised by direct HTTP. DASH does not receive a design until those
interfaces have been tested by HLS.

## Client and process boundaries

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

These are architectural constraints, not claims that the current placeholder
binary implements security controls.

## Decisions

- [ADR 0001: Use SQLite for durable job state](../adr/0001-sqlite-for-durable-job-state.md)
- [ADR 0002: Keep the core independent of client frameworks](../adr/0002-framework-independent-core.md)
- [ADR 0003: Implement native protocol engines over shared HTTP transport](../adr/0003-native-protocol-engines.md)
