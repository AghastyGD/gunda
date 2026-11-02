const MEDIA_SELECTOR = 'video, audio, source';
const OVERLAY_LABEL = 'Download';
const DOWNLOAD_ICON =
  '<svg width="12" height="12" viewBox="0 0 16 16" fill="none" aria-hidden="true"><path d="M8 2.5v6.5m0 0 2.75-2.75M8 9 5.25 6.25M3 12.25h10" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/></svg>';
const OVERLAY_CSS = `
  .controls { all: initial; position: relative; display: inline-block; }
  .button {
    all: unset;
    box-sizing: border-box;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    gap: 6px;
    min-width: 96px;
    padding: 9px 12px;
    border: 1px solid rgba(55, 67, 82, .9);
    border-radius: 6px;
    background: rgba(19, 25, 34, .92);
    color: #e5ebf2;
    font: 600 12px/1 "Segoe UI Variable Text", "Segoe UI", system-ui, sans-serif;
    cursor: pointer;
    box-shadow: 0 2px 10px rgba(0, 0, 0, .35);
    opacity: .9;
    transition: background-color .12s ease, border-color .12s ease, opacity .12s ease;
  }
  .button:hover { background: rgba(35, 44, 56, .96); border-color: #4a5a6c; opacity: 1; }
  .button:focus-visible { outline: 2px solid #519df0; outline-offset: 2px; opacity: 1; }
  .button:disabled { cursor: default; opacity: .62; }
  .button svg { color: #519df0; flex: none; }
  .panel {
    all: initial;
    box-sizing: border-box;
    display: none;
    position: absolute;
    top: calc(100% + 6px);
    right: 0;
    width: 236px;
    max-height: 320px;
    overflow: auto;
    padding: 4px;
    border: 1px solid #374352;
    border-radius: 6px;
    background: rgba(19, 25, 34, .98);
    color: #e5ebf2;
    font-family: "Segoe UI Variable Text", "Segoe UI", system-ui, sans-serif;
    box-shadow: 0 10px 28px rgba(0, 0, 0, .45);
    pointer-events: auto;
    scrollbar-width: thin;
    scrollbar-color: #374352 transparent;
  }
  .panel::-webkit-scrollbar { width: 8px; }
  .panel::-webkit-scrollbar-thumb { background: #374352; border-radius: 4px; }
  .group {
    padding: 8px 8px 3px;
    color: #9ba8b8;
    font: 600 10px/1.3 "Segoe UI Variable Text", "Segoe UI", system-ui, sans-serif;
  }
  .option {
    all: unset;
    box-sizing: border-box;
    display: block;
    width: 100%;
    padding: 8px;
    border-radius: 4px;
    color: #e5ebf2;
    font: 600 12px/1.3 "Segoe UI Variable Text", "Segoe UI", system-ui, sans-serif;
    text-align: left;
    cursor: pointer;
  }
  .option:hover { background: #232c38; }
  .option:focus-visible { outline: 1px solid #519df0; outline-offset: -1px; }
  .option:disabled { cursor: default; opacity: .55; }
  .details {
    display: block;
    margin-top: 2px;
    color: #9ba8b8;
    font: 400 10px/1.35 "Segoe UI Variable Text", "Segoe UI", system-ui, sans-serif;
  }
`;
const overlays = new WeakMap<
  HTMLVideoElement,
  { host: HTMLElement; button: HTMLButtonElement; panel: HTMLElement }
>();

function extensionAlive(): boolean {
  try {
    return Boolean(chrome.runtime?.id);
  } catch {
    return false;
  }
}

async function sendRuntimeMessage<Response>(message: RuntimeMessage): Promise<Response | undefined> {
  if (!extensionAlive()) {
    removeExtensionUi();
    return undefined;
  }
  try {
    return (await chrome.runtime.sendMessage(message)) as Response | undefined;
  } catch {
    removeExtensionUi();
    return undefined;
  }
}

function removeExtensionUi(): void {
  for (const host of document.querySelectorAll<HTMLElement>('[data-gunda-overlay]')) {
    host.remove();
  }
}

function pageMessage(value: unknown): PageMessage | null {
  if (!value || typeof value !== 'object') {
    return null;
  }
  const message = value as Partial<PageMessage>;
  if (message.source !== 'gunda-page') {
    return null;
  }
  if (
    message.kind === 'source' &&
    typeof message.source_url === 'string' &&
    message.document !== null &&
    typeof message.document === 'object'
  ) {
    return message as PageSource;
  }
  return message.kind === 'candidate' && typeof message.url === 'string'
    ? (message as PageMedia)
    : message.kind === 'refresh'
      ? (message as PageRefresh)
      : null;
}

