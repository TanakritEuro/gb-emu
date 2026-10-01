// Run with: node --test "web/*.test.js"
import test from "node:test";
import assert from "node:assert/strict";
import { BreakpointList } from "./breakpoints.js";

/** Stands in for an Emulator: just its breakpoints. */
function fakeEmu() {
  const set = new Set();
  return {
    set,
    set_breakpoint(addr, on) {
      if (on) set.add(addr);
      else set.delete(addr);
    },
  };
}

test("breakpoints are kept in the list and in the emulator", () => {
  const list = new BreakpointList();
  const emu = fakeEmu();
  list.set(0x0153, true, emu);
  list.toggle(0x0150, emu);
  assert.deepEqual(list.sorted(), [0x0150, 0x0153]);
  assert.deepEqual([...emu.set].sort((a, b) => a - b), [0x0150, 0x0153]);
  list.toggle(0x0150, emu);
  assert.ok(!list.has(0x0150));
  assert.deepEqual([...emu.set], [0x0153]);
});

test("a new emulator gets the whole list", () => {
  const list = new BreakpointList();
  list.set(0x0040, true, fakeEmu());
  list.set(0x4abc, true, fakeEmu());
  const fresh = fakeEmu(); // e.g. after Reset
  list.applyTo(fresh);
  assert.deepEqual([...fresh.set].sort((a, b) => a - b), [0x0040, 0x4abc]);
});

test("clearing empties both", () => {
  const list = new BreakpointList();
  const emu = fakeEmu();
  list.set(1, true, emu);
  list.set(2, true, emu);
  list.clear(emu);
  assert.deepEqual(list.sorted(), []);
  assert.equal(emu.set.size, 0);
});

test("works before there is an emulator", () => {
  const list = new BreakpointList();
  list.set(0x0150, true, null);
  assert.ok(list.has(0x0150));
});
