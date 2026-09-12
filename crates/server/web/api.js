export class ApiError extends Error {
  constructor(message, { status = 0, code = "network", action = null, recoverable = true, technical = "" } = {}) {
    super(message);
    this.name = "ApiError";
    this.status = status;
    this.code = code;
    this.action = action;
    this.recoverable = recoverable;
    this.technical = technical;
  }
}

async function responseJson(response) {
  let payload = null;
  let parsed = false;
  try {
    payload = await response.json();
    parsed = true;
  } catch (_) {
    // Error mapping below deliberately avoids exposing a raw response body.
  }
  if (parsed && payload?.schema_version !== 2) {
    throw new ApiError("The player and server API versions do not match.", {
      status: response.status,
      code: "schema_mismatch",
      recoverable: false,
    });
  }
  if (!response.ok) {
    const body = payload?.error || {};
    throw new ApiError(body.message || "The server request failed.", {
      status: response.status,
      code: body.code || "server_error",
      action: body.action || null,
      recoverable: body.recoverable !== false,
      technical: `HTTP ${response.status}`,
    });
  }
  if (!parsed) {
    throw new ApiError("The player and server API versions do not match.", {
      status: response.status,
      code: "schema_mismatch",
      recoverable: false,
    });
  }
  return payload;
}

export class WebApi {
  #libraryController = null;
  #itemController = null;

  abortLibrary() {
    this.#libraryController?.abort();
    this.#libraryController = null;
  }

  abortItem() {
    this.#itemController?.abort();
    this.#itemController = null;
  }

  async library(navigation, { offset = 0, limit = 60, generation = null, replace = true, signal = null } = {}) {
    if (replace) this.abortLibrary();
    const controller = signal ? null : new AbortController();
    if (controller) this.#libraryController = controller;
    const params = new URLSearchParams({
      view: navigation.view,
      kind: navigation.kind,
      q: navigation.query || "",
      sort: navigation.sort || "title",
      offset: String(offset),
      limit: String(limit),
    });
    if (navigation.view === "folders" && navigation.folder) params.set("folder", navigation.folder);
    if (generation !== null && generation !== undefined) params.set("generation", String(generation));
    const response = await fetch(`/api/web/library?${params}`, {
      headers: { Accept: "application/json" },
      signal: signal || controller.signal,
    });
    return responseJson(response);
  }