window.addEventListener('message', (event: MessageEvent<unknown>) => {
  if (event.source !== window || event.origin !== location.origin) {
    return;
  }

  const message = pageMessage(event.data);
  if (message?.kind === 'source') {
    void sendRuntimeMessage({
      kind: 'source',
      source_url: message.source_url,
      document: message.document,
    });
  } else if (message?.kind === 'candidate') {
    void sendRuntimeMessage({
      kind: 'candidate',
      url: message.url,
      content_type: null,
      size_bytes: null,
    });
  }
});

function positionOverlay(video: HTMLVideoElement): void {
  const overlay = overlays.get(video);
  if (!overlay) {
    return;
  }
  if (!video.isConnected) {
    overlay.host.remove();
    overlays.delete(video);
    return;
  }

  const rect = video.getBoundingClientRect();
  if (
    rect.width < 180 ||
    rect.height < 100 ||
    rect.bottom <= 8 ||
    rect.right <= 8 ||
    rect.top >= window.innerHeight ||
    rect.left >= window.innerWidth
  ) {
    overlay.host.style.display = 'none';
    return;
  }

  overlay.host.style.display = 'block';
  overlay.host.style.right = `${Math.max(8, window.innerWidth - rect.right + 8)}px`;
  overlay.host.style.top = `${Math.max(8, rect.top + 8)}px`;
}

function setButtonLabel(button: HTMLButtonElement, label: string): void {
  const span = button.querySelector<HTMLSpanElement>('.label');
  if (span) {
    span.textContent = label;
  } else {
    button.textContent = label;
  }
}

function setOverlayStatus(
  button: HTMLButtonElement,
  label: string,
  options: { busy?: boolean; resetAfter?: number } = {},
): void {
  setButtonLabel(button, label);
  button.disabled = options.busy === true;
  if (options.resetAfter) {
    window.setTimeout(() => {
      if (button.isConnected && button.textContent === label) {
        setButtonLabel(button, OVERLAY_LABEL);
        button.disabled = false;
      }
    }, options.resetAfter);
  }
}

function clearFormats(panel: HTMLElement): void {
  panel.replaceChildren();
  panel.style.display = 'none';
}

async function openDownloadStart(
  button: HTMLButtonElement,
  panel: HTMLElement,
  captureId: number,
  format: MediaFormat,
  directory: string | null,
  filename: string | null,
): Promise<void> {
  const opened = await sendRuntimeMessage<{ opened?: boolean }>({
    kind: 'open-download',
    capture_id: captureId,
    format: format.format,
    label: format.group ? `${format.group}: ${format.label}` : format.label,
    details: format.details,
    directory,
    filename,
  });
  if (opened !== undefined) {
    clearFormats(panel);
    button.setAttribute('aria-expanded', 'false');
    setOverlayStatus(button, OVERLAY_LABEL);
  } else {
    clearFormats(panel);
    button.setAttribute('aria-expanded', 'false');
    setOverlayStatus(button, 'Try again', { resetAfter: 3000 });
  }
}

function showFormats(
  button: HTMLButtonElement,
  panel: HTMLElement,
  captureId: number,
  formats: MediaFormat[],
  directory: string | null,
  filename: string | null,
): void {
  panel.replaceChildren();

  const groups = new Map<string | null, MediaFormat[]>();
  for (const format of formats) {
    const group = format.group ?? null;
    const entries = groups.get(group);
    if (entries) {
      entries.push(format);
    } else {
      groups.set(group, [format]);
    }
  }

  for (const [group, entries] of groups) {
    if (group) {
      const heading = document.createElement('div');
      heading.className = 'group';
      heading.textContent = group;
      panel.append(heading);
    }

    for (const format of entries) {
      panel.append(
        formatOption(button, panel, captureId, format, directory, filename),
      );
    }
  }

  panel.style.display = 'block';
  button.disabled = false;
  button.setAttribute('aria-expanded', 'true');
  setButtonLabel(button, OVERLAY_LABEL);
}

function formatOption(
  button: HTMLButtonElement,
  panel: HTMLElement,
  captureId: number,
  format: MediaFormat,
  directory: string | null,
  filename: string | null,
): HTMLButtonElement {
  const option = document.createElement('button');
  option.type = 'button';
  option.className = 'option';
  option.textContent = format.label;
  option.title = format.details;
  option.addEventListener('click', (event) => {
    event.preventDefault();
    event.stopPropagation();
    option.disabled = true;
    void openDownloadStart(
      button,
      panel,
      captureId,
      format,
      directory,
      filename,
    );
  });

  const details = document.createElement('span');
  details.className = 'details';
  details.textContent = format.details;
  option.append(details);

  return option;
}

