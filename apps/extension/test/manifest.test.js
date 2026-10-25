import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const manifest = JSON.parse(
  readFileSync(new URL('../src/manifest.json', import.meta.url), 'utf8'),
);

test('manifest declares automatic native pairing', () => {
  assert.ok(manifest.permissions.includes('nativeMessaging'));
  assert.equal(typeof manifest.key, 'string');
});

test('manifest key resolves to the registered extension id', () => {
  const digest = createHash('sha256')
    .update(Buffer.from(manifest.key, 'base64'))
    .digest()
    .subarray(0, 16);
  const id = [...digest]
    .flatMap((byte) => [byte >> 4, byte & 0x0f])
    .map((nibble) => String.fromCharCode('a'.charCodeAt(0) + nibble))
    .join('');

  assert.equal(id, 'mpjbdgojfmnkhfmlcggfdephcjcoklce');
});
