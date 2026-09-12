import test from "node:test";
import assert from "node:assert/strict";
import { initialState, Store } from "./store.js";

test("first-page capabilities update negotiation without publishing a partial list or queue", () => {
  const store = new Store(initialState({}, {}));
  store.dispatch({ type: "LIBRARY_LOADING", requestId: 1 });
  const payload = {
    generation: 17, server_name: "Test", root_folder_id: "64", library_state: "ready",
    capabilities: { transcoding: true }, entries: [{ id: "3" }], total: 400,
  };
  store.dispatch({ type: "LIBRARY_CAPABILITIES", requestId: 1, payload });
  assert.equal(store.getState().server.generation, 17);
  assert.equal(store.getState().server.negotiationEpoch, 1);
  assert.equal(store.getState().library.status, "loading");
  assert.deepEqual(store.getState().library.entries, []);
  assert.deepEqual(store.getState().queue.entries, []);
  store.dispatch({ type: "LIBRARY_ERROR", requestId: 1, error: new Error("page two unavailable") });
  assert.equal(store.getState().server.capabilities.transcoding, true);
  assert.equal(store.getState().server.negotiationEpoch, 1);
  store.dispatch({ type: "LIBRARY_LOADING", requestId: 2 });
  store.dispatch({ type: "LIBRARY_CAPABILITIES", requestId: 1, payload: {
    ...payload, capabilities: { transcoding: false },
  } });
  assert.equal(store.getState().server.capabilities.transcoding, true);
  store.dispatch({ type: "LIBRARY_CAPABILITIES", requestId: 2, payload: {
    ...payload, generation: 18, capabilities: { transcoding: false },
  } });
  assert.equal(store.getState().server.negotiationEpoch, 2);
  assert.equal(store.getState().server.generation, 18);
});

test("complete-list publication retains the first-page negotiation epoch and linked queue", () => {
  const store = new Store(initialState({}, {}));
  const item = { id: "3" };
  const payload = {
    generation: 17, capabilities: { transcoding: true },
    entries: [item, { id: "4" }], total: 2,
  };
  store.dispatch({ type: "LIBRARY_LOADING", requestId: 1 });
  store.dispatch({ type: "LIBRARY_CAPABILITIES", requestId: 1, payload });
  store.dispatch({ type: "QUEUE_REPLACE", entries: [item], generation: null });
  store.dispatch({ type: "LIBRARY_SUCCESS", requestId: 1, payload });
  assert.equal(store.getState().server.negotiationEpoch, 1);
  assert.equal(store.getState().library.status, "ready");
  assert.deepEqual(store.getState().library.entries, payload.entries);
  assert.deepEqual(store.getState().queue.entries, [item]);
});
