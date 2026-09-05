import {
  clockLabel,
  itemDuration,
  mediaDetails,
  mediaMatchesQuery,
  navigationUrl,
  reconcileQualityPreference,
  resumePosition,
  validDetailId,
} from "./core.js";
import { clearProgress, progressDetails, progressSnapshot, savePreference } from "./preferences.js";

const CONTINUE_BATCH_SIZE = 100;
const MAX_CONTINUE_ITEMS = 500;
const LIBRARY_PAGE_SIZE = 24;
const MAX_ACTIVE_ARTWORK = 4;

export class LibraryController {
  #store;
  #api;
  #dom;
  #onSelect;
  #onNavigate;
  #request = 0;
  #searchTimer = null;
  #queueController = null;
  #continueController = null;
  #continueProgress = null;
  #liveMessage = "";
  #pagingObserver = null;
  #pagingFrame = null;
  #pagingActive = false;
  #pagingNeedsExit = false;
  #artworkObserver = null;
  #artworkQueue = new Set();
  #artworkRequests = new Map();

  constructor({ store, api, dom, onSelect, onNavigate = () => {} }) {
    this.#store = store;
    this.#api = api;
    this.#dom = dom;
    this.#onSelect = onSelect;
    this.#onNavigate = onNavigate;
    this.#bind();
    this.#setupInfiniteScroll();
    this.#setupArtworkLoading();
  }

  start() {
    const navigation = this.#store.getState().navigation;
    this.#dom.searchInput.value = navigation.query;
    this.#dom.sortControl.value = navigation.sort;
    this.syncTabs();
    return this.load({ reset: true });
  }

