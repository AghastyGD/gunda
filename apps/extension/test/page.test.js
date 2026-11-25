import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';

const source = readFileSync(new URL('../dist/page.js', import.meta.url), 'utf8');

class MockXMLHttpRequest {
  open() {}
  send() {}
  addEventListener() {}
}

function install(
  fetch,
  locationUrl = 'https://www.instagram.com/reels/example/',
  xhr = MockXMLHttpRequest,
  performanceEntries = null,
) {
  const messages = [];
  const window = {
    fetch,
    addEventListener() {},
    postMessage(message) {
      messages.push(message);
    },
  };
  const context = vm.createContext({
    window,
    location: new URL(locationUrl),
    XMLHttpRequest: xhr,
    URL,
    TextDecoder,
    setInterval(callback) {
      callback();
      return 0;
    },
    clearInterval() {},
    ...(performanceEntries ? { performance: { getEntriesByType: () => performanceEntries } } : {}),
    console,
  });

  vm.runInContext(source, context);
  return { messages, window };
}

async function settle() {
  await new Promise((resolve) => setImmediate(resolve));
  await new Promise((resolve) => setImmediate(resolve));
}

test('forwards only media objects from Instagram JSON', async () => {
  const response = new Response(
    JSON.stringify({
      data: {
        viewer: { username: 'someone', email: 'private@example.com' },
        media: {
          code: 'ABC123',
          video_versions: [
            {
              url: 'https://cdn.example.com/video-low.mp4',
              width: 640,
              height: 360,
            },
            {
              url: 'https://cdn.example.com/video-high.mp4',
              width: 1080,
              height: 1920,
            },
          ],
          video_dash_manifest: '<MPD />',
        },
      },
    }),
    { headers: { 'content-type': 'application/json' } },
  );
  Object.defineProperty(response, 'url', {
    value: 'https://www.instagram.com/api/graphql?query=example',
  });
  const { messages, window } = install(async () => response);

  await window.fetch(response.url);
  await settle();

  assert.equal(messages.length, 1);
  assert.equal(messages[0].source, 'gunda-page');
  assert.equal(messages[0].kind, 'source');
  assert.equal(messages[0].source_url, 'https://www.instagram.com/api/graphql?query=example');
  const { items } = messages[0].document;
  assert.equal(items.length, 1);
  assert.equal(items[0].code, 'ABC123');
  assert.equal(items[0].video_versions[1].url, 'https://cdn.example.com/video-high.mp4');
  assert.equal(JSON.stringify(messages[0].document).includes('private@example.com'), false);
});

test('reads Instagram API responses served as JavaScript', async () => {
  const response = new Response(
    JSON.stringify({
      items: [{ pk: '7', video_versions: [{ url: 'https://cdn.example.com/reel.mp4' }] }],
    }),
    { headers: { 'content-type': 'text/javascript; charset=utf-8' } },
  );
  Object.defineProperty(response, 'url', {
    value: 'https://www.instagram.com/api/v1/clips/home/',
  });
  const { messages, window } = install(async () => response);

  await window.fetch(response.url);
  await settle();

  assert.equal(messages.length, 1);
  assert.equal(
    messages[0].document.items[0].video_versions[0].url,
    'https://cdn.example.com/reel.mp4',
  );
});

test('reads every line of a streamed Facebook GraphQL response', async () => {
  const body = [
    'for (;;);{"data":{"node":{"id":"1","message":"not media"}}}',
    '{"data":{"video":{"id":"42","browser_native_hd_url":"https://cdn.example.com/hd.mp4"}}}',
    '{"label":"deferred","data":{"video":{"id":"43","playable_url":"https://cdn.example.com/sd.mp4"}}}',
  ].join('\r\n');
  const response = new Response(body, {
    headers: { 'content-type': 'text/html; charset="utf-8"' },
  });
  Object.defineProperty(response, 'url', {
    value: 'https://www.facebook.com/api/graphql/',
  });
  const { messages, window } = install(
    async () => response,
    'https://www.facebook.com/watch/?v=42',
  );

  await window.fetch(response.url);
  await settle();

  assert.equal(messages.length, 1);
  const { items } = JSON.parse(JSON.stringify(messages[0].document));
  assert.deepEqual(items.map((item) => item.id).sort(), ['42', '43']);
});

test('keeps the enclosing Facebook video id for nested media', async () => {
  const reel = (id) => ({
    id,
    owner: { id: '900', name: 'Someone' },
    videoDeliveryResponseFragment: {
      videoDeliveryResponseResult: {
        progressive_urls: [
          {
            progressive_url: `https://cdn.example.com/${id}-hd.mp4`,
            metadata: { quality: 'HD' },
          },
        ],
      },
    },
  });
  const response = new Response(
    JSON.stringify({ data: { reels: [reel('41'), reel('42')] } }),
    { headers: { 'content-type': 'text/html' } },
  );
  Object.defineProperty(response, 'url', {
    value: 'https://www.facebook.com/api/graphql/',
  });
  const { messages, window } = install(
    async () => response,
    'https://www.facebook.com/reel/41',
  );

  await window.fetch(response.url);
  await settle();

  const { items } = JSON.parse(JSON.stringify(messages[0].document));
  assert.deepEqual(
    items
      .map((item) => [item.id, item.media.progressive_urls[0].progressive_url])
      .sort(),
    [
      ['41', 'https://cdn.example.com/41-hd.mp4'],
      ['42', 'https://cdn.example.com/42-hd.mp4'],
    ],
  );
  assert.equal(JSON.stringify(items).includes('Someone'), false);
});

