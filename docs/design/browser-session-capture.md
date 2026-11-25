# Browser Session Capture

Status: Proposed design

## 1. Problem

Many media URLs are bound to the browser session that discovered them. The URL
may be signed, the origin may require cookies, and the CDN may reject requests
without the browser's `Referer` or `User-Agent`. Instagram and Facebook are the
clearest examples: their players often request a manifest or media URL with
session cookies and then use the same session for the media itself.

The current capture channel reports a URL, a content type, a page URL, and a
page title. It deliberately sends no cookies and no request headers. That keeps
credentials out of the daemon, but it also means a captured URL can fail with
`401` or `403` even though the browser can play it.

Site-specific extractors help, but they cannot keep up with every player change.
The more durable approach is the one used by IDM-style download tools: observe
the real media request, keep the small set of request headers that identify the
session, and replay those headers for the same resource.

## 2. Goals

- Capture the browser session headers that a media or manifest request actually
  used.
- Replay those headers for format inspection and download.
- Keep credentials out of SQLite, logs, error messages, diagnostics, and
  crash reports.
- Bound the lifetime and size of captured sessions.
- Preserve redirect and segment origin safety.
- Stay compatible with older extensions and older daemons.
- Keep the existing site extractors as a complement, not as the only path.

## 3. Non-goals

- Persisting cookies or authorization values across daemon restarts.
- Downloading `blob:` URLs or inline MSE data that the browser never exposes as
  a fetchable HTTP resource.
- Bypassing authentication, DRM, TLS protections, or site access controls.
- Capturing every header from every request.
- Firefox support. The extension is Chromium MV3.

## 4. Current behavior and gap

The extension already observes media responses with
`chrome.webRequest.onResponseStarted`. The page agent also forwards bounded JSON
from Instagram, Facebook, YouTube, and generic sites. The daemon validates the
report, stores a candidate, and later inspects or downloads it.

The missing piece is the request context:

- `Candidate` carries no request headers.
- `formats_for_candidate` builds `RequestContext::new(candidate.url(), Vec::new())`
  before inspecting an HLS or DASH manifest.
- `download_capture` builds `RequestContext::new(url, Vec::new())` before
  creating the download job.
- `gunda-core` already supports headers and sensitivity, but storage rejects
  sensitive headers and browser jobs never populate them.

The HTTP transport already has useful safety rules:

- `HttpClient` drops caller headers on a cross-origin redirect.
- Stream segments only inherit manifest headers when the segment has the same
  origin as the manifest.
- `RequestHeader` redacts sensitive values in `Debug`.

Those rules are the foundation for session replay.

## 5. Decision

Gunda will support runtime-only browser session capture.

1. The extension captures a whitelisted set of request headers for media
   requests, source JSON requests, and page document requests.
2. A session is attached to the candidate or source report that it belongs to.
3. The daemon stores the session in memory for the lifetime of the candidate.
4. The daemon applies the session headers when inspecting a manifest and when
   executing a download.
5. Session values are never written to SQLite, logs, error messages, or
   diagnostics.
6. A public, non-sensitive flag records that a download needs a browser session.
   After a daemon restart, the user is asked to start the download again from the
   browser.
7. The extension sends sessions only when the daemon advertises the
   `browser_session` capability.

This keeps the useful part of the IDM approach without turning Gunda into a
credential store.

## 6. Data model

### Protocol session

```text
SessionCapture {
    origin: String,
    headers: [
        SessionHeader {
            name: String,
            value: String
        }
    ]
}
```

`origin` is the serialized HTTP origin (`scheme://host[:port]`) of the request
whose headers were observed. It contains no path, query, user name, or password.
It tells the daemon where sensitive headers may be sent.

### Daemon session

```text
BrowserSession {
    origin: Origin,
    headers: Vec<SessionHeader>,
    captured_at: Instant,
    expires_at: Instant,
}
```

`BrowserSession` implements a custom `Debug` that prints only the origin, the
header names, and `[REDACTED]` for values. It does not implement `Serialize`.

### Header policy

The daemon classifies headers by name. It does not trust a sensitivity flag from
the extension.

| Header | Sensitivity | Scope | Reason |
| --- | --- | --- | --- |
| `Cookie` | Sensitive | Session origin only | Session credential |
| `Authorization` | Sensitive | Session origin only | Bearer or custom credential |
| `Referer` | Sensitive | Any origin | May contain signed query values |
| `Origin` | Public | Any origin | CORS metadata, not a credential |
| `User-Agent` | Public | Any origin | CDNs may reject unknown clients |
| `Accept` | Public | Any origin | Content negotiation |
| `Accept-Language` | Public | Any origin | Content negotiation |
| `Sec-Fetch-Dest` | Public | Any origin | Optional request metadata |
| `Sec-Fetch-Mode` | Public | Any origin | Optional request metadata |
| `Sec-Fetch-Site` | Public | Any origin | Optional request metadata |

