// The debugger's VRAM viewer: every tile, and a whole background map with
// the part on screen outlined. The helpers at the top are plain functions,
// tested with node --test "web/*.test.js"; VramView touches the page.
import { hex } from "./format.js";

export const SHEET_W = 128; // 16 tiles across
export const SHEET_H = 192; // 24 tiles down: all 384
export const MAP_SIZE = 256; // 32 × 32 tiles
const SCREEN_W = 160;
const SCREEN_H = 144;

/** The tile under pixel (x, y) of the tile sheet, 0-383. */
export const sheetTile = (x, y) => (y >> 3) * 16 + (x >> 3);

/** Where tile `n` (0-383) of the sheet lives: $8000 + 16 bytes per tile. */
export const tileAddress = (n) => 0x8000 + n * 16;

/** Who can use the tile at `n`: the three 128-tile blocks are shared
 * differently. https://gbdev.io/pandocs/Tile_Data.html */
export function tileUsers(n) {
  if (n < 128) return "sprites, and BG/window when LCDC bit 4 is set";
  if (n < 256) return "sprites and BG/window";
  return "BG/window when LCDC bit 4 is clear";
}

/** The map entry (a tile number's address) for map pixel (x, y). */
export const mapEntryAddress = (highMap, x, y) =>
  (highMap ? 0x9c00 : 0x9800) + (y >> 3) * 32 + (x >> 3);

/** Where background tile number `tile` (0-255) is, given LCDC: bit 4 set
 * counts 0-255 from $8000, clear counts -128..127 from $9000. */
export function bgTileAddress(tile, lcdc) {
  if (lcdc & 0x10) return 0x8000 + tile * 16;
  return 0x9000 + ((tile << 24) >> 24) * 16; // as a signed byte
}

/** What a Color tile's attribute byte says, e.g. "palette 5, bank 1, X flip",
 * https://gbdev.io/pandocs/Tile_Maps.html#bg-map-attributes-cgb-mode-only */
export function describeAttrs(attrs) {
  const parts = [`palette ${attrs & 7}`, `bank ${(attrs >> 3) & 1}`];
  const flips = [attrs & 0x20 && "X", attrs & 0x40 && "Y"].filter(Boolean);
  if (flips.length) parts.push(`${flips.join("+")} flip`);
  if (attrs & 0x80) parts.push("over sprites");
  return parts.join(", ");
}

/** A run of `len` pixels from `start` on a 256-wide map that wraps: one or
 * two [start, length] pieces. */
export function wrapSpan(start, len, size = MAP_SIZE) {
  start %= size;
  if (start + len <= size) return [[start, len]];
  return [
    [start, size - start],
    [0, len - (size - start)],
  ];
}

/**
 * The screen's 160×144 window onto the background map at scroll (scx, scy),
 * as 1-pixel-thick [x, y, w, h] rectangles to fill. It wraps around the map's
 * edges like the picture does. https://gbdev.io/pandocs/Scrolling.html
 */
export function viewportOutline(scx, scy) {
  const right = (scx + SCREEN_W - 1) % MAP_SIZE;
  const bottom = (scy + SCREEN_H - 1) % MAP_SIZE;
  return [
    ...wrapSpan(scx, SCREEN_W).flatMap(([x, w]) => [
      [x, scy, w, 1],
      [x, bottom, w, 1],
    ]),
    ...wrapSpan(scy, SCREEN_H).flatMap(([y, h]) => [
      [scx, y, 1, h],
      [right, y, 1, h],
    ]),
  ];
}

/** The part of the window's map that is on screen: the window starts at
 * screen (WX - 7, WY) and shows its map from (0, 0), so this is the
 * top-left corner. Null if none of it is on screen. */
export function windowRect(wx, wy) {
  const w = SCREEN_W - Math.max(wx - 7, 0);
  const h = SCREEN_H - wy;
  return w > 0 && h > 0 ? [0, 0, w, h] : null;
}

/** Map choices in the select: which map to draw, from LCDC. */
export function chosenMap(choice, lcdc) {
  if (choice === "9800") return false;
  if (choice === "9C00") return true;
  if (choice === "window") return Boolean(lcdc & 0x40);
  return Boolean(lcdc & 0x08); // "bg": the one the background uses
}

export class VramView {
  /** `els`: { section (a <details>), tiles, map, mapChoice, tileBank, info }
   * (tiles and map are canvases; tileBank a select shown on the Color);
   * `pick(addr)` shows an address in the memory view. */
  constructor(els, pick) {
    this.els = els;
    this.pick = pick;
    this.lcdc = 0;
    this.mapEntries = new Uint8Array(0x800); // $9800-$9FFF, for hover info
    this.mapAttrs = null; // the same in bank 1, on the Color
    this.color = false;
    this.bank = 0; // the tile sheet's bank
    this.highMap = false;
    this.els.tiles.addEventListener("mousemove", (e) => this.hoverTile(e));
    this.els.map.addEventListener("mousemove", (e) => this.hoverMap(e));
    this.els.tiles.addEventListener("click", (e) => this.pick(tileAddress(this.tileUnder(e))));
    this.els.map.addEventListener("click", (e) => {
      const [x, y] = canvasPoint(e, MAP_SIZE, MAP_SIZE);
      this.pick(mapEntryAddress(this.highMap, x, y));
    });
    for (const canvas of [this.els.tiles, this.els.map]) {
      canvas.addEventListener("mouseleave", () => this.showSummary());
    }
  }

