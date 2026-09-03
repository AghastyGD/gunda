# Use SQLx with an asynchronous repository contract

## Status

Accepted

## Context

Gunda uses SQLite as its initial persistent storage backend.

The application layer is expected to become asynchronous because direct HTTP downloads, HLS, DASH, retries, timers, and the future daemon will run within an async runtime.

A synchronous SQLite adapter such as `rusqlite` would require Gunda to explicitly isolate blocking database operations from async executor threads, for example through `spawn_blocking`, a dedicated storage thread, or an actor-like storage worker.

SQLx provides an asynchronous interface for SQLite and internally isolates SQLite work from the async executor. It also provides integrated migrations, connection pooling, and optional compile-time SQL validation.

The repository contract therefore needs to align with the execution model expected by the application layer without coupling the domain to a specific UI client or operating system.

## Decision

Gunda will use SQLx as the initial Rust persistence library for SQLite.

The download repository contract will be asynchronous.

Conceptually:

```rust
pub trait DownloadRepository {
    async fn create(
        &self,
        download: NewDownload,
        created_at: OffsetDateTime,
    ) -> Result<DownloadJob, RepositoryError>;

    async fn find_by_id(
        &self,
        id: DownloadId,
    ) -> Result<Option<DownloadJob>, RepositoryError>;

    async fn update(
        &self,
        job: &DownloadJob,
    ) -> Result<(), RepositoryError>;
}
```

The repository abstraction remains part of the core/application boundary, while SQLx-specific types remain inside the storage implementation.

The initial SQLite implementation will use:

* SQLx with SQLite support
* embedded migrations
* an async runtime compatible with the application layer
* a `SqlitePool`
* an initial maximum connection count of one

The pool size may be increased later only when measurements or concrete concurrency requirements justify it.

## Rationale

### Async application integration

HTTP and streaming operations will already be asynchronous.

Using an asynchronous repository avoids introducing a separate blocking-storage execution mechanism at the beginning of the project.

The expected flow becomes:

```text
DownloadManager
      |
      v
DownloadRepository
      |
      v
SQLx
      |
      v
SQLite
```

rather than:

```text
DownloadManager
      |
      v
blocking isolation layer
      |
      v
rusqlite
      |
      v
SQLite
```

### Migrations

SQLx provides integrated migration support.

Migrations can be embedded into the application and applied in a deterministic order during storage initialization.

Schema evolution remains migration-driven from the first database version.

### SQL validation

Where practical, Gunda will use SQLx query macros to detect schema and query mismatches before runtime.

Offline query metadata may be committed so CI and contributors do not require access to a pre-existing development database during normal builds.

Compile-time checked queries are preferred where they provide useful guarantees, but they are not an absolute requirement for every query.

### Pooling

`SqlitePool` provides a shareable storage handle suitable for a future concurrent application and daemon architecture.

SQLite still permits only one writer at a time, so the initial pool will remain deliberately small.

Pooling is not being adopted to increase write throughput.

## Architectural boundaries

SQLx must not leak into `gunda-core`.

The core repository API must not expose:

* `SqlitePool`
* SQLx rows
* SQLx error types
* SQL query representations
* migration types

Storage-specific failures must be translated into repository or application-level errors.

Similarly, the repository contract does not require the core to depend directly on Tokio.

The asynchronous contract describes the operation model, while the concrete storage implementation chooses the runtime integration required by SQLx.

## Consequences

### Positive

* repository calls integrate naturally with the async application layer;
* no dedicated storage actor or manual blocking thread is required initially;
* migrations are supported by the selected persistence library;
* SQL/schema mismatches can be detected earlier;
* the storage handle can be shared across future concurrent application tasks;
* the design fits the expected future daemon architecture.

### Negative

* SQLx adds more dependencies and compile-time cost than a direct SQLite wrapper;
* repository APIs become asynchronous;
* query metadata may need to be regenerated when migrations or checked queries change;
* the project must maintain async runtime integration;
* connection pooling introduces configuration that must be chosen deliberately;
* developers must understand that async SQLx does not make SQLite itself concurrent or faster.

## SQLite concurrency

Using SQLx does not change SQLite's fundamental concurrency model.

Multiple readers may operate concurrently, depending on journal mode and connection configuration, but SQLite still serializes writes.

The initial pool will therefore use one connection.

This favors predictable behavior during the foundation milestone and avoids introducing concurrency that has not demonstrated a measurable benefit.

## Alternatives considered

### `rusqlite` with synchronous repository methods

`rusqlite` provides a smaller and more direct SQLite API.

It would be a strong choice if Gunda intentionally used a synchronous repository and routed all persistence through a dedicated blocking execution strategy.

Advantages:

* smaller conceptual surface;
* lower dependency cost;
* direct SQLite API;
* explicit control over storage execution.

Disadvantages for Gunda:

* async callers must isolate blocking operations explicitly;
* a storage thread, actor, or blocking pool would need to be designed and maintained;
* migrations and pooling require additional implementation or libraries;
* the repository execution model would differ from the rest of the application.

This remains a valid architecture but is not the selected initial design.

### Synchronous repository with SQLx hidden behind an adapter

An application adapter could expose synchronous repository methods while internally executing SQLx futures.

This would obscure the actual execution model and add unnecessary bridging complexity.

It was rejected.

### Async repository with a remote-database-oriented abstraction

Gunda could design its persistence interface around general-purpose remote databases.

This was rejected because Gunda is a local application and SQLite is an intentional architectural choice, not a temporary substitute for PostgreSQL or another server database.

## Revisit conditions

This decision should be revisited if:

* SQLx becomes a disproportionate dependency or maintenance burden;
* SQLite access is intentionally centralized into a dedicated storage actor;
* measured workloads show that the selected async approach creates unnecessary complexity;
* the application's runtime model changes substantially;
* another SQLite adapter provides materially better guarantees for Gunda's needs.

Changing the Rust persistence library does not by itself invalidate the separate decision to use SQLite as Gunda's local database.
