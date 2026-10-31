import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';

const source = readFileSync(new URL('../dist/background.js', import.meta.url), 'utf8');

function install({ initialSession = {}, localValues = {} } = {}) {
  const session = { ...initialSession };
  const local = { ...localValues };
  const sockets = [];
  const cancelled = [];
  const erased = [];
  let downloadListener = null;
  let determiningFilenameListener = null;
  let runtimeMessageListener = null;
  let cancelFails = false;

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
      onDeterminingFilename: {
        addListener(listener) {
          determiningFilenameListener = listener;
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
        callback?.();
      },
    },
    webRequest: {
      onResponseStarted: { addListener() {} },
    },
    tabs: {
      async get() {
        return { url: 'https://example.com/page', title: 'Example' };
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
    failCancellation() {
      cancelFails = true;
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
    triggerDeterminingFilename(item) {
      assert.ok(
        determiningFilenameListener,
        'the filename listener is registered',
      );
      const suggestions = [];
      const result = determiningFilenameListener(item, (suggestion) => {
        suggestions.push(suggestion);
      });
      return { result, suggestions };
    },
  };
}

async function settle() {
  for (let index = 0; index < 6; index += 1) {
    await new Promise((resolve) => setImmediate(resolve));
  }
}

async function connected({ features = ['browser_download'] } = {}) {
  const harness = install({
    localValues: {
      endpoint: 'ws://127.0.0.1:1/capture',
      token: 'a'.repeat(64),
    },
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
    mime: 'application/pdf',
    referrer: 'https://example.com/reports',
    filename: '/home/user/Downloads/report.pdf',
    byExtensionId: undefined,
    ...overrides,
  };
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

  triggerDownload({
    url: 'https://example.com/files/report.pdf',
    mime: 'application/pdf',
    referrer: 'https://example.com/reports',
    filename: '/home/user/Downloads/report.pdf',
    byExtensionId: undefined,
  });
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

  triggerDownload({
    url: 'https://example.com/files/report.pdf',
    mime: 'application/pdf',
    referrer: 'https://example.com/reports',
    filename: 'report.pdf',
    byExtensionId: 'another-extension',
  });
  await settle();

  assert.equal(session.stats.ignored_by_extension, 1);
  assert.equal(session.pending, undefined);
});

test('downloads with a non-HTTP URL are counted without being queued', async () => {
  const { session, triggerDownload } = install();

  triggerDownload({
    url: 'blob:https://example.com/1234',
    mime: 'application/pdf',
    referrer: 'https://example.com/reports',
    filename: 'report.pdf',
  });
  await settle();

  assert.equal(session.stats.ignored_non_http, 1);
  assert.equal(session.pending, undefined);
});

test('a browser download is cancelled after the daemon queues it', async () => {
  const { socket, session, cancelled, erased, triggerDeterminingFilename } =
    await connected();

  const { result, suggestions } = triggerDeterminingFilename(downloadItem());
  assert.equal(result, true);
  await settle();

  const request = socket.sent
    .map((entry) => JSON.parse(entry))
    .find((entry) => entry.kind === 'download_browser');
  assert.ok(request);
  assert.equal(request.url, 'https://example.com/files/report.pdf');
  assert.equal(request.content_type, 'application/pdf');
  assert.equal(request.page_url, 'https://example.com/reports');
  assert.equal(request.filename, 'report.pdf');

  socket.message({ kind: 'download_queued', id: 7 });
  await settle();

  assert.deepEqual(cancelled, [42]);
  assert.deepEqual(erased, [42]);
  assert.equal(suggestions.length, 0);
  assert.equal(session.stats.browser_download_intercepted, 1);
  assert.equal(session.last_download.outcome, 'intercepted');
  assert.equal(session.last_download.scheme, 'https:');
  assert.equal(session.last_download.host, 'example.com');
});

test('a browser download continues when the daemon refuses it', async () => {
  const { socket, session, cancelled, triggerDeterminingFilename } =
    await connected();

  const { result, suggestions } = triggerDeterminingFilename(downloadItem());
  assert.equal(result, true);
  await settle();

  socket.message({ kind: 'download_unavailable', reason: 'capture_unavailable' });
  await settle();

  assert.deepEqual(cancelled, []);
  assert.equal(suggestions.length, 1);
  assert.equal(session.stats.browser_download_kept, 1);
  assert.equal(session.stats.browser_download, 1);
  assert.equal(session.last_download.outcome, 'kept');
  assert.equal(session.last_download.reason, 'capture_unavailable');

  const sent = socket.sent.map((entry) => JSON.parse(entry));
  assert.ok(sent.some((entry) => entry.kind === 'candidate'));
});

test('a browser download is not intercepted without the daemon capability', async () => {
  const { socket, session, cancelled, triggerDeterminingFilename } =
    await connected({ features: [] });

  const { result, suggestions } = triggerDeterminingFilename(downloadItem());
  assert.equal(result, false);
  assert.equal(suggestions.length, 1);
  await settle();

  assert.deepEqual(cancelled, []);
  const sent = socket.sent.map((entry) => JSON.parse(entry));
  assert.equal(sent.some((entry) => entry.kind === 'download_browser'), false);
  assert.ok(sent.some((entry) => entry.kind === 'candidate'));
  assert.equal(session.stats.browser_download, 1);
});

test('a download started by another extension is not intercepted', async () => {
  const { socket, session, cancelled, triggerDeterminingFilename } =
    await connected();

  const { result, suggestions } = triggerDeterminingFilename(
    downloadItem({ byExtensionId: 'another-extension' }),
  );
  assert.equal(result, false);
  assert.equal(suggestions.length, 1);
  await settle();

  assert.deepEqual(cancelled, []);
  const sent = socket.sent.map((entry) => JSON.parse(entry));
  assert.equal(sent.some((entry) => entry.kind === 'download_browser'), false);
  assert.equal(session.stats.ignored_by_extension, 1);
  assert.equal(session.last_download.outcome, 'ignored_extension');
});

test('a blob download is not intercepted', async () => {
  const { socket, session, cancelled, triggerDeterminingFilename } =
    await connected();

  const { result, suggestions } = triggerDeterminingFilename(
    downloadItem({ url: 'blob:https://example.com/1234' }),
  );
  assert.equal(result, false);
  assert.equal(suggestions.length, 1);
  await settle();

  assert.deepEqual(cancelled, []);
  const sent = socket.sent.map((entry) => JSON.parse(entry));
  assert.equal(sent.some((entry) => entry.kind === 'download_browser'), false);
  assert.equal(session.stats.ignored_non_http, 1);
  assert.equal(session.last_download.scheme, 'blob:');
  assert.equal(session.last_download.outcome, 'ignored_non_http');
});

test('a browser download continues when Chrome refuses to cancel it', async () => {
  const {
    socket,
    session,
    cancelled,
    triggerDeterminingFilename,
    failCancellation,
  } = await connected();
  failCancellation();

  const { result, suggestions } = triggerDeterminingFilename(downloadItem());
  assert.equal(result, true);
  await settle();

  socket.message({ kind: 'download_queued', id: 7 });
  await settle();

  assert.deepEqual(cancelled, [42]);
  assert.equal(suggestions.length, 1);
  assert.equal(session.stats.browser_download_cancel_failed, 1);
  assert.equal(session.last_download.outcome, 'kept');
  assert.equal(session.last_download.reason, 'cancel_failed');
});
