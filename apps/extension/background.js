// Gunda Capture -- the half of the capture channel that runs in the browser.
//
// This service worker is the only part that talks to the desktop application.
// It watches responses, because a media response is impossible to miss there,
// and it also accepts what the content script finds in the page.
//
// Everything here is observational: nothing is blocked, modified, or redirected,
// and nothing is sent anywhere except the endpoint the user paired with.

const PROTOCOL_VERSION = 1;
const AGENT = 'gunda-capture/0.1.0';

// Reports are held while the desktop is unreachable, but only in memory, and
// only for as long as this worker lives. A media URL stays valid for as long as
// its page does, so a page that was missed can simply be played again.
const MAX_PENDING = 64;

// The same URL is ignored for this long. The desktop deduplicates as well; this
// keeps a playlist that is re-fetched every few seconds from flooding a socket.
const DEDUP_WINDOW_MS = 10 * 60 * 1000;
const DEDUP_LIMIT = 2048;

const MEDIA_TYPES = /^(video|audio)\//i;
const PLAYLIST_TYPES =
  /^(application\/(vnd\.apple\.mpegurl|x-mpegurl|dash\+xml)|audio\/(x-)?mpegurl)$/i;
const SEGMENT_TYPES = /^video\/(mp2t|iso\.segment)$/i;

const MEDIA_EXTENSIONS = /\.(m3u8|mpd|mp4|m4v|webm|mkv|mov|mp3|m4a|aac|ogg|oga|opus|flac|wav)$/i;
const SEGMENT_EXTENSIONS = /\.(ts|m4s)$/i;

let connection = null;
let greeted = false;
let reconnectTimer = null;
let attempt = 0;

const pending = [];
const seen = new Map();

// --- the desktop side of the channel ---------------------------------------

async function settings() {
  const stored = await chrome.storage.local.get(['endpoint', 'token']);

  return {
    endpoint: String(stored.endpoint ?? '').trim(),
    token: String(stored.token ?? '').trim(),
  };
}

async function setStatus(status) {
  await chrome.storage.session.set({ status: { ...status, at: Date.now() } });
}

async function count(kind) {
  const stored = await chrome.storage.session.get('stats');
  const stats = stored.stats ?? {};

  stats[kind] = (stats[kind] ?? 0) + 1;

  await chrome.storage.session.set({ stats });
}

function connect() {
  if (
    connection &&
    (connection.readyState === WebSocket.CONNECTING || connection.readyState === WebSocket.OPEN)
  ) {
    return;
  }

  settings().then(({ endpoint, token }) => {
    if (!endpoint || !token) {
      setStatus({ state: 'unpaired' });

      return;
    }

    let socket;

    try {
      socket = new WebSocket(endpoint);
    } catch (error) {
      setStatus({ state: 'unreachable', detail: String(error) });
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
        handle(JSON.parse(event.data));
      } catch {
        setStatus({ state: 'malformed', detail: 'the desktop sent something unreadable' });
      }
    });

    socket.addEventListener('close', () => {
      if (connection === socket) {
        connection = null;
      }

      greeted = false;
      setStatus({ state: 'closed' });
      scheduleReconnect();
    });

    // A failed connection is always followed by 'close', which does the work.
    socket.addEventListener('error', () => {});
  });
}

function scheduleReconnect() {
  if (reconnectTimer) {
    return;
  }

  const delay = Math.min(30000, 1000 * 2 ** Math.min(attempt, 5));
  attempt += 1;

  // Timers do not survive the worker being suspended; the keepalive alarm below
  // is what brings the channel back when this one is gone.
  reconnectTimer = setTimeout(() => {
    reconnectTimer = null;
    connect();
  }, delay);
}

function disconnect() {
  if (reconnectTimer) {
    clearTimeout(reconnectTimer);
    reconnectTimer = null;
  }

  const socket = connection;

  connection = null;
  greeted = false;

  if (socket) {
    try {
      socket.close();
    } catch {
      // The socket was already gone.
    }
  }
}

