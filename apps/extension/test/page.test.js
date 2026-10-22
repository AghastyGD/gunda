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

function install(fetch) {
  const messages = [];
  const window = {
    fetch,
    postMessage(message) {
      messages.push(message);
    },
  };
  const context = vm.createContext({
    window,
    location: new URL('https://www.instagram.com/reels/example/'),
    XMLHttpRequest: MockXMLHttpRequest,
    URL,
    TextDecoder,
    console,
  });

  vm.runInContext(source, context);
  return { messages, window };
}

async function settle() {
  await new Promise((resolve) => setImmediate(resolve));
  await new Promise((resolve) => setImmediate(resolve));
}

test('reports the best progressive Instagram video', async () => {
  const response = new Response(
    JSON.stringify({
      data: {
        media: {
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
  assert.equal(messages[0].kind, 'candidate');
  assert.equal(messages[0].url, 'https://cdn.example.com/video-high.mp4');
});

test('ignores unrelated JSON responses', async () => {
  const response = new Response(JSON.stringify({ video_versions: [] }), {
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