All other request headers are ignored. In particular, the extension never sends
`Host`, `Content-Length`, `Transfer-Encoding`, `Connection`, `Range`,
`If-Range`, `If-Match`, `If-None-Match`, `Accept-Encoding`, or
`Proxy-Authorization`. The transport owns those headers.

`Referer` is treated as sensitive because it is a URL and can contain tokens.
Sensitivity controls diagnostic output. Scope controls where a header may be
sent.

### Limits

| Limit | Value | Purpose |
| --- | --- | --- |
| Headers per session | 16 | Bound memory and protocol size |
| Value length | 8 KiB | Accommodate cookies without unbounded data |
| Total session bytes | 16 KiB | Bound message growth |
| Session lifetime | 10 minutes | Match signed URL and cookie freshness |
| Extension pending sessions | 32 | Bound service worker memory |
| Daemon sessions | Bound by the candidate catalog | One session per candidate at most |

The daemon drops a session that exceeds a limit and still records the candidate.
A malformed session must not hide a usable media URL. The daemon may increment a
non-sensitive diagnostic counter for dropped sessions.

## 7. Extension capture

### Request observation

Add a `chrome.webRequest.onBeforeSendHeaders` listener with
`["requestHeaders", "extraHeaders"]`. The existing `webRequest` permission and
host permissions are sufficient. `extraHeaders` is required to see `Cookie` and
`Authorization` on Chromium.

The listener observes these request types:

- `main_frame`, for a page session that source JSON scripts can reuse.
- `media`, for direct media requests.
- `xmlhttprequest`, `other`, `object`, and `sub_frame`, filtered by a media URL
  heuristic or a playlist extension.

The extension keeps two bounded maps:

- `pageSessions`: tab identifier to the latest `main_frame` session.
- `requestSessions`: request identifier and tab identifier plus URL to the
  latest media or source session.

Both maps use a ten minute TTL and are evicted on tab removal, navigation, or
capacity pressure. The values live in memory or in `chrome.storage.session`.
`chrome.storage.session` is acceptable because Chromium keeps it out of the
persistent profile and clears it when the browser exits. Cookies must never go
to `chrome.storage.local` or `chrome.storage.sync`.

### Session selection

| Report | Session source |
| --- | --- |
| Direct candidate from `onResponseStarted` | The matching request session |
| Source JSON from the page agent | The matching source request session, or the page session |
| Browser download prompt | The matching download URL session, or the page session |
| Media detected only from the DOM | The page session |

If no session exists, the report is sent without one. The download still works
for public resources.

### Feature gating and opt-out

The extension reads the daemon's `features` list from the `ready` message. It
attaches a session only when `browser_session` is present. This prevents an
older daemon from receiving credentials it cannot use.

The extension options page gets a setting named "Use the browser session for
protected downloads". The default is on because the feature exists for sites
that require it. Turning it off makes the extension omit every session field.

### Protocol shape

The protocol version becomes 4. The daemon still accepts older versions, and an
older daemon ignores the new optional field because the protocol ignores unknown
fields.

```json
{
  "kind": "candidate",
  "url": "https://cdn.example/media.mp4",
  "content_type": "video/mp4",
  "page_url": "https://www.example.com/watch/1",
  "session": {
    "origin": "https://cdn.example",
    "headers": [
      { "name": "user-agent", "value": "..." },
      { "name": "referer", "value": "https://www.example.com/watch/1" },
      { "name": "cookie", "value": "session=..." }
    ]
  }
}
```

`SessionCapture` is optional on `Candidate`, `DownloadBrowser`, and `Source`.
`DownloadPage`, `ListFormats`, and `DownloadStatus` do not carry sessions because
they refer to a candidate that already holds one.

## 8. Daemon behavior

### Candidate storage

`Candidate` gains an `Option<Arc<BrowserSession>>` field. The catalog records it
with the report. A duplicate report replaces the stored session with the newer
one when the new report has a session. A duplicate report without a session keeps
the existing session. When a candidate is evicted, its session is dropped with
it.

The session stays in the candidate catalog. No separate session store is needed,
and no session value enters `Report` or any persisted type.

### Format inspection

`formats_for_candidate` and the desktop inspection path build the request with
the session headers that apply to the candidate URL:

```text
RequestContext::new(candidate.url().clone(), candidate.session_headers_for(candidate.url()))
```

