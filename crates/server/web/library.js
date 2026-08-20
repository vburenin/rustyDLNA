import { clockLabel, itemDuration, mediaDetails, navigationUrl, resumePosition } from "./core.js";
import { clearProgress, progressDetails } from "./preferences.js";

export class LibraryController {
  #store;
  #api;
  #dom;
  #onSelect;
  #request = 0;
  #searchTimer = null;
  #queueController = null;
  #continueController = null;
  #focusAfterLoad = false;

  constructor({ store, api, dom, onSelect }) {
    this.#store = store;
    this.#api = api;
    this.#dom = dom;
    this.#onSelect = onSelect;
    this.#bind();
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

  navigate(navigation, { history = "push", focusAfterLoad = true } = {}) {
    this.cancelPendingSearch();
    this.#api.abortLibrary();
    this.#continueController?.abort();
    this.#focusAfterLoad = focusAfterLoad;
    this.#store.dispatch({ type: "NAVIGATE", navigation });
    const state = this.#store.getState();
    this.#dom.searchInput.value = state.navigation.query;
    this.#dom.sortControl.value = state.navigation.sort;
    this.syncTabs();
    if (history !== "none") {
      const target = navigationUrl(window.location.href, state.navigation, state.server.rootFolderId);
      history === "replace" ? window.history.replaceState({}, "", target) : window.history.pushState({}, "", target);
    }
    return this.load({ reset: true });
  }

  async load({ reset = false } = {}) {
    const state = this.#store.getState();
    if (!reset && (state.library.status === "loading_more" || !state.library.hasMore)) return;
    const requestId = ++this.#request;
    this.#store.dispatch({ type: "LIBRARY_LOADING", append: !reset, requestId });
    const current = this.#store.getState();
    const offset = reset ? 0 : current.library.offset;
    const generation = reset ? null : current.library.generation;
    try {
      const payload = current.navigation.view === "continue"
        ? await this.#continueWatchingPage(current.navigation)
        : await this.#api.library(current.navigation, { offset, generation, replace: reset });
      this.#store.dispatch({ type: "LIBRARY_SUCCESS", append: !reset, requestId, payload });
      if (current.navigation.view === "folders" && !current.navigation.folder) {
        this.#store.dispatch({ type: "NAVIGATE", navigation: { folder: payload.root_folder_id } });
      }
      this.render();
      if (this.#focusAfterLoad) {
        this.#focusAfterLoad = false;
        this.#dom.libraryPanel.focus({ preventScroll: true });
      }
    } catch (error) {
      if (error?.name === "AbortError") return;
      this.#store.dispatch({ type: "LIBRARY_ERROR", requestId, error });
      this.render();
    }
  }

  render() {
    const state = this.#store.getState();
    const { library, navigation, server, playback } = state;
    document.title = playback.item ? `${playback.item.title} · ${server.name}` : `${server.name} · Library`;
    this.#dom.serverName.textContent = server.name;
    this.#dom.serverState.dataset.state = server.state;
    this.#dom.libraryRetryTop.hidden = library.status !== "error";
    this.#dom.loading.hidden = !["loading", "loading_more"].includes(library.status);
    this.#dom.loadMore.hidden = library.status !== "ready" || !library.hasMore;
    this.#dom.libraryEmpty.hidden = !["ready", "error"].includes(library.status)
      || (library.status === "ready" && library.total > 0);
    this.#dom.libraryRetry.hidden = library.status !== "error";
    this.#dom.searchInput.placeholder = navigation.view === "folders" ? "Filter this folder…" : "Search titles, artists, albums…";
    if (library.status === "error") {
      this.#dom.libraryEmptyTitle.textContent = "Could not load the library";
      this.#dom.libraryEmptyDetail.textContent = friendlyLibraryError(library.error);
      this.#dom.libraryCount.textContent = "Library unavailable";
      this.#dom.libraryPanel.setAttribute("aria-busy", "false");
      return;
    }
    this.#dom.libraryPanel.setAttribute("aria-busy", String(["loading", "loading_more"].includes(library.status)));
    const noun = navigation.view === "folders" ? (library.total === 1 ? "entry" : "entries") : (library.total === 1 ? "item" : "items");
    this.#dom.libraryCount.textContent = library.status === "loading" ? "Connecting…" : `${library.total} ${noun}`;
    this.#dom.libraryEmptyTitle.textContent = navigation.query ? `No results for “${navigation.query}”` : "No media found";
    this.#dom.libraryEmptyDetail.textContent = navigation.query ? "Try a different search." : "This view is empty.";
    this.#dom.resultsSummary.textContent = navigation.query
      ? `${library.total} ${library.total === 1 ? "result" : "results"} for “${navigation.query}”`
      : `${library.total} ${noun}`;
    this.renderBreadcrumbs();
    this.renderCards();
    this.syncTabs();
  }

  async #continueWatchingPage(navigation) {
    this.#continueController?.abort();
    const controller = new AbortController();
    this.#continueController = controller;
    const context = { ...navigation, view: "library", kind: "all" };
    let offset = 0;
    let generation = null;
    let first = null;
    const entries = [];
    do {
      const page = await this.#api.library(context, {
        offset,
        limit: 200,
        generation,
        replace: false,
        signal: controller.signal,
      });
      first ||= page;
      generation = page.generation;
      entries.push(...page.entries.filter((entry) => entry.entry_type === "media"));
      offset += page.entries.length;
      if (!page.has_more || page.entries.length === 0) break;
    } while (true);
    const resumable = entries
      .filter((item) => {
        const progress = progressDetails(item.id);
        return resumePosition(progress.position, itemDuration(item) || progress.duration) > 0;
      })
      .sort((left, right) => progressDetails(right.id).updated - progressDetails(left.id).updated);
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

  renderCards() {
    const { library, playback } = this.#store.getState();
    this.#dom.grid.replaceChildren();
    for (const entry of library.entries) {
      const card = entry.entry_type === "folder" ? this.#folderCard(entry) : this.#mediaCard(entry);
      if (entry.entry_type === "media" && String(entry.id) === String(playback.item?.id)) card.classList.add("playing");
      this.#dom.grid.append(card);
    }
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
      image.alt = "";
      image.src = item.art_url;
      image.addEventListener("error", () => image.classList.add("failed"), { once: true });
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
      const progress = progressDetails(item.id);
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
    this.#dom.itemDetailsSummary.textContent = item.summary || "";
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
    this.#queueController = new AbortController();
    const state = this.#store.getState();
    const context = { ...state.navigation };
    const generation = state.library.generation;
    const entries = state.library.entries.filter((entry) => entry.entry_type === "media");
    this.#store.dispatch({ type: "QUEUE_LOADING", entries, generation });
    if (!state.library.hasMore) {
      this.#store.dispatch({ type: "QUEUE_SUCCESS", entries, generation });
      return;
    }
    this.#completeQueue(context, generation, state.library.offset, state.library.total, entries, this.#queueController.signal);
  }

  async #completeQueue(context, generation, offset, total, initial, signal) {
    const entries = [...initial];
    try {
      while (offset < total) {
        const payload = await this.#api.library(context, { offset, limit: 200, generation, replace: false, signal });
        entries.push(...payload.entries.filter((entry) => entry.entry_type === "media"));
        const advanced = payload.entries.length;
        if (advanced === 0) break;
        offset += advanced;
      }
      this.#store.dispatch({ type: "QUEUE_SUCCESS", entries, generation });
    } catch (error) {
      if (error?.name === "AbortError") return;
      this.#store.dispatch({ type: "QUEUE_ERROR", error });
    }
  }

  #bind() {
    this.#dom.loadMore.addEventListener("click", () => this.load({ reset: false }));
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
