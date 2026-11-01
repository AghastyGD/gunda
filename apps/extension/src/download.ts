const parameters = new URLSearchParams(location.search);
let downloadId = Number(parameters.get('id'));
const captureParameter = parameters.get('capture_id');
const captureId = captureParameter === null ? Number.NaN : Number(captureParameter);
const browserPrompt = parameters.get('browser_prompt') === '1';
const browserDownloadValue = Number(parameters.get('download_id'));
const browserDownload =
  Number.isSafeInteger(browserDownloadValue) && browserDownloadValue >= 0
    ? browserDownloadValue
    : null;
const browserUrl = parameters.get('url') ?? '';
const browserContentType = parameters.get('content_type') ?? '';
const browserSizeValue = Number(parameters.get('size_bytes'));
const browserSize =
  Number.isSafeInteger(browserSizeValue) && browserSizeValue > 0
    ? browserSizeValue
    : null;
const browserPageUrl = parameters.get('page_url') ?? '';
const browserPageTitle = parameters.get('page_title') ?? '';

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

function downloadTypeLabel(contentType: string): string {
  const essence = contentType.split(';')[0]?.trim().toLowerCase() ?? '';
  if (!essence) {
    return 'File';
  }
  if (essence.startsWith('video/')) {
    return 'Video';
  }
  if (essence.startsWith('audio/')) {
    return 'Audio';
  }
  if (essence.startsWith('image/')) {
    return 'Image';
  }
  if (essence === 'application/pdf') {
    return 'PDF document';
  }
  if (essence.startsWith('text/')) {
    return 'Text file';
  }
  if (/zip|compressed|tar|rar|7z/i.test(essence)) {
    return 'Archive';
  }
  if (/(word|document|opendocument\.text)/i.test(essence)) {
    return 'Document';
  }
  if (/(sheet|excel|spreadsheet)/i.test(essence)) {
    return 'Spreadsheet';
  }
  if (/(presentation|powerpoint)/i.test(essence)) {
    return 'Presentation';
  }
  return 'File';
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
      return 'This file needs a sign-in.';
    case 'remote_rejected':
      return 'The site refused the download.';
    case 'invalid_response':
      return 'The site sent an unexpected response.';
    case 'unsupported_resource':
      return 'Gunda cannot download this file type.';
    case 'permission_denied':
      return 'Gunda cannot save the file in the selected folder.';
    case 'disk_full':
      return 'There is not enough disk space.';
    case 'integrity':
      return 'The downloaded file could not be verified.';
    default:
      return 'The download stopped unexpectedly.';
  }
}

function browserStartFailureLabel(reason: string | undefined): string {
  if (!reason) {
    return 'Gunda is not available.';
  }
  switch (reason) {
    case 'connection_unavailable':
      return 'Gunda is not connected. The browser download was restored.';
    case 'invalid_download':
      return 'This download is no longer available. Close this window and try again.';
    case 'invalid_destination':
      return 'Choose a folder and enter a file name.';
    default:
      return 'Gunda could not start this download. The browser download was restored.';
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
    stateField.textContent = 'Waiting for Gunda…';
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
cancelButton.addEventListener('click', () => {
  if (!browserPrompt) {
    window.close();
    return;
  }
  void restoreInBrowser();
});

async function restoreInBrowser(): Promise<void> {
  if (browserDownload === null) {
    window.close();
    return;
  }

  cancelButton.disabled = true;
  startButton.disabled = true;
  setupFailure.textContent = '';
  try {
    const response = (await chrome.runtime.sendMessage({
      kind: 'cancel-browser-download',
      download_id: browserDownload,
      url: browserUrl,
      content_type: browserContentType || null,
      size_bytes: browserSize,
      page_url: browserPageUrl,
      page_title: browserPageTitle || null,
      filename: filenameInput.value.trim() || parameters.get('filename') || 'download',
      directory: directoryInput.value.trim(),
    })) as { restored?: boolean } | undefined;
    if (response?.restored === false) {
      setupFailure.textContent =
        'Chrome could not restart this download. Try again from the page.';
      cancelButton.disabled = false;
      startButton.disabled = false;
      return;
    }
  } catch {
    // Nothing else can be done; the browser item stays cancelled.
  }
  window.close();
}

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

  if (browserPrompt) {
    if (!browserUrl || !browserPageUrl || browserDownload === null) {
      setupFailure.textContent = 'This download is no longer available.';
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
        kind: 'start-browser-download',
        download_id: browserDownload,
        url: browserUrl,
        content_type: browserContentType || null,
        size_bytes: browserSize,
        page_url: browserPageUrl,
        page_title: browserPageTitle || null,
        directory,
        filename,
      })) as DownloadResponse | undefined;
      if (response?.kind === 'queued') {
        beginProgress(response.id);
      } else {
        setupFailure.textContent = browserStartFailureLabel(response?.reason);
        startButton.disabled = response?.reason === 'invalid_download';
      }
    } catch {
      setupFailure.textContent = 'Gunda is not available.';
      startButton.disabled = false;
    }
    return;
  }

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
} else if (browserPrompt) {
  const filename = parameters.get('filename') ?? 'download';
  formatLabel.textContent = filename;
  formatDetails.textContent = [
    downloadTypeLabel(browserContentType),
    browserSize === null ? 'Size unknown' : downloadFormatBytes(browserSize),
  ].join(' · ');
  filenameInput.value = filename;
  directoryInput.value = parameters.get('directory') ?? '';
  if (!browserUrl || !browserPageUrl || browserDownload === null) {
    setupFailure.textContent = 'This download is no longer available.';
    startButton.disabled = true;
  }
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
