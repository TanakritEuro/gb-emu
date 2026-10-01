// Save state slots: a few per game, kept in IndexedDB between visits (it
// holds bytes and images as they are, and has far more room than
// localStorage). Each slot holds the state, a picture of the screen and when
// it was saved. The storage backend is passed in, so tests use memoryBackend.

export const SLOTS = 4;

const DB_NAME = "gb-emu";
const STORE = "states";

/** The key for `slot` (1-based) of the game whose battery save key is
 * `romKey` (see saves.js): "gb-emu:save:<hash>" → "gb-emu:state:<hash>:2". */
export function slotKey(romKey, slot) {
  return `${romKey.replace(/^gb-emu:save:/, "gb-emu:state:")}:${slot}`;
}

export class StateStore {
  /** `backend`: { get(key), put(key, value), delete(key) }, all async. */
  constructor(backend) {
    this.backend = backend;
  }

  /** Every slot of a game, in order: { state, thumb, savedAt } or null. */
  async list(romKey) {
    const slots = [];
    for (let slot = 1; slot <= SLOTS; slot++) slots.push(await this.load(romKey, slot));
    return slots;
  }

  /** `record`: { state (Uint8Array), thumb (a Blob, or null), savedAt (ms) }. */
  async save(romKey, slot, record) {
    await this.backend.put(slotKey(romKey, slot), record);
  }

  async load(romKey, slot) {
    return (await this.backend.get(slotKey(romKey, slot))) ?? null;
  }

  async remove(romKey, slot) {
    await this.backend.delete(slotKey(romKey, slot));
  }
}

/** A backend that forgets everything on reload: for tests, and for browsers
 * that refuse IndexedDB (some private modes). */
export function memoryBackend() {
  const items = new Map();
  return {
    persistent: false,
    items,
    get: async (key) => items.get(key),
    put: async (key, value) => void items.set(key, value),
    delete: async (key) => void items.delete(key),
  };
}

/** The IndexedDB backend; rejects if the browser won't open a database. */
export function indexedDbBackend(indexedDB = globalThis.indexedDB) {
  const db = new Promise((resolve, reject) => {
    if (!indexedDB) return reject(new Error("no IndexedDB"));
    const open = indexedDB.open(DB_NAME, 1);
    open.onupgradeneeded = () => open.result.createObjectStore(STORE);
    open.onsuccess = () => resolve(open.result);
    open.onerror = () => reject(open.error);
  });
  // One request in its own transaction, as a promise.
  const run = (mode, op) =>
    db.then(
      (d) =>
        new Promise((resolve, reject) => {
          const tx = d.transaction(STORE, mode);
          const req = op(tx.objectStore(STORE));
          tx.oncomplete = () => resolve(req.result);
          tx.onerror = tx.onabort = () => reject(tx.error ?? req.error);
        }),
    );
  return {
    persistent: true,
    ready: db,
    get: (key) => run("readonly", (s) => s.get(key)),
    put: (key, value) => run("readwrite", (s) => s.put(value, key)),
    delete: (key) => run("readwrite", (s) => s.delete(key)),
  };
}

/** "just now", "5 min ago", "3 h ago", then the date: when a slot was saved. */
export function savedAgo(savedAt, now = Date.now()) {
  const minutes = Math.floor((now - savedAt) / 60_000);
  if (minutes < 1) return "just now";
  if (minutes < 60) return `${minutes} min ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours} h ago`;
  return new Date(savedAt).toLocaleDateString(undefined, { day: "numeric", month: "short", year: "numeric" });
}

/** The slot cards on the page: a picture, when, and Save / Load buttons. */
export class SlotsPanel {
  /** `els`: { slots (a container), status }; `actions`: { save(slot),
   * load(slot), select(slot) }. */
  constructor(els, actions) {
    this.els = els;
    this.cards = [];
    this.urls = []; // thumbnail object URLs, freed when replaced
    for (let slot = 1; slot <= SLOTS; slot++) {
      const card = document.createElement("figure");
      card.className = "slot";
      const img = document.createElement("img");
      img.alt = "";
      img.width = 160;
      img.height = 144;
      const caption = document.createElement("figcaption");
      const buttons = document.createElement("div");
      const save = button("Save", `Save a state in slot ${slot} (select it with ${slot}, then S)`);
      const load = button("Load", `Load slot ${slot} (select it with ${slot}, then L)`);
      save.addEventListener("click", () => actions.save(slot));
      load.addEventListener("click", () => actions.load(slot));
      card.addEventListener("click", () => actions.select(slot));
      buttons.append(save, load);
      card.append(img, caption, buttons);
      els.slots.append(card);
      this.cards.push({ card, img, caption, save, load });
    }
  }

  /** Shows `records` (from StateStore.list, or [] without a game). */
  show(records, loaded) {
    for (const url of this.urls) URL.revokeObjectURL(url);
    this.urls = [];
    this.cards.forEach((c, i) => {
      const r = records[i] ?? null;
      c.caption.textContent = `${i + 1} · ${r ? savedAgo(r.savedAt) : "empty"}`;
      c.card.classList.toggle("empty", !r);
      if (r?.thumb) {
        const url = URL.createObjectURL(r.thumb);
        this.urls.push(url);
        c.img.src = url;
      } else {
        c.img.removeAttribute("src");
      }
      c.save.disabled = !loaded;
      c.load.disabled = !loaded || !r;
    });
  }

  select(slot) {
    this.cards.forEach((c, i) => c.card.classList.toggle("selected", i + 1 === slot));
  }

  status(text, isError = false) {
    this.els.status.textContent = text;
    this.els.status.classList.toggle("error-text", isError);
  }
}

function button(text, title) {
  const b = document.createElement("button");
  b.type = "button";
  b.textContent = text;
  b.title = title;
  return b;
}

/** The slot keys 1-4 pick, from a KeyboardEvent.code, or null. */
export function slotForKey(code) {
  const m = /^(?:Digit|Numpad)([1-9])$/.exec(code);
  const slot = m ? Number(m[1]) : 0;
  return slot >= 1 && slot <= SLOTS ? slot : null;
}
