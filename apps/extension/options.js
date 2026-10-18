// Gunda Capture -- pairing settings and channel status.

const endpointField = document.getElementById('endpoint');
const tokenField = document.getElementById('token');
const statusField = document.getElementById('status');
const statsField = document.getElementById('stats');

const TOKEN_PATTERN = /^[0-9a-f]{64}$/i;

async function load() {
  const stored = await chrome.storage.local.get(['endpoint', 'token']);

  endpointField.value = stored.endpoint ?? '';
  tokenField.value = stored.token ?? '';
}

function describe(status) {
  if (!status) {
    return 'Status: the channel has not been started yet.';
  }

  const state = status.state ?? 'unknown';
  const detail = status.detail ? ` (${status.detail})` : '';
  const refused = status.refused ? `\nLast refusal: ${status.refused}` : '';

  return `Status: ${state}${detail}${refused}\nLast change: ${new Date(status.at).toLocaleTimeString()}`;
}

function describeStats(stats) {
  if (!stats || Object.keys(stats).length === 0) {
    return 'Nothing reported yet.';
  }

  return Object.entries(stats)
    .map(([kind, count]) => `${kind}: ${count}`)
    .join('\n');
}

async function refresh() {
  const stored = await chrome.storage.session.get(['status', 'stats']);

  statusField.textContent = describe(stored.status);
  statsField.textContent = describeStats(stored.stats);
}

async function save() {
  const endpoint = endpointField.value.trim();
  const token = tokenField.value.trim();

  if (!/^wss?:\/\//i.test(endpoint)) {
    statusField.textContent = 'The endpoint must start with ws:// or wss://.';

    return;
  }

  if (!TOKEN_PATTERN.test(token)) {
    statusField.textContent = 'The token must be exactly 64 hexadecimal characters.';

    return;
  }

  await chrome.storage.local.set({ endpoint, token });
  await chrome.runtime.sendMessage({ kind: 'reconnect' });

  statusField.textContent = 'Saved. Connecting...';
}

async function forget() {
  await chrome.storage.local.remove(['endpoint', 'token']);
  await chrome.storage.session.clear();
  await chrome.runtime.sendMessage({ kind: 'reconnect' });

  endpointField.value = '';
  tokenField.value = '';

  statusField.textContent = 'Forgotten. Nothing will be reported until a pairing is saved.';
}

document.getElementById('save').addEventListener('click', save);
document.getElementById('clear').addEventListener('click', forget);

load().then(refresh);
setInterval(refresh, 2000);
