const MAX_BODY_BYTES = 512 * 1024;
const MAX_DEPTH = 32;
const MAX_NODES = 50_000;

if (!window.__gundaPageAgent) {
  window.__gundaPageAgent = true;
  installPageAgent();
}

function installPageAgent(): void {
  function target(url: string): boolean {
    try {
      const parsed = new URL(url, location.href);
      const host = parsed.hostname.toLowerCase();
      if (host.endsWith('instagram.com')) {
        return /graphql|api\/v1\/media\/[^/]+\/info|__a=1/i.test(
          parsed.pathname + parsed.search,
        );
      }
      if (host.endsWith('facebook.com')) {
        return /graphql|api/i.test(parsed.pathname);
      }
    } catch {
      return false;
    }
    return false;
  }

  function report(url: string): void {
    try {
      const parsed = new URL(url, location.href);
      if (!/^https?:$/.test(parsed.protocol)) {
        return;
      }
      window.postMessage(
        { source: 'gunda-page', kind: 'candidate', url: parsed.href } satisfies PageCandidate,
        location.origin,
      );
    } catch {
      return;
    }
  }

  async function boundedText(response: Response): Promise<string | null> {
    const length = Number(response.headers.get('content-length'));
    if (Number.isFinite(length) && length > MAX_BODY_BYTES) {
      return null;
    }

    if (!response.body) {
      const text = await response.text();
      return text.length > MAX_BODY_BYTES ? null : text;
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

  function progressiveUrl(value: unknown): string | null {
    const best: { value: { url: string; score: number } | null } = { value: null };
    let nodes = 0;

    function consider(
      url: unknown,
      width: unknown,
      height: unknown,
      quality: unknown = 0,
    ): void {
      if (typeof url !== 'string' || !url.startsWith('http')) {
        return;
      }
      const score =
        Math.max(1, Number(width) || 0) * Math.max(1, Number(height) || 0) +
        (Number(quality) || 0);
      if (!best.value || score > best.value.score) {
        best.value = { url, score };
      }
    }

    function visit(node: unknown, depth: number): void {
      if (!node || depth > MAX_DEPTH || nodes >= MAX_NODES) {
        return;
      }
      nodes += 1;

      if (Array.isArray(node)) {
        for (const item of node) {
          visit(item, depth + 1);
        }
        return;
      }
      if (typeof node !== 'object') {
        return;
      }

      const object = node as Record<string, unknown>;
      if (Array.isArray(object.video_versions)) {
        for (const version of object.video_versions) {
          if (version && typeof version === 'object') {
            const item = version as Record<string, unknown>;
            consider(item.url, item.width, item.height, item.type);
          }
        }
      }

      consider(object.browser_native_hd_url, 1920, 1080, 1);
      consider(object.browser_native_sd_url, 1280, 720);
      consider(object.playable_url_quality_hd, 1920, 1080, 1);
      consider(object.playable_url, 1280, 720);
      consider(object.video_url, 1280, 720);

      if (Array.isArray(object.progressive_urls)) {
        for (const version of object.progressive_urls) {
          if (version && typeof version === 'object') {
            const item = version as Record<string, unknown>;
            consider(item.url, 1280, 720, item.quality);
          }
        }
      }

      for (const child of Object.values(object)) {
        visit(child, depth + 1);
      }
    }

    visit(value, 0);
    return best.value ? best.value.url : null;
  }

  async function capture(url: string, response: Response): Promise<void> {
    if (!target(url)) {
      return;
    }
    const type = response.headers.get('content-type') ?? '';
    if (!/json|text\/plain/i.test(type)) {
      return;
    }

    try {
      const text = await boundedText(response);
      if (!text) {
        return;
      }
      const value = jsonBody(text);
      const media = value ? progressiveUrl(value) : null;
      if (media) {
        report(media);
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
            typeof this.responseText === 'string'
              ? this.responseText
              : this.responseType === 'json'
                ? JSON.stringify(this.response)
                : null;
          if (!text || text.length > MAX_BODY_BYTES) {
            return;
          }
          const value = jsonBody(text);
          const media = value ? progressiveUrl(value) : null;
          if (media) {
            report(media);
          }
        } catch {
          return;
        }
      },
      { once: true },
    );
    Reflect.apply(originalSend, this, args);
  } as typeof XMLHttpRequest.prototype.send;
}
