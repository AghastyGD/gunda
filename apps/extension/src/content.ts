const MEDIA_SELECTOR = 'video, audio, source';
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
  overlay.host.style.left = `${Math.max(8, Math.min(window.innerWidth - 92, rect.right - 82))}px`;
  overlay.host.style.top = `${Math.max(8, rect.top + 8)}px`;
}

function setOverlayStatus(
  button: HTMLButtonElement,
  label: string,
  reset = false,
): void {
  button.textContent = label;
  button.disabled = label !== 'Gunda';
  if (reset) {
    setTimeout(() => {
      if (button.isConnected) {
        button.textContent = 'Gunda';
        button.disabled = false;
      }
    }, 2200);
  }
}

function clearFormats(panel: HTMLElement): void {
  panel.replaceChildren();
  panel.style.display = 'none';
}

async function downloadFormat(
  button: HTMLButtonElement,
  panel: HTMLElement,
  format: MediaFormat,
): Promise<void> {
  setOverlayStatus(button, 'Queued...');
  const response = await sendRuntimeMessage<DownloadResponse>({
    kind: 'download-page',
    format: format.format,
  });
  if (response) {
    clearFormats(panel);
    if (response?.kind === 'queued') {
      setOverlayStatus(button, 'Queued', true);
    } else {
      setOverlayStatus(button, 'Not found', true);
    }
  } else {
    setOverlayStatus(button, 'Try again', true);
  }
}

function showFormats(
  button: HTMLButtonElement,
  panel: HTMLElement,
  formats: MediaFormat[],
): void {
  panel.replaceChildren();
  for (const format of formats) {
    const option = document.createElement('button');
    option.type = 'button';
    option.textContent = format.label;
    option.title = format.details;
    option.style.cssText = [
      'all: initial',
      'box-sizing: border-box',
      'display: block',
      'width: 100%',
      'border: 0',
      'border-radius: 4px',
      'background: transparent',
      'color: #fff',
      'cursor: pointer',
      'font: 600 12px/1.25 system-ui, sans-serif',
      'padding: 8px',
      'text-align: left',
    ].join(';');
    option.addEventListener('mouseenter', () => {
      option.style.background = 'rgba(255,255,255,.12)';
    });
    option.addEventListener('mouseleave', () => {
      option.style.background = 'transparent';
    });
    option.addEventListener('click', (event) => {
      event.preventDefault();
      event.stopPropagation();
      option.disabled = true;
      void downloadFormat(button, panel, format);
    });

    const details = document.createElement('span');
    details.textContent = format.details;
    details.style.cssText =
      'display:block;color:rgba(255,255,255,.65);font:400 10px/1.3 system-ui,sans-serif;margin-top:3px;';
    option.append(details);
    panel.append(option);
  }
  panel.style.display = 'block';
  button.textContent = 'Formats';
  button.disabled = false;
}

async function openFormats(
  button: HTMLButtonElement,
  panel: HTMLElement,
): Promise<void> {
  if (panel.style.display === 'block') {
    clearFormats(panel);
    setOverlayStatus(button, 'Gunda');
    return;
  }

  setOverlayStatus(button, 'Loading...');
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
      showFormats(button, panel, response.formats);
    } else {
      setOverlayStatus(button, 'Not found', true);
    }
  } else {
    setOverlayStatus(button, 'Try again', true);
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
  const panel = document.createElement('div');
  panel.style.cssText = [
    'all: initial',
    'box-sizing: border-box',
    'display: none',
    'position: absolute',
    'top: calc(100% + 6px)',
    'right: 0',
    'width: 230px',
    'max-height: 320px',
    'overflow: auto',
    'pointer-events: auto',
    'border: 1px solid rgba(255,255,255,.28)',
    'border-radius: 7px',
    'background: rgba(12,18,28,.96)',
    'box-shadow: 0 8px 24px rgba(0,0,0,.42)',
    'padding: 5px',
  ].join(';');
  const button = document.createElement('button');
  button.type = 'button';
  button.textContent = 'Gunda';
  button.title = 'Download this video with Gunda';
  button.style.cssText = [
    'all: initial',
    'box-sizing: border-box',
    'pointer-events: auto',
    'border: 1px solid rgba(255,255,255,.35)',
    'border-radius: 6px',
    'background: rgba(12,18,28,.82)',
    'color: #fff',
    'cursor: pointer',
    'font: 600 12px/1 system-ui, sans-serif',
    'padding: 8px 10px',
    'box-shadow: 0 4px 14px rgba(0,0,0,.3)',
    'opacity: .82',
  ].join(';');
  button.addEventListener('mouseenter', () => {
    button.style.opacity = '1';
  });
  button.addEventListener('mouseleave', () => {
    button.style.opacity = '.82';
  });
  button.addEventListener('click', (event) => {
    event.preventDefault();
    event.stopPropagation();
    void openFormats(button, panel);
  });

  const controls = document.createElement('div');
  controls.style.cssText = 'all: initial; position: relative; display: inline-block;';
  controls.append(button, panel);
  shadow.append(controls);
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
