"use strict";
const MEDIA_SELECTOR = 'video, audio, source';
function pageCandidate(value) {
    if (!value || typeof value !== 'object') {
        return null;
    }
    const candidate = value;
    return candidate.source === 'gunda-page' &&
        candidate.kind === 'candidate' &&
        typeof candidate.url === 'string'
        ? candidate
        : null;
}
window.addEventListener('message', (event) => {
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
            .catch(() => { });
    }
});
void chrome.runtime.sendMessage({ kind: 'start-page-agent' }).catch(() => { });
function reportElement(element) {
    const url = element instanceof HTMLMediaElement ? element.currentSrc || element.src : element.src;
    if (!url || /^(blob|data|mediasource):/i.test(url)) {
        return;
    }
    let absolute;
    try {
        absolute = new URL(url, document.baseURI).href;
    }
    catch {
        return;
    }
    void chrome.runtime
        .sendMessage({
        kind: 'candidate',
        url: absolute,
        content_type: element instanceof HTMLSourceElement ? element.type || null : null,
    })
        .catch(() => { });
}
function scan(node) {
    if (node instanceof HTMLMediaElement || node instanceof HTMLSourceElement) {
        reportElement(node);
        return;
    }
    if (!(node instanceof Element || node instanceof Document)) {
        return;
    }
    for (const element of node.querySelectorAll(MEDIA_SELECTOR)) {
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
function observe() {
    observer.observe(document.documentElement, { childList: true, subtree: true });
}
if (document.documentElement) {
    observe();
}
else {
    window.addEventListener('DOMContentLoaded', observe, { once: true });
}
document.addEventListener('loadedmetadata', (event) => {
    if (event.target instanceof Node) {
        scan(event.target);
    }
}, true);
document.addEventListener('canplay', (event) => {
    if (event.target instanceof Node) {
        scan(event.target);
    }
}, true);
