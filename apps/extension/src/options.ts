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
const lastDownloadField = element<HTMLElement>('last-download');
const saveButton = element<HTMLButtonElement>('save');
const clearButton = element<HTMLButtonElement>('clear');

async function load(): Promise<void> {
  const stored = await chrome.storage.local.get(['endpoint', 'token']);
  endpointField.value = String(stored.endpoint ?? '');
  tokenField.value = String(stored.token ?? '');
}

function channelLabel(state: string): string {
  switch (state) {
    case 'connected':
      return 'Connected';
    case 'unpaired':
      return 'Not paired';
    case 'unreachable':
      return 'Gunda is not running';
    case 'closed':
      return 'Connection closed';
    case 'malformed':
      return 'Unexpected reply';
    default:
      return state;
  }
}

function describe(value: unknown): string {
  if (!value || typeof value !== 'object') {
    return 'Not started yet.';
  }

  const status = value as Partial<ChannelStatus>;
  const state = channelLabel(status.state ?? 'unknown');
  const detail = status.detail ? ` · ${status.detail}` : '';
  const refused = status.refused ? `\nLast rejected attempt: ${status.refused}` : '';
  const changed = status.at ? new Date(status.at).toLocaleTimeString() : 'unknown';
  return `${state}${detail}${refused}\nLast change: ${changed}`;
}

const STAT_LABELS: Record<string, string> = {
  accepted: 'Captured',
  duplicate: 'Duplicates',
  refused: 'Refused',
  malformed: 'Unreadable',
  source_too_large: 'Pages too large',
  ignored_by_extension: 'Ignored (extension)',
  ignored_non_http: 'Ignored (not web)',
  browser_download: 'Browser downloads',
  browser_download_prompted: 'Prompts shown',
  browser_download_intercepted: 'Taken over',
  browser_download_kept: 'Left in browser',
  browser_download_cancel_failed: 'Cancel failed',
};

function describeStats(value: unknown): string {
  if (!value || typeof value !== 'object') {
    return 'Nothing reported yet.';
  }
  const entries = Object.entries(value);
  return entries.length
    ? entries
        .map(([kind, count]) => `${STAT_LABELS[kind] ?? kind}: ${String(count)}`)
        .join('\n')
    : 'Nothing reported yet.';
}

const OUTCOME_LABELS: Record<string, string> = {
  intercepted: 'Taken over by Gunda',
  kept: 'Left in the browser',
  ignored_extension: 'Started by another extension',
  ignored_non_http: 'Not a web address',
};

function describeLastDownload(value: unknown): string {
  if (!value || typeof value !== 'object') {
    return 'None seen yet.';
  }

  const download = value as Partial<BrowserDownloadDiagnostic>;
  const parts = [download.filename || 'Unnamed file'];
  const outcome = download.outcome ? OUTCOME_LABELS[download.outcome] : null;
  if (outcome) {
    parts.push(outcome);
  }
  if (download.host) {
    parts.push(download.host);
  }
  if (download.at) {
    parts.push(new Date(download.at).toLocaleTimeString());
  }
  return parts.join(' · ');
}

async function refresh(): Promise<void> {
  const stored = await chrome.storage.session.get(['status', 'stats', 'last_download']);
  statusField.textContent = describe(stored.status);
  statsField.textContent = describeStats(stored.stats);
  lastDownloadField.textContent = describeLastDownload(stored.last_download);
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
  statusField.textContent = 'Saved. Connecting…';
}

async function forget(): Promise<void> {
  await chrome.storage.local.remove(['endpoint', 'token']);
  await chrome.storage.session.clear();
  await chrome.runtime.sendMessage({ kind: 'forget' });
  endpointField.value = '';
  tokenField.value = '';
  statusField.textContent = 'Forgotten. Nothing is reported until you pair again.';
}

saveButton.addEventListener('click', () => void save());
clearButton.addEventListener('click', () => void forget());

void load().then(refresh);
setInterval(() => void refresh(), 2000);
