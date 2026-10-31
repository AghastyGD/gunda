const PROTOCOL_VERSION = 3;
const AGENT = 'gunda-capture/0.1.0';
const NATIVE_HOST_NAME = 'com.gunda.capture';
const NATIVE_HOST_PROTOCOL_VERSION = 1;
const MAX_PENDING = 32;
const PENDING_KEY = 'pending';
const MAX_SOURCE_MESSAGE_BYTES = 1792 * 1024;
const DEDUP_WINDOW_MS = 10 * 60 * 1000;
const DEDUP_LIMIT = 2048;
const BROWSER_DOWNLOAD_FEATURE = 'browser_download';
const BROWSER_DOWNLOAD_TIMEOUT_MS = 6000;
const HANDLED_DOWNLOAD_TTL_MS = 60 * 1000;

const MEDIA_TYPES = /^(video|audio)\//i;
const PLAYLIST_TYPES =
  /^(application\/(vnd\.apple\.mpegurl|x-mpegurl|dash\+xml)|audio\/(x-)?mpegurl)$/i;
const SEGMENT_TYPES = /^video\/(mp2t|iso\.segment)$/i;
const MEDIA_EXTENSIONS =
  /\.(m3u8|mpd|mp4|m4v|webm|mkv|mov|flv|avi|wmv|mpeg|mpg|3gp|ogv|mp3|m4a|aac|ogg|oga|opus|flac|wav)$/i;
const SEGMENT_EXTENSIONS = /\.(ts|m4s)$/i;

let connection: WebSocket | null = null;
let greeted = false;
let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
let attempt = 0;
let automaticPairing: PairingSettings | null = null;
let pendingLoad: Promise<void> | null = null;
let pendingWrite: Promise<void> = Promise.resolve();
let pendingEpoch = 0;
let flushing: Promise<void> | null = null;
let serverFeatures = new Set<string>();

const pending: string[] = [];
const seen = new Map<string, number>();
const handledDownloads = new Map<number, number>();
const reportedDownloads = new Map<number, number>();
const downloadRequests: {
  resolve: (response: DownloadResponse) => void;
  timer: ReturnType<typeof setTimeout>;
}[] = [];
const formatRequests: {
  resolve: (response: FormatResponse) => void;
  timer: ReturnType<typeof setTimeout>;
}[] = [];
const statusRequests: {
  resolve: (response: DownloadStatusResponse) => void;
  timer: ReturnType<typeof setTimeout>;
}[] = [];

async function nativePairing(): Promise<PairingSettings | null> {
  return new Promise((resolve) => {
    let settled = false;
    let port: chrome.runtime.Port;
    const finish = (pairing: PairingSettings | null): void => {
      if (settled) {
        return;
      }
      settled = true;
      clearTimeout(timer);
      try {
        port.disconnect();
      } catch {
        return;
      }
      resolve(pairing);
    };
    const timer = setTimeout(() => finish(null), 5000);

    try {
      port = chrome.runtime.connectNative(NATIVE_HOST_NAME);
    } catch {
      clearTimeout(timer);
      resolve(null);
      return;
    }

    port.onMessage.addListener((message: unknown) => {
      if (!message || typeof message !== 'object' || !('kind' in message)) {
        return;
      }
      const response = message as {
        kind: string;
        version?: number;
        endpoint?: string;
        token?: string;
      };
      if (
        response.kind === 'pairing' &&
        response.version === NATIVE_HOST_PROTOCOL_VERSION &&
        typeof response.endpoint === 'string' &&
        typeof response.token === 'string'
      ) {
        finish({ endpoint: response.endpoint, token: response.token });
      }
    });
    port.onDisconnect.addListener(() => finish(null));
    try {
      port.postMessage({
        kind: 'pairing',
        version: NATIVE_HOST_PROTOCOL_VERSION,
        agent: AGENT,
      });
    } catch {
      finish(null);
    }
  });
}