  cancelPendingSearch() {
    if (this.#searchTimer !== null) window.clearTimeout(this.#searchTimer);
    this.#searchTimer = null;
  }

  navigate(navigation, { history = "push", focusAfterLoad = true, supersedePending = true } = {}) {
    if (supersedePending) this.#onNavigate();
    this.cancelPendingSearch();
    this.#api.abortLibrary();
    this.#continueController?.abort();
    this.#store.dispatch({ type: "NAVIGATE", navigation });
    const state = this.#store.getState();
    this.#dom.searchInput.value = state.navigation.query;
    this.#dom.sortControl.value = state.navigation.sort;
    this.syncTabs();
    if (history !== "none") {
      const target = navigationUrl(window.location.href, state.navigation, state.server.rootFolderId);
      history === "replace" ? window.history.replaceState({}, "", target) : window.history.pushState({}, "", target);
    }
    return this.load({ reset: true, focusAfterLoad });
  }

  async load({ reset = false, focusAfterLoad = false } = {}) {
    const state = this.#store.getState();
    if (!reset && (state.library.status === "loading_more" || !state.library.hasMore)) return;
    if (reset) this.#pagingNeedsExit = false;
    const append = !reset;
    const appendFrom = state.library.entries.length;
    const appendAnchor = append ? this.#captureAppendAnchor() : null;
    const requestId = ++this.#request;
    this.#store.dispatch({ type: "LIBRARY_LOADING", append, requestId });
    const current = this.#store.getState();
    this.render({ preserveCards: append });
    if (current.navigation.view !== "continue") this.#continueProgress = null;
    const offset = reset ? 0 : current.library.offset;
    const generation = reset ? null : current.library.generation;
    try {
      const payload = current.navigation.view === "continue"
        ? await this.#continueWatchingPage(current.navigation.query)
        : await this.#api.library(current.navigation, {
          offset,
          limit: LIBRARY_PAGE_SIZE,
          generation,
          replace: reset,
        });
      if (requestId !== this.#request) return;
      const preferredQuality = this.#store.getState().preferences.quality;
      const quality = reconcileQualityPreference(
        preferredQuality,
        payload.capabilities?.quality_profiles,
      );
      if (quality !== preferredQuality) {
        savePreference("quality", quality);
        this.#store.dispatch({ type: "PREFERENCE", name: "quality", value: quality });
      }
      this.#store.dispatch({ type: "LIBRARY_SUCCESS", append, requestId, payload });
      if (current.navigation.view === "folders" && !current.navigation.folder) {
        this.#store.dispatch({ type: "NAVIGATE", navigation: { folder: payload.root_folder_id } });
      }
      this.render({ appendFrom: append ? appendFrom : null });
      if (appendAnchor) this.#restoreAppendAnchor(appendAnchor, requestId);
      if (append) {
        // Appending a page can move the sentinel without producing an
        // IntersectionObserver exit in WebKit. Re-arm from the new geometry
        // so the next real approach to the end can request another page.
        this.#pagingNeedsExit = false;
        this.#schedulePagingCheck();
      }
      if (focusAfterLoad) {
        this.#dom.libraryPanel.focus({ preventScroll: true });
      }
    } catch (error) {
      if (error?.name === "AbortError" || requestId !== this.#request) return;
      this.#store.dispatch({ type: "LIBRARY_ERROR", requestId, error });
      this.render({ preserveCards: append });
    }
  }

  render({ preserveCards = false, appendFrom = null } = {}) {
    const state = this.#store.getState();
    const { library, navigation, server, playback } = state;
    document.title = playback.item ? `${playback.item.title} · ${server.name}` : `${server.name} · Library`;
    this.#dom.serverName.textContent = server.name;
    this.#dom.serverState.dataset.state = server.state;
    this.#dom.libraryRetryTop.hidden = library.status !== "error";
    this.#dom.loading.hidden = !["loading", "loading_more"].includes(library.status);
    this.#dom.loadingLabel.textContent = library.status === "loading_more"
      ? "Loading more…"
      : "Loading library…";
    this.#dom.libraryEmpty.hidden = !["ready", "error"].includes(library.status)
      || (library.status === "ready" && library.total > 0);
    this.#dom.libraryRetry.hidden = library.status !== "error";
    this.#dom.libraryClearSearch.hidden = library.status !== "ready" || library.total > 0 || !navigation.query;
    this.#dom.searchInput.placeholder = navigation.view === "folders" ? "Filter this folder…" : "Search titles, artists, albums…";
    const noun = navigation.view === "folders" ? (library.total === 1 ? "entry" : "entries") : (library.total === 1 ? "item" : "items");
    this.#announceState(library, server, noun);
    if (library.status === "error") {
      this.#dom.libraryEmptyTitle.textContent = "Could not load the library";
      this.#dom.libraryEmptyDetail.textContent = friendlyLibraryError(library.error);
      this.#dom.libraryCount.textContent = "Library unavailable";
      this.#dom.libraryPanel.setAttribute("aria-busy", "false");
      this.#syncInfiniteScroll(library);
      return;
    }
    this.#dom.libraryPanel.setAttribute("aria-busy", String(["loading", "loading_more"].includes(library.status)));
    this.#dom.libraryCount.textContent = library.status === "loading" ? "Connecting…" : `${library.total} ${noun}`;
    this.#dom.libraryEmptyTitle.textContent = navigation.query ? `No results for “${navigation.query}”`
      : navigation.view === "continue" ? "Nothing to continue yet" : "No media found";
    this.#dom.libraryEmptyDetail.textContent = navigation.query ? "Try a different search or clear it to see this view."
      : navigation.view === "continue" ? "Start watching or listening. Your saved progress will appear here on this browser."
        : "Try another folder or media view.";
    this.#dom.resultsSummary.textContent = navigation.query
      ? `${library.total} ${library.total === 1 ? "result" : "results"} for “${navigation.query}”`
      : `${library.total} ${noun}`;
    this.renderBreadcrumbs();
    if (!preserveCards) this.renderCards({ appendFrom });
    this.syncTabs();
    this.#syncInfiniteScroll(library);
  }

  #announceState(library, server, noun) {
    let message = "";
    if (library.status === "loading_more") {
      message = "Loading more library items.";
    } else if (library.status === "loading") {
      message = server.state === "connecting" ? "Connecting to the library." : "Loading the library.";
    } else if (library.status === "error") {
      message = "The library is unavailable. Check the server connection and try again.";
    } else if (library.status === "ready" && server.state === "empty") {
      message = "Library ready. The server has no indexed media.";
    } else if (library.status === "ready" && library.total > 0) {
      message = `Library ready. ${library.total} ${noun}.`;
    } else if (library.status === "ready") {
      message = `Library ready. No ${noun} in this view.`;
    }
    if (message === this.#liveMessage) return;
    this.#liveMessage = message;
    this.#dom.libraryLive.textContent = message;
  }

  async #continueWatchingPage(query) {
    this.#continueController?.abort();
    const controller = new AbortController();
    this.#continueController = controller;
    const progress = progressSnapshot();
    this.#continueProgress = progress;
    const savedIds = [...progress.entries()]
      .filter(([itemId, details]) => validDetailId(itemId) && details.position > 0)
      .sort((left, right) => right[1].updated - left[1].updated)
      .slice(0, MAX_CONTINUE_ITEMS)
      .map(([itemId]) => itemId);
    let generation = null;
    let first = null;
    const entries = [];
    const batches = savedIds.length === 0
      ? [[]]
      : Array.from({ length: Math.ceil(savedIds.length / CONTINUE_BATCH_SIZE) }, (_, index) => (
        savedIds.slice(index * CONTINUE_BATCH_SIZE, (index + 1) * CONTINUE_BATCH_SIZE)
      ));
    for (const ids of batches) {
      const page = await this.#api.continueItems(ids, {
        generation,
        signal: controller.signal,
      });
      first ||= page;
      generation = page.generation;
      entries.push(...page.entries.filter((entry) => entry.entry_type === "media"));
    }
    const resumable = entries
      .filter((item) => {
        const details = progressDetails(item.id, progress);
        return resumePosition(details.position, itemDuration(item) || details.duration) > 0
          && mediaMatchesQuery(item, query);
      })
      .sort((left, right) => (
        progressDetails(right.id, progress).updated - progressDetails(left.id, progress).updated
      ));
    return {
      ...first,
      view: "continue",
      offset: 0,
      limit: resumable.length,
      total: resumable.length,
      has_more: false,
      entries: resumable,
    };
  }

  syncTabs() {
    const { navigation } = this.#store.getState();
    for (const tab of this.#dom.tabs) {
      const selected = tab.dataset.view === navigation.view
        && (navigation.view === "folders" || tab.dataset.kind === navigation.kind);
      tab.classList.toggle("active", selected);
      tab.setAttribute("aria-selected", String(selected));
      tab.tabIndex = selected ? 0 : -1;
      if (selected) this.#dom.libraryPanel.setAttribute("aria-labelledby", tab.id);
    }
  }

  renderBreadcrumbs() {
    const { navigation, library } = this.#store.getState();
    this.#dom.breadcrumbs.replaceChildren();
    this.#dom.breadcrumbs.hidden = navigation.view !== "folders";
    if (this.#dom.breadcrumbs.hidden) return;
    library.breadcrumbs.forEach((item, index) => {
      if (index > 0) {
        const separator = document.createElement("span");
        separator.textContent = "/";
        separator.setAttribute("aria-hidden", "true");
        this.#dom.breadcrumbs.append(separator);
      }
      const button = document.createElement("button");
      button.type = "button";
      button.textContent = item.title;
      const current = index === library.breadcrumbs.length - 1;
      if (current) button.setAttribute("aria-current", "page");
      else button.addEventListener("click", () => this.navigate({ view: "folders", folder: item.id, kind: "all", query: "" }));
      this.#dom.breadcrumbs.append(button);
    });
  }

  renderCards({ appendFrom = null } = {}) {
    const { library, playback } = this.#store.getState();
    if (appendFrom === null) {
      this.#artworkObserver?.disconnect();
      this.#artworkQueue.clear();
      // Detached images may never emit load/error. Release their admission
      // slots explicitly so a slow old view cannot starve the current one.
      for (const cancel of this.#artworkRequests.values()) cancel();
      this.#dom.grid.replaceChildren();
    }
    const entries = appendFrom === null ? library.entries : library.entries.slice(appendFrom);
    for (const entry of entries) {
      const card = entry.entry_type === "folder" ? this.#folderCard(entry) : this.#mediaCard(entry);
      if (entry.entry_type === "media" && String(entry.id) === String(playback.item?.id)) card.classList.add("playing");
      this.#dom.grid.append(card);
    }
    // IntersectionObserver delivery is advisory: WebKit can omit the initial
    // callback while a large grid is appended under load. Seed the same
    // bounded queue from actual post-layout geometry so visible artwork never
    // remains permanently blank.
    window.requestAnimationFrame(() => this.#queueNearbyArtwork());
  }

  markCurrent(itemId) {
    for (const card of this.#dom.grid.querySelectorAll(".media-card.playing")) card.classList.remove("playing");
    const selected = [...this.#dom.grid.querySelectorAll("[data-media-id]")]
      .find((card) => card.dataset.mediaId === String(itemId));
    selected?.classList.add("playing");
  }

  #folderCard(folder) {
    const article = document.createElement("article");
    article.className = "media-card folder";
    const button = document.createElement("button");
    button.type = "button";
    button.className = "card-button";
    const count = Number(folder.child_count || 0);
    button.setAttribute("aria-label", `Open ${folder.title}, ${count} ${count === 1 ? "item" : "items"}`);
    button.addEventListener("click", () => this.navigate({ view: "folders", folder: folder.id, kind: "all", query: "" }));
    const art = document.createElement("span");
    art.className = "art";
    const icon = document.createElement("span");
    icon.className = "folder-icon";
    icon.setAttribute("aria-hidden", "true");
    const badge = document.createElement("span");
    badge.className = "folder-count";
    badge.textContent = count > 999 ? "999+" : String(count);
    icon.append(badge);
    art.append(icon);
    const title = document.createElement("span");
    title.className = "card-title";
    title.textContent = folder.title;
    button.append(art, title);
    article.append(button);
    return article;
  }

  #mediaCard(item) {
    const article = document.createElement("article");
    article.className = `media-card ${item.kind}`;
    article.dataset.mediaId = String(item.id);
    const button = document.createElement("button");
    button.type = "button";
    button.className = "card-button";
    button.setAttribute("aria-label", `Play ${item.title}. ${mediaDetails(item)}`.trim());
    button.addEventListener("click", () => {
      this.#onNavigate();
      this.snapshotQueue();
      this.#store.dispatch({ type: "NAVIGATE", navigation: { itemId: String(item.id), start: 0 } });
      window.history.replaceState({}, "", navigationUrl(window.location.href, this.#store.getState().navigation, this.#store.getState().server.rootFolderId));
      this.#onSelect(item, { preserveQueue: true });
      this.markCurrent(item.id);
    });
    const art = document.createElement("span");
    art.className = "art";
    if (item.art_url) {
      const image = document.createElement("img");
      image.loading = "lazy";
      image.decoding = "async";
      image.fetchPriority = "low";
      image.alt = "";
      image.dataset.src = item.art_url;
      this.#observeArtwork(image);
      art.append(image);
    }
    const fallback = document.createElement("span");
    fallback.className = "art-fallback";
    fallback.textContent = item.kind === "audio" ? "AUDIO" : "VIDEO";
    fallback.setAttribute("aria-hidden", "true");
    art.prepend(fallback);
    const play = document.createElement("span");
    play.className = "card-play";
    play.setAttribute("aria-hidden", "true");
    art.append(play);
    const title = document.createElement("span");
    title.className = "card-title";
    title.textContent = item.title;
    button.append(art, title);
    if (item.file_name && item.file_name !== item.title) {
      const file = document.createElement("span");
      file.className = "card-file";
      file.textContent = item.file_name;
      file.title = item.file_name;
      button.append(file);
    }
    const meta = document.createElement("span");
    meta.className = "card-meta";
    const details = mediaDetails(item).split(" · ").filter(Boolean);
    details.forEach((detail, index) => {
      if (index > 0) {
        const dot = document.createElement("i");
        dot.setAttribute("aria-hidden", "true");
        meta.append(dot);
      }
      const value = document.createElement("span");
      value.textContent = detail;
      meta.append(value);
    });
    if (!details.length && itemDuration(item)) meta.textContent = clockLabel(itemDuration(item));
    button.append(meta);
    article.append(button);
    const cardActions = document.createElement("div");
    cardActions.className = "card-actions";
    const detailsButton = document.createElement("button");
    detailsButton.type = "button";
    detailsButton.textContent = "Details";
    detailsButton.setAttribute("aria-label", `Details for ${item.title}`);
    detailsButton.addEventListener("click", () => this.#showDetails(item));
    cardActions.append(detailsButton);
    article.append(cardActions);
    if (this.#store.getState().navigation.view === "continue") {
      const progress = progressDetails(item.id, this.#continueProgress);
      const actions = document.createElement("div");
      actions.className = "progress-actions";
      const label = document.createElement("span");
      label.textContent = `${clockLabel(progress.position)} watched`;
      const clear = document.createElement("button");
      clear.type = "button";
      clear.textContent = "Clear progress";
      clear.setAttribute("aria-label", `Clear progress for ${item.title}`);
      clear.addEventListener("click", () => {
        clearProgress(item.id);
        this.#store.dispatch({ type: "LIBRARY_REMOVE_ENTRY", id: item.id });
        this.render();
      });
      actions.append(label, clear);
      article.append(actions);
    }
    return article;
  }

  #showDetails(item) {
    this.#dom.itemDetailsTitle.textContent = item.title;
    const about = item.about || (item.kind === "video" ? "" : item.summary) || "";
    const plot = item.plot || (item.kind === "video" ? item.summary : "") || "";
    this.#dom.itemDetailsSummary.textContent = about;
    this.#dom.itemDetailsAbout.hidden = !about;
    this.#dom.itemDetailsPlot.open = false;
    this.#dom.itemDetailsPlot.hidden = !plot;
    this.#dom.itemDetailsPlotText.textContent = plot;
    const downloadUrl = typeof item.download_url === "string" && item.download_url.startsWith("/web/download/")
      ? item.download_url
      : null;
    this.#dom.itemDetailsDownload.hidden = !downloadUrl;
    if (downloadUrl) {
      this.#dom.itemDetailsDownload.href = downloadUrl;
      this.#dom.itemDetailsDownload.download = item.file_name || "";
      this.#dom.itemDetailsDownload.setAttribute("aria-label", `Download original file ${item.file_name || item.title}`);
    } else {
      this.#dom.itemDetailsDownload.removeAttribute("href");
      this.#dom.itemDetailsDownload.removeAttribute("download");
      this.#dom.itemDetailsDownload.removeAttribute("aria-label");
    }
    this.#dom.itemDetailsFacts.replaceChildren();
    const facts = [
      ["File", item.file_name],
      [item.kind === "video" ? "Show / album" : "Album", item.album],
      [item.kind === "video" ? "Season / disc" : "Disc", item.disc],
      [item.kind === "video" ? "Episode / track" : "Track", item.track],
      ["Artist", item.artist],
      ["Genre", item.genre],
      ["Date", item.date],
      ["Duration", itemDuration(item) ? clockLabel(itemDuration(item)) : null],
      ["Resolution", item.resolution],
      ["Video", [item.video_codec, item.video_profile, item.video_level ? `level ${item.video_level}` : null, item.pixel_format, item.bit_depth ? `${item.bit_depth}-bit` : null, item.frame_rate ? `${item.frame_rate} fps` : null, item.hdr].filter(Boolean).join(" · ")],
      ["Audio", [item.audio_codec, item.audio_layout].filter(Boolean).join(" · ")],
      ["Container", item.container],
    ].filter(([, value]) => value !== null && value !== undefined && String(value).trim());
    for (const [name, value] of facts) {
      const row = document.createElement("div");
      const term = document.createElement("dt");
      const detail = document.createElement("dd");
      term.textContent = name;
      detail.textContent = String(value);
      row.append(term, detail);
      this.#dom.itemDetailsFacts.append(row);
    }
    this.#dom.itemDetailsDialog.showModal();
  }

  snapshotQueue() {
    this.#queueController?.abort();
    const controller = new AbortController();
    this.#queueController = controller;
    const state = this.#store.getState();
    const requestId = state.queue.requestId + 1;
    const context = { ...state.navigation };
    const generation = state.library.generation;
    const entries = state.library.entries.filter((entry) => entry.entry_type === "media");
    this.#store.dispatch({ type: "QUEUE_LOADING", entries, generation, requestId });
    if (!state.library.hasMore) {
      this.#store.dispatch({ type: "QUEUE_SUCCESS", entries, generation, requestId });
      if (this.#queueController === controller) this.#queueController = null;
      return;
    }
    this.#completeQueue(
      context,
      generation,
      state.library.offset,
      state.library.total,
      entries,
      controller,
      requestId,
    );
  }

  async #completeQueue(context, generation, offset, total, initial, controller, requestId) {
    const entries = [...initial];
    const current = () => this.#queueController === controller
      && !controller.signal.aborted
      && this.#store.getState().queue.requestId === requestId;
    try {
      while (offset < total) {
        const payload = await this.#api.library(context, {
          offset,
          limit: 200,
          generation,
          replace: false,
          signal: controller.signal,
        });
        if (!current()) return;
        entries.push(...payload.entries.filter((entry) => entry.entry_type === "media"));
        const advanced = payload.entries.length;
        if (advanced === 0) break;
        offset += advanced;
      }
      if (current()) this.#store.dispatch({ type: "QUEUE_SUCCESS", entries, generation, requestId });
    } catch (error) {
      if (error?.name === "AbortError" || !current()) return;
      this.#store.dispatch({ type: "QUEUE_ERROR", error, requestId });
    } finally {
      if (this.#queueController === controller) this.#queueController = null;
    }
  }

  #setupInfiniteScroll() {
    if (typeof window.IntersectionObserver === "function") {
      this.#pagingObserver = new IntersectionObserver((entries) => {
        const intersecting = entries.some((entry) => entry.isIntersecting);
        if (!intersecting) {
          this.#pagingNeedsExit = false;
          return;
        }
        if (this.#pagingNeedsExit || !this.#nextPageIsNear()) return;
        this.#pagingNeedsExit = true;
        void this.load({ reset: false });
      }, { rootMargin: "800px 0px" });
    } else {
      const schedule = () => this.#schedulePagingCheck();
      window.addEventListener("scroll", schedule, { passive: true });
      window.addEventListener("resize", schedule);
    }
    const rearm = () => {
      if (this.#store.getState().library.status !== "ready") return;
      this.#pagingNeedsExit = false;
      // Input events arrive before their default scroll. The animation-frame
      // check observes the resulting position and also covers WebKit missing
      // a sentinel intersection callback.
      this.#schedulePagingCheck();
    };
    window.addEventListener("wheel", rearm, { passive: true });
    window.addEventListener("touchmove", rearm, { passive: true });
    window.addEventListener("pointerdown", rearm, { passive: true });
    window.addEventListener("keydown", rearm);
  }

  #setupArtworkLoading() {
    if (typeof window.IntersectionObserver !== "function") return;
    this.#artworkObserver = new IntersectionObserver((entries) => {
      for (const entry of entries) {
        if (entry.isIntersecting) this.#artworkQueue.add(entry.target);
        else this.#artworkQueue.delete(entry.target);
      }
      this.#drainArtworkQueue();
    }, { rootMargin: "400px 0px" });
  }

  #observeArtwork(image) {
    if (this.#artworkObserver) {
      this.#artworkObserver.observe(image);
      return;
    }
    this.#artworkQueue.add(image);
    window.queueMicrotask(() => this.#drainArtworkQueue());
  }

  #queueNearbyArtwork() {
    const margin = 400;
    for (const image of this.#dom.grid.querySelectorAll("img[data-src]")) {
      const bounds = image.getBoundingClientRect();
      if (bounds.bottom >= -margin && bounds.top <= window.innerHeight + margin) {
        this.#artworkQueue.add(image);
      }
    }
    this.#drainArtworkQueue();
  }

  #drainArtworkQueue() {
    for (const image of this.#artworkQueue) {
      if (!image.isConnected || !image.dataset.src) this.#artworkQueue.delete(image);
    }
    while (this.#artworkRequests.size < MAX_ACTIVE_ARTWORK && this.#artworkQueue.size > 0) {
      const viewportCenter = window.innerHeight / 2;
      const image = [...this.#artworkQueue].sort((left, right) => {
        const leftBounds = left.getBoundingClientRect();
        const rightBounds = right.getBoundingClientRect();
        const leftDistance = Math.abs((leftBounds.top + leftBounds.bottom) / 2 - viewportCenter);
        const rightDistance = Math.abs((rightBounds.top + rightBounds.bottom) / 2 - viewportCenter);
        return leftDistance - rightDistance;
      })[0];
      this.#artworkQueue.delete(image);
      this.#artworkObserver?.unobserve(image);
      this.#startArtwork(image);
    }
  }

  #startArtwork(image) {
    const source = image.dataset.src;
    if (!source) return;
    delete image.dataset.src;
    let settled = false;
    const settle = () => {
      if (settled) return;
      settled = true;
      image.removeEventListener("load", settle);
      image.removeEventListener("error", failed);
      this.#artworkRequests.delete(image);
      this.#drainArtworkQueue();
    };
    const failed = () => {
      image.classList.add("failed");
      settle();
    };
    this.#artworkRequests.set(image, () => {
      settle();
      image.removeAttribute("src");
    });
    image.addEventListener("load", settle, { once: true });
    image.addEventListener("error", failed, { once: true });
    // Visibility and concurrency are already controlled by this queue.
    image.loading = "eager";
    image.src = source;
    window.queueMicrotask(() => {
      // A cached failure can already be complete before its error event.
      if (!settled && image.complete) {
        if (image.naturalWidth === 0) failed();
        else settle();
      }
    });
  }

  #captureAppendAnchor() {
    let element = document.activeElement;
    const focusedBounds = element?.getBoundingClientRect();
    const focused = this.#dom.grid.contains(element)
      && focusedBounds.bottom > 0 && focusedBounds.top < window.innerHeight;
    if (!focused) {
      element = [...this.#dom.grid.children].find((card) => {
        const bounds = card.getBoundingClientRect();
        return bounds.bottom > 0 && bounds.top < window.innerHeight;
      });
    }
    if (!element) return null;
    return { element, focused, top: element.getBoundingClientRect().top };
  }

  #restoreAppendAnchor(anchor, requestId) {
    const restore = () => {
      if (!anchor.element.isConnected
        || this.#store.getState().library.requestId !== requestId) return;
      const delta = anchor.element.getBoundingClientRect().top - anchor.top;
      if (Math.abs(delta) > 0.5) window.scrollBy({ top: delta, left: 0, behavior: "auto" });
      if (anchor.focused) {
        const bounds = anchor.element.getBoundingClientRect();
        if (bounds.bottom <= 0 || bounds.top >= window.innerHeight) {
          anchor.element.scrollIntoView({ block: "nearest", inline: "nearest", behavior: "auto" });
        }
      }
    };
    restore();
    window.requestAnimationFrame(() => {
      restore();
      window.requestAnimationFrame(restore);
    });
  }

  #syncInfiniteScroll(library) {
    const enabled = ["ready", "loading_more"].includes(library.status) && library.hasMore;
    this.#dom.loadMoreSentinel.hidden = !enabled;
    if (this.#pagingObserver) {
      if (enabled && !this.#pagingActive) {
        this.#pagingObserver.observe(this.#dom.loadMoreSentinel);
        this.#pagingActive = true;
      } else if (!enabled && this.#pagingActive) {
        this.#pagingObserver.unobserve(this.#dom.loadMoreSentinel);
        this.#pagingActive = false;
      }
    } else if (enabled && !this.#pagingActive) {
      this.#pagingActive = true;
      this.#schedulePagingCheck();
    } else if (!enabled) {
      this.#pagingActive = false;
    }
  }

  #schedulePagingCheck() {
    if (this.#pagingFrame !== null) return;
    this.#pagingFrame = window.requestAnimationFrame(() => {
      this.#pagingFrame = null;
      if (!this.#nextPageIsNear()) return;
      if (this.#pagingNeedsExit) return;
      this.#pagingNeedsExit = true;
      void this.load({ reset: false });
    });
  }

  #nextPageIsNear() {
    const state = this.#store.getState();
    if (state.library.status !== "ready" || !state.library.hasMore
      || this.#dom.loadMoreSentinel.hidden) return false;
    const bounds = this.#dom.loadMoreSentinel.getBoundingClientRect();
    return bounds.top <= window.innerHeight + 800 && bounds.bottom >= -800;
  }

  #bind() {
    this.#dom.libraryClearSearch.addEventListener("click", () => {
      this.navigate({ query: "" }, { history: "replace", focusAfterLoad: false });
      this.#dom.searchInput.focus();
    });
    this.#dom.libraryRetry.addEventListener("click", () => this.load({ reset: true }));
    this.#dom.libraryRetryTop.addEventListener("click", () => this.load({ reset: true }));
    this.#dom.searchInput.addEventListener("input", () => {
      this.cancelPendingSearch();
      this.#searchTimer = window.setTimeout(() => {
        this.#searchTimer = null;
        const query = this.#dom.searchInput.value.trim();
        this.navigate({ query }, { history: "replace", focusAfterLoad: false });
      }, 250);
    });
    this.#dom.sortControl.addEventListener("change", () => {
      this.navigate({ sort: this.#dom.sortControl.value }, { focusAfterLoad: false });
    });
    this.#dom.tabs.forEach((tab, tabIndex) => {
      tab.addEventListener("click", () => this.navigate({
        view: tab.dataset.view,
        kind: tab.dataset.kind,
        folder: tab.dataset.view === "folders" ? this.#store.getState().server.rootFolderId : null,
        query: "",
      }));
      tab.addEventListener("keydown", (event) => {
        const keys = { ArrowLeft: -1, ArrowRight: 1, Home: -tabIndex, End: this.#dom.tabs.length - 1 - tabIndex };
        if (!(event.key in keys)) return;
        event.preventDefault();
        const next = (tabIndex + keys[event.key] + this.#dom.tabs.length) % this.#dom.tabs.length;
        this.#dom.tabs[next].focus();
        this.#dom.tabs[next].click();
      });
    });
  }
}

function friendlyLibraryError(error) {
  if (error?.code === "catalog_changed") return "The library changed while loading. Retry to refresh it.";
  if (!navigator.onLine) return "You appear to be offline. Reconnect, then retry.";
  return "Check the server connection, then retry.";
}
