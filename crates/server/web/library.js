import {
  cardProgress,
  clockLabel,
  itemDuration,
  libraryErrorMessage,
  libraryRetryDelay,
  mediaDetails,
  mediaMatchesQuery,
  navigationUrl,
  originalDownloadUrl,
  reconcileQualityPreference,
  resumePosition,
  validDetailId,
} from "./core.js";
import { clearProgress, progressDetails, progressSnapshot, savePreference, watchedSnapshot } from "./preferences.js";
import { fetchArtwork } from "./artwork.js";

const CONTINUE_BATCH_SIZE = 100;
const MAX_CONTINUE_ITEMS = 500;
const MAX_ACTIVE_ARTWORK = 4;
const ARTWORK_DEADLINE_MS = 60_000;
const ARTWORK_OFFSCREEN_GRACE_MS = 5_000;
// Browsers rate-limit history writes; record the library place only after
// scrolling settles, and always immediately before leaving the entry.
const PLACE_SAVE_DELAY_MS = 500;

// History entries carry the library place beside other state. URL query
// parameters remain the navigation identity; this state is only a hint.
export function historyState(patch = {}) {
  const current = window.history.state;
  return { ...(current && typeof current === "object" ? current : {}), ...patch };
}

function cardKey(card) {
  if (card?.dataset.mediaId) return `m:${card.dataset.mediaId}`;
  if (card?.dataset.folderId) return `f:${card.dataset.folderId}`;
  return null;
}

export class LibraryController {
  #store;
  #api;
  #dom;
  #onSelect;
  #onNavigate;
  #request = 0;
  #capabilitiesReady = Promise.resolve(null);
  #searchTimer = null;
  #continueController = null;
  #continueProgress = null;
  #liveMessage = "";
  #artworkFrame = null;
  #gridObserver = null;
  #gridWidth = -1;
  #artworkQueue = new Set();
  #artworkRequests = new Map();
  #chunkKeys = new WeakMap();
  #chunkHeights = new Map();
  #cardsById = new Map();
  #retryWake = null;
  #placeTimer = null;

  constructor({ store, api, dom, onSelect, onNavigate = () => {} }) {
    this.#store = store;
    this.#api = api;
    this.#dom = dom;
    this.#onSelect = onSelect;
    this.#onNavigate = onNavigate;
    this.#bind();
    this.#setupArtworkLoading();
  }

  start({ restore = null } = {}) {
    const navigation = this.#store.getState().navigation;
    this.#dom.searchInput.value = navigation.query;
    this.#dom.sortControl.value = navigation.sort;
    this.syncTabs();
    return this.load({ restore });
  }

  get capabilitiesReady() {
    return this.#capabilitiesReady;
  }

