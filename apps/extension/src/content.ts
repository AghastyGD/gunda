const MEDIA_SELECTOR = 'video, audio, source';

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
    : null;
}

window.addEventListener('message', (event: MessageEvent<unknown>) => {
  if (event.source !== window || event.origin !== location.origin) {
    return;
  }

  const message = pageMessage(event.data);
  if (message?.kind === 'source') {
    void chrome.runtime
      .sendMessage({
        kind: 'source',
        source_url: message.source_url,
        document: message.document,
      })
      .catch(() => {});
  } else if (message?.kind === 'candidate') {
    void chrome.runtime
      .sendMessage({
        kind: 'candidate',
        url: message.url,
        content_type: null,
      })
      .catch(() => {});
  }
});

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

  void chrome.runtime
    .sendMessage({
      kind: 'candidate',
      url: absolute,
      content_type: element instanceof HTMLSourceElement ? element.type || null : null,
    })
    .catch(() => {});
}

function scan(node: Node): void {
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
  }
}

scan(document);

const observer = new MutationObserver((records) => {
  for (const record of records) {
    for (const node of record.addedNodes) {
      scan(node);
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
document.addEventListener(
  'canplay',
  (event) => {
    if (event.target instanceof Node) {
      scan(event.target);
    }
  },
  true,
);