test('carries Facebook post text to nested media', async () => {
  const response = new Response(
    JSON.stringify({
      story: {
        message: { text: 'A cat that likes boxes', ranges: [] },
        attachments: [
          {
            media: {
              id: '4211',
              videoDeliveryLegacyFields: {
                browser_native_hd_url: 'https://cdn.example.com/4211.mp4',
              },
            },
          },
        ],
      },
    }),
    { headers: { 'content-type': 'text/html' } },
  );
  Object.defineProperty(response, 'url', {
    value: 'https://www.facebook.com/api/graphql/',
  });
  const { messages, window } = install(
    async () => response,
    'https://www.facebook.com/reel/4211',
  );

  await window.fetch(response.url);
  await settle();

  const { items } = JSON.parse(JSON.stringify(messages[0].document));
  assert.equal(items.length, 1);
  assert.equal(items[0].id, '4211');
  assert.equal(items[0].title, 'A cat that likes boxes');
  assert.equal(
    items[0].media.browser_native_hd_url,
    'https://cdn.example.com/4211.mp4',
  );
});

test('ignores HTML from unrecognised endpoints', async () => {
  const response = new Response('{"video_url":"https://cdn.example.com/a.mp4"}', {
    headers: { 'content-type': 'text/html' },
  });
  Object.defineProperty(response, 'url', {
    value: 'https://www.facebook.com/some/page',
  });
  const { messages, window } = install(
    async () => response,
    'https://www.facebook.com/some/page',
  );

  await window.fetch(response.url);
  await settle();

  assert.equal(messages.length, 0);
});

test('ignores unrelated JSON responses', async () => {
  const response = new Response(JSON.stringify({ title: 'No media here' }), {
    headers: { 'content-type': 'application/json' },
  });
  Object.defineProperty(response, 'url', {
    value: 'https://www.instagram.com/static/data.json',
  });
  const { messages, window } = install(async () => response);

  await window.fetch(response.url);
  await settle();

  assert.equal(messages.length, 0);
});

test('forwards YouTube player JSON for daemon extraction', async () => {
  const response = new Response(
    JSON.stringify({
      videoDetails: { videoId: 'video-1', title: 'A video' },
      streamingData: { hlsManifestUrl: 'https://cdn.example.com/master.m3u8' },
    }),
    { headers: { 'content-type': 'application/json' } },
  );
  Object.defineProperty(response, 'url', {
    value: 'https://www.youtube.com/youtubei/v1/player',
  });
  const { messages, window } = install(
    async () => response,
    'https://www.youtube.com/watch?v=video-1',
  );

  await window.fetch(response.url);
  await settle();

  assert.equal(messages.length, 1);
  assert.equal(messages[0].kind, 'source');
  assert.equal(messages[0].document.videoDetails.videoId, 'video-1');
});

test('reads JSON XHR responses without touching responseText', async () => {
  class JsonXhr {
    responseType = '';
    response = {
      media: {
        video_versions: [{ url: 'https://cdn.example.com/reel.mp4', width: 1080, height: 1920 }],
      },
    };
    listeners = new Map();

    open() {}

    addEventListener(kind, listener) {
      this.listeners.set(kind, listener);
    }

    send() {
      this.responseType = 'json';
      this.listeners.get('load')?.();
    }

    getResponseHeader() {
      return 'application/json';
    }

    get responseText() {
      throw new Error('responseText is unavailable for JSON responses');
    }
  }

  const { messages } = install(async () => new Response(), undefined, JsonXhr);
  const xhr = new JsonXhr();

  xhr.open('GET', 'https://www.instagram.com/api/graphql?query=reel');
  xhr.send();
  await settle();

  assert.equal(messages.length, 1);
  assert.equal(messages[0].kind, 'source');
  assert.equal(messages[0].document.items[0].video_versions[0].width, 1080);
});

test('reports complete media URLs from resource timing', () => {
  const { messages } = install(
    async () => new Response(),
    undefined,
    MockXMLHttpRequest,
    [{ name: 'https://cdn.example.com/reel.mp4?token=public' }],
  );

  assert.deepEqual(JSON.parse(JSON.stringify(messages)), [
    {
      source: 'gunda-page',
      kind: 'candidate',
      url: 'https://cdn.example.com/reel.mp4?token=public',
    },
  ]);
});

test('forwards media hints from JSON on an unrecognised site', async () => {
  const response = new Response(
    JSON.stringify({
      player: { video_url: 'https://cdn.example.com/unknown-site.mp4' },
    }),
    { headers: { 'content-type': 'application/json' } },
  );
  Object.defineProperty(response, 'url', {
    value: 'https://example.com/api/player',
  });
  const { messages, window } = install(
    async () => response,
    'https://example.com/watch',
  );

  await window.fetch(response.url);
  await settle();

  assert.equal(messages.length, 1);
  assert.equal(messages[0].kind, 'source');
  assert.equal(
    messages[0].document.player.video_url,
    'https://cdn.example.com/unknown-site.mp4',
  );
});
