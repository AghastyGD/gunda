# Gunda Daemon

Status: implemented foundation, desktop migration pending

## Ownership

`gundad` owns the runtime lease, SQLite repository, `DownloadManager`, HTTP and
stream executors, and the bounded job scheduler. It is the only process that
executes new transfers when a client is connected to it.

The current desktop still runs its own in-process runtime. It has not been
migrated to the daemon client yet, so the two runtimes must not share a data
directory. The next milestone is to replace the desktop worker with
`gunda-ipc` client calls.

## Transport

The first implementation uses a loopback TCP listener and newline-delimited
JSON. The endpoint is stored in `gundad.endpoint`; the shared token is stored in
`gundad-token` with owner-only permissions where the platform supports them.
Clients must send `hello` with the token before any command.

The transport is local and authenticated, but loopback TCP is reachable by local
processes. A later transport pass should use Unix domain sockets on Linux and
named pipes on Windows, keeping the same protocol and command semantics.

## Protocol

`gunda-ipc` defines versioned commands:

- create
- pause, resume, retry, restart, and cancel
- save a published status

The daemon replies with complete snapshots after state changes. Each snapshot
contains job state, progress, failure classification, active IDs, pause
capability, and pending publication status. A reconnecting client receives the
current snapshot immediately.

## Scheduling

The daemon uses the same bounded scheduler semantics as the desktop:

- up to three jobs execute concurrently;
- additional jobs remain queued;
- pause and cancel are per job;
- queued jobs are started only after a command or a slot becomes free.

## Security and limits

The daemon binds only `127.0.0.1`, validates the token for every connection, and
never accepts a non-HTTP download URL. Paths cross IPC as platform-native
bytes. The endpoint file contains no secret; the token is not printed by
`gundad`.

The first transport does not yet provide permission isolation equivalent to a
Unix socket or named pipe. Windows compilation and the desktop client
migration remain to be completed.