  get open() {
    return this.els.section.open;
  }

  update(emu) {
    if (!this.open) return;
    const [lcdc, , scy, scx] = emu.memory(0xff40, 4);
    const [, , wy, wx] = emu.memory(0xff48, 4); // OBP0 OBP1 WY WX
    this.lcdc = lcdc;
    // Straight from VRAM, not through the bus, which shows whichever bank
    // the game has picked.
    const vram = emu.vram();
    this.color = emu.is_color();
    this.mapEntries = vram.subarray(0x1800, 0x2000);
    this.mapAttrs = this.color ? vram.subarray(0x3800, 0x4000) : null;
    this.highMap = chosenMap(this.els.mapChoice.value, lcdc);
    this.els.tileBank.hidden = !this.color;
    this.bank = this.color ? Number(this.els.tileBank.value) : 0;

    draw(this.els.tiles, emu.tile_sheet(this.bank), SHEET_W, SHEET_H);
    const ctx = draw(this.els.map, emu.tile_map(this.highMap), MAP_SIZE, MAP_SIZE);
    // Outline what is on screen, if this map is the one shown there.
    const lcdOn = Boolean(lcdc & 0x80);
    ctx.fillStyle = "rgba(255, 64, 96, 0.9)";
    if (lcdOn && this.highMap === Boolean(lcdc & 0x08)) {
      for (const r of viewportOutline(scx, scy)) ctx.fillRect(...r);
    }
    ctx.fillStyle = "rgba(64, 128, 255, 0.9)";
    const win = windowRect(wx, wy);
    if (lcdOn && lcdc & 0x20 && win && this.highMap === Boolean(lcdc & 0x40)) {
      const [x, y, w, h] = win;
      for (const r of [[x, y, w, 1], [x, y + h - 1, w, 1], [x, y, 1, h], [x + w - 1, y, 1, h]]) {
        ctx.fillRect(...r);
      }
    }
    if (!this.hovering) this.showSummary();
  }

  /** The line under the pictures when nothing is hovered: what LCDC says. */
  showSummary() {
    this.hovering = false;
    const l = this.lcdc;
    const map = (bit) => (l & bit ? "$9C00" : "$9800");
    const parts = [
      `BG map ${map(0x08)}`,
      `window map ${map(0x40)}${l & 0x20 ? "" : " (off)"}`,
      `BG tiles from ${l & 0x10 ? "$8000" : "$8800 (signed, around $9000)"}`,
    ];
    if (!(l & 0x80)) parts.unshift("LCD off");
    this.els.info.textContent = parts.join(" · ");
  }

  tileUnder(e) {
    const [x, y] = canvasPoint(e, SHEET_W, SHEET_H);
    return sheetTile(x, y);
  }

  hoverTile(e) {
    this.hovering = true;
    this.els.info.textContent = describeSheetTile(this.tileUnder(e), this.color ? this.bank : null);
  }

  hoverMap(e) {
    this.hovering = true;
    const [x, y] = canvasPoint(e, MAP_SIZE, MAP_SIZE);
    const entry = mapEntryAddress(this.highMap, x, y);
    const tile = this.mapEntries[entry - 0x9800];
    const attrs = this.mapAttrs?.[entry - 0x9800];
    this.els.info.textContent = describeMapEntry(entry, tile, this.lcdc, attrs);
  }
}

/** The hover line for tile \`n\` (0-383) of the sheet; \`bank\` on the Color. */
export function describeSheetTile(n, bank = null) {
  const number = n < 256 ? `$${hex(n, 2)}` : `$${hex(n - 256, 2)} (signed ${n - 256})`;
  const where = `$${hex(tileAddress(n), 4)}${bank === null ? "" : ` in bank ${bank}`}`;
  return `tile ${number} at ${where}: ${tileUsers(n)}`;
}

/** The hover line for the map entry at \`entry\` holding \`tile\`, with its
 * Color attributes if there are any. */
export function describeMapEntry(entry, tile, lcdc, attrs) {
  const col = (entry - 0x9800) & 31;
  const row = ((entry - 0x9800) >> 5) & 31;
  const color = attrs === undefined ? "" : ` · ${describeAttrs(attrs)}`; // names the bank
  return (
    `map (${col}, ${row}) at $${hex(entry, 4)} → tile $${hex(tile, 2)} ` +
    `at $${hex(bgTileAddress(tile, lcdc), 4)}${color}`
  );
}

/** Puts RGBA `bytes` on `canvas` (w × h) and returns its 2D context. */
function draw(canvas, bytes, w, h) {
  const ctx = canvas.getContext("2d");
  ctx.putImageData(new ImageData(new Uint8ClampedArray(bytes.buffer, bytes.byteOffset, w * h * 4), w, h), 0, 0);
  return ctx;
}

/** The canvas pixel under a mouse event, for a canvas drawn at w × h. */
function canvasPoint(e, w, h) {
  const r = e.currentTarget.getBoundingClientRect();
  const x = Math.floor(((e.clientX - r.left) / r.width) * w);
  const y = Math.floor(((e.clientY - r.top) / r.height) * h);
  return [Math.min(Math.max(x, 0), w - 1), Math.min(Math.max(y, 0), h - 1)];
}
