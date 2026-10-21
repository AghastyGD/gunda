# Gunda Daemon

Status: implemented; the desktop is a daemon client

## Ownership

`gundad` owns the runtime lease, SQLite repository, `DownloadManager`, HTTP and
stream executors, and the bounded job scheduler. It is the only production
transfer owner, and jobs continue when desktop clients disconnect.

The desktop connects through `gunda-ipc`. It starts `gunda-desktop --daemon`
with the same data directory when no daemon is available, then reads snapshots
and sends lifecycle commands. The executable therefore acts as either the
desktop client or its background daemon.

Browser capture and stream inspection remain desktop-owned. Moving the capture
listener into `gundad` is a separate change.

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
contains job state, progress, failure classification, native destination and
output paths, active IDs, pause capability, and pending publication status. Every
connected client receives the same state, and a reconnecting client receives the
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
Unix socket or named pipe. Windows verification and daemon-owned capture remain
to be completed.