  cancelPendingSearch() {
    if (this.#searchTimer !== null) window.clearTimeout(this.#searchTimer);
    this.#searchTimer = null;
  }

  navigate(navigation, {
    history = "push", focusAfterLoad = true, supersedePending = true, restore = null, originKey = null,
  } = {}) {
    if (supersedePending) this.#onNavigate();
    this.cancelPendingSearch();
    // Record where the outgoing entry was before its list is replaced.
    if (history === "push") this.#rememberPlace(originKey);
    else this.#cancelPlaceSave();
    this.#api.abortLibrary();
    this.#continueController?.abort();
    this.#store.dispatch({ type: "NAVIGATE", navigation });
    const state = this.#store.getState();
    this.#dom.searchInput.value = state.navigation.query;
    this.#dom.sortControl.value = state.navigation.sort;
    this.syncTabs();
    if (history !== "none") {
      const target = navigationUrl(window.location.href, state.navigation, state.server.rootFolderId);
      // A replaced search or sort is a different list; its old place no longer applies.
      history === "replace"
        ? window.history.replaceState(historyState({ library: null }), "", target)
        : window.history.pushState({}, "", target);
    }
    return this.load({ focusAfterLoad, restore });
  }

  #capabilityRequest() {
    let resolve;
    let reject;
    this.#capabilitiesReady = new Promise((resolvePromise, rejectPromise) => {
      resolve = resolvePromise;
      reject = rejectPromise;
    });
    // Ordinary library navigation has no linked selection awaiting this promise.
    // Its error is still reported by the complete-library path below.
    void this.#capabilitiesReady.catch(() => {});
    return { resolve, reject };
  }

  #retryDelay(delay) {
    return new Promise((resolve) => {
      const timer = window.setTimeout(() => {
        this.#retryWake = null;
        resolve();
      }, delay);
      this.#retryWake = () => {
        window.clearTimeout(timer);
        resolve();
      };
    });
  }

  #cancelRetryDelay() {
    const wake = this.#retryWake;
    this.#retryWake = null;
    wake?.();
  }

  async load({ focusAfterLoad = false, restore = null } = {}) {
    const requestId = ++this.#request;
    // A superseded load wakes from its backoff, sees the newer request, and stops.
    this.#cancelRetryDelay();
    let capabilities = this.#capabilityRequest();
    let published = false;
    const onFirstPage = (payload) => {
      if (requestId !== this.#request) throw new DOMException("Library request replaced.", "AbortError");
      // An automatic retry publishes a newer first page. A linked selection
      // follows the replacement promise, as it does after a manual Retry.
      if (published) capabilities = this.#capabilityRequest();
      published = true;
      this.#publishCapabilities(requestId, payload);
      capabilities.resolve(payload);
    };
    // Loading hides Retry and the empty-state actions. Keep keyboard focus in
    // the library rather than letting it fall to the document, and return it to
    // the same Retry if this load fails again.
    const hiddenFocus = this.#focusLeavingHiddenControls();
    const focusOrigin = document.activeElement;
    this.#store.dispatch({ type: "LIBRARY_LOADING", requestId });
    const current = this.#store.getState();
    this.render();
    if (current.navigation.view !== "continue") this.#continueProgress = null;
    try {
      let payload;
      for (let attempt = 0; ; attempt += 1) {
        try {
          // Each attempt restarts from the first page without a generation, so
          // a published list never mixes pages from two catalog snapshots.
          payload = current.navigation.view === "continue"
            ? await this.#continueWatchingPage(current.navigation.query, onFirstPage)
            : await this.#api.librarySnapshot(current.navigation, { onFirstPage });
          break;
        } catch (error) {
          const delay = requestId === this.#request ? libraryRetryDelay(error, attempt) : null;
          if (delay === null) throw error;
          await this.#retryDelay(delay);
          if (requestId !== this.#request) throw new DOMException("Library request replaced.", "AbortError");
        }
      }
      if (requestId !== this.#request) return;
      const batches = this.#cardBatches(payload.entries, current.navigation);
      let batch = batches.next();
      while (!batch.done) {
        // Build privately so Find, keyboard traversal, and the queue see one
        // complete list. Navigation can abandon the fragment between batches.
        await new Promise((resolve) => window.setTimeout(resolve, 0));
        if (requestId !== this.#request) return;
        batch = batches.next();
      }
      this.#store.dispatch({ type: "LIBRARY_SUCCESS", requestId, payload });
      if (current.navigation.view === "folders" && !current.navigation.folder) {
        this.#store.dispatch({ type: "NAVIGATE", navigation: { folder: payload.root_folder_id } });
      }
      this.render(batch.value);
      if (restore && typeof restore === "object") {
        this.#restorePlace(requestId, restore, focusAfterLoad && (focusOrigin || document.body));
      } else if (focusAfterLoad && this.#focusUnmoved(focusOrigin)) {
        this.#dom.libraryPanel.focus({ preventScroll: true });
      }
    } catch (error) {
      capabilities.reject(error);
      if (error?.name === "AbortError" || requestId !== this.#request) return;
      this.#store.dispatch({ type: "LIBRARY_ERROR", requestId, error });
      this.render();
      if (hiddenFocus?.retry && !hiddenFocus.retry.hidden && document.activeElement === this.#dom.libraryPanel) {
        hiddenFocus.retry.focus({ preventScroll: true });
      }
    }
  }

  // A load may move focus only while the user has not taken it elsewhere, for
  // example into Search or the player while a slow page was still arriving.
  #focusUnmoved(origin) {
    const active = document.activeElement;
    return !active || active === document.body || active === document.documentElement
      || active === origin || this.#dom.libraryPanel.contains(active);
  }

  // Moves focus from a control the loading state is about to hide to the
  // library panel. Returns null when focus was elsewhere; otherwise `retry` is
  // the Retry button to refocus if the load fails again.
  #focusLeavingHiddenControls() {
    const active = document.activeElement;
    const { libraryEmpty, libraryRetry, libraryRetryTop, libraryPanel } = this.#dom;
    if (!(active instanceof Element) || !(active === libraryRetryTop || libraryEmpty.contains(active))) return null;
    libraryPanel.focus({ preventScroll: true });
    return { retry: active === libraryRetryTop || active === libraryRetry ? active : null };
  }

  // Landscape Watch scrolls the library beside the player instead of the page.
  #libraryScroller() {
    const section = this.#dom.libraryPanel.closest(".library");
    if (!section) return null;
    const overflow = getComputedStyle(section).overflowY;
    return ["auto", "scroll"].includes(overflow) && section.scrollHeight > section.clientHeight ? section : null;
  }

  #cardForKey(key) {
    if (typeof key !== "string") return null;
    const id = key.slice(2);
    if (key.startsWith("m:")) return this.#cardsById.get(id) ?? null;
    if (!key.startsWith("f:")) return null;
    for (const card of this.#dom.grid.querySelectorAll(".media-card.folder")) {
      if (card.dataset.folderId === id) return card;
    }
    return null;
  }

  #cancelPlaceSave() {
    if (this.#placeTimer !== null) window.clearTimeout(this.#placeTimer);
    this.#placeTimer = null;
  }

  #schedulePlaceSave() {
    this.#cancelPlaceSave();
    this.#placeTimer = window.setTimeout(() => {
      this.#placeTimer = null;
      this.#rememberPlace();
    }, PLACE_SAVE_DELAY_MS);
  }

  #rememberPlace(originKey = null) {
    this.#cancelPlaceSave();
    // Loading and failed views have no list position worth returning to.
    if (this.#store.getState().library.status !== "ready") return;
    const active = document.activeElement;
    const focused = active instanceof Element && this.#dom.grid.contains(active)
      ? active.closest(".media-card") : null;
    const card = this.#cardForKey(originKey) || focused;
    const scroller = this.#libraryScroller();
    const place = {
      scrollY: window.scrollY,
      libraryScrollTop: scroller ? scroller.scrollTop : null,
      cardKey: cardKey(card),
      cardTop: card ? card.getBoundingClientRect().top : null,
    };
    try {
      window.history.replaceState(historyState({ library: place }), "");
    } catch (_) {
      // A rate-limited history write only loses this optional hint.
    }
  }

  #restorePlace(requestId, place, focus) {
    // Wait for the published grid, including reserved chunk heights, to lay out.
    window.requestAnimationFrame(() => {
      if (requestId !== this.#request) return;
      const scroller = this.#libraryScroller();
      const libraryTop = Number(place.libraryScrollTop);
      const pageTop = Number(place.scrollY);
      if (scroller && place.libraryScrollTop !== null && Number.isFinite(libraryTop)) scroller.scrollTop = libraryTop;
      else if (!scroller && Number.isFinite(pageTop)) window.scrollTo({ top: pageTop, left: 0, behavior: "auto" });
      const card = this.#cardForKey(place.cardKey);
      const cardTop = Number(place.cardTop);
      if (card && place.cardTop !== null && Number.isFinite(cardTop)) {
        // Align the card the user left from, even if card heights changed.
        const delta = card.getBoundingClientRect().top - cardTop;
        if (Math.abs(delta) >= 1) {
          if (scroller) scroller.scrollTop += delta;
          else window.scrollBy({ top: delta, left: 0, behavior: "auto" });
        }
      }
      if (focus && this.#focusUnmoved(focus)) {
        (card?.querySelector(".card-button") || this.#dom.libraryPanel).focus({ preventScroll: true });
      }
    });
  }

  #publishCapabilities(requestId, payload) {
    const preferredQuality = this.#store.getState().preferences.quality;
    const quality = reconcileQualityPreference(preferredQuality, payload.capabilities?.quality_profiles);
    if (quality !== preferredQuality) {
      savePreference("quality", quality);
      this.#store.dispatch({ type: "PREFERENCE", name: "quality", value: quality });
    }
    this.#store.dispatch({ type: "LIBRARY_CAPABILITIES", requestId, payload });
    this.render();
  }

  render(cards = null) {
    const state = this.#store.getState();
    const { library, navigation, server, playback } = state;
    document.title = playback.item ? `${playback.item.title} · ${server.name}` : `${server.name} · Library`;
    this.#dom.serverName.textContent = server.name;
    this.#dom.serverState.dataset.state = server.state;
    this.#dom.libraryRetryTop.hidden = library.status !== "error";
    this.#dom.loading.hidden = library.status !== "loading";
    this.#dom.libraryEmpty.hidden = !["ready", "error"].includes(library.status)
      || (library.status === "ready" && library.total > 0);
    this.#dom.libraryRetry.hidden = library.status !== "error";
    this.#dom.libraryClearSearch.hidden = library.status !== "ready" || library.total > 0 || !navigation.query;
    this.#dom.searchInput.placeholder = navigation.view === "folders" ? "Filter this folder…" : "Search titles, artists, albums…";
    const noun = navigation.view === "folders" ? (library.total === 1 ? "entry" : "entries") : (library.total === 1 ? "item" : "items");
    this.#announceState(library, server, noun);
    if (library.status === "error") {
      this.#dom.libraryEmptyTitle.textContent = "Could not load the library";
      this.#dom.libraryEmptyDetail.textContent = libraryErrorMessage(library.error, navigator.onLine);
      this.#dom.libraryCount.textContent = "Library unavailable";
      // The previous view's count and folder path do not describe this view.
      this.#dom.resultsSummary.textContent = "";
      this.#dom.breadcrumbs.replaceChildren();
      this.#dom.breadcrumbs.hidden = true;
      this.#dom.libraryPanel.setAttribute("aria-busy", "false");
      return;
    }
    const loading = library.status === "loading";
    this.#dom.libraryPanel.setAttribute("aria-busy", String(loading));
    this.#dom.libraryCount.textContent = loading
      ? (server.state === "connecting" ? "Connecting…" : "Loading…")
      : `${library.total} ${noun}`;
    // `empty` describes the whole server; an empty search or folder is not one.
    const emptyServer = server.state === "empty" && !navigation.query && navigation.view !== "continue";
    this.#dom.libraryEmptyTitle.textContent = navigation.query ? `No results for “${navigation.query}”`
      : navigation.view === "continue" ? "Nothing to continue yet"
        : emptyServer ? "No media indexed yet" : "No media found";
    this.#dom.libraryEmptyDetail.textContent = navigation.query ? "Try a different search or clear it to see this view."
      : navigation.view === "continue" ? "Start watching or listening. Your saved progress will appear here on this browser."
        : emptyServer ? "The server has not indexed any video or audio yet. If a scan is running, check Server status."
          : "Try another folder or media view.";
    this.#dom.resultsSummary.textContent = loading ? "Loading…"
      : navigation.query
        ? `${library.total} ${library.total === 1 ? "result" : "results"} for “${navigation.query}”`
        : `${library.total} ${noun}`;
    this.renderBreadcrumbs();
    this.renderCards(cards);
    this.syncTabs();
  }

  #announceState(library, server, noun) {
    let message = "";
    if (library.status === "loading") {
      // First-page capabilities can connect the server while this same list is
      // still loading. Keep one announcement for that loading transition.
      if (["Connecting to the library.", "Loading the library."].includes(this.#liveMessage)) return;
      message = server.state === "connecting" ? "Connecting to the library." : "Loading the library.";
    } else if (library.status === "error") {
      message = `The library is unavailable. ${libraryErrorMessage(library.error, navigator.onLine)}`;
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

  async #continueWatchingPage(query, onFirstPage) {
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
      controller.signal.throwIfAborted();
      if (!first) {
        first = page;
        onFirstPage(first);
      }
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
    // Continue watching is always ordered by most recent progress; a Sort
    // control there would only add history entries without changing the list.
    const sort = this.#dom.sortControl.closest("label");
    if (sort) {
      const hide = navigation.view === "continue";
      if (hide && sort.contains(document.activeElement)) {
        this.#dom.tabs.find((tab) => tab.getAttribute("aria-selected") === "true")?.focus();
      }
      sort.hidden = hide;
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

  renderCards(cards = null) {
    const { library, playback, navigation } = this.#store.getState();
    this.#artworkQueue.clear();
    // Detached images may never emit load/error. Release their admission
    // slots explicitly so a slow old view cannot starve the current one.
    for (const request of this.#artworkRequests.values()) request.cancel();
    if (!cards) {
      const batches = this.#cardBatches(library.entries, navigation);
      let batch;
      do { batch = batches.next(); } while (!batch.done);
      cards = batch.value;
    }
    this.#cardsById = cards.byId;
    const current = this.#cardsById.get(String(playback.item?.id));
    current?.classList.add("playing");
    this.#dom.grid.replaceChildren(cards.fragment);
    this.#gridWidth = -1;
    this.#chunkHeights.clear();
    this.#sizeChunks();
    this.#scheduleArtwork();
  }

  *#cardBatches(entries, navigation) {
    const fragment = document.createDocumentFragment();
    const byId = new Map();
    const columns = getComputedStyle(this.#dom.grid).gridTemplateColumns.split(" ").length;
    this.#dom.grid.style.setProperty("--library-columns", columns);
    const chunkSize = entries.length >= 500
      && CSS.supports("content-visibility", "auto") ? columns * 8 : 0;
    const appendCard = (parent, card, entry) => {
      if (!chunkSize) {
        parent.append(card);
        return;
      }
      let chunk = parent.lastElementChild;
      if (!chunk?.classList.contains("media-chunk") || chunk.children.length >= chunkSize) {
        chunk = document.createElement("div");
        chunk.className = "media-chunk";
        // Only representative chunks need layout before exact reserved heights
        // are assigned in the same publication task.
        chunk.style.contentVisibility = "hidden";
        this.#chunkKeys.set(chunk, []);
        parent.append(chunk);
      }
      chunk.append(card);
      // Titles occupy a fixed two-line box, and filenames one non-wrapping
      // line. Metadata is the remaining variable-height card content. Continue
      // watching is small and has additional progress controls: measure it
      // independently instead of sharing its geometry.
      this.#chunkKeys.get(chunk).push(navigation.view === "continue" ? String(entry.id) : JSON.stringify([
        entry.entry_type, entry.kind, Boolean(entry.file_name && entry.file_name !== entry.title), mediaDetails(entry),
      ]));
    };
    // Read browser-local progress once per published list, not once per card.
    const hasMedia = entries.some((entry) => entry.entry_type === "media");
    const marks = {
      progress: navigation.view === "continue" ? this.#continueProgress : (hasMedia ? progressSnapshot() : null),
      watched: hasMedia ? watchedSnapshot() : new Set(),
    };
    let batchStarted = performance.now();
    let batchCount = 0;
    for (const entry of entries) {
      const card = entry.entry_type === "folder" ? this.#folderCard(entry) : this.#mediaCard(entry, marks);
      if (entry.entry_type === "media" && !byId.has(String(entry.id))) byId.set(String(entry.id), card);
      const collection = navigation.view === "library" && navigation.sort === "title"
        ? entry.collection : null;
      if (collection?.id && collection.title) {
        let section = fragment.lastElementChild;
        if (section?.dataset.collectionId !== collection.id) {
          section = document.createElement("section");
          section.className = "collection-group";
          section.dataset.collectionId = collection.id;
          const heading = document.createElement("h3");
          heading.id = `collection-${fragment.children.length}`;
          heading.className = "collection-heading";
          heading.textContent = collection.title;
          section.setAttribute("aria-labelledby", heading.id);
          section.append(heading);
          fragment.append(section);
        }
        appendCard(section, card, entry);
      } else {
        appendCard(fragment, card, entry);
      }
      batchCount += 1;
      if (entries.length >= 500 && (batchCount >= 128 || (batchCount % 16 === 0 && performance.now() - batchStarted >= 8))) {
        yield;
        batchStarted = performance.now();
        batchCount = 0;
      }
    }
    return { fragment, byId };
  }

  markCurrent(itemId) {
    for (const card of this.#dom.grid.querySelectorAll(".media-card.playing")) card.classList.remove("playing");
    const selected = this.#cardsById.get(String(itemId));
    selected?.classList.add("playing");
  }

  #folderCard(folder) {
    const article = document.createElement("article");
    article.className = "media-card folder";
    article.dataset.folderId = String(folder.id);
    const button = document.createElement("button");
    button.type = "button";
    button.className = "card-button";
    const count = Number(folder.child_count || 0);
    button.setAttribute("aria-label", `Open ${folder.title}, ${count} ${count === 1 ? "item" : "items"}`);
    button.addEventListener("click", () => this.navigate(
      { view: "folders", folder: folder.id, kind: "all", query: "" },
      { originKey: `f:${folder.id}` },
    ));
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

  #mediaCard(item, { progress = null, watched = new Set() } = {}) {
    const article = document.createElement("article");
    article.className = `media-card ${item.kind}`;
    article.dataset.mediaId = String(item.id);
    const button = document.createElement("button");
    button.type = "button";
    button.className = "card-button";
    // Never fall back to one storage read per card.
    const saved = progressDetails(item.id, progress instanceof Map ? progress : new Map());
    const marks = cardProgress({
      position: saved.position,
      duration: itemDuration(item) || saved.duration,
      watched: watched.has(String(item.id)),
    });
    const description = [mediaDetails(item), marks?.label].filter(Boolean).join(". ");
    button.setAttribute("aria-label", `Play ${item.title}. ${description}`.trim());
    button.addEventListener("click", () => {
      this.#onNavigate();
      this.snapshotQueue();
      this.#store.dispatch({ type: "NAVIGATE", navigation: { itemId: String(item.id), start: 0 } });
      window.history.replaceState(historyState(), "", navigationUrl(window.location.href, this.#store.getState().navigation, this.#store.getState().server.rootFolderId));
      this.#onSelect(item, { preserveQueue: true, focusPrompt: true });
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
    // Overlays keep every card's geometry identical for chunk measurement.
    // The button label carries the same text for assistive technology.
    if (marks?.state === "partial") {
      const bar = document.createElement("span");
      bar.className = "card-progress";
      bar.setAttribute("aria-hidden", "true");
      const fill = document.createElement("span");
      fill.className = "card-progress-fill";
      fill.style.setProperty("--card-progress", `${marks.percent}%`);
      bar.append(fill);
      art.append(bar);
    } else if (marks?.state === "watched") {
      const badge = document.createElement("span");
      badge.className = "card-watched";
      badge.setAttribute("aria-hidden", "true");
      badge.textContent = marks.label;
      art.append(badge);
    }
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
      const actions = document.createElement("div");
      actions.className = "progress-actions";
      const label = document.createElement("span");
      label.textContent = `${clockLabel(saved.position)} watched`;
      const clear = document.createElement("button");
      clear.type = "button";
      clear.textContent = "Clear progress";
      clear.setAttribute("aria-label", `Clear progress for ${item.title}`);
      clear.addEventListener("click", () => this.#clearProgress(item));
      actions.append(label, clear);
      article.append(actions);
    }
    return article;
  }

  #clearProgress(item) {
    const entries = this.#store.getState().library.entries;
    const index = entries.findIndex((entry) => String(entry.id) === String(item.id));
    // Re-rendering removes the focused button. Keep the keyboard position on
    // the neighbouring card instead of dropping focus to the document.
    const neighbor = index < 0 ? null : (entries[index + 1] ?? entries[index - 1] ?? null);
    clearProgress(item.id);
    this.#store.dispatch({ type: "LIBRARY_REMOVE_ENTRY", id: item.id });
    this.render();
    const card = neighbor ? this.#cardsById.get(String(neighbor.id)) : null;
    const target = card?.querySelector(".card-button") || this.#dom.libraryPanel;
    target.focus({ preventScroll: true });
    card?.scrollIntoView({ block: "nearest" });
    this.#liveMessage = `Progress cleared for ${item.title}.`;
    this.#dom.libraryLive.textContent = this.#liveMessage;
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
    const downloadUrl = originalDownloadUrl(item);
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
    const { library } = this.#store.getState();
    this.#store.dispatch({
      type: "QUEUE_REPLACE",
      entries: library.entries.filter((entry) => entry.entry_type === "media"),
      generation: library.generation,
    });
  }

  #setupArtworkLoading() {
    const schedule = () => this.#scheduleArtwork();
    // Capture also covers the independently scrolling landscape library.
    window.addEventListener("scroll", schedule, { passive: true, capture: true });
    window.addEventListener("scroll", () => this.#schedulePlaceSave(), { passive: true, capture: true });
    window.addEventListener("pagehide", () => this.#rememberPlace());
    window.addEventListener("resize", schedule);
    this.#dom.grid.addEventListener("contentvisibilityautostatechange", schedule);
    if (typeof window.ResizeObserver === "function") {
      this.#gridObserver = new ResizeObserver((entries) => {
        if (entries.some((entry) => entry.contentRect.width !== this.#gridWidth)) schedule();
      });
      this.#gridObserver.observe(this.#dom.grid);
    }
  }

  #scheduleArtwork() {
    if (this.#artworkFrame !== null) return;
    this.#artworkFrame = window.requestAnimationFrame(() => {
      this.#artworkFrame = null;
      this.#sizeChunks();
      this.#queueNearbyArtwork();
    });
  }

  #sizeChunks() {
    const width = this.#dom.grid.getBoundingClientRect().width;
    if (!width || width === this.#gridWidth) return;
    this.#gridWidth = width;
    const columns = getComputedStyle(this.#dom.grid).gridTemplateColumns.split(" ").length;
    this.#dom.grid.style.setProperty("--library-columns", columns);
    const chunks = [...this.#dom.grid.querySelectorAll(".media-chunk")];
    const widthKey = `${width}:${columns}`;
    let heights = this.#chunkHeights.get(widthKey);
    if (!heights) {
      heights = new Map();
      // Two widths cover switching Browse/Watch or returning after resize.
      // The cache is also cleared whenever the published list changes.
      if (this.#chunkHeights.size >= 2) this.#chunkHeights.delete(this.#chunkHeights.keys().next().value);
      this.#chunkHeights.set(widthKey, heights);
    }
    const keys = chunks.map((chunk) => JSON.stringify(this.#chunkKeys.get(chunk)));
    const representatives = new Map();
    chunks.forEach((chunk, index) => {
      chunk.style.contentVisibility = "hidden";
      const key = keys[index];
      if (!heights.has(key) && !representatives.has(key)) representatives.set(key, chunk);
    });
    for (const chunk of representatives.values()) chunk.style.contentVisibility = "visible";
    for (const [key, chunk] of representatives) heights.set(key, chunk.getBoundingClientRect().height);
    chunks.forEach((chunk, index) => {
      chunk.style.setProperty("--chunk-height", `${heights.get(keys[index])}px`);
      chunk.classList.add("sized");
      chunk.style.removeProperty("content-visibility");
    });
  }

  #queueNearbyArtwork() {
    this.#artworkQueue.clear();
    const visit = (parent) => {
      for (const element of parent.children) {
        if (element.classList.contains("collection-group")) {
          // The non-subgrid fallback uses display: contents and has no box.
          visit(element);
          continue;
        }
        const bounds = element.getBoundingClientRect();
        if (bounds.width <= 0 || bounds.height <= 0
          || bounds.bottom < -400 || bounds.top > window.innerHeight + 400) continue;
        if (element.classList.contains("media-card")) {
          const image = element.querySelector("img[data-src]");
          if (image) this.#artworkQueue.add(image);
        } else if (element.classList.contains("media-chunk")) {
          visit(element);
        }
      }
    };
    // Skip distant batches without reading geometry inside skipped subtrees.
    visit(this.#dom.grid);
    for (const request of this.#artworkRequests.values()) request.checkVisibility();
    this.#drainArtworkQueue();
  }

  #drainArtworkQueue() {
    for (const image of this.#artworkQueue) {
      const bounds = image.closest(".media-card").getBoundingClientRect();
      if (!image.isConnected || !image.dataset.src || bounds.width <= 0 || bounds.height <= 0
        || bounds.bottom < -400 || bounds.top > window.innerHeight + 400) this.#artworkQueue.delete(image);
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
      this.#startArtwork(image);
    }
  }

  #startArtwork(image) {
    const source = image.dataset.src;
    if (!source) return;
    delete image.dataset.src;
    let settled = false;
    let offscreenTimer = null;
    let objectUrl = null;
    let transportDone = false;
    const controller = new AbortController();
    const release = () => {
      this.#artworkRequests.delete(image);
      // Let aborted network operations settle before starting the next batch.
      // Recompute visibility too: scrolling may have superseded the old queue.
      this.#scheduleArtwork();
    };
    const deadline = window.setTimeout(() => expire(), ARTWORK_DEADLINE_MS);
    const settle = (discard = false) => {
      if (settled) return;
      settled = true;
      window.clearTimeout(deadline);
      window.clearTimeout(offscreenTimer);
      controller.abort();
      image.removeEventListener("load", settle);
      image.removeEventListener("error", failed);
      // Close the old network load before admitting its replacement. Each DOM
      // image gets one attempt per view, so expiry cannot cause a retry loop.
      if (discard === true) image.removeAttribute("src");
      if (objectUrl) URL.revokeObjectURL(objectUrl);
      if (transportDone) release();
    };
    const failed = () => {
      if (settled) return;
      image.classList.add("failed");
      settle();
    };
    const expire = () => {
      if (settled) return;
      image.classList.add("failed");
      settle(true);
    };
    this.#artworkRequests.set(image, {
      cancel: () => settle(true),
      checkVisibility: () => {
        if (settled) return;
        if (!image.isConnected) { settle(true); return; }
        const bounds = image.closest(".media-card").getBoundingClientRect();
        const nearby = bounds.width > 0 && bounds.height > 0
          && bounds.bottom >= -400 && bounds.top <= window.innerHeight + 400;
        if (nearby) {
          window.clearTimeout(offscreenTimer);
          offscreenTimer = null;
        } else if (offscreenTimer === null) {
          // Ordinary scrolling back to a healthy slow image retains its load;
          // repeated scroll events must not extend an abandoned request forever.
          offscreenTimer = window.setTimeout(expire, ARTWORK_OFFSCREEN_GRACE_MS);
        }
      },
    });
    image.addEventListener("load", settle, { once: true });
    image.addEventListener("error", failed, { once: true });
    // Visibility and concurrency are already controlled by this queue.
    image.loading = "eager";
    void fetchArtwork(source, controller.signal).then((blob) => {
      if (settled) return;
      objectUrl = URL.createObjectURL(blob);
      image.src = objectUrl;
      window.queueMicrotask(() => {
        // A cached failure can already be complete before its error event.
        if (!settled && image.complete) {
          if (image.naturalWidth === 0) failed();
          else settle();
        }
      });
    }).catch(failed).finally(() => {
      transportDone = true;
      if (settled) release();
    });
  }

  #bind() {
    this.#dom.libraryClearSearch.addEventListener("click", () => {
      this.navigate({ query: "" }, { history: "replace", focusAfterLoad: false });
      this.#dom.searchInput.focus();
    });
    this.#dom.libraryRetry.addEventListener("click", () => this.load());
    this.#dom.libraryRetryTop.addEventListener("click", () => this.load());
    window.addEventListener("online", () => {
      // Reconnecting retries only a load that failed in transport. Server
      // answers keep their manual Retry; a newer load supersedes this one.
      const { library } = this.#store.getState();
      if (library.status !== "error") return;
      if (library.error?.name === "ApiError" && library.error.code !== "network") return;
      void this.load();
    });
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