async function settings(): Promise<PairingSettings> {
  automaticPairing ??= await nativePairing();
  if (automaticPairing) {
    return automaticPairing;
  }

  const stored = await chrome.storage.local.get(['endpoint', 'token']);

  return {
    endpoint: String(stored.endpoint ?? '').trim(),
    token: String(stored.token ?? '').trim(),
  };
}

async function setStatus(status: Omit<ChannelStatus, 'at'>): Promise<void> {
  await chrome.storage.session.set({ status: { ...status, at: Date.now() } });
}

async function count(kind: string, amount = 1): Promise<void> {
  const stored = await chrome.storage.session.get('stats');
  const stats: ChannelStats =
    stored.stats && typeof stored.stats === 'object' ? { ...stored.stats } : {};

  stats[kind] = (stats[kind] ?? 0) + amount;
  await chrome.storage.session.set({ stats });
}

function loadPending(): Promise<void> {
  const epoch = pendingEpoch;
  pendingLoad ??= chrome.storage.session
    .get(PENDING_KEY)
    .then((stored) => {
      if (epoch !== pendingEpoch) {
        return;
      }
      const value = stored[PENDING_KEY];
      if (!Array.isArray(value)) {
        return;
      }
      for (const entry of value) {
        if (typeof entry === 'string' && pending.length < MAX_PENDING) {
          pending.push(entry);
        }
      }
    })
    .catch(() => {});
  return pendingLoad;
}

function savePending(): void {
  const snapshot = [...pending];
  pendingWrite = pendingWrite
    .then(() => chrome.storage.session.set({ [PENDING_KEY]: snapshot }))
    .catch(() => {});
}

function enqueue(message: string): void {
  void loadPending().then(() => {
    if (pending.length >= MAX_PENDING) {
      pending.shift();
    }
    pending.push(message);
    savePending();
    connect();
    flush();
    setTimeout(() => flush(), 0);
  });
}

function connect(): void {
  if (
    connection &&
    (connection.readyState === WebSocket.CONNECTING || connection.readyState === WebSocket.OPEN)
  ) {
    return;
  }

  void settings().then(({ endpoint, token }) => {
    if (!endpoint || !token) {
      void setStatus({ state: 'unpaired' });
      return;
    }

    let socket: WebSocket;

    try {
      socket = new WebSocket(endpoint);
    } catch (error) {
      void setStatus({ state: 'unreachable', detail: String(error) });
      scheduleReconnect();
      return;
    }

    connection = socket;
    greeted = false;

    socket.addEventListener('open', () => {
      attempt = 0;
      socket.send(
        JSON.stringify({ kind: 'hello', version: PROTOCOL_VERSION, token, agent: AGENT }),
      );
    });

    socket.addEventListener('message', (event) => {
      try {
        handle(JSON.parse(String(event.data)) as unknown);
      } catch {
        void setStatus({ state: 'malformed', detail: 'the desktop sent something unreadable' });
      }
    });

    socket.addEventListener('close', () => {
      if (connection === socket) {
        connection = null;
      }
      automaticPairing = null;
      greeted = false;
      serverFeatures = new Set<string>();
      for (const request of downloadRequests.splice(0)) {
        clearTimeout(request.timer);
        request.resolve({ kind: 'unavailable', reason: 'connection_closed' });
      }
      for (const request of formatRequests.splice(0)) {
        clearTimeout(request.timer);
        request.resolve({ kind: 'unavailable', reason: 'connection_closed' });
      }
      for (const request of statusRequests.splice(0)) {
        clearTimeout(request.timer);
        request.resolve({ kind: 'unavailable', reason: 'connection_closed' });
      }
      void setStatus({ state: 'closed' });
      scheduleReconnect();
    });

    socket.addEventListener('error', () => {});
  });
}

function scheduleReconnect(): void {
  if (reconnectTimer) {
    return;
  }

  const delay = Math.min(30000, 1000 * 2 ** Math.min(attempt, 5));
  attempt += 1;
  reconnectTimer = setTimeout(() => {
    reconnectTimer = null;
    connect();
  }, delay);
}

