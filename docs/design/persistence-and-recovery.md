# Persistence and Recovery

Status: Accepted design, partially implemented

## Responsibilities

SQLite is the durable job store. The core defines the persistence operations
required by the application, while `gunda-storage` owns SQL, database-library
types, connections, migrations, and schema mapping.

The abstraction isolates application policy from storage mechanics. It is not a
commitment to support arbitrary database engines.

## Current implementation

The SQLite repository supports transactional creation, lookup by ID, and listing
of initial queued download jobs.

Creation commits the job and its public request headers before returning.
Reloading preserves header order, native destination paths, and timestamps
normalized to millisecond precision.

Lookup and listing read job rows and their associated headers within a single
transaction, producing a consistent database snapshot.

The manager uses listing to populate its runtime registry before startup
succeeds. This is startup loading, not interrupted-download recovery.

The adapter currently accepts only initial queued-job records. Unsupported
persisted lifecycle states are rejected rather than silently reset to `Queued`.

Persistent lifecycle updates, resource inspection metadata, resolved
destinations, failures, mutable progress checkpoints, and interrupted-download
recovery are not implemented yet. They will be introduced with the repository
operations that maintain their invariants.

Destination paths use native platform encodings. Supporting Linux and Windows
does not guarantee that a database containing native paths can be transferred
between operating systems.

The execution, finalization, and recovery sections below describe required
future behavior, not existing transfer capabilities.

## Data model

The generic schema is introduced incrementally with the behavior that maintains
it. The initial migration stores creation-time identity, request metadata,
origin, destination intent, initial lifecycle state, progress defaults, and
timestamps.

Resolved paths, resource information, failures, and mutable lifecycle
checkpoints are added with the repository operations responsible for persisting
and restoring them.

Enums use explicit stable text values. Database constraints should reject
unknown states and other impossible values where practical. Protocol-specific
resume data belongs in protocol-specific tables introduced with the relevant
engine. HLS segment state, for example, does not belong in a generic downloads
row.

The current adapter persists only headers explicitly classified as public.
It rejects requests containing explicitly sensitive headers and rejects
browser-originated jobs before writing them.

Browser-derived cookies, authorization values, and tokens remain runtime-only
until storage, access, expiry, and deletion behavior have an accepted security
design. Authenticated restart recovery is not currently supported.

Header classification is not general-purpose secret detection. Source URLs are
currently persisted as supplied and may themselves contain credentials or
tokens. Restricting diagnostic output does not encrypt or sanitize stored data;
URL credential handling requires a separate policy before claiming safe
persistence of arbitrary authenticated requests.

## Migrations

Schema changes use ordered, versioned migration files from the first schema.
Startup applies migrations before loading or scheduling jobs. Schema creation
must not be scattered through application code with opportunistic
`CREATE TABLE IF NOT EXISTS` statements.

Migration tests use temporary databases and cover both a new database and
upgrades from every supported schema version.

## Partial output and finalization

Incomplete data is written to an engine-owned partial file or staging directory,
never directly over an existing final destination. The resolved final path and
the staging layout are different concerns.

Remote names are untrusted. Filename selection sanitizes them, validates the
result against the chosen directory, and applies an explicit conflict policy.
Overwrite must be selected explicitly. The default choice between renaming and
failing remains open.

After transfer and protocol validation, finalization commits output with an
atomic filesystem operation when the platform and filesystem allow it. The
manager persists `Completed` only after that commit succeeds, then emits the
completion event.

The filesystem operation and SQLite transaction cannot be one atomic
transaction. Recovery must therefore handle a process exit after the final file
was committed but before `Completed` was persisted. It may mark the job complete
only after validating the output expected by that engine.

## Startup recovery

Startup migration and recovery finish before normal scheduling begins.
`Inspecting`, `Downloading`, and `Finalizing` records cannot remain active because
their owning process no longer exists. Recovery moves them to `Interrupted` and
examines the associated protocol metadata and filesystem state.

A SQLite byte count is a checkpoint, not proof that bytes are present and valid.
Recovery does not blindly resume from the larger of a database count and a file
length. The selected engine validates the partial data and any remote validator,
such as an ETag or modification timestamp, before establishing a safe resume
point. A resumable job returns to `Queued` through the normal state machine.

When safe reconciliation is impossible, the job remains interrupted or becomes
failed with an actionable error. Recovery must prefer repeating work over
silently producing a corrupt final file.

## Failure behavior

Storage errors are translated to application errors before reaching callers.
During creation, a failed database operation does not add a job to the manager
or return a successful creation event.

As persistent lifecycle updates are introduced, state and progress changes must
not be published as committed when their database write failed. Error
classification must distinguish disk-full, permission, integrity, and storage
failures sufficiently for retry policy and user action.

Removing a job record and deleting partial or final output are separate choices.
Deletion must operate on resolved, validated paths owned by that job.

## Test obligations

As the corresponding behavior is introduced, persistence and recovery tests
must cover:

- creation before scheduling and reload after reopening the database;
- valid and invalid enum values and state changes;
- migration ordering and constraint enforcement;
- interrupted active states at startup;
- database checkpoints ahead of and behind partial-file state;
- an output commit followed by a simulated crash before completion is stored;
- file conflicts, unsafe remote names, permissions, and disk errors where the
  platform permits deterministic simulation.

Tests use temporary SQLite databases and filesystems. They do not depend on a
live public service.

## Open implementation questions

Checkpoint frequency, flush and sync policy, staging names, cleanup retention,
transaction boundaries for batched progress, and protocol-specific resume
schemas remain implementation questions. Each must be resolved before relying
on the corresponding recovery guarantee.
