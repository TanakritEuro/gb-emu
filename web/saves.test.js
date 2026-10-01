// Run with: node --test "web/*.test.js"
import test from "node:test";
import assert from "node:assert/strict";
import { saveKey, encode, decode, readSave, writeSave, saveFileName, nowSeconds } from "./saves.js";

/** A localStorage stand-in; `quota` makes setItem throw like a full store. */
function fakeStorage({ quota = Infinity } = {}) {
  const items = new Map();
  return {
    items,
    getItem: (k) => (items.has(k) ? items.get(k) : null),
    setItem: (k, v) => {
      if (v.length > quota) throw new Error("QuotaExceededError");
      items.set(k, String(v));
    },
  };
}

test("saves round-trip through base64, including large ones", () => {
  for (const size of [0, 1, 512, 8192, 128 * 1024 + 48]) {
    const bytes = Uint8Array.from({ length: size }, (_, i) => (i * 37 + 11) & 0xff);
    assert.deepEqual(decode(encode(bytes)), bytes, `${size} bytes`);
  }
});

test("each ROM gets its own key", async () => {
  const a = await saveKey(new Uint8Array([1, 2, 3]));
  const b = await saveKey(new Uint8Array([1, 2, 4]));
  assert.notEqual(a, b);
  assert.equal(a, await saveKey(new Uint8Array([1, 2, 3])), "same ROM, same key");
  assert.match(a, /^gb-emu:save:[0-9a-f]{32}$/);
});

test("write then read gives the same bytes", () => {
  const storage = fakeStorage();
  const bytes = new Uint8Array([0, 0x42, 0xff, 7]);
  assert.equal(writeSave(storage, "k", bytes), true);
  assert.deepEqual(readSave(storage, "k"), bytes);
  assert.equal(readSave(storage, "missing"), null);
});

test("storage trouble doesn't throw", () => {
  const full = fakeStorage({ quota: 4 });
  assert.equal(writeSave(full, "k", new Uint8Array(100)), false, "quota exceeded");
  assert.equal(readSave(null, "k"), null, "no storage at all");
  const broken = { getItem: () => { throw new Error("SecurityError"); } };
  assert.equal(readSave(broken, "k"), null);
  const garbage = fakeStorage();
  garbage.items.set("k", "not base64 !!!");
  assert.equal(readSave(garbage, "k"), null, "corrupt entry");
});

test("export file names come from the title", () => {
  assert.equal(saveFileName("TOBU"), "TOBU.sav");
  assert.equal(saveFileName("POKEMON RED"), "POKEMON RED.sav");
  assert.equal(saveFileName("a/b:c?"), "abc.sav");
  assert.equal(saveFileName(""), "game.sav");
});

test("time is whole Unix seconds", () => {
  const t = nowSeconds();
  assert.ok(Number.isInteger(t));
  assert.ok(Math.abs(t - Date.now() / 1000) < 2);
});