function disconnect(): void {
  if (reconnectTimer) {
    clearTimeout(reconnectTimer);
    reconnectTimer = null;
  }

  const socket = connection;
  connection = null;
  automaticPairing = null;
  greeted = false;
  serverFeatures = new Set<string>();

  if (socket) {
    socket.close();
  }
}

function handle(message: unknown): void {
  if (!message || typeof message !== 'object' || !('kind' in message)) {
    return;
  }

  const server = message as ServerMessage;
  switch (server.kind) {
    case 'ready':
      greeted = true;
      serverFeatures = new Set(server.features ?? []);
      void setStatus({ state: 'connected', protocol: server.version });
      flush();
      break;
    case 'accepted':
      void count('accepted');
      break;
    case 'duplicate':
      void count('duplicate');
      break;
    case 'recorded':
      void count('accepted', server.accepted.length);
      void count('duplicate', server.duplicates.length);
      break;
    case 'download_queued':
      finishDownloadRequest({ kind: 'queued', id: server.id });
      break;
    case 'download_unavailable':
      finishDownloadRequest({ kind: 'unavailable', reason: server.reason });
      break;
    case 'page_formats':
      finishFormatRequest({
        kind: 'formats',
        capture_id: server.capture_id,
        formats: server.formats,
        directory: server.directory,
        filename: server.filename,
      });
      break;
    case 'formats_unavailable':
      finishFormatRequest({ kind: 'unavailable', reason: server.reason });
      break;
    case 'download_status':
      finishStatusRequest({ kind: 'status', status: server.status });
      break;
    case 'download_status_unavailable':
      finishStatusRequest({ kind: 'unavailable', reason: server.reason });
      break;
    case 'refused':
      void count('refused');
      void setStatus({ state: 'connected', refused: server.reason });
      break;
    case 'malformed':
      void count('malformed');
      void setStatus({ state: 'malformed', detail: server.reason });
      break;
  }
}

function flush(): void {
  if (flushing) {
    return;
  }

  flushing = loadPending()
    .then(() => {
      while (pending.length > 0 && greeted && connection?.readyState === WebSocket.OPEN) {
        const message = pending.shift();
        if (message) {
          connection.send(message);
        }
      }
    })
    .finally(() => {
      savePending();
      flushing = null;
    });
}

function reportCandidate(candidate: CandidateReport): void {
  if (!candidate.url) {
    return;
  }

  const now = Date.now();
  const key = `${candidate.url}\u0000${candidate.filename ?? ''}`;
  const last = seen.get(key);
  if (last !== undefined && now - last < DEDUP_WINDOW_MS) {
    return;
  }

  seen.set(key, now);
  if (seen.size > DEDUP_LIMIT) {
    for (const [seenKey, at] of seen) {
      if (now - at >= DEDUP_WINDOW_MS) {
        seen.delete(seenKey);
      }
    }
  }

  const message = JSON.stringify({ kind: 'candidate', ...candidate });
  if (greeted && connection?.readyState === WebSocket.OPEN) {
    connection.send(message);
    return;
  }

  enqueue(message);
}

function reportSource(
  source: { source_url: string; document: unknown },
): void {
  const message = JSON.stringify({ kind: 'source', ...source });
  if (new TextEncoder().encode(message).byteLength > MAX_SOURCE_MESSAGE_BYTES) {
    void count('source_too_large');
    return;
  }

  if (greeted && connection?.readyState === WebSocket.OPEN) {
    connection.send(message);
    return;
  }

  enqueue(message);
}

function finishDownloadRequest(response: DownloadResponse): void {
  const request = downloadRequests.shift();
  if (request) {
    clearTimeout(request.timer);
    request.resolve(response);
  }
}

function finishFormatRequest(response: FormatResponse): void {
  const request = formatRequests.shift();
  if (request) {
    clearTimeout(request.timer);
    request.resolve(response);
  }
}

