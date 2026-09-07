# Download Lifecycle

Status: Accepted design, partially implemented

## Purpose

A download is a persistent job, not a URL or a running task. This design defines
the lifecycle that clients, the application layer, engines, and storage share.

## Current implementation

The core implements the download aggregate, lifecycle states, transition
validation, progress invariants, and command and event types.

The manager creates persisted queued jobs, loads them at startup, and exposes
read-only access to its runtime registry. Creation returns a `Created` event
after persistence succeeds; no event delivery infrastructure is implemented.

Domain methods support in-memory lifecycle changes, but the storage adapter
currently persists only initial queued jobs. Persistent transitions, transfer
execution, pause, resume, cancellation, retry, removal, and interrupted-download
recovery are not implemented as manager operations.

The following sections define the lifecycle contract, including behavior that
will be implemented with transfer execution and recovery.

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

`DownloadCommand` defines create, pause, resume, cancel, retry, and remove
intent. Defining these variants does not mean every operation has an
application handler. The manager currently exposes creation directly rather
than through a general command dispatcher.

Removal separates removing a job from deciding whether its partial data should
also be deleted. Presentation clients must not mutate job fields directly.

`DownloadEvent` defines notifications for creation, state changes, progress,
completion, failure, and removal. Only creation is currently returned by the
manager; event publication and subscription remain unimplemented.

Events are not the source of durable truth and do not require event sourcing.
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

The engine trait, engine registration strategy, progress checkpoint interval,
and event delivery mechanism remain open.

These choices should be made alongside the corresponding implementation and
kept close to the relevant code unless they introduce a repository-wide
constraint.