  async librarySnapshot(navigation, { onFirstPage = () => {} } = {}) {
    this.abortLibrary();
    const controller = new AbortController();
    this.#libraryController = controller;
    const { signal } = controller;
    try {
      const first = await this.library(navigation, { limit: 200, replace: false, signal });
      signal.throwIfAborted();
      const { total, limit, generation } = first;
      const validate = (page, offset) => {
        signal.throwIfAborted();
        if (page.generation !== generation || page.total !== total) {
          throw new ApiError("The library changed while loading.", { code: "catalog_changed" });
        }
        if (!Number.isSafeInteger(generation) || generation < 0 || generation > 0xffff_ffff
          || !Number.isSafeInteger(total) || total < 0
          || !Number.isSafeInteger(limit) || limit < 1 || limit > 200
          || page.offset !== offset || !Array.isArray(page.entries)
          || page.entries.length !== Math.min(limit, total - offset)
          || page.has_more !== (offset + page.entries.length < total)) {
          throw new ApiError("The server returned an incomplete library page.", { code: "invalid_page" });
        }
      };
      validate(first, 0);
      onFirstPage(first);
      const pages = [first.entries];
      let nextOffset = limit;
      // Keep requests bounded while collecting one generation before publishing
      // the grid. Scrolling then changes only which posters need to be loaded.
      const worker = async () => {
        while (nextOffset < total) {
          signal.throwIfAborted();
          const offset = nextOffset;
          nextOffset += limit;
          const page = await this.library(navigation, { offset, limit, generation, replace: false, signal });
          validate(page, offset);
          pages[offset / limit] = page.entries;
        }
      };
      const workers = Math.max(0, Math.min(4, Math.ceil(total / limit) - 1));
      await Promise.all(Array.from({ length: workers }, worker));
      signal.throwIfAborted();
      return { ...first, entries: pages.flat(), limit: total, has_more: false };
    } catch (error) {
      controller.abort();
      throw error;
    } finally {
      if (this.#libraryController === controller) this.#libraryController = null;
    }
  }

  async continueItems(ids, { generation = null, signal = null } = {}) {
    const params = new URLSearchParams({
      view: "continue",
      ids: ids.map(String).join(","),
    });
    if (generation !== null && generation !== undefined) params.set("generation", String(generation));
    const response = await fetch(`/api/web/library?${params}`, {
      headers: { Accept: "application/json" },
      signal,
    });
    return responseJson(response);
  }

  async item(id, { signal = null, enrich = false, generation = null } = {}) {
    if (!signal) this.abortItem();
    const controller = signal ? null : new AbortController();
    if (controller) this.#itemController = controller;
    const params = new URLSearchParams();
    if (enrich) params.set("enrich", "1");
    if (generation !== null && generation !== undefined) params.set("generation", String(generation));
    const query = params.size ? `?${params}` : "";
    const response = await fetch(`/api/web/item/${encodeURIComponent(String(id))}${query}`, {
      headers: { Accept: "application/json" },
      signal: signal || controller.signal,
    });
    return responseJson(response);
  }

  async preview(url, { signal = null } = {}) {
    const response = await fetch(url, {
      priority: "low",
      headers: { Accept: "application/json" },
      signal,
    });
    return responseJson(response);
  }

  async previewSheet(url, { signal = null } = {}) {
    const limit = 16 * 1024 * 1024;
    let failure;
    for (const cache of ["force-cache", "reload"]) {
      const controller = new AbortController();
      const abort = () => controller.abort();
      if (signal?.aborted) controller.abort();
      else signal?.addEventListener("abort", abort, { once: true });
      const timer = window.setTimeout(abort, 15_000);
      let reader;
      let completed = false;
      let rejectAbort;
      const aborted = new Promise((_, reject) => { rejectAbort = reject; });
      const onAbort = () => rejectAbort(new DOMException("Preview was cancelled.", "AbortError"));
      controller.signal.addEventListener("abort", onAbort, { once: true });
      if (controller.signal.aborted) onAbort();
      try {
        return await Promise.race([aborted, (async () => {
          const response = await fetch(url, { headers: { Accept: "image/jpeg" }, cache,
            priority: "low", signal: controller.signal });
          if (controller.signal.aborted) {
            void response.body?.cancel().catch(() => {});
            throw new DOMException("Preview was cancelled.", "AbortError");
          }
          if (!response.ok) throw new ApiError("A timeline preview image could not be loaded.", {
            status: response.status, code: "preview_unavailable", recoverable: true,
            technical: `HTTP ${response.status}`,
          });
          if (Number(response.headers.get("content-length")) > limit || !response.body) {
            throw new Error("Preview image exceeds its resource budget.");
          }
          reader = response.body.getReader();
          const chunks = [];
          let length = 0;
          let reads = 0;
          while (true) {
            const { value, done } = await Promise.race([aborted, reader.read()]);
            if (done) break;
            length += value.byteLength;
            if (length > limit || ++reads > 65_536) throw new Error("Preview image exceeds its resource budget.");
            if (value.byteLength) chunks.push(value);
          }
          if (!length) throw new Error("Preview image is empty.");
          completed = true;
          return new Blob(chunks, { type: response.headers.get("content-type") || "image/jpeg" });
        })()]);
      } catch (error) {
        if (signal?.aborted) throw error;
        failure = error;
      } finally {
        window.clearTimeout(timer);
        signal?.removeEventListener("abort", abort);
        if (!completed) { controller.abort(); void reader?.cancel().catch(() => {}); }
        controller.signal.removeEventListener("abort", onAbort);
        try { reader?.releaseLock(); } catch (_) { /* Pending cancellation. */ }
      }
    }
    throw failure;
  }

  async preloadPreviewSheets(urls, { signal = null } = {}) {
    for (const url of urls) await this.previewSheet(url, { signal });
  }

  async transcodeStatus(id, requestId = null, sessionId = null, signal = null) {
    const params = new URLSearchParams();
    if (requestId !== null) params.set("request", String(requestId));
    if (sessionId !== null) params.set("session", String(sessionId));
    const encoded = params.toString();
    const query = encoded ? `?${encoded}` : "";
    const controller = new AbortController();
    const abort = () => controller.abort();
    if (signal?.aborted) controller.abort();
    else signal?.addEventListener("abort", abort, { once: true });
    const timer = window.setTimeout(() => controller.abort(), 15_000);
    let rejectAbort;
    const aborted = new Promise((_, reject) => { rejectAbort = reject; });
    const onAbort = () => rejectAbort(new DOMException("Stream status request was aborted.", "AbortError"));
    controller.signal.addEventListener("abort", onAbort, { once: true });
    if (controller.signal.aborted) onAbort();
    try {
      return await Promise.race([aborted, (async () => {
        const response = await fetch(`/api/web/transcode/${encodeURIComponent(String(id))}${query}`, {
          headers: { Accept: "application/json" },
          signal: controller.signal,
        });
        return responseJson(response);
      })()]);
    } catch (error) {
      if (controller.signal.aborted && !signal?.aborted) {
        throw new ApiError("The stream status request timed out.", { code: "network" });
      }
      throw error;
    } finally {
      window.clearTimeout(timer);
      controller.signal.removeEventListener("abort", onAbort);
      signal?.removeEventListener("abort", abort);
    }
  }

  async reportTranscodeStartup(id, requestId, sessionId, event, signal = null, elapsedMs = null) {
    if (elapsedMs !== null && (!Number.isFinite(elapsedMs) || elapsedMs < 0 || elapsedMs > 120_000)) return;
    const params = new URLSearchParams({
      request: String(requestId),
      session: String(sessionId),
      event: String(event),
    });
    if (Number.isFinite(elapsedMs)) params.set("elapsed_ms", String(Math.round(elapsedMs)));
    const response = await fetch(`/api/web/transcode/${encodeURIComponent(String(id))}?${params}`, {
      method: "POST",
      headers: { Accept: "application/json" },
      signal,
    });
    return responseJson(response);
  }

  async cancelTranscode(id, requestId, sessionId = null) {
    const params = new URLSearchParams({ request: String(requestId) });
    if (sessionId !== null) params.set("session", String(sessionId));
    const response = await fetch(`/api/web/transcode/${encodeURIComponent(String(id))}?${params}`, {
      method: "DELETE",
      headers: { Accept: "application/json" },
      keepalive: true,
    });
    return responseJson(response);
  }
}