function finishStatusRequest(response: DownloadStatusResponse): void {
  const request = statusRequests.shift();
  if (request) {
    clearTimeout(request.timer);
    request.resolve(response);
  }
}

async function waitForCaptureConnection(): Promise<boolean> {
  connect();
  const deadline = Date.now() + 6000;
  while (
    !connection ||
    connection.readyState !== WebSocket.OPEN ||
    !greeted
  ) {
    if (Date.now() >= deadline) {
      return false;
    }
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  return true;
}

async function requestPageFormats(pageUrl: string): Promise<FormatResponse> {
  if (!(await waitForCaptureConnection())) {
    return { kind: 'unavailable', reason: 'connection_unavailable' };
  }

  return new Promise((resolve) => {
    const timer = setTimeout(() => {
      const index = formatRequests.findIndex((request) => request.resolve === resolve);
      if (index >= 0) {
        formatRequests.splice(index, 1);
      }
      resolve({ kind: 'unavailable', reason: 'timeout' });
    }, 15000);
    formatRequests.push({ resolve, timer });
    connection?.send(JSON.stringify({ kind: 'list_formats', page_url: pageUrl }));
  });
}

async function requestPageDownload(
  pageUrl: string,
  captureId: number,
  format?: string,
  directory?: string,
  filename?: string,
): Promise<DownloadResponse> {
  if (!(await waitForCaptureConnection())) {
    return { kind: 'unavailable', reason: 'connection_unavailable' };
  }

  return new Promise((resolve) => {
    const timer = setTimeout(() => {
      const index = downloadRequests.findIndex((request) => request.resolve === resolve);
      if (index >= 0) {
        downloadRequests.splice(index, 1);
      }
      resolve({ kind: 'unavailable', reason: 'timeout' });
    }, 6000);
    downloadRequests.push({ resolve, timer });
    connection?.send(
      JSON.stringify({
        kind: 'download_page',
        page_url: pageUrl,
        capture_id: captureId,
        format,
        directory,
        filename,
      }),
    );
  });
}

async function chooseDownloadFolder(): Promise<{ path?: string; error?: string }> {
  try {
    const response = (await chrome.runtime.sendNativeMessage(NATIVE_HOST_NAME, {
      kind: 'choose_directory',
      version: NATIVE_HOST_PROTOCOL_VERSION,
    })) as { kind?: string; path?: string; message?: string };
    if (response.kind === 'directory' && typeof response.path === 'string') {
      return { path: response.path };
    }
    return {
      error: response.message || 'The folder picker did not return a folder.',
    };
  } catch {
    return { error: 'The native folder picker is unavailable.' };
  }
}

function openDownloadWindow(
  pageUrl: string,
  pageTitle: string | undefined,
  message: Extract<RuntimeMessage, { kind: 'open-download' }>,
): void {
  const url = new URL(chrome.runtime.getURL('download.html'));
  url.searchParams.set('page_url', pageUrl);
  url.searchParams.set('capture_id', String(message.capture_id));
  url.searchParams.set('format', message.format);
  url.searchParams.set('label', message.label);
  url.searchParams.set('details', message.details);
  if (message.directory) {
    url.searchParams.set('directory', message.directory);
  }
  url.searchParams.set(
    'filename',
    message.filename ?? titleFilename(pageTitle ?? '', 'mp4'),
  );
  void chrome.windows.create({
    url: url.href,
    type: 'popup',
    width: 520,
    height: 340,
  });
}

function titleFilename(title: string, extension: string): string {
  const cleaned = title
    .replace(/\s*-\s*YouTube\s*$/i, '')
    .replace(/\s*\|\s*(Instagram|Facebook)\s*$/i, '')
    .replace(/[~/\\?%*:|"<>\u0000-\u001f]/g, ' ')
    .replace(/\s+/g, ' ')
    .trim()
    .slice(0, 180);
  const name = cleaned || 'download';
  return /\.[a-z0-9]{2,5}$/i.test(name) ? name : `${name}.${extension}`;
}

async function requestDownloadStatus(id: number): Promise<DownloadStatusResponse> {
  if (!(await waitForCaptureConnection())) {
    return { kind: 'unavailable', reason: 'connection_unavailable' };
  }

  return new Promise((resolve) => {
    const timer = setTimeout(() => {
      const index = statusRequests.findIndex((request) => request.resolve === resolve);
      if (index >= 0) {
        statusRequests.splice(index, 1);
      }
      resolve({ kind: 'unavailable', reason: 'timeout' });
    }, 5000);
    statusRequests.push({ resolve, timer });
    connection?.send(JSON.stringify({ kind: 'download_status', id }));
  });
}

function headerValue(headers: chrome.webRequest.HttpHeader[] | undefined, name: string): string | null {
  const found = headers?.find((header) => header.name.toLowerCase() === name);
  return found?.value ?? null;
}

function essence(contentType: string | null): string {
  return (contentType ?? '').split(';')[0]?.trim().toLowerCase() ?? '';
}

function looksLikeMedia(url: string, contentType: string): boolean {
  return (
    MEDIA_TYPES.test(contentType) ||
    PLAYLIST_TYPES.test(contentType) ||
    MEDIA_EXTENSIONS.test(new URL(url).pathname)
  );
}

function isSegment(url: string, contentType: string): boolean {
  const parsed = new URL(url);

  return (
    SEGMENT_TYPES.test(contentType) ||
    SEGMENT_EXTENSIONS.test(parsed.pathname) ||
    /[?&](?:bytestart|byteend|range)=/i.test(parsed.search)
  );
}

function downloadFilename(filename: string | undefined): string | null {
  const name = filename?.split(/[\\/]/).pop()?.trim();
  return name ? [...name].slice(0, 200).join('') : null;
}

function schemeHost(url: string): { scheme: string; host: string } {
  try {
    const parsed = new URL(url);
    return { scheme: parsed.protocol, host: parsed.host };
  } catch {
    return { scheme: '', host: '' };
  }
}

async function rememberDownload(
  item: chrome.downloads.DownloadItem,
  outcome: BrowserDownloadOutcome,
  reason: string | null = null,
): Promise<void> {
  const { scheme, host } = schemeHost(item.url);
  const diagnostic: BrowserDownloadDiagnostic = {
    at: Date.now(),
    scheme,
    host,
    mime: item.mime || '',
    filename: downloadFilename(item.filename),
    by_extension: Boolean(item.byExtensionId),
    outcome,
    reason,
  };
  await chrome.storage.session.set({ last_download: diagnostic });
}

function rememberFor(map: Map<number, number>, id: number): void {
  const now = Date.now();
  map.set(id, now);
  for (const [key, at] of map) {
    if (now - at >= HANDLED_DOWNLOAD_TTL_MS) {
      map.delete(key);
    }
  }
}

function reportBrowserDownload(
  item: chrome.downloads.DownloadItem,
  outcome: BrowserDownloadOutcome = 'kept',
  reason: string | null = null,
): void {
  if (reportedDownloads.has(item.id)) {
    return;
  }
  rememberFor(reportedDownloads, item.id);

  if (item.byExtensionId) {
    void count('ignored_by_extension');
    void rememberDownload(item, 'ignored_extension');
    return;
  }
  if (!/^https?:/i.test(item.url)) {
    void count('ignored_non_http');
    void rememberDownload(item, 'ignored_non_http');
    return;
  }

  void count('browser_download');
  void rememberDownload(item, outcome, reason);
  reportCandidate({
    url: item.url,
    content_type: item.mime || null,
    page_url: /^https?:/i.test(item.referrer) ? item.referrer : item.url,
    page_title: null,
    filename: downloadFilename(item.filename),
  });
}

function canInterceptBrowserDownload(item: chrome.downloads.DownloadItem): boolean {
  return (
    !item.byExtensionId &&
    /^https?:/i.test(item.url) &&
    greeted &&
    connection?.readyState === WebSocket.OPEN &&
    serverFeatures.has(BROWSER_DOWNLOAD_FEATURE)
  );
}

function requestBrowserDownload(
  item: chrome.downloads.DownloadItem,
): Promise<DownloadResponse> {
  if (!connection || connection.readyState !== WebSocket.OPEN || !greeted) {
    return Promise.resolve({ kind: 'unavailable', reason: 'connection_unavailable' });
  }

  return new Promise((resolve) => {
    const timer = setTimeout(() => {
      const index = downloadRequests.findIndex((request) => request.resolve === resolve);
      if (index >= 0) {
        downloadRequests.splice(index, 1);
      }
      resolve({ kind: 'unavailable', reason: 'timeout' });
    }, BROWSER_DOWNLOAD_TIMEOUT_MS);
    downloadRequests.push({ resolve, timer });
    connection?.send(
      JSON.stringify({
        kind: 'download_browser',
        url: item.url,
        content_type: item.mime || null,
        page_url: /^https?:/i.test(item.referrer) ? item.referrer : item.url,
        page_title: null,
        filename: downloadFilename(item.filename),
      }),
    );
  });
}

function cancelBrowserDownload(id: number): Promise<boolean> {
  return new Promise((resolve) => {
    try {
      chrome.downloads.cancel(id, () => {
        if (chrome.runtime.lastError) {
          resolve(false);
          return;
        }
        chrome.downloads.erase({ id }, () => {
          void chrome.runtime.lastError;
          resolve(true);
        });
      });
    } catch {
      resolve(false);
    }
  });
}

async function interceptBrowserDownload(
  item: chrome.downloads.DownloadItem,
  suggest?: (suggestion?: chrome.downloads.FilenameSuggestion) => void,
): Promise<void> {
  let finished = false;
  const finish = (): void => {
    if (finished) {
      return;
    }
    finished = true;
    try {
      suggest?.();
    } catch {
      // The browser may already have removed the item.
    }
  };

  try {
    const response = await requestBrowserDownload(item);
    if (response.kind !== 'queued') {
      await count('browser_download_kept');
      reportBrowserDownload(item, 'kept', response.reason);
      finish();
      return;
    }

    const cancelled = await cancelBrowserDownload(item.id);
    if (!cancelled) {
      await count('browser_download_cancel_failed');
      reportBrowserDownload(item, 'kept', 'cancel_failed');
      finish();
      return;
    }

    await count('browser_download_intercepted');
    void rememberDownload(item, 'intercepted');
  } catch {
    await count('browser_download_kept');
    reportBrowserDownload(item, 'kept', 'intercept_failed');
    finish();
  }
}

function beginBrowserIntercept(
  item: chrome.downloads.DownloadItem,
  suggest?: (suggestion?: chrome.downloads.FilenameSuggestion) => void,
): boolean {
  if (handledDownloads.has(item.id) || !canInterceptBrowserDownload(item)) {
    return false;
  }

  rememberFor(handledDownloads, item.id);
  void interceptBrowserDownload(item, suggest);
  return true;
}

async function attribute(tabId: number): Promise<{ url: string; title: string | null } | null> {
  try {
    const tab = await chrome.tabs.get(tabId);
    if (!tab.url || !/^https?:/i.test(tab.url)) {
      return null;
    }
    return { url: tab.url, title: tab.title ?? null };
  } catch {
    return null;
  }
}

function runtimeMessage(value: unknown): RuntimeMessage | null {
  if (!value || typeof value !== 'object' || !('kind' in value)) {
    return null;
  }

  const message = value as RuntimeMessage;
  if (message.kind === 'candidate' && typeof message.url === 'string') {
    return message;
  }
  if (
    message.kind === 'source' &&
    typeof message.source_url === 'string' &&
    message.document !== null &&
    typeof message.document === 'object'
  ) {
    return message;
  }
  return message.kind === 'reconnect' ||
    message.kind === 'forget' ||
    message.kind === 'list-formats' ||
    message.kind === 'download-status' ||
    message.kind === 'open-download' ||
    message.kind === 'choose-folder' ||
    message.kind === 'start-download'
    ? message
    : null;
}

chrome.webRequest.onResponseStarted.addListener(
  (details) => {
    if (details.tabId < 0) {
      return;
    }

    const contentType = essence(headerValue(details.responseHeaders, 'content-type'));
    if (details.type !== 'media' && !looksLikeMedia(details.url, contentType)) {
      return;
    }

    const contentRange = headerValue(details.responseHeaders, 'content-range');
    if (details.statusCode === 206 || contentRange || isSegment(details.url, contentType)) {
      return;
    }

    void attribute(details.tabId).then((page) => {
      if (page) {
        reportCandidate({
          url: details.url,
          content_type: contentType || null,
          page_url: page.url,
          page_title: page.title,
        });
      }
    });
  },
  {
    urls: ['<all_urls>'],
    types: ['media', 'xmlhttprequest', 'other', 'object', 'sub_frame'],
  },
  ['responseHeaders'],
);

if (chrome.downloads.onDeterminingFilename) {
  chrome.downloads.onDeterminingFilename.addListener((item, suggest) => {
    if (handledDownloads.has(item.id)) {
      suggest();
      return false;
    }
    if (beginBrowserIntercept(item, suggest)) {
      return true;
    }

    suggest();
    reportBrowserDownload(item);
    return false;
  });
}

chrome.downloads.onCreated.addListener((item) => {
  if (handledDownloads.has(item.id)) {
    return;
  }
  if (beginBrowserIntercept(item)) {
    return;
  }
  reportBrowserDownload(item);
});

chrome.runtime.onMessage.addListener((value: unknown, sender, sendResponse) => {
  const message = runtimeMessage(value);
  if (!message) {
    return;
  }

  if (message.kind === 'reconnect') {
    disconnect();
    connect();
    return;
  }

  if (message.kind === 'forget') {
    pendingEpoch += 1;
    pendingLoad = Promise.resolve();
    pending.length = 0;
    savePending();
    disconnect();
    connect();
    return;
  }

  if (message.kind === 'list-formats') {
    if (!sender.tab?.url || !/^https?:/i.test(sender.tab.url)) {
      return;
    }
    void requestPageFormats(sender.tab.url).then(sendResponse);
    return true;
  }

  if (message.kind === 'download-status') {
    void requestDownloadStatus(message.id).then(sendResponse);
    return true;
  }

  if (message.kind === 'open-download') {
    if (!sender.tab?.url || !/^https?:/i.test(sender.tab.url)) {
      return;
    }
    openDownloadWindow(sender.tab.url, sender.tab.title, message);
    sendResponse({ opened: true });
    return;
  }

  if (message.kind === 'choose-folder') {
    void chooseDownloadFolder().then(sendResponse);
    return true;
  }

  if (message.kind === 'start-download') {
    void requestPageDownload(
      message.page_url,
      message.capture_id,
      message.format,
      message.directory,
      message.filename,
    ).then(sendResponse);
    return true;
  }

  if (!sender.tab?.url || !/^https?:/i.test(sender.tab.url)) {
    return;
  }

  if (message.kind === 'source') {
    reportSource({
      source_url: message.source_url,
      document: message.document,
    });
    return;
  }

  if (message.kind === 'candidate') {
    reportCandidate({
      url: message.url,
      content_type: message.content_type ?? null,
      page_url: sender.tab.url,
      page_title: sender.tab.title ?? null,
    });
  }
});

void loadPending().then(() => {
  if (pending.length > 0) {
    connect();
  }
});

chrome.runtime.onInstalled.addListener(() => connect());
chrome.runtime.onStartup.addListener(() => connect());

void chrome.alarms.create('gunda-capture-keepalive', { periodInMinutes: 1 });
chrome.alarms.onAlarm.addListener((alarm) => {
  if (alarm.name === 'gunda-capture-keepalive') {
    connect();
  }
});
