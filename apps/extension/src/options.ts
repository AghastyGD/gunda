const TOKEN_PATTERN = /^[0-9a-f]{64}$/i;

function element<T extends HTMLElement>(id: string): T {
  const found = document.getElementById(id);
  if (!(found instanceof HTMLElement)) {
    throw new Error(`Missing extension element: ${id}`);
  }
  return found as T;
}

const endpointField = element<HTMLInputElement>('endpoint');
const tokenField = element<HTMLInputElement>('token');
const statusField = element<HTMLElement>('status');
const statsField = element<HTMLElement>('stats');
const saveButton = element<HTMLButtonElement>('save');
const clearButton = element<HTMLButtonElement>('clear');

async function load(): Promise<void> {
  const stored = await chrome.storage.local.get(['endpoint', 'token']);
  endpointField.value = String(stored.endpoint ?? '');
  tokenField.value = String(stored.token ?? '');
}

function describe(value: unknown): string {
  if (!value || typeof value !== 'object') {
    return 'Status: the channel has not been started yet.';
  }

  const status = value as Partial<ChannelStatus>;
  const state = status.state ?? 'unknown';
  const detail = status.detail ? ` (${status.detail})` : '';
  const refused = status.refused ? `\nLast refusal: ${status.refused}` : '';
  const changed = status.at ? new Date(status.at).toLocaleTimeString() : 'unknown';
  return `Status: ${state}${detail}${refused}\nLast change: ${changed}`;
}

function describeStats(value: unknown): string {
  if (!value || typeof value !== 'object') {
    return 'Nothing reported yet.';
  }
  const entries = Object.entries(value);
  return entries.length
    ? entries.map(([kind, count]) => `${kind}: ${String(count)}`).join('\n')
    : 'Nothing reported yet.';
}

async function refresh(): Promise<void> {
  const stored = await chrome.storage.session.get(['status', 'stats']);
  statusField.textContent = describe(stored.status);
  statsField.textContent = describeStats(stored.stats);
}

async function save(): Promise<void> {
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

async function forget(): Promise<void> {
  await chrome.storage.local.remove(['endpoint', 'token']);
  await chrome.storage.session.clear();
  await chrome.runtime.sendMessage({ kind: 'reconnect' });
  endpointField.value = '';
  tokenField.value = '';
  statusField.textContent = 'Forgotten. Nothing will be reported until a pairing is saved.';
}

saveButton.addEventListener('click', () => void save());
clearButton.addEventListener('click', () => void forget());

void load().then(refresh);
setInterval(() => void refresh(), 2000);
