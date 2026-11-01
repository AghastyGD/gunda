import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';

const source = readFileSync(new URL('../dist/background.js', import.meta.url), 'utf8');

function install({
  initialSession = {},
  localValues = {},
  tabs = [{ url: 'https://example.com/reports', title: 'Example' }],
} = {}) {
  const session = { ...initialSession };
  const local = { ...localValues };
  const sockets = [];
  const cancelled = [];
  const erased = [];
  const removedFiles = [];
  const createdWindows = [];
  const restoredDownloads = [];
  let downloadListener = null;
  let runtimeMessageListener = null;
  let headersListener = null;
  let cancelFails = false;
  let removalFails = false;

  class MockWebSocket {
    static CONNECTING = 0;
    static OPEN = 1;
    static CLOSED = 3;

    constructor(url) {
      this.url = url;
      this.readyState = MockWebSocket.CONNECTING;
      this.listeners = new Map();
      this.sent = [];
      sockets.push(this);
    }

    addEventListener(name, listener) {
      const listeners = this.listeners.get(name) ?? [];
      listeners.push(listener);
      this.listeners.set(name, listeners);
    }

    send(data) {
      this.sent.push(data);
    }

    close() {
      this.readyState = MockWebSocket.CLOSED;
      this.emit('close', {});
    }

    emit(name, event) {
      for (const listener of this.listeners.get(name) ?? []) {
        listener(event);
      }
    }

    open() {
      this.readyState = MockWebSocket.OPEN;
      this.emit('open', {});
    }

    message(value) {
      this.emit('message', { data: JSON.stringify(value) });
    }
  }

  const storage = (values) => ({
    async get(key) {
      if (typeof key === 'string') {
        return { [key]: values[key] };
      }
      return { ...values };
    },
    async set(next) {
      Object.assign(values, next);
    },
    async remove(keys) {
      for (const key of keys) {
        delete values[key];
      }
    },
    async clear() {
      for (const key of Object.keys(values)) {
        delete values[key];
      }
    },
  });

  const chrome = {
    storage: {
      local: storage(local),
      session: storage(session),
    },
    runtime: {
      lastError: undefined,
      connectNative() {
        throw new Error('native host unavailable');
      },
      sendNativeMessage: async () => ({ kind: 'error' }),
      getURL: (path) => `chrome-extension://test/${path}`,
      onInstalled: { addListener() {} },
      onStartup: { addListener() {} },
      onMessage: {
        addListener(listener) {
          runtimeMessageListener = listener;
        },
      },
    },
    downloads: {
      onCreated: {
        addListener(listener) {
          downloadListener = listener;
        },
      },
      cancel(id, callback) {
        cancelled.push(id);
        if (cancelFails) {
          chrome.runtime.lastError = { message: 'cancel failed' };
          callback?.();
          chrome.runtime.lastError = undefined;
          return;
        }
        callback?.();
      },
      erase(query, callback) {
        erased.push(query.id);
        const outer = chrome.runtime.lastError;
        chrome.runtime.lastError = undefined;
        callback?.();
        chrome.runtime.lastError = outer;
      },
      removeFile(id, callback) {
        removedFiles.push(id);
        const outer = chrome.runtime.lastError;
        chrome.runtime.lastError = removalFails ? { message: 'remove failed' } : undefined;
        callback?.();
        chrome.runtime.lastError = outer;
      },
      download(options, callback) {
        restoredDownloads.push(options);
        callback?.(1000 + restoredDownloads.length);
      },
    },
    windows: {
      create(options, callback) {
        const created = { id: createdWindows.length + 1, ...options };
        createdWindows.push(created);
        callback?.(created);
        return Promise.resolve(created);
      },
      onRemoved: { addListener() {} },
      remove(_id, callback) {
        callback?.();
      },
    },
    webRequest: {
      onResponseStarted: { addListener() {} },
      onHeadersReceived: {
        addListener(listener) {
          headersListener = listener;
        },
      },
    },
    tabs: {
      async get() {
        return { url: 'https://example.com/page', title: 'Example' };
      },
      async query() {
        return tabs;
      },
    },
    alarms: {
      create() {},
      onAlarm: { addListener() {} },
    },
  };

  const context = vm.createContext({
    chrome,
    console,
    URL,
    TextEncoder,
    setTimeout,
    clearTimeout,
    WebSocket: MockWebSocket,
  });

  vm.runInContext(source, context);

  return {
    local,
    session,
    sockets,
    cancelled,
    erased,
    removedFiles,
    createdWindows,
    restoredDownloads,
    failCancellation() {
      cancelFails = true;
    },
    failRemoval() {
      removalFails = true;
    },
    reconnect() {
      assert.ok(
        runtimeMessageListener,
        'the runtime message listener is registered',
      );
      runtimeMessageListener({ kind: 'reconnect' }, {}, () => {});
    },
    triggerDownload(item) {
      assert.ok(downloadListener, 'the browser download listener is registered');
      downloadListener(item);
    },
    triggerHeaders(details) {
      assert.ok(headersListener, 'the response header listener is registered');
      headersListener(details);
    },
    send(message, sender = {}) {
      assert.ok(
        runtimeMessageListener,
        'the runtime message listener is registered',
      );
      return new Promise((resolve) => {
        const result = runtimeMessageListener(message, sender, resolve);
        if (result !== true) {
          resolve(undefined);
        }
      });
    },
  };
}