async function openFormats(
  button: HTMLButtonElement,
  panel: HTMLElement,
): Promise<void> {
  if (panel.style.display === 'block') {
    clearFormats(panel);
    button.setAttribute('aria-expanded', 'false');
    setOverlayStatus(button, OVERLAY_LABEL);
    return;
  }

  setOverlayStatus(button, 'Finding formats', { busy: true });
  button.setAttribute('aria-expanded', 'false');
  clearFormats(panel);
  window.postMessage(
    { source: 'gunda-page', kind: 'refresh' } satisfies PageRefresh,
    location.origin,
  );
  await new Promise((resolve) => setTimeout(resolve, 1500));

  const response = await sendRuntimeMessage<FormatResponse>({
    kind: 'list-formats',
  });
  if (response) {
    if (response?.kind === 'formats' && response.formats.length > 0) {
      showFormats(
        button,
        panel,
        response.capture_id,
        response.formats,
        response.directory,
        response.filename,
      );
    } else {
      setOverlayStatus(button, 'No formats', { resetAfter: 2600 });
    }
  } else {
    setOverlayStatus(button, 'Try again', { resetAfter: 3000 });
  }
}

function addOverlay(video: HTMLVideoElement): void {
  if (!extensionAlive()) {
    removeExtensionUi();
    return;
  }
  if (overlays.has(video)) {
    positionOverlay(video);
    return;
  }

  const host = document.createElement('div');
  host.dataset.gundaOverlay = '';
  host.style.cssText =
    'all: initial; position: fixed; z-index: 2147483647; display: none; pointer-events: none;';
  const shadow = host.attachShadow({ mode: 'open' });
  const style = document.createElement('style');
  style.textContent = OVERLAY_CSS;
  const panel = document.createElement('div');
  panel.className = 'panel';
  panel.setAttribute('role', 'group');
  panel.setAttribute('aria-label', 'Available formats');
  const button = document.createElement('button');
  button.type = 'button';
  button.className = 'button';
  button.title = 'Download this video with Gunda';
  button.setAttribute('aria-haspopup', 'true');
  button.setAttribute('aria-expanded', 'false');
  button.innerHTML = `${DOWNLOAD_ICON}<span class="label">${OVERLAY_LABEL}</span>`;
  button.addEventListener('click', (event) => {
    event.preventDefault();
    event.stopPropagation();
    void openFormats(button, panel);
  });

  const controls = document.createElement('div');
  controls.className = 'controls';
  controls.append(button, panel);
  shadow.append(style, controls);
  document.documentElement.append(host);
  overlays.set(video, { host, button, panel });
  positionOverlay(video);
}

function refreshOverlays(): void {
  if (!extensionAlive()) {
    removeExtensionUi();
    return;
  }
  for (const video of document.querySelectorAll('video')) {
    if (video instanceof HTMLVideoElement) {
      addOverlay(video);
    }
  }
}

function removeOverlays(node: Node): void {
  const videos =
    node instanceof HTMLVideoElement
      ? [node]
      : node instanceof Element
        ? Array.from(node.querySelectorAll('video'))
        : [];

  for (const video of videos) {
    overlays.get(video)?.host.remove();
    overlays.delete(video);
  }
}

function reportElement(element: HTMLMediaElement | HTMLSourceElement): void {
  const url =
    element instanceof HTMLMediaElement ? element.currentSrc || element.src : element.src;
  if (!url || /^(blob|data|mediasource):/i.test(url)) {
    return;
  }

  let absolute: string;
  try {
    absolute = new URL(url, document.baseURI).href;
  } catch {
    return;
  }

  void sendRuntimeMessage({
    kind: 'candidate',
    url: absolute,
    content_type: element instanceof HTMLSourceElement ? element.type || null : null,
    size_bytes: null,
  });
}

function scan(node: Node): void {
  if (node instanceof HTMLVideoElement) {
    addOverlay(node);
  }

  if (node instanceof HTMLMediaElement || node instanceof HTMLSourceElement) {
    reportElement(node);
    return;
  }

  if (!(node instanceof Element || node instanceof Document)) {
    return;
  }

  for (const element of node.querySelectorAll<HTMLMediaElement | HTMLSourceElement>(
    MEDIA_SELECTOR,
  )) {
    reportElement(element);
    if (element instanceof HTMLVideoElement) {
      addOverlay(element);
    }
  }
}

scan(document);

const observer = new MutationObserver((records) => {
  for (const record of records) {
    for (const node of record.addedNodes) {
      scan(node);
    }
    for (const node of record.removedNodes) {
      removeOverlays(node);
    }
  }
});

function observe(): void {
  observer.observe(document.documentElement, { childList: true, subtree: true });
}

if (document.documentElement) {
  observe();
} else {
  window.addEventListener('DOMContentLoaded', observe, { once: true });
}

document.addEventListener(
  'loadedmetadata',
  (event) => {
    if (event.target instanceof Node) {
      scan(event.target);
    }
  },
  true,
);

window.addEventListener('scroll', refreshOverlays, true);
window.addEventListener('resize', refreshOverlays);
refreshOverlays();
document.addEventListener(
  'canplay',
  (event) => {
    if (event.target instanceof Node) {
      scan(event.target);
    }
  },
  true,
);
