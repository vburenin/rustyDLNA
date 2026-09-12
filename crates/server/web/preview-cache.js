// Optional traffic has one worker, one latest scrub target and a bounded
// speculative queue. An evicted promise does not leave its fetch/decode alive.
export const PREVIEW_RETAINED_MAX_BYTES = 64 * 1024 * 1024;
export const PREVIEW_SHEET_MAX_PIXELS = 12_000_000;
export const PREVIEW_HEADER_MAX_BYTES = 256 * 1024;

// JPEG frame dimensions precede entropy-coded image data. Refuse an uncertain
// or excessive header before asking the browser to allocate a decoded bitmap.
export function previewJpegDimensions(bytes) {
  if (!(bytes instanceof Uint8Array) || bytes.length < 4 || bytes[0] !== 0xff || bytes[1] !== 0xd8) return null;
  const end = Math.min(bytes.length, PREVIEW_HEADER_MAX_BYTES);
  let offset = 2;
  let dimensions = null;
  let components = 0;
  while (offset < end) {
    if (bytes[offset++] !== 0xff) return null;
    while (offset < end && bytes[offset] === 0xff) offset += 1;
    if (offset >= end) return null;
    const marker = bytes[offset++];
    if (marker === 0x01) continue;
    if (marker === 0x00 || marker === 0xd8 || marker === 0xd9 || marker === 0xde || marker === 0xdf
      || (marker >= 0xd0 && marker <= 0xd7) || offset + 2 > end) return null;
    const length = bytes[offset] * 256 + bytes[offset + 1];
    if (length < 2 || offset + length > end) return null;
    if (marker === 0xda) {
      const scanComponents = bytes[offset + 2];
      if (!dimensions || scanComponents < 1 || scanComponents > components
        || length !== 6 + 2 * scanComponents) return null;
      return dimensions;
    }
    if (marker >= 0xc0 && marker <= 0xcf && ![0xc4, 0xc8, 0xcc].includes(marker)) {
      if (dimensions || length < 8 || bytes[offset + 2] !== 8) return null;
      const height = bytes[offset + 3] * 256 + bytes[offset + 4];
      const width = bytes[offset + 5] * 256 + bytes[offset + 6];
      components = bytes[offset + 7];
      if (components < 1 || components > 4 || length !== 8 + components * 3
        || width < 1 || height < 1 || width > 4096 || height > 4096
        || width * height > PREVIEW_SHEET_MAX_PIXELS) return null;
      dimensions = { width, height };
    }
    offset += length;
  }
  return null;
}

function aborted() { return new DOMException("Preview was replaced.", "AbortError"); }