For HLS and DASH, this lets the stream engine fetch a protected manifest. The
existing stream rules then apply the same headers to same-origin segments and
drop sensitive headers for cross-origin segments.

### Download creation

`download_capture` resolves the selected URL and the runtime headers for that
URL. It creates the job with no sensitive headers. When a non-expired session is
available, it also sets a public `session_required` flag:

```text
request = RequestContext::new(url, public_headers).with_session_required(true)
```

After `manager.create` returns the download identifier, the daemon binds the
`Arc<BrowserSession>` to that identifier in a runtime-only map. The map is
consulted on every execution attempt. The job's persisted request never contains
the session.

### Execution

`DownloadManager` gets an execution variant that accepts runtime headers:

```text
execute_controlled_with_updates_and_headers(id, executor, runtime_headers, ...)
```

The manager still loads the persisted job for identity, state, destination, and
progress. It builds the `ExecutionInput` request from the job request and merges
the runtime headers. Runtime headers replace persisted headers with the same
name, case-insensitively, and keep their session sensitivity and scope. The
persisted job is not modified or re-committed with those headers.

The daemon keeps the runtime binding across pause, resume, retry, and automatic
retry while the daemon is running. It removes the binding when the job completes,
is cancelled, or the session expires. A user retry after a non-authentication
failure can therefore still use the same session until the expiry deadline.

### Storage

Add one public column to the download record:

```text
session_required INTEGER NOT NULL DEFAULT 0
```

The repository persists the flag and restores it. It continues to reject
sensitive headers. No cookie, authorization value, or session header is stored.

A migration adds the column. Existing rows default to `0`.

### Restart behavior

Runtime sessions do not survive a daemon restart. A job with
`session_required = true` is recovered as interrupted, but the daemon has no
session to replay.

When such a job is resumed, the daemon attempts the request without session
headers only if the user explicitly resumes it. A `401` or `403` is mapped to an
authentication failure with the message:

> This download needs a fresh browser session. Open the page in your browser and
> start the download again.

The daemon does not retry authentication failures. Signed URLs may also have
expired, so re-capture is the supported recovery path.

## 9. Origin and redirect rules

Sensitive headers are sent only when the request URL origin equals the session
origin. Safe headers may be sent to any origin.

Examples:

- A media request to the same origin as the captured session receives `Cookie`,
  `Authorization`, `Referer`, `User-Agent`, and `Accept`.
- A signed CDN URL on another origin receives `Referer`, `User-Agent`, `Origin`,
  and `Accept`, but never `Cookie` or `Authorization`.
- A same-origin redirect keeps all headers.
- A cross-origin redirect keeps only transport headers and safe session
  headers. It never keeps `Cookie`, `Authorization`, or unknown headers.
- A stream segment on the manifest origin inherits the manifest session headers.
  A segment on another origin inherits only safe headers.

This requires `RequestHeader` to carry a scope in addition to sensitivity. The
existing constructor defaults to `SameOrigin` so current callers stay safe.
Session construction explicitly marks the safe headers as `AnyOrigin`.

`HttpClient::send_redirected` currently keeps only transport-owned headers on a
cross-origin redirect. It will additionally keep headers whose scope is
`AnyOrigin`. The same rule applies in `segment_request`.

## 10. Security invariants

- Session values never appear in `Debug`, `tracing`, error messages, SQLite,
  `chrome.storage.local`, `chrome.storage.sync`, diagnostics, or crash reports.
- The extension captures only the headers in the policy table and only for the
  request types listed above.
- The daemon ignores unknown headers and never trusts extension-provided
  sensitivity or scope.
- `Cookie` and `Authorization` never cross an origin boundary.
- `Referer` is redacted from diagnostics even though it may cross origins.
- A malformed or oversized session is dropped without failing the candidate.
- A session older than its TTL is treated as absent.
- The extension sends sessions only after the daemon advertises the capability.
- The user can disable session capture in the extension options.
- The loopback channel remains protected by the pairing token and extension
  origin policy. Session capture does not add a new network listener.
- The daemon does not weaken TLS, bypass DRM, or retry authentication failures.

## 11. Failure behavior

- If the daemon does not advertise `browser_session`, the extension omits the
  session. The download may still work for public resources.
- If the session is missing or expired, the candidate and format list still
  work. The download may fail with an authentication error.
- An authentication failure on a `session_required` job shows the re-capture
  message. It is not retryable.
- A cross-origin media URL that needs its own cookies is not solved by the page
  session. The user can start the media request directly so the extension
  captures that request's session.
- A browser download prompt with no captured request session falls back to the
  page session. If the download URL is on another origin, only safe headers are
  replayed.

