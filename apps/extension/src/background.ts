const PROTOCOL_VERSION = 2;
const AGENT = 'gunda-capture/0.1.0';
const NATIVE_HOST_NAME = 'com.gunda.capture';
const NATIVE_HOST_PROTOCOL_VERSION = 1;
const MAX_PENDING = 64;
const MAX_SOURCE_MESSAGE_BYTES = 480 * 1024;
const DEDUP_WINDOW_MS = 10 * 60 * 1000;
const DEDUP_LIMIT = 2048;

const MEDIA_TYPES = /^(video|audio)\//i;
const PLAYLIST_TYPES =
  /^(application\/(vnd\.apple\.mpegurl|x-mpegurl|dash\+xml)|audio\/(x-)?mpegurl)$/i;
const SEGMENT_TYPES = /^video\/(mp2t|iso\.segment)$/i;
const MEDIA_EXTENSIONS = /\.(m3u8|mpd|mp4|m4v|webm|mkv|mov|mp3|m4a|aac|ogg|oga|opus|flac|wav)$/i;
const SEGMENT_EXTENSIONS = /\.(ts|m4s)$/i;

let connection: WebSocket | null = null;
let greeted = false;
let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
let attempt = 0;
let automaticPairing: PairingSettings | null = null;

const pending: string[] = [];
const seen = new Map<string, number>();

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
  while (pending.length > 0 && greeted && connection?.readyState === WebSocket.OPEN) {
    const message = pending.shift();
    if (message) {
      connection.send(message);
    }
  }
}

function reportCandidate(candidate: CandidateReport): void {
  if (!candidate.url) {
    return;
  }

  const now = Date.now();
  const last = seen.get(candidate.url);
  if (last !== undefined && now - last < DEDUP_WINDOW_MS) {
    return;
  }

  seen.set(candidate.url, now);
  if (seen.size > DEDUP_LIMIT) {
    for (const [url, at] of seen) {
      if (now - at >= DEDUP_WINDOW_MS) {
        seen.delete(url);
      }
    }
  }

  const message = JSON.stringify({ kind: 'candidate', ...candidate });
  if (greeted && connection?.readyState === WebSocket.OPEN) {
    connection.send(message);
    return;
  }

  if (pending.length >= MAX_PENDING) {
    pending.shift();
  }
  pending.push(message);
  connect();
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

  if (pending.length >= MAX_PENDING) {
    pending.shift();
  }
  pending.push(message);
  connect();
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
  return message.kind === 'reconnect' ? message : null;
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

chrome.runtime.onMessage.addListener((value: unknown, sender) => {
  const message = runtimeMessage(value);
  if (!message) {
    return;
  }

  if (message.kind === 'reconnect') {
    disconnect();
    connect();
    return;
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

chrome.runtime.onInstalled.addListener(() => connect());
chrome.runtime.onStartup.addListener(() => connect());

void chrome.alarms.create('gunda-capture-keepalive', { periodInMinutes: 1 });
chrome.alarms.onAlarm.addListener((alarm) => {
  if (alarm.name === 'gunda-capture-keepalive') {
    connect();
  }
});
