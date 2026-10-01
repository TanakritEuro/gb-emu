// Run with: node --test "web/*.test.js"
import test from "node:test";
import assert from "node:assert/strict";
import { hex, registerPairs, flagStates } from "./debugger.js";

test("hex pads and upper-cases", () => {
  assert.equal(hex(0x3e, 2), "3E");
  assert.equal(hex(0x1, 4), "0001");
  assert.equal(hex(0xfffe, 4), "FFFE");
});

test("register pairs put the first register in the high byte", () => {
  // The state the boot ROM leaves behind on a DMG.
  const state = { a: 0x01, f: 0xb0, b: 0x00, c: 0x13, d: 0x00, e: 0xd8, h: 0x01, l: 0x4d, sp: 0xfffe, pc: 0x0100 };
  assert.deepEqual(registerPairs(state), [
    ["AF", "01B0"],
    ["BC", "0013"],
    ["DE", "00D8"],
    ["HL", "014D"],
    ["SP", "FFFE"],
    ["PC", "0100"],
  ]);
});

test("flags come from the top four bits of F, Z first", () => {
  assert.deepEqual(flagStates(0xb0), [["Z", true], ["N", false], ["H", true], ["C", true]]);
  assert.deepEqual(flagStates(0x40), [["Z", false], ["N", true], ["H", false], ["C", false]]);
  assert.deepEqual(flagStates(0x0f), [["Z", false], ["N", false], ["H", false], ["C", false]]);
});
