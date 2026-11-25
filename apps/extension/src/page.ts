const MAX_BODY_BYTES = 1536 * 1024;
const MAX_SOCIAL_BODY_BYTES = 12 * 1024 * 1024;
const MAX_SOCIAL_NODES = 500_000;
const MAX_SOCIAL_ITEMS = 64;
const MAX_SOCIAL_SCALAR_CHARS = 4096;
const YOUTUBE_POLL_MS = 500;
const YOUTUBE_POLL_LIMIT = 30;
const MEDIA_FILE_PATH =
  /\.(mp4|m4v|webm|mkv|mov|flv|avi|wmv|mpeg|mpg|3gp|ogv|mp3|m4a|aac|ogg|oga|opus|flac|wav)$/i;
const MEDIA_SOURCE_HINT =
  /video(?:_versions|_url|Url)|playable_url|playableUrl|browser_native_\w+_url|progressive_urls|hlsManifestUrl|dashManifestUrl|manifest_url|stream_url|playback_url|contentUrl|hd_src|sd_src|\.(?:mp4|m4v|webm|mkv|mov|flv|avi|wmv|mpeg|mpg|3gp|ogv|m3u8|mpd|mp3|m4a|aac|ogg|oga|opus|flac|wav)(?:["'\\?])/i;
// Fields that carry media URLs on Instagram and Facebook. Objects owning one are
// the only part of those sites' page data that leaves the page.
const SOCIAL_MEDIA_FIELDS = new Set([
  'video_versions',
  'video_resources',
  'video_url',
  'browser_native_hd_url',
  'browser_native_sd_url',
  'playable_url',
  'playable_url_quality_hd',
  'hd_src',
  'sd_src',
  'progressive_urls',
  'playback_url',
]);
const SOCIAL_CONTEXT_FIELDS = new Set(['caption', 'edge_media_to_caption', 'title']);
const SOCIAL_ID_FIELDS = ['code', 'shortcode', 'pk', 'video_id', 'videoId', 'id'];
const FACEBOOK_TEXT_FIELDS = ['title', 'message', 'savable_description'];
const MAX_SOCIAL_TITLE_CHARS = 200;

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
        return /graphql|api\/v1\/|ajax\/route-definition|__a=1/i.test(
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

  function socialPage(): boolean {
    const host = location.hostname.toLowerCase();
    return site(host, 'instagram.com') || site(host, 'facebook.com');
  }

  function bodyLimit(): number {
    return socialPage() ? MAX_SOCIAL_BODY_BYTES : MAX_BODY_BYTES;
  }

  // Instagram answers its API with text/javascript and Facebook with text/html,
  // so known endpoints accept any textual body and let JSON parsing decide.
  function textual(type: string, knownSource: boolean): boolean {
    if (/json|text\/plain/i.test(type)) {
      return true;
    }
    return knownSource && (type === '' || /javascript|text\/html/i.test(type));
  }

  function socialIds(object: Record<string, unknown>): Record<string, unknown> | null {
    const ids: Record<string, unknown> = {};
    for (const name of SOCIAL_ID_FIELDS) {
      const field = object[name];
      if (
        (typeof field === 'string' && field.length > 0 && field.length <= 64) ||
        typeof field === 'number'
      ) {
        ids[name] = field;
      }
    }
    return Object.keys(ids).length > 0 ? ids : null;
  }

  // Facebook keeps a post's text beside or above the video, not inside it.
  function facebookText(object: Record<string, unknown>): string | null {
    if (!site(location.hostname.toLowerCase(), 'facebook.com')) {
      return null;
    }
    for (const name of FACEBOOK_TEXT_FIELDS) {
      const field = object[name];
      const text =
        typeof field === 'string'
          ? field
          : field && typeof field === 'object'
            ? (field as { text?: unknown }).text
            : null;
      if (typeof text === 'string' && text.trim()) {
        return text.trim().slice(0, MAX_SOCIAL_TITLE_CHARS);
      }
    }
    return null;
  }

  // Keeps only objects that own a media field, with their scalar fields and
  // caption, so private page data on social sites never leaves the page. Media
  // nested below its post keeps the nearest enclosing identifiers and text.
  function socialMedia(value: unknown): unknown {
    type Context = { ids: Record<string, unknown> | null; title: string | null };
    const items: Record<string, unknown>[] = [];
    const pending: [unknown, Context][] = [[value, { ids: null, title: null }]];
    let nodes = 0;

    while (pending.length > 0 && nodes < MAX_SOCIAL_NODES && items.length < MAX_SOCIAL_ITEMS) {
      const entry = pending.pop();
      if (!entry) {
        break;
      }
      const [current, parent] = entry;
      nodes += 1;
      if (Array.isArray(current)) {
        for (const item of current) {
          pending.push([item, parent]);
        }
        continue;
      }
      if (!current || typeof current !== 'object') {
        continue;
      }
      const object = current as Record<string, unknown>;
      const entries = Object.entries(object);
      const ownIds = socialIds(object);
      const context: Context = {
        ids: ownIds ?? parent.ids,
        title: facebookText(object) ?? parent.title,
      };
      if (entries.some(([name]) => SOCIAL_MEDIA_FIELDS.has(name))) {
        const item: Record<string, unknown> = {};
        for (const [name, field] of entries) {
          if (SOCIAL_MEDIA_FIELDS.has(name) || SOCIAL_CONTEXT_FIELDS.has(name)) {
            item[name] = field;
          } else if (
            typeof field === 'number' ||
            typeof field === 'boolean' ||
            (typeof field === 'string' && field.length <= MAX_SOCIAL_SCALAR_CHARS)
          ) {
            item[name] = field;
          }
        }
        const wrap = (!ownIds && parent.ids !== null) || context.title !== null;
        items.push(
          wrap
            ? {
                ...context.ids,
                ...(context.title ? { title: context.title } : {}),
                media: item,
              }
            : item,
        );
      }
      for (const [, field] of entries) {
        if (field && typeof field === 'object') {
          pending.push([field, context]);
        }
      }
    }

    return items.length > 0 ? { items } : null;
  }

  function sourceDocument(value: unknown): unknown {
    return socialPage() ? socialMedia(value) : value;
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
        text.length > bodyLimit() ||
        !(type === 'application/json' || type === 'application/ld+json')
      ) {
        continue;
      }
      reportDocument(location.href, text);
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

  async function boundedText(response: Response, limit: number): Promise<string | null> {
    const length = Number(response.headers.get('content-length'));
    if (Number.isFinite(length) && length > limit) {
      return null;
    }

    if (!response.body) {
      const text = await response.text();
      return new TextEncoder().encode(text).byteLength > limit ? null : text;
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
      if (bytes > limit) {
        await reader.cancel();
        return null;
      }
      text += decoder.decode(value, { stream: true });
    }

    return text + decoder.decode();
  }

  function withoutGuard(text: string): string {
    return text
      .trim()
      .replace(/^\)\]\}',?\s*/, '')
      .replace(/^for\s*\(;;\);/, '');
  }

  // Facebook streams deferred GraphQL results as one JSON document per line.
  function jsonBody(text: string): unknown {
    const clean = withoutGuard(text);
    try {
      return JSON.parse(clean) as unknown;
    } catch {
      const values: unknown[] = [];
      for (const line of clean.split(/\r?\n/)) {
        const part = withoutGuard(line);
        if (!part) {
          continue;
        }
        try {
          values.push(JSON.parse(part) as unknown);
        } catch {
          continue;
        }
      }
      return values.length > 0 ? values : null;
    }
  }

  function reportDocument(sourceUrl: string, text: string): void {
    const value = jsonBody(text);
    if (value === null) {
      return;
    }
    const document = sourceDocument(value);
    if (document !== null) {
      reportSource(sourceUrl, document);
    }
  }

  async function capture(url: string, response: Response): Promise<void> {
    const knownSource = target(url);
    const type = response.headers.get('content-type') ?? '';
    if (!textual(type, knownSource)) {
      return;
    }

    try {
      const text = await boundedText(response, knownSource ? bodyLimit() : MAX_BODY_BYTES);
      if (!text) {
        return;
      }
      if (!knownSource && !MEDIA_SOURCE_HINT.test(text)) {
        return;
      }
      reportDocument(url, text);
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
        if (!target(requestUrl) || !textual(type, true)) {
          return;
        }
        try {
          const text =
            this.responseType === '' || this.responseType === 'text'
              ? this.responseText
              : this.responseType === 'json'
                ? JSON.stringify(this.response)
                : null;
          if (!text || text.length > bodyLimit()) {
            return;
          }
          reportDocument(new URL(requestUrl, location.href).href, text);
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
