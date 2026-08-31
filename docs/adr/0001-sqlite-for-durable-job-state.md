# Use SQLite for Durable Job State

Status: Accepted

## Context

Downloads must survive process and system restarts. Job creation, lifecycle
changes, failure details, and progress checkpoints need transactional updates and
schema evolution. The store is local to a Gunda installation and does not need a
separate database service.

## Decision

Gunda will use SQLite as its durable job store from the first functional
implementation. A job is committed before it becomes eligible for execution.

The core defines the persistence interface and durable domain requirements.
`gunda-storage` owns SQLite, SQL, schema mapping, and ordered versioned
migrations. Database-specific types do not enter the core.

The abstraction exists to protect the domain boundary, not to promise support
for multiple database engines.

## Alternatives considered

- An in-memory queue cannot provide restart recovery.
- Flat JSON files would require Gunda to design locking, atomic multi-field
  updates, indexing, migrations, and recovery semantics.
- A client-server database would add deployment and administrative requirements
  without serving the local application model.

## Consequences

- Startup must apply migrations before jobs can run.
- Schema and migration behavior require tests against temporary databases.
- Stable textual values represent domain enums in the database.
- Protocol-specific state uses protocol-specific tables when introduced.
- The database can transact job metadata but cannot atomically commit filesystem
  output. Recovery must reconcile both stores.
- SQLite progress is a checkpoint, not evidence that bytes are durably present.
- Sensitive browser credentials cannot be persisted until a separate security
  design defines their protection and lifecycle.

Recovery behavior is specified in
[Persistence and recovery](../design/persistence-and-recovery.md).