function handle(message) {
  if (!message || typeof message !== 'object') {
    return;
  }

  switch (message.kind) {
    case 'ready':
      greeted = true;
      setStatus({ state: 'connected', protocol: message.version });
      flush();
      break;

    case 'accepted':
      count('accepted');
      break;

    case 'duplicate':
      count('duplicate');
      break;

    case 'refused':
      // The desktop refused this report; it is a stable reason the desktop
      // chose, so it is safe to show. The URL is not kept.
      count('refused');
      setStatus({ state: 'connected', refused: message.reason });
      break;

    case 'malformed':
      count('malformed');
      setStatus({ state: 'malformed', detail: message.reason });
      break;

    default:
      break;
  }
}

function flush() {
  while (pending.length > 0 && greeted && connection?.readyState === WebSocket.OPEN) {
    connection.send(pending.shift());
  }
}

// --- what the browser tells us about --------------------------------------

function report(candidate) {
  if (!candidate?.url) {
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

function headerValue(headers, name) {
  const found = (headers ?? []).find((header) => header.name.toLowerCase() === name);

  return found?.value ?? null;
}

function essence(contentType) {
  return String(contentType ?? '')
    .split(';')[0]
    .trim()
    .toLowerCase();
}

function looksLikeMedia(url, contentType) {
  return (
    MEDIA_TYPES.test(contentType) ||
    PLAYLIST_TYPES.test(contentType) ||
    MEDIA_EXTENSIONS.test(new URL(url).pathname)
  );
}

function isSegment(url, contentType) {
  return SEGMENT_TYPES.test(contentType) || SEGMENT_EXTENSIONS.test(new URL(url).pathname);
}

// The page a capture belongs to. Without it the desktop cannot say where a
// download came from, so a request with no page is not reported at all.
async function attribute(tabId) {
  try {
    const tab = await chrome.tabs.get(tabId);

    if (!tab?.url || !/^https?:/i.test(tab.url)) {
      return null;
    }

    return { url: tab.url, title: tab.title ?? null };
  } catch {
    return null;
  }
}

chrome.webRequest.onResponseStarted.addListener(
  (details) => {
    if (details.tabId < 0) {
      return;
    }

    const contentType = essence(headerValue(details.responseHeaders, 'content-type'));
    const mediaElement = details.type === 'media';

    if (!mediaElement && !looksLikeMedia(details.url, contentType)) {
      return;
    }

    if (isSegment(details.url, contentType)) {
      return;
    }

    attribute(details.tabId).then((page) => {
      if (!page) {
        return;
      }

      report({
        url: details.url,
        content_type: contentType || null,
        page_url: page.url,
        page_title: page.title,
      });
    });
  },
  {
    urls: ['<all_urls>'],
    types: ['media', 'xmlhttprequest', 'other', 'object', 'sub_frame'],
  },
  ['responseHeaders'],
);

chrome.runtime.onMessage.addListener((message, sender) => {
  if (!message) {
    return;
  }

  if (message.kind === 'reconnect') {
    disconnect();
    connect();

    return;
  }

  if (message.kind !== 'candidate' || !sender.tab) {
    return;
  }

  const tab = sender.tab;

  if (!tab.url || !/^https?:/i.test(tab.url)) {
    return;
  }

  report({
    url: message.url,
    content_type: message.content_type ?? null,
    page_url: tab.url,
    page_title: tab.title ?? null,
  });
});

chrome.runtime.onInstalled.addListener(() => connect());
chrome.runtime.onStartup.addListener(() => connect());

chrome.alarms.create('gunda-capture-keepalive', { periodInMinutes: 1 });
chrome.alarms.onAlarm.addListener((alarm) => {
  if (alarm.name === 'gunda-capture-keepalive') {
    connect();
  }
});
