# Gunda Daemon

Status: implemented; the desktop is a daemon client

## Ownership

`gundad` owns the runtime lease, SQLite repository, `DownloadManager`, HTTP and
stream executors, browser capture listener, source extractors, candidate
catalog, and bounded job scheduler. It is the only production transfer owner,
and downloads and capture continue when desktop clients disconnect.

The desktop connects through `gunda-ipc`. It starts `gunda-desktop --daemon`
with the same data directory when no daemon is available, then reads snapshots
and sends lifecycle commands. The executable therefore acts as either the
desktop client or its background daemon.

The daemon validates browser source JSON, extracts supported presentations,
inspects streamed qualities, and creates jobs from captured candidates. Source
documents are not persisted. The desktop capture panel renders daemon snapshots
and sends inspection and download commands.

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
- inspect, download, dismiss, and clear captured resources

The daemon replies with complete snapshots after state changes. Each snapshot
contains job state, progress, failure classification, native destination and
output paths, active IDs, pause capability, pending publication status, capture
pairing values, candidates, and inspected tracks. Every connected client receives
the same state, and a reconnecting client receives the current snapshot
immediately.

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
Unix socket or named pipe. Windows verification remains to be completed.
