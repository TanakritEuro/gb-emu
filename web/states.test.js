// Run with: node --test "web/*.test.js"
import test from "node:test";
import assert from "node:assert/strict";
import { SLOTS, slotKey, StateStore, memoryBackend, savedAgo, slotForKey } from "./states.js";

const GAME = "gb-emu:save:00112233";
const OTHER = "gb-emu:save:ffeeddcc";

test("each game's slots have their own keys", () => {
  assert.equal(slotKey(GAME, 1), "gb-emu:state:00112233:1");
  assert.equal(slotKey(GAME, 4), "gb-emu:state:00112233:4");
  assert.notEqual(slotKey(GAME, 1), slotKey(OTHER, 1));
  assert.notEqual(slotKey(GAME, 1), GAME, "never the battery save's key");
});

test("states are stored per slot and per game", async () => {
  const store = new StateStore(memoryBackend());
  const record = { state: new Uint8Array([1, 2, 3]), thumb: null, savedAt: 1000 };
  await store.save(GAME, 2, record);
  assert.deepEqual(await store.load(GAME, 2), record);
  assert.equal(await store.load(GAME, 1), null, "other slots are empty");
  assert.equal(await store.load(OTHER, 2), null, "other games don't see it");
  const slots = await store.list(GAME);
  assert.equal(slots.length, SLOTS);
  assert.deepEqual(slots, [null, record, null, null]);
});

test("saving again replaces a slot; removing empties it", async () => {
  const store = new StateStore(memoryBackend());
  await store.save(GAME, 1, { state: new Uint8Array([1]), thumb: null, savedAt: 1 });
  await store.save(GAME, 1, { state: new Uint8Array([2]), thumb: null, savedAt: 2 });
  assert.deepEqual((await store.load(GAME, 1)).state, new Uint8Array([2]));
  await store.remove(GAME, 1);
  assert.equal(await store.load(GAME, 1), null);
});

test("slot times read naturally", () => {
  const now = Date.UTC(2026, 9, 1, 12, 0, 0);
  assert.equal(savedAgo(now - 10_000, now), "just now");
  assert.equal(savedAgo(now - 5 * 60_000, now), "5 min ago");
  assert.equal(savedAgo(now - 59 * 60_000, now), "59 min ago");
  assert.equal(savedAgo(now - 3 * 3600_000, now), "3 h ago");
  assert.match(savedAgo(now - 3 * 86400_000, now), /2026/, "older: the date");
});

test("keys 1 to 4 pick a slot", () => {
  assert.equal(slotForKey("Digit1"), 1);
  assert.equal(slotForKey("Digit4"), 4);
  assert.equal(slotForKey("Numpad3"), 3);
  assert.equal(slotForKey("Digit5"), null, "only four slots");
  assert.equal(slotForKey("Digit0"), null);
  assert.equal(slotForKey("KeyS"), null);
});
