// Finite artwork transport. Native image loaders do not consistently cancel
// their HTTP requests when a visible image's src is removed (notably WebKit).
// Match the server's MAX_SIDECAR_BYTES: admitted originals up to 16 MiB must
// remain displayable. The four-slot owner also bounds aggregate transport.
export const ARTWORK_MAX_BYTES = 16 * 1024 * 1024;

export async function fetchArtwork(source, signal) {
  const url = new URL(source, location.href);
  if (url.origin !== location.origin) throw new Error("Artwork must be same-origin.");
  const response = await fetch(url, { signal, credentials: "same-origin", priority: "low" });
  let reader;
  let completed = false;
  try {
    if (!response.ok || !response.body) throw new Error("Artwork is unavailable.");
    if (Number(response.headers.get("content-length")) > ARTWORK_MAX_BYTES) throw new Error("Artwork is too large.");
    reader = response.body.getReader();
    const chunks = [];
    let bytes = 0;
    let reads = 0;
    while (true) {
      const { done, value } = await reader.read();
      if (signal.aborted) throw signal.reason;
      if (done) break;
      bytes += value.byteLength;
      if (bytes > ARTWORK_MAX_BYTES || ++reads > 65_536) throw new Error("Artwork is too large.");
      if (value.byteLength) chunks.push(value);
    }
    if (!bytes) throw new Error("Artwork is empty.");
    completed = true;
    return new Blob(chunks, { type: response.headers.get("content-type") || "" });
  } finally {
    if (!completed) void (reader ? reader.cancel() : response.body?.cancel())?.catch(() => {});
    try { reader?.releaseLock(); } catch (_) { /* Aborted read. */ }
  }
}
