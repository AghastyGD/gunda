import { execFileSync } from 'node:child_process';
import { cp, rm } from 'node:fs/promises';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = dirname(dirname(fileURLToPath(import.meta.url)));
const dist = join(root, 'dist');

await rm(dist, { force: true, recursive: true });
execFileSync(
  process.execPath,
  [join(root, 'node_modules', 'typescript', 'bin', 'tsc'), '-p', join(root, 'tsconfig.json')],
  { cwd: root, stdio: 'inherit' },
);
await cp(join(root, 'src', 'manifest.json'), join(dist, 'manifest.json'));
await cp(join(root, 'src', 'options.html'), join(dist, 'options.html'));
await cp(join(root, 'src', 'download.html'), join(dist, 'download.html'));
