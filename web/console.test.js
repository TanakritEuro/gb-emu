// Run with: node --test "web/*.test.js"
import test from "node:test";
import assert from "node:assert/strict";
import {
  AUTO_PALETTE,
  paletteKey,
  readConsole,
  readPalette,
  rgb555ToCss,
  writeConsole,
  writePalette,
} from "./console.js";

/** A localStorage stand-in. */
function memory() {
  const data = new Map();
  return {
    data,
    getItem: (k) => (data.has(k) ? data.get(k) : null),
    setItem: (k, v) => data.set(k, String(v)),
    removeItem: (k) => data.delete(k),
  };
}

/** Storage that throws on every call, like a blocked one. */
const broken = {
  getItem() {
    throw new Error("blocked");
  },
  setItem() {
    throw new Error("blocked");
  },
  removeItem() {
    throw new Error("blocked");
  },
};

test("original games run on the original unless the Color was picked", () => {
  const s = memory();
  assert.equal(readConsole(s), "dmg");
  writeConsole(s, "cgb");
  assert.equal(readConsole(s), "cgb");
  writeConsole(s, "something else");
  assert.equal(readConsole(s), "dmg");
  assert.equal(readConsole(null), "dmg", "no storage at all");
  assert.equal(readConsole(broken), "dmg");
  writeConsole(broken, "cgb"); // doesn't throw
});

test("palettes are remembered per game", () => {
  const s = memory();
  const tetris = "gb-emu:save:00aa";
  const zelda = "gb-emu:save:11bb";
  assert.equal(paletteKey(tetris), "gb-emu:palette:00aa");
  assert.equal(readPalette(s, tetris, 12), AUTO_PALETTE, "nothing picked yet");
  writePalette(s, tetris, 4);
  assert.equal(readPalette(s, tetris, 12), 4);
  assert.equal(readPalette(s, zelda, 12), AUTO_PALETTE);
  writePalette(s, tetris, AUTO_PALETTE);
  assert.equal(s.data.size, 0, "back to automatic forgets it");
});

test("a stored palette that doesn't fit counts as automatic", () => {
  const s = memory();
  const key = "gb-emu:save:00aa";
  for (const bad of ["12", "-3", "1.5", "red"]) {
    s.setItem(paletteKey(key), bad);
    assert.equal(readPalette(s, key, 12), AUTO_PALETTE, bad);
  }
  assert.equal(readPalette(broken, key, 12), AUTO_PALETTE);
  writePalette(broken, key, 3); // doesn't throw
});

test("RGB555 widens to 8 bits a channel like the emulator's picture", () => {
  assert.equal(rgb555ToCss(0x7fff), "rgb(255, 255, 255)");
  assert.equal(rgb555ToCss(0x0000), "rgb(0, 0, 0)");
  assert.equal(rgb555ToCss(0x001f), "rgb(255, 0, 0)");
  assert.equal(rgb555ToCss(0x1bef), "rgb(123, 255, 49)", "the default green");
  // The default blue: 198, where the usual quote (#0063C5) scales by 255/31.
  assert.equal(rgb555ToCss(0x6180), "rgb(0, 99, 198)");
});