## 12. Alternatives considered

### Persist encrypted credentials in SQLite

Rejected for the first increment. It needs key management, migration, deletion,
backup, and recovery rules. A local encryption key stored beside the database
does not protect against the same threats. OS keyring integration is a separate
security project.

### Capture every request header

Rejected. It captures unrelated credentials, increases fingerprinting, and makes
the origin rules harder to reason about. The whitelist is the security boundary.

### Use `chrome.cookies` as the only source

Rejected as the only source. It does not provide `Authorization`, `Referer`, or
the exact request headers. It is a useful future refresh mechanism because it
can return structured cookies with domain and path metadata.

### Keep site-specific extractors only

Rejected as the only strategy. Extractors remain useful for metadata and for
sites that expose progressive URLs, but they break when a site changes its JSON.
Request capture is stable across player changes.

### Proxy the download through the browser

Rejected. It loses Gunda's resume, progress, scheduling, and lifecycle
guarantees.

### Handle inline MSE data

Out of scope. If a player never exposes a fetchable HTTP resource, session
headers do not help. That requires a different media pipeline.

## 13. Implementation plan

### Phase 1: protocol and extension capture

- Add `SessionCapture`, `SessionHeader`, and the `browser_session` feature to
  `gunda-capture`.
- Bump `PROTOCOL_VERSION` to 4.
- Add the `onBeforeSendHeaders` listener and bounded session maps to
  `apps/extension/src/background.ts`.
- Attach sessions to `Candidate`, `Source`, and `DownloadBrowser` messages.
- Add the options toggle and extension tests.

### Phase 2: candidate and format inspection

- Add `BrowserSession` and header policy to `gunda-capture`.
- Store an `Arc<BrowserSession>` on `Candidate`.
- Validate and bound sessions on the daemon side.
- Use session headers for HLS and DASH inspection.
- Add Rust tests for validation, redaction, and format inspection.

### Phase 3: execution and persistence

- Add header scope to `RequestHeader`.
- Update `HttpClient` redirect handling and `segment_request`.
- Add the runtime header execution variant to `DownloadManager`.
- Add the runtime session map to `gundad`.
- Add `session_required` to the persisted download record and migration.
- Add integration tests with a local HTTP server that requires headers.

### Phase 4: recovery and UX

- Map authentication failures on session-bound jobs to the re-capture message.
- Keep runtime sessions across pause, resume, and retry.
- Drop sessions on completion, permanent failure, and cancellation.
- Add desktop and extension text for the expired-session case.

### Phase 5: optional refresh

- Add a daemon request for a fresh session for a specific URL.
- The extension answers from `chrome.cookies.getAll` plus the current user agent
  and page referer.
- This requires the `cookies` permission and a separate privacy review.
- Structured cookies can also replace the raw `Cookie` header for better domain
  and path scoping.

## 14. Test plan

### Extension

- A media response captures the whitelisted headers and attaches them to the
  candidate.
- A source report uses the matching request session, then the page session.
- Unknown headers and oversized sessions are dropped.
- A session is not attached when the daemon lacks the feature.
- Disabling the option removes every session field.
- Session values never appear in `chrome.storage.local` or diagnostics.
- The session map evicts expired entries and respects its capacity.

### Daemon

- A session with valid headers is accepted and attached to the candidate.
- A malformed header name, control character, or oversized value drops only the
  session.
- Sensitive header values are redacted from `Debug` output.
- A same-origin request receives `Cookie` and `Authorization`.
- A cross-origin request receives safe headers but not sensitive headers.
- A cross-origin redirect drops sensitive headers and keeps safe headers.
- A same-origin redirect keeps all headers.
- A protected HLS or DASH manifest is inspected with the session.
- A protected direct file downloads with the session.
- A session-bound job after restart fails with the re-capture message.
- Storage contains `session_required` but no session value.
- Sensitive headers are still rejected by the repository.
- `tracing` output contains no cookie or authorization value.

### Manual

- A public Instagram reel and a public Facebook video.
- An authenticated Instagram or Facebook page that requires the browser session.
- A site that redirects the media to a CDN.
- A site whose HLS or DASH segments are on a different origin from the manifest.
- A daemon restart while a session-bound job is paused.

## 15. Open decisions

- Default state of the extension option. The recommended default is on.
- Whether `session_required` belongs in the request context or in the download
  origin record.
- Whether cross-origin redirects should keep safe headers in the first
  increment or keep the current strict stripping.
- Whether on-demand session refresh belongs in the first implementation or in a
  later phase.
- Whether `chrome.storage.session` or a plain service worker map is the better
  short-lived extension store.
