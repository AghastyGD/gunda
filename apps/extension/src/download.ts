const parameters = new URLSearchParams(location.search);
let downloadId = Number(parameters.get('id'));
const captureParameter = parameters.get('capture_id');
const captureId = captureParameter === null ? Number.NaN : Number(captureParameter);

function downloadElement<T extends HTMLElement>(id: string): T {
  const found = document.getElementById(id);
  if (!(found instanceof HTMLElement)) {
    throw new Error(`Missing download element: ${id}`);
  }
  return found as T;
}

const nameField = downloadElement<HTMLElement>('name');
const stateField = downloadElement<HTMLElement>('state');
const writtenField = downloadElement<HTMLElement>('written');
const totalField = downloadElement<HTMLElement>('total');
const failureField = downloadElement<HTMLElement>('failure');
const bar = downloadElement<HTMLElement>('bar');
const closeButton = downloadElement<HTMLButtonElement>('close');
const setup = downloadElement<HTMLElement>('setup');
const progress = downloadElement<HTMLElement>('progress');
const formatLabel = downloadElement<HTMLElement>('format-label');
const formatDetails = downloadElement<HTMLElement>('format-details');
const filenameInput = downloadElement<HTMLInputElement>('filename');
const directoryInput = downloadElement<HTMLInputElement>('directory');
const chooseButton = downloadElement<HTMLButtonElement>('choose');
const startButton = downloadElement<HTMLButtonElement>('start');
const cancelButton = downloadElement<HTMLButtonElement>('cancel');
const setupFailure = downloadElement<HTMLElement>('setup-failure');

function downloadFormatBytes(value: number): string {
  if (!Number.isFinite(value) || value < 0) {
    return 'Unknown';
  }
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
  let amount = value;
  let unit = 0;
  while (amount >= 1024 && unit < units.length - 1) {
    amount /= 1024;
    unit += 1;
  }
  return `${amount.toFixed(unit === 0 ? 0 : 1)} ${units[unit]}`;
}

function downloadStateLabel(state: string): string {
  switch (state) {
    case 'queued':
      return 'Waiting to start';
    case 'inspecting':
      return 'Reading media information';
    case 'downloading':
      return 'Downloading';
    case 'finalizing':
      return 'Finishing file';
    case 'paused':
      return 'Paused';
    case 'completed':
      return 'Download complete';
    case 'failed':
      return 'Download failed';
    case 'cancelled':
      return 'Download cancelled';
    case 'interrupted':
      return 'Download interrupted';
    default:
      return state;
  }
}

function downloadFailureLabel(failure: string): string {
  switch (failure) {
    case 'network':
      return 'Connection problem.';
    case 'authentication':
      return 'The server requires authorization.';
    case 'remote_rejected':
      return 'The server refused the download.';
    case 'invalid_response':
      return 'The server returned an unexpected response.';
    case 'unsupported_resource':
      return 'This media type is not supported.';
    case 'permission_denied':
      return 'Gunda cannot save the file in the selected folder.';
    case 'disk_full':
      return 'There is not enough disk space.';
    case 'integrity':
      return 'The downloaded media could not be verified.';
    default:
      return 'The download stopped unexpectedly.';
  }
}

async function requestDownloadStatusView(): Promise<DownloadStatusResponse | null> {
  try {
    return (await chrome.runtime.sendMessage({
      kind: 'download-status',
      id: downloadId,
    })) as DownloadStatusResponse | null;
  } catch {
    stateField.textContent = 'Reload the page after reloading the extension.';
    return null;
  }
}

let downloadTimer: ReturnType<typeof setInterval> | null = null;

async function refreshDownload(): Promise<void> {
  const response = await requestDownloadStatusView();
  if (!response) {
    return;
  }
  if (response.kind !== 'status') {
    stateField.textContent = 'Waiting for Gunda daemon...';
    return;
  }

  const status = response.status;
  nameField.textContent = status.name;
  nameField.title = status.name;
  stateField.textContent = downloadStateLabel(status.state);
  writtenField.textContent = downloadFormatBytes(status.written);
  totalField.textContent =
    status.total === null ? 'Size unknown' : downloadFormatBytes(status.total);
  failureField.textContent = status.failure ? downloadFailureLabel(status.failure) : '';

  if (status.total !== null && status.total > 0) {
    const percent = Math.min(100, (status.written / status.total) * 100);
    bar.style.width = `${percent.toFixed(1)}%`;
  } else if (status.state === 'completed') {
    bar.style.width = '100%';
  }

  if (
    status.state === 'completed' ||
    status.state === 'failed' ||
    status.state === 'cancelled'
  ) {
    if (downloadTimer) {
      clearInterval(downloadTimer);
      downloadTimer = null;
    }
  }
}

function beginProgress(id: number): void {
  downloadId = id;
  setup.style.display = 'none';
  progress.style.display = 'block';
  void refreshDownload();
  downloadTimer = setInterval(() => void refreshDownload(), 750);
}

closeButton.addEventListener('click', () => window.close());
cancelButton.addEventListener('click', () => window.close());

chooseButton.addEventListener('click', async () => {
  chooseButton.disabled = true;
  try {
    const response = (await chrome.runtime.sendMessage({
      kind: 'choose-folder',
    })) as { path?: string; error?: string } | undefined;
    if (response?.path) {
      directoryInput.value = response.path;
    } else {
      setupFailure.textContent = response?.error ?? 'No folder was selected.';
    }
  } catch {
    setupFailure.textContent = 'The folder picker is unavailable.';
  } finally {
    chooseButton.disabled = false;
  }
});

startButton.addEventListener('click', async () => {
  const pageUrl = parameters.get('page_url') ?? '';
  const format = parameters.get('format') ?? '';
  const directory = directoryInput.value.trim();
  const filename = filenameInput.value.trim();
  if (!pageUrl || !Number.isSafeInteger(captureId) || captureId < 0) {
    setupFailure.textContent = 'This format is no longer available. Choose it again.';
    return;
  }
  if (!directory || !filename) {
    setupFailure.textContent = 'Choose a folder and enter a file name.';
    return;
  }

  startButton.disabled = true;
  setupFailure.textContent = '';
  try {
    const response = (await chrome.runtime.sendMessage({
      kind: 'start-download',
      page_url: pageUrl,
      capture_id: captureId,
      format,
      directory,
      filename,
    })) as DownloadResponse | undefined;
    if (response?.kind === 'queued') {
      beginProgress(response.id);
    } else {
      setupFailure.textContent = 'Gunda could not start this download.';
      startButton.disabled = false;
    }
  } catch {
    setupFailure.textContent = 'Gunda is not available.';
    startButton.disabled = false;
  }
});

if (Number.isSafeInteger(downloadId) && downloadId > 0) {
  beginProgress(downloadId);
} else {
  formatLabel.textContent = parameters.get('label') ?? 'Selected format';
  formatDetails.textContent = parameters.get('details') ?? '';
  filenameInput.value = parameters.get('filename') ?? 'download.mp4';
  directoryInput.value = parameters.get('directory') ?? '';
  if (!parameters.get('page_url') || !Number.isSafeInteger(captureId) || captureId < 0) {
    setupFailure.textContent = 'This format is no longer available. Close this window and choose it again.';
    startButton.disabled = true;
  }
}