async function settle() {
  for (let index = 0; index < 8; index += 1) {
    await new Promise((resolve) => setImmediate(resolve));
  }
}

async function connected({ features = ['browser_download'], tabs } = {}) {
  const harness = install({
    localValues: {
      endpoint: 'ws://127.0.0.1:1/capture',
      token: 'a'.repeat(64),
    },
    tabs,
  });
  harness.reconnect();
  await settle();
  const socket = harness.sockets[0];
  assert.ok(socket, 'the reconnect opens a socket');
  socket.open();
  await settle();
  socket.message({ kind: 'ready', version: 3, features });
  await settle();
  return { ...harness, socket };
}

function downloadItem(overrides = {}) {
  return {
    id: 42,
    url: 'https://example.com/files/report.pdf',
    finalUrl: 'https://example.com/files/report.pdf',
    mime: 'application/pdf',
    referrer: 'https://example.com/reports',
    filename: '/home/user/Downloads/report.pdf',
    fileSize: 8_000_000_000,
    totalBytes: 8_000_000_000,
    byExtensionId: undefined,
    ...overrides,
  };
}

function promptParameters(window) {
  return new URL(window.url).searchParams;
}

function startMessage(window, overrides = {}) {
  const parameters = promptParameters(window);
  const size = parameters.get('size_bytes');
  return {
    kind: 'start-browser-download',
    download_id: Number(parameters.get('download_id')),
    url: parameters.get('url') ?? '',
    content_type: parameters.get('content_type'),
    size_bytes: size === null ? null : Number(size),
    page_url: parameters.get('page_url') ?? '',
    page_title: parameters.get('page_title'),
    directory: parameters.get('directory') ?? '',
    filename: parameters.get('filename') ?? '',
    ...overrides,
  };
}

function browserDownloadRequest(socket) {
  return socket.sent
    .map((entry) => JSON.parse(entry))
    .find((entry) => entry.kind === 'download_browser');
}

test('a pending download survives a worker restart and is sent after pairing', async () => {
  const message = JSON.stringify({
    kind: 'candidate',
    url: 'https://example.com/files/report.pdf',
    content_type: 'application/pdf',
    page_url: 'https://example.com/reports',
    page_title: null,
    filename: 'report.pdf',
  });
  const { session, sockets } = install({
    initialSession: { pending: [message] },
    localValues: {
      endpoint: 'ws://127.0.0.1:1/capture',
      token: 'a'.repeat(64),
    },
  });

  await settle();
  assert.equal(sockets.length, 1);

  const socket = sockets[0];
  socket.open();
  await settle();
  socket.message({ kind: 'ready', version: 3 });
  await settle();

  assert.deepEqual(socket.sent.map((entry) => JSON.parse(entry)), [
    { kind: 'hello', version: 3, token: 'a'.repeat(64), agent: 'gunda-capture/0.1.0' },
    {
      kind: 'candidate',
      url: 'https://example.com/files/report.pdf',
      content_type: 'application/pdf',
      page_url: 'https://example.com/reports',
      page_title: null,
      filename: 'report.pdf',
    },
  ]);
  assert.equal(session.pending.length, 0);
});

