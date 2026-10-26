const MAX_BODY_BYTES = 1536 * 1024;
const YOUTUBE_POLL_MS = 500;
const YOUTUBE_POLL_LIMIT = 30;
const MEDIA_FILE_PATH =
  /\.(mp4|m4v|webm|mkv|mov|flv|avi|wmv|mpeg|mpg|3gp|ogv|mp3|m4a|aac|ogg|oga|opus|flac|wav)$/i;
const MEDIA_SOURCE_HINT =
  /video(?:_versions|_url|Url)|playable_url|playableUrl|browser_native_\w+_url|progressive_urls|hlsManifestUrl|dashManifestUrl|manifest_url|stream_url|playback_url|contentUrl|\.(?:mp4|m4v|webm|mkv|mov|flv|avi|wmv|mpeg|mpg|3gp|ogv|m3u8|mpd|mp3|m4a|aac|ogg|oga|opus|flac|wav)(?:["'\\?])/i;

if (!window.__gundaPageAgent) {
  window.__gundaPageAgent = true;
  installPageAgent();
}

function installPageAgent(): void {
  function site(host: string, domain: string): boolean {
    return host === domain || host.endsWith(`.${domain}`);
  }

  function target(url: string): boolean {
    try {
      const parsed = new URL(url, location.href);
      const host = parsed.hostname.toLowerCase();
      if (site(host, 'instagram.com')) {
        return /graphql|api\/v1\/media\/[^/]+\/info|ajax\/route-definition|__a=1/i.test(
          parsed.pathname + parsed.search,
        );
      }
      if (site(host, 'facebook.com')) {
        return /graphql|api/i.test(parsed.pathname);
      }
      if (site(host, 'youtube.com')) {
        return /youtubei\/v1\/(player|next)/i.test(parsed.pathname);
      }
    } catch {
      return false;
    }
    return false;
  }

  function reportSource(sourceUrl: string, document: unknown): void {
    try {
      const parsed = new URL(sourceUrl, location.href);
      if (!/^https?:$/.test(parsed.protocol)) {
        return;
      }
      window.postMessage(
        {
          source: 'gunda-page',
          kind: 'source',
          source_url: parsed.href,
          document,
        } satisfies PageSource,
        location.origin,
      );
    } catch {
      return;
    }
  }

  function reportMedia(url: string): void {
    try {
      const parsed = new URL(url, location.href);
      if (
        /^https?:$/.test(parsed.protocol) &&
        MEDIA_FILE_PATH.test(parsed.pathname) &&
        !['bytestart', 'byteend', 'range'].some((name) => parsed.searchParams.has(name))
      ) {
        window.postMessage(
          {
            source: 'gunda-page',
            kind: 'candidate',
            url: parsed.href,
          } satisfies PageMedia,
          location.origin,
        );
      }
    } catch {
      return;
    }
  }

  function reportPerformanceMedia(): void {
    if (typeof performance === 'undefined') {
      return;
    }
    for (const entry of performance.getEntriesByType('resource')) {
      reportMedia(entry.name);
    }
  }

  function reportJsonScripts(): void {
    if (typeof document === 'undefined') {
      return;
    }
    for (const script of document.scripts) {
      const type = script.type.toLowerCase();
      const text = script.textContent;
      if (
        !text ||
        text.length > MAX_BODY_BYTES ||
        !(type === 'application/json' || type === 'application/ld+json')
      ) {
        continue;
      }
      const value = jsonBody(text);
      if (value !== null) {
        reportSource(location.href, value);
      }
    }
  }

  async function refreshPageMedia(): Promise<void> {
    reportPerformanceMedia();
    reportJsonScripts();

    if (!location.hostname.endsWith('instagram.com')) {
      return;
    }

    try {
      const url = new URL(location.href);
      url.searchParams.set('__a', '1');
      url.searchParams.set('__d', 'dis');
      const response = await originalFetch(url, { credentials: 'include' });
      await capture(response.url || url.href, response);
    } catch {
      return;
    }
  }

  window.addEventListener('message', (event) => {
    if (event.source !== window || event.origin !== location.origin) {
      return;
    }
    const message = event.data as Partial<PageRefresh> | null;
    if (message?.source === 'gunda-page' && message.kind === 'refresh') {
      void refreshPageMedia();
    }
  });

  async function boundedText(response: Response): Promise<string | null> {
    const length = Number(response.headers.get('content-length'));
    if (Number.isFinite(length) && length > MAX_BODY_BYTES) {
      return null;
    }

    if (!response.body) {
      const text = await response.text();
      return new TextEncoder().encode(text).byteLength > MAX_BODY_BYTES ? null : text;
    }

    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    let bytes = 0;
    let text = '';

    while (true) {
      const { done, value } = await reader.read();
      if (done) {
        break;
      }
      bytes += value.byteLength;
      if (bytes > MAX_BODY_BYTES) {
        await reader.cancel();
        return null;
      }
      text += decoder.decode(value, { stream: true });
    }

    return text + decoder.decode();
  }

  function jsonBody(text: string): unknown {
    const clean = text
      .trim()
      .replace(/^\)\]\}',?\s*/, '')
      .replace(/^for\s*\(;;\);/, '');
    try {
      return JSON.parse(clean) as unknown;
    } catch {
      return null;
    }
  }

  async function capture(url: string, response: Response): Promise<void> {
    const knownSource = target(url);
    const type = response.headers.get('content-type') ?? '';
    if (!/json|text\/plain/i.test(type)) {
      return;
    }

    try {
      const text = await boundedText(response);
      if (!text) {
        return;
      }
      if (!knownSource && !MEDIA_SOURCE_HINT.test(text)) {
        return;
      }
      const value = jsonBody(text);
      if (value !== null) {
        reportSource(url, value);
      }
    } catch {
      return;
    }
  }

  const originalFetch = window.fetch.bind(window);
  window.fetch = async (...args: Parameters<typeof window.fetch>): Promise<Response> => {
    const response = await originalFetch(...args);
    void capture(response.url, response.clone());
    return response;
  };

  const originalOpen = XMLHttpRequest.prototype.open;
  const originalSend = XMLHttpRequest.prototype.send;

  XMLHttpRequest.prototype.open = function (
    this: XMLHttpRequest,
    method: string,
    url: string | URL,
    ...rest: unknown[]
  ): void {
    this.__gundaUrl = String(url);
    Reflect.apply(originalOpen, this, [method, url, ...rest]);
  } as typeof XMLHttpRequest.prototype.open;

  XMLHttpRequest.prototype.send = function (
    this: XMLHttpRequest,
    ...args: Parameters<typeof originalSend>
  ): void {
    this.addEventListener(
      'load',
      () => {
        const type = this.getResponseHeader('content-type') ?? '';
        const requestUrl = this.__gundaUrl ?? '';
        if (!/json|text\/plain/i.test(type) || !target(requestUrl)) {
          return;
        }
        try {
          const text =
            this.responseType === '' || this.responseType === 'text'
              ? this.responseText
              : this.responseType === 'json'
                ? JSON.stringify(this.response)
                : null;
          if (!text || text.length > MAX_BODY_BYTES) {
            return;
          }
          const value = jsonBody(text);
          if (value !== null) {
            reportSource(new URL(requestUrl, location.href).href, value);
          }
        } catch {
          return;
        }
      },
      { once: true },
    );
    Reflect.apply(originalSend, this, args);
  } as typeof XMLHttpRequest.prototype.send;

  if (location.hostname.endsWith('youtube.com')) {
    let youtubeReports = 0;
    let youtubePolls = 0;
    const youtubeTimer = setInterval(() => {
      youtubePolls += 1;
      const player = (window as Window & { ytInitialPlayerResponse?: unknown })
        .ytInitialPlayerResponse;

      if (player && typeof player === 'object') {
        youtubeReports += 1;
        reportSource(location.href, player);
        if (youtubeReports >= 2) {
          clearInterval(youtubeTimer);
        }
      } else if (youtubePolls >= YOUTUBE_POLL_LIMIT) {
        clearInterval(youtubeTimer);
      }
    }, YOUTUBE_POLL_MS);
  }

  reportPerformanceMedia();
  if (typeof document !== 'undefined') {
    if (document.readyState === 'loading') {
      document.addEventListener('DOMContentLoaded', reportJsonScripts, { once: true });
    } else {
      reportJsonScripts();
    }
  }
  if (typeof PerformanceObserver !== 'undefined') {
    const observer = new PerformanceObserver((list) => {
      for (const entry of list.getEntries()) {
        reportMedia(entry.name);
      }
    });
    observer.observe({ type: 'resource', buffered: true });
  }
}
