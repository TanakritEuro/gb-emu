// Run with: node --test "web/*.test.js"
import test from "node:test";
import assert from "node:assert/strict";
import {
  regionName,
  ioName,
  parseAddress,
  clampStart,
  startAround,
  asciiChar,
  LAST_START,
} from "./memview.js";

test("every part of the memory map has a name", () => {
  assert.equal(regionName(0x0000), "ROM bank 0");
  assert.equal(regionName(0x4000, 5), "ROM bank 5");
  assert.equal(regionName(0x7fff, 5), "ROM bank 5");
  assert.equal(regionName(0x8000), "VRAM tile data");
  assert.equal(regionName(0x9800), "VRAM tile map 0");
  assert.equal(regionName(0x9c00), "VRAM tile map 1");
  assert.equal(regionName(0xa000), "cartridge RAM");
  assert.equal(regionName(0xc000), "work RAM");
  assert.equal(regionName(0xdfff), "work RAM");
  assert.equal(regionName(0xe000), "echo of work RAM");
  assert.equal(regionName(0xfe00), "OAM (sprites)");
  assert.equal(regionName(0xfea0), "unusable");
  assert.equal(regionName(0xff44), "I/O registers");
  assert.equal(regionName(0xff80), "HRAM");
  assert.equal(regionName(0xffff), "IE");
});

test("hardware registers have their Pan Docs names", () => {
  assert.equal(ioName(0xff44), "LY");
  assert.equal(ioName(0xff40), "LCDC");
  assert.equal(ioName(0xff0f), "IF");
  assert.equal(ioName(0xff26), "NR52");
  assert.equal(ioName(0xff35), "wave RAM");
  assert.equal(ioName(0xffff), "IE");
  assert.equal(ioName(0xff03), "", "nothing there");
  assert.equal(ioName(0xc000), "");
});

test("addresses can be typed as hex, registers or hardware names", () => {
  assert.equal(parseAddress("C000"), 0xc000);
  assert.equal(parseAddress(" $ff44 "), 0xff44);
  assert.equal(parseAddress("0x150"), 0x150);
  assert.equal(parseAddress("0"), 0);
  assert.equal(parseAddress("pc", { PC: 0x0150 }), 0x0150);
  assert.equal(parseAddress("ly"), 0xff44);
  assert.equal(parseAddress("JOYP"), 0xff00);
  assert.equal(parseAddress("P1"), 0xff00);
  assert.equal(parseAddress("10000"), null, "too big");
  assert.equal(parseAddress("hello"), null);
  assert.equal(parseAddress(""), null);
});

test("the view starts on a row boundary and never runs past $FFFF", () => {
  assert.equal(clampStart(0xc012), 0xc010);
  assert.equal(clampStart(-0x100), 0);
  assert.equal(clampStart(0xfff0), LAST_START);
  assert.equal(LAST_START, 0xff00);
});

test("following a register keeps it a few rows down", () => {
  assert.equal(startAround(0xc0a5), 0xc060);
  assert.equal(startAround(0x0010), 0, "can't go above $0000");
  assert.equal(startAround(0xfffe), 0xff00, "the stack at the top of HRAM");
});

test("bytes show as ASCII only when printable", () => {
  assert.equal(asciiChar(0x41), "A");
  assert.equal(asciiChar(0x20), " ");
  assert.equal(asciiChar(0x00), ".");
  assert.equal(asciiChar(0x7f), ".");
  assert.equal(asciiChar(0xff), ".");
});