test('a browser download is counted and kept when the channel is unavailable', async () => {
  const { session, triggerDownload } = install();

  triggerDownload(downloadItem());
  await settle();

  assert.equal(session.stats.browser_download, 1);
  assert.equal(Array.isArray(session.pending), true);
  assert.equal(session.pending.length, 1);

  const message = JSON.parse(session.pending[0]);
  assert.equal(message.kind, 'candidate');
  assert.equal(message.url, 'https://example.com/files/report.pdf');
  assert.equal(message.content_type, 'application/pdf');
  assert.equal(message.filename, 'report.pdf');
});

test('downloads started by extensions are counted without being queued', async () => {
  const { session, triggerDownload } = install();

  triggerDownload(
    downloadItem({ byExtensionId: 'another-extension' }),
  );
  await settle();

  assert.equal(session.stats.ignored_by_extension, 1);
  assert.equal(session.pending, undefined);
});

test('downloads with a non-HTTP URL are counted without being queued', async () => {
  const { session, triggerDownload } = install();

  triggerDownload(
    downloadItem({ url: 'blob:https://example.com/1234' }),
  );
  await settle();

  assert.equal(session.stats.ignored_non_http, 1);
  assert.equal(session.pending, undefined);
});

test('a browser download opens a prompt and starts only after Start', async () => {
  const {
    socket,
    session,
    local,
    cancelled,
    erased,
    createdWindows,
    triggerDownload,
    send,
  } = await connected();

  triggerDownload(downloadItem());
  await settle();

  assert.deepEqual(cancelled, [42]);
  assert.deepEqual(erased, [42]);
  assert.equal(createdWindows.length, 1);
  assert.equal(browserDownloadRequest(socket), undefined);

  const parameters = promptParameters(createdWindows[0]);
  assert.equal(parameters.get('browser_prompt'), '1');
  assert.equal(parameters.get('download_id'), '42');
  assert.equal(parameters.get('url'), 'https://example.com/files/report.pdf');
  assert.equal(parameters.get('content_type'), 'application/pdf');
  assert.equal(parameters.get('size_bytes'), '8000000000');
  assert.equal(parameters.get('page_url'), 'https://example.com/reports');
  assert.equal(parameters.get('page_title'), 'Example');
  assert.equal(parameters.get('filename'), 'report.pdf');
  assert.equal(parameters.get('directory'), '/home/user/Downloads');

  const responsePromise = send(startMessage(createdWindows[0]));
  await settle();

  assert.deepEqual(cancelled, [42]);
  assert.deepEqual(erased, [42]);

  const request = browserDownloadRequest(socket);
  assert.ok(request);
  assert.equal(request.url, 'https://example.com/files/report.pdf');
  assert.equal(request.content_type, 'application/pdf');
  assert.equal(request.size_bytes, 8_000_000_000);
  assert.equal(request.page_url, 'https://example.com/reports');
  assert.equal(request.page_title, 'Example');
  assert.equal(request.filename, 'report.pdf');
  assert.equal(request.directory, '/home/user/Downloads');

  socket.message({ kind: 'download_queued', id: 7 });
  const response = await responsePromise;
  assert.equal(response.kind, 'queued');
  assert.equal(response.id, 7);
  await settle();

  assert.equal(session.stats.browser_download_prompted, 1);
  assert.equal(session.stats.browser_download_intercepted, 1);
  assert.equal(session.last_download.outcome, 'intercepted');
  assert.equal(local.browser_directory, '/home/user/Downloads');
});

test('a browser download is restored when the daemon refuses it', async () => {
  const {
    socket,
    session,
    cancelled,
    createdWindows,
    restoredDownloads,
    triggerDownload,
    send,
  } = await connected();

  triggerDownload(downloadItem());
  await settle();

  const responsePromise = send(startMessage(createdWindows[0]));
  await settle();

  assert.deepEqual(cancelled, [42]);
  assert.equal(restoredDownloads.length, 0);

  socket.message({ kind: 'download_unavailable', reason: 'capture_unavailable' });
  const response = await responsePromise;
  assert.equal(response.kind, 'unavailable');
  assert.equal(response.reason, 'capture_unavailable');
  await settle();

  assert.equal(restoredDownloads.length, 1);
  assert.equal(restoredDownloads[0].url, 'https://example.com/files/report.pdf');
  assert.equal(restoredDownloads[0].filename, 'report.pdf');
  assert.equal(session.stats.browser_download_kept, 1);
  assert.equal(session.last_download.outcome, 'kept');
  assert.equal(session.last_download.reason, 'capture_unavailable');
});