export class PreviewCache {
  #fetch;
  #decode;
  #active = null;
  #target = null;
  #speculative = [];
  #cache = new Map();
  #cancelled = false;
  #allowed = false;
  constructor(fetchSheet, decode = decodePreview) { this.#fetch = fetchSheet; this.#decode = decode; }
  snapshot() { return { active: this.#active ? 1 : 0, pendingTargets: this.#target ? 1 : 0,
    pendingSpeculative: this.#speculative.length, retained: this.#cache.size,
    retainedBytes: [...this.#cache.values()].reduce((sum, image) => sum + (image.retainedBytes ?? image.width * image.height * 4), 0) }; }
  setAllowed(allowed) {
    this.#allowed = allowed;
    if (!allowed && this.#active && !this.#active.target) this.#active.controller.abort();
    if (allowed) this.#pump();
  }
  preload(urls) {
    this.#allowed = true;
    this.#speculative = [...new Set(urls)].filter((url) => !this.#cache.has(url)
      && url !== this.#active?.url && url !== this.#target?.url).slice(0, 8);
    this.#pump();
  }
  request(url) {
    if (this.#cancelled) return Promise.reject(aborted());
    const cached = this.#cache.get(url);
    if (cached) {
      this.#cache.delete(url); this.#cache.set(url, cached);
      this.#target?.reject(aborted()); this.#target = null;
      if (this.#active && this.#active.url !== url) this.#active.controller.abort();
      return Promise.resolve(cached);
    }
    if (this.#target?.url === url) return this.#target.promise;
    if (this.#active?.url === url && !this.#active.controller.signal.aborted) {
      this.#target?.reject(aborted()); this.#target = null;
      this.#active.target = true;
      return this.#active.promise;
    }
    this.#target?.reject(aborted());
    let resolve;
    let reject;
    const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
    // Ignored speculative/cancelled requests cannot cause unhandled rejections.
    void promise.catch(() => {});
    this.#target = { url, promise, resolve, reject, target: true };
    this.#speculative = this.#speculative.filter((candidate) => candidate !== url);
    if (this.#active) this.#active.controller.abort();
    this.#pump();
    return promise;
  }
  cancel() {
    this.#cancelled = true;
    this.#active?.controller.abort();
    this.#target?.reject(aborted()); this.#target = null;
    this.#speculative = [];
    for (const image of this.#cache.values()) image.close?.();
    this.#cache.clear();
  }
  #reserve(bytes) {
    if (bytes > PREVIEW_RETAINED_MAX_BYTES) throw new Error("Preview exceeds its retained memory budget.");
    while (this.#cache.size && (this.#cache.size >= 2 || this.snapshot().retainedBytes + bytes > PREVIEW_RETAINED_MAX_BYTES)) {
      const oldest = this.#cache.keys().next().value;
      this.#cache.get(oldest).close?.(); this.#cache.delete(oldest);
    }
  }
  #pump() {
    if (this.#cancelled || this.#active) return;
    let task = this.#target;
    this.#target = null;
    if (!task && this.#allowed) {
      let url;
      do { url = this.#speculative.shift(); } while (url && this.#cache.has(url));
      if (url) task = { url, resolve() {}, reject() {} };
    }
    if (!task) return;
    const controller = new AbortController();
    const active = this.#active = { ...task, controller };
    active.promise = (async () => {
      const blob = await this.#fetch(active.url, { signal: controller.signal });
      if (controller.signal.aborted) throw aborted();
      const image = await this.#decode(blob, controller.signal, (bytes) => this.#reserve(bytes));
      if (this.#cancelled || controller.signal.aborted) { image.close?.(); throw aborted(); }
      const bytes = image.retainedBytes ?? image.width * image.height * 4;
      try { this.#reserve(bytes); }
      catch (error) { image.close?.(); throw error; }
      this.#cache.set(active.url, image);
      task.resolve(image);
      return image;
    })().catch((error) => { task.reject(error); throw error; }).finally(() => {
      if (this.#active === active) this.#active = null;
      this.#pump();
    });
    void active.promise.catch(() => {});
  }
}

async function decodePreview(blob, signal, reserve) {
  if (signal.aborted) throw aborted();
  if (!(blob instanceof Blob) || blob.size > 16 * 1024 * 1024) throw new Error("Preview image exceeds its resource budget.");
  const dimensions = previewJpegDimensions(new Uint8Array(await blob.slice(0, PREVIEW_HEADER_MAX_BYTES).arrayBuffer()));
  if (signal.aborted) throw aborted();
  if (!dimensions) throw new Error("Preview JPEG dimensions are unavailable or exceed their resource budget.");
  const retainedBytes = dimensions.width * dimensions.height * 4 + Math.ceil(blob.size / 3) * 8 + 256;
  reserve(retainedBytes);
  return new Promise((resolve, reject) => {
    if (signal.aborted) { reject(aborted()); return; }
    const image = new Image();
    const reader = new FileReader();
    image.decoding = "async";
    image.fetchPriority = "low";
    let finished = false;
    const timer = setTimeout(() => finish(new Error("Preview decode timed out.")), 15_000);
    const abort = () => finish(aborted());
    const finish = (error) => {
      if (finished) return;
      finished = true;
      clearTimeout(timer);
      signal.removeEventListener("abort", abort);
      reader.onload = null; reader.onerror = null;
      if (reader.readyState === FileReader.LOADING) reader.abort();
      image.onload = null; image.onerror = null;
      if (error) { image.removeAttribute("src"); reject(error); }
      else {
        // The existing image CSP permits data URLs. Charge their conservative
        // UTF-16/base64 representation as well as the decoded bitmap instead
        // of widening the policy or issuing a second image HTTP request.
        image.retainedBytes = retainedBytes;
        image.close = () => image.removeAttribute("src");
        resolve(image);
      }
    };
    image.onload = () => finish(image.naturalWidth * image.naturalHeight !== dimensions.width * dimensions.height
      ? new Error("Preview dimensions do not match its JPEG header.") : null);
    image.onerror = () => finish(new Error("Preview could not be decoded."));
    reader.onload = () => { if (!signal.aborted) image.src = reader.result; };
    reader.onerror = () => finish(new Error("Preview image could not be read."));
    signal.addEventListener("abort", abort, { once: true });
    reader.readAsDataURL(blob);
  });
}
