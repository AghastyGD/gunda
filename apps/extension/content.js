// Gunda Capture -- what the page declares about itself.
//
// The service worker sees what is fetched over the network, but a page can build
// a media source entirely from data it already has. This script covers that
// case. It reads, it reports, and it never changes the page.

const MEDIA_SELECTOR = 'video, audio, source';

function report(element) {
  const url = element.currentSrc || element.src;

  // Media Source Extensions hand the element a blob URL that means nothing
  // outside the page. What the desktop can download is the manifest the page
  // fetched for it, which the service worker sees on the wire.
  if (!url || /^(blob|data|mediasource):/i.test(url)) {
    return;
  }

  let absolute;

  try {
    absolute = new URL(url, document.baseURI).href;
  } catch {
    return;
  }

  chrome.runtime
    .sendMessage({
      kind: 'candidate',
      url: absolute,
      content_type: element.type || null,
    })
    .catch(() => {
      // The service worker is asleep or the extension was reloaded; the page is
      // not affected either way.
    });
}

function scan(node) {
  if (node instanceof HTMLMediaElement || node instanceof HTMLSourceElement) {
    report(node);

    return;
  }

  if (!(node instanceof Element || node instanceof Document)) {
    return;
  }

  for (const element of node.querySelectorAll(MEDIA_SELECTOR)) {
    report(element);
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

observer.observe(document.documentElement, { childList: true, subtree: true });

// A source is often chosen after the element exists, or after metadata loads.
document.addEventListener('loadedmetadata', (event) => scan(event.target), true);
document.addEventListener('canplay', (event) => scan(event.target), true);