test('a browser download is not prompted without the daemon capability', async () => {
  const { socket, session, cancelled, createdWindows, triggerDownload } =
    await connected({ features: [] });

  triggerDownload(downloadItem());
  await settle();

  assert.deepEqual(cancelled, []);
  assert.equal(createdWindows.length, 0);
  assert.equal(browserDownloadRequest(socket), undefined);
  assert.ok(
    socket.sent
      .map((entry) => JSON.parse(entry))
      .some((entry) => entry.kind === 'candidate'),
  );
  assert.equal(session.stats.browser_download, 1);
});

test('a download started by another extension is not prompted', async () => {
  const { session, cancelled, createdWindows, triggerDownload } =
    await connected();

  triggerDownload(
    downloadItem({ byExtensionId: 'another-extension' }),
  );
  await settle();

  assert.deepEqual(cancelled, []);
  assert.equal(createdWindows.length, 0);
  assert.equal(session.stats.ignored_by_extension, 1);
  assert.equal(session.last_download.outcome, 'ignored_extension');
});

test('a blob download is not prompted', async () => {
  const { session, cancelled, createdWindows, triggerDownload } =
    await connected();

  triggerDownload(
    downloadItem({ url: 'blob:https://example.com/1234' }),
  );
  await settle();

  assert.deepEqual(cancelled, []);
  assert.equal(createdWindows.length, 0);
  assert.equal(session.stats.ignored_non_http, 1);
  assert.equal(session.last_download.scheme, 'blob:');
  assert.equal(session.last_download.outcome, 'ignored_non_http');
});

test('a finished browser download is removed before the prompt when Chrome refuses to cancel it', async () => {
  const {
    socket,
    cancelled,
    erased,
    removedFiles,
    createdWindows,
    triggerDownload,
    send,
    failCancellation,
  } = await connected();
  failCancellation();

  triggerDownload(downloadItem());
  await settle();

  assert.equal(createdWindows.length, 1);
  assert.deepEqual(cancelled, [42]);
  assert.deepEqual(removedFiles, [42]);
  assert.deepEqual(erased, [42]);

  const responsePromise = send(startMessage(createdWindows[0]));
  await settle();

  const request = browserDownloadRequest(socket);
  assert.ok(request);
  assert.equal(request.url, 'https://example.com/files/report.pdf');

  socket.message({ kind: 'download_queued', id: 7 });
  const response = await responsePromise;
  assert.equal(response.kind, 'queued');
  assert.equal(response.id, 7);
});

test('a browser download keeps running when Chrome cannot cancel or remove it', async () => {
  const {
    socket,
    session,
    cancelled,
    restoredDownloads,
    createdWindows,
    triggerDownload,
    failCancellation,
    failRemoval,
  } = await connected();
  failCancellation();
  failRemoval();

  triggerDownload(downloadItem());
  await settle();

  assert.deepEqual(cancelled, [42]);
  assert.equal(createdWindows.length, 0);
  assert.equal(restoredDownloads.length, 0);
  assert.equal(browserDownloadRequest(socket), undefined);
  assert.equal(session.stats.browser_download_cancel_failed, 1);
  assert.equal(session.stats.browser_download_kept, 1);
  assert.equal(session.last_download.outcome, 'kept');
  assert.equal(session.last_download.reason, 'cancel_failed');
});

test('cancelling the prompt restores the browser download', async () => {
  const {
    socket,
    session,
    cancelled,
    createdWindows,
    restoredDownloads,
    triggerDownload,
    send,
  } = await connected();

  triggerDownload(downloadItem());
  await settle();
  assert.equal(createdWindows.length, 1);
  assert.deepEqual(cancelled, [42]);

  const response = await send(
    startMessage(createdWindows[0], { kind: 'cancel-browser-download' }),
  );
  assert.equal(response.restored, true);
  await settle();

  assert.equal(restoredDownloads.length, 1);
  assert.equal(restoredDownloads[0].url, 'https://example.com/files/report.pdf');
  assert.equal(restoredDownloads[0].filename, 'report.pdf');
  assert.equal(browserDownloadRequest(socket), undefined);
  assert.equal(session.stats.browser_download_kept, 1);
  assert.equal(session.last_download.outcome, 'kept');
  assert.equal(session.last_download.reason, 'prompt_cancelled');

  // Chrome reports the restored item without byExtensionId; it must not prompt again.
  triggerDownload(downloadItem({ id: 1001 }));
  await settle();
  assert.equal(createdWindows.length, 1);
  assert.equal(browserDownloadRequest(socket), undefined);

  // The restore marker is one-shot, so a new user download of the URL prompts.
  triggerDownload(downloadItem({ id: 1002 }));
  await settle();
  assert.equal(createdWindows.length, 2);
});

