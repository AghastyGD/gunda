# Download Lifecycle

Status: Accepted design, not implemented

## Purpose

A download is a persistent job, not a URL or a running task. This design defines
the lifecycle that clients, the application layer, engines, and storage share.
It does not freeze the public Rust types before implementation begins.

## Aggregate

Creation starts with request context, destination intent, and origin. Request
context contains a URL and headers without exposing HTTP-client-specific types.
Header values carry sensitivity metadata so that credentials can be excluded
from logs and presentation events.

Persistence assigns a local `DownloadId` and returns a `DownloadJob`. A job may
initially have an unknown resource kind or filename. Inspection can resolve the
resource as a direct file, HLS stream, or later protocol without adding
protocol-specific models to the generic aggregate.

Durable progress contains downloaded bytes and an optional total. Percentage is
derived. Transfer speed and ETA are runtime estimates.

## States

The lifecycle states are:

- `Queued`: persisted and eligible to wait for execution.
- `Inspecting`: resolving metadata and selecting an engine.
- `Downloading`: transfer work is active.
- `Paused`: intentionally suspended.
- `Finalizing`: transfer is complete but output is not yet committed.
- `Completed`: validated output has been committed to its final path.
- `Failed`: execution needs retry or user action.
- `Cancelled`: intentionally stopped and terminal for execution.
- `Interrupted`: active work lost its owning process.

Allowed transitions are:

```text
Queued       -> Inspecting | Paused | Cancelled
Inspecting   -> Downloading | Paused | Failed | Interrupted | Cancelled
Downloading  -> Paused | Finalizing | Failed | Interrupted | Cancelled
Paused       -> Queued | Cancelled
Finalizing   -> Completed | Failed | Interrupted
Failed       -> Queued | Cancelled
Interrupted  -> Queued | Cancelled
```

`Completed` and `Cancelled` are terminal execution states. Removing a job from
history is a separate application operation, not another state.

## Ownership and ordering

The download manager is the only component that authorizes lifecycle changes.
An engine reports inspection results, progress, completion, or failure. It does
not update a `DownloadJob` or call storage directly.

For a state change, the manager validates the transition, commits it through the
persistence interface, then publishes the corresponding runtime event. A failed
persistence write must not be presented as a successful transition.

A newly created job is persisted in `Queued` before the scheduler can see it.
This ordering makes restart recovery part of normal behavior rather than a
special path for selected downloads.

## Commands and events

The initial command vocabulary is expected to cover create, pause, resume,
cancel, retry, and remove. Removal separately decides whether partial data is
deleted. Presentation clients communicate intent through these commands and do
not mutate job fields.

Runtime events report creation, state changes, progress, completion, failure,
and removal. They let an in-process desktop client render current state. Events
are not the source of durable truth and do not require event sourcing.

## Invariants

- Every persisted job has a valid local ID.
- A job is persisted before it can execute.
- Every lifecycle change follows an allowed transition.
- Only the application layer changes generic job state.
- Downloaded bytes never exceed a known total during normal execution.
- A job is not completed before output finalization succeeds.
- Protocol-specific resume and manifest data does not enter the generic model
  without a demonstrated cross-protocol use.
- Sensitive request data does not enter logs or ordinary events in plaintext.
- Recovery may replace a persisted progress checkpoint with verified filesystem
  and protocol state.

## Open implementation questions

The concrete Rust aggregate layout, engine trait, engine registration strategy,
progress checkpoint interval, and event delivery mechanism remain open. These
choices should be made alongside the first implementation and kept close to the
relevant code unless they introduce a repository-wide constraint.
