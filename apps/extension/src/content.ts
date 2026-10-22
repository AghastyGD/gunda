const MEDIA_SELECTOR = 'video, audio, source';

function pageCandidate(value: unknown): PageCandidate | null {
  if (!value || typeof value !== 'object') {
    return null;
  }
  const candidate = value as Partial<PageCandidate>;
  return candidate.source === 'gunda-page' &&
    candidate.kind === 'candidate' &&
    typeof candidate.url === 'string'
    ? (candidate as PageCandidate)
    : null;
}

window.addEventListener('message', (event: MessageEvent<unknown>) => {
  if (event.source !== window || event.origin !== location.origin) {
    return;
  }

  const candidate = pageCandidate(event.data);
  if (candidate) {
    void chrome.runtime
      .sendMessage({
        kind: 'candidate',
        url: candidate.url,
        content_type: null,
      })
      .catch(() => {});
  }
});

void chrome.runtime.sendMessage({ kind: 'start-page-agent' }).catch(() => {});

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