test('a generic Drive filename uses the page title', async () => {
  const { createdWindows, triggerDownload } = await connected({
    tabs: [
      {
        url: 'https://drive.google.com/file/d/abc123456789/view',
        title: 'My video - Google Drive',
      },
    ],
  });

  triggerDownload(
    downloadItem({
      url: 'https://drive.google.com/uc?export=download&id=abc123456789',
      finalUrl:
        'https://drive.usercontent.google.com/download?id=abc123456789&export=download',
      referrer:
        'https://drive.usercontent.google.com/download?id=abc123456789&export=download',
      filename: '/home/user/Downloads/download.mp4',
      mime: 'video/mp4',
    }),
  );
  await settle();

  const parameters = promptParameters(createdWindows[0]);
  assert.equal(parameters.get('filename'), 'My video.mp4');
  assert.equal(
    parameters.get('page_url'),
    'https://drive.google.com/file/d/abc123456789/view',
  );
  assert.equal(
    parameters.get('url'),
    'https://drive.usercontent.google.com/download?id=abc123456789&export=download',
  );
});

test('a generic download without a real name falls back to a plain name', async () => {
  const { createdWindows, triggerDownload } = await connected({
    tabs: [
      {
        url: 'https://drive.google.com/file/d/xyz123456789/view',
        title: 'Google Drive: Aviso de verificação de vírus',
      },
    ],
  });

  triggerDownload(
    downloadItem({
      url: 'https://drive.google.com/uc?export=download&id=other987654321',
      finalUrl:
        'https://drive.usercontent.google.com/download?id=other987654321&export=download',
      referrer: 'https://drive.google.com/file/d/xyz123456789/view',
      filename: '/home/user/Downloads/uc',
      mime: 'video/mp4',
    }),
  );
  await settle();

  const parameters = promptParameters(createdWindows[0]);
  assert.equal(parameters.get('filename'), 'download.mp4');
});

test('a browser download uses the name the server declared', async () => {
  const { createdWindows, triggerDownload, triggerHeaders } = await connected();

  triggerHeaders({
    url: 'https://example.com/files/report.pdf',
    responseHeaders: [
      {
        name: 'Content-Disposition',
        value: "attachment; filename*=UTF-8''Annual%20report.pdf",
      },
    ],
  });

  triggerDownload(
    downloadItem({
      filename: '/home/user/Downloads/download',
      mime: 'application/pdf',
    }),
  );
  await settle();

  const parameters = promptParameters(createdWindows[0]);
  assert.equal(parameters.get('filename'), 'Annual report.pdf');
});

test('a browser download uses the URL name instead of the page title', async () => {
  const { createdWindows, triggerDownload } = await connected();

  triggerDownload(
    downloadItem({
      url: 'https://cdn.example.com/files/Holiday%20clip.mp4',
      finalUrl: 'https://cdn.example.com/files/Holiday%20clip.mp4',
      filename: '/home/user/Downloads/download',
      mime: 'video/mp4',
    }),
  );
  await settle();

  const parameters = promptParameters(createdWindows[0]);
  assert.equal(parameters.get('filename'), 'Holiday clip.mp4');
});

test('a browser prompt prefers the final URL and keeps the known size', async () => {
  const { createdWindows, triggerDownload } = await connected();

  triggerDownload(
    downloadItem({
      url: 'https://example.com/download?id=42',
      finalUrl: 'https://cdn.example.com/files/movie.mp4',
      mime: 'video/mp4',
      filename: '/home/user/Downloads/movie.mp4',
      fileSize: 12_000_000,
      totalBytes: 12_000_000,
    }),
  );
  await settle();

  const parameters = promptParameters(createdWindows[0]);
  assert.equal(parameters.get('url'), 'https://cdn.example.com/files/movie.mp4');
  assert.equal(parameters.get('size_bytes'), '12000000');
});
