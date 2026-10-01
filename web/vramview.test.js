// Run with: node --test "web/*.test.js"
import test from "node:test";
import assert from "node:assert/strict";
import {
  sheetTile,
  tileAddress,
  tileUsers,
  mapEntryAddress,
  bgTileAddress,
  wrapSpan,
  viewportOutline,
  windowRect,
  chosenMap,
} from "./vramview.js";

test("the tile sheet is 16 tiles per row in address order", () => {
  assert.equal(sheetTile(0, 0), 0);
  assert.equal(sheetTile(8, 0), 1);
  assert.equal(sheetTile(127, 7), 15);
  assert.equal(sheetTile(0, 8), 16);
  assert.equal(sheetTile(127, 191), 383);
  assert.equal(tileAddress(0), 0x8000);
  assert.equal(tileAddress(1), 0x8010);
  assert.equal(tileAddress(383), 0x97f0);
});

test("each 128-tile block has its own users", () => {
  assert.match(tileUsers(0), /^sprites, and BG\/window when LCDC bit 4 is set/);
  assert.equal(tileUsers(128), "sprites and BG/window");
  assert.equal(tileUsers(383), "BG/window when LCDC bit 4 is clear");
});

test("map pixels come from 32 entries per row", () => {
  assert.equal(mapEntryAddress(false, 0, 0), 0x9800);
  assert.equal(mapEntryAddress(false, 16, 8), 0x9822);
  assert.equal(mapEntryAddress(true, 255, 255), 0x9fff);
});

test("LCDC bit 4 picks how background tile numbers are read", () => {
  assert.equal(bgTileAddress(0x00, 0x91), 0x8000);
  assert.equal(bgTileAddress(0x80, 0x91), 0x8800);
  assert.equal(bgTileAddress(0x00, 0x81), 0x9000, "signed: 0 is at $9000");
  assert.equal(bgTileAddress(0x7f, 0x81), 0x97f0);
  assert.equal(bgTileAddress(0x80, 0x81), 0x8800, "-128");
  assert.equal(bgTileAddress(0xff, 0x81), 0x8ff0, "-1");
});

test("spans wrap around the 256-pixel map", () => {
  assert.deepEqual(wrapSpan(10, 160), [[10, 160]]);
  assert.deepEqual(wrapSpan(96, 160), [[96, 160]], "ends exactly at the edge");
  assert.deepEqual(wrapSpan(200, 160), [[200, 56], [0, 104]]);
});

test("the on-screen outline is the screen's border on the map", () => {
  assert.deepEqual(viewportOutline(0, 0), [
    [0, 0, 160, 1], // top
    [0, 143, 160, 1], // bottom
    [0, 0, 1, 144], // left
    [159, 0, 1, 144], // right
  ]);
});

test("the outline wraps when the screen scrolls past the map's edge", () => {
  const rects = viewportOutline(200, 240);
  // Top and bottom rows split at x = 256; left and right columns at y = 256.
  assert.deepEqual(rects, [
    [200, 240, 56, 1],
    [200, 127, 56, 1],
    [0, 240, 104, 1],
    [0, 127, 104, 1],
    [200, 240, 1, 16],
    [103, 240, 1, 16],
    [200, 0, 1, 128],
    [103, 0, 1, 128],
  ]);
  for (const [x, y, w, h] of rects) {
    assert.ok(x >= 0 && y >= 0 && x + w <= 256 && y + h <= 256, "inside the map");
  }
});

test("the window's visible part is the top-left of its map", () => {
  assert.deepEqual(windowRect(7, 0), [0, 0, 160, 144], "covering the screen");
  assert.deepEqual(windowRect(7, 100), [0, 0, 160, 44], "a status bar at the bottom");
  assert.deepEqual(windowRect(87, 0), [0, 0, 80, 144], "the right half");
  assert.deepEqual(windowRect(0, 0), [0, 0, 160, 144], "WX < 7 starts off the left edge");
  assert.equal(windowRect(167, 0), null, "off the right");
  assert.equal(windowRect(7, 144), null, "below the screen");
});

test("the map choice follows LCDC unless a map is picked", () => {
  assert.equal(chosenMap("bg", 0x91), false);
  assert.equal(chosenMap("bg", 0x99), true, "LCDC bit 3");
  assert.equal(chosenMap("window", 0xd1), true, "LCDC bit 6");
  assert.equal(chosenMap("window", 0x91), false);
  assert.equal(chosenMap("9C00", 0x91), true);
  assert.equal(chosenMap("9800", 0x99), false);
});
