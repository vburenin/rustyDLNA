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
  try {
    payload = await response.json();
  } catch (_) {
    // Error mapping below deliberately avoids exposing a raw response body.
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
  if (payload?.schema_version !== 1) {
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

  async item(id, { signal = null, enrich = false } = {}) {
    if (!signal) this.abortItem();
    const controller = signal ? null : new AbortController();
    if (controller) this.#itemController = controller;
    const query = enrich ? "?enrich=1" : "";
    const response = await fetch(`/api/web/item/${encodeURIComponent(String(id))}${query}`, {
      headers: { Accept: "application/json" },
      signal: signal || controller.signal,
    });
    return responseJson(response);
  }

  async transcodeStatus(id, requestId = null, signal = null) {
    const query = requestId === null ? "" : `?request=${encodeURIComponent(String(requestId))}`;
    const response = await fetch(`/api/web/transcode/${encodeURIComponent(String(id))}${query}`, {
      headers: { Accept: "application/json" },
      signal,
    });
    return responseJson(response);
  }

  async cancelTranscode(id, requestId) {
    const query = `?request=${encodeURIComponent(String(requestId))}`;
    const response = await fetch(`/api/web/transcode/${encodeURIComponent(String(id))}${query}`, {
      method: "DELETE",
      headers: { Accept: "application/json" },
      keepalive: true,
    });
    return responseJson(response);
  }
}
