// The debugger's memory hex view: 256 bytes of the CPU's address space,
// live while the game runs. The helpers at the top are plain functions,
// tested with node --test "web/*.test.js"; MemoryView touches the page.
import { hex } from "./debugger.js";

export const COLS = 16;
export const ROWS = 16;
export const VIEW_BYTES = COLS * ROWS;
/** The last start address that still fits a whole view below $FFFF. */
export const LAST_START = 0x10000 - VIEW_BYTES;
/** How long a changed byte stays lit while the game runs, in ms. */
const CHANGED_MS = 400;

/** The memory map, https://gbdev.io/pandocs/Memory_Map.html, as [first, name]. */
const REGIONS = [
  [0x0000, "ROM"],
  [0x8000, "VRAM tile data"],
  [0x9800, "VRAM tile map 0"],
  [0x9c00, "VRAM tile map 1"],
  [0xa000, "cartridge RAM"],
  [0xc000, "work RAM"],
  [0xe000, "echo of work RAM"],
  [0xfe00, "OAM (sprites)"],
  [0xfea0, "unusable"],
  [0xff00, "I/O registers"],
  [0xff80, "HRAM"],
  [0xffff, "IE"],
];

/** Which part of the memory map `addr` is in. `romBank` is the bank mapped
 * there, for addresses below $8000. */
export function regionName(addr, romBank = 0) {
  let name = REGIONS[0][1];
  for (const [first, n] of REGIONS) if (addr >= first) name = n;
  return name === "ROM" ? `ROM bank ${romBank}` : name;
}

/** Hardware register names, https://gbdev.io/pandocs/Hardware_Reg_List.html */
const IO = {
  0xff00: "P1/JOYP", 0xff01: "SB", 0xff02: "SC", 0xff04: "DIV", 0xff05: "TIMA",
  0xff06: "TMA", 0xff07: "TAC", 0xff0f: "IF",
  0xff10: "NR10", 0xff11: "NR11", 0xff12: "NR12", 0xff13: "NR13", 0xff14: "NR14",
  0xff16: "NR21", 0xff17: "NR22", 0xff18: "NR23", 0xff19: "NR24",
  0xff1a: "NR30", 0xff1b: "NR31", 0xff1c: "NR32", 0xff1d: "NR33", 0xff1e: "NR34",
  0xff20: "NR41", 0xff21: "NR42", 0xff22: "NR43", 0xff23: "NR44",
  0xff24: "NR50", 0xff25: "NR51", 0xff26: "NR52",
  0xff40: "LCDC", 0xff41: "STAT", 0xff42: "SCY", 0xff43: "SCX", 0xff44: "LY",
  0xff45: "LYC", 0xff46: "DMA", 0xff47: "BGP", 0xff48: "OBP0", 0xff49: "OBP1",
  0xff4a: "WY", 0xff4b: "WX", 0xffff: "IE",
};

/** The name of the hardware register at `addr`, or "". */
export function ioName(addr) {
  if (addr >= 0xff30 && addr <= 0xff3f) return "wave RAM";
  return IO[addr] ?? "";
}

/**
 * Reads what was typed into the address box: hex ("C000", "$C000",
 * "0xC000"), a register in `regs` ("PC", "SP", "HL", ...), or a hardware
 * register name ("LY", "LCDC"). Returns the address, or null.
 */
export function parseAddress(text, regs = {}) {
  const t = text.trim().toUpperCase();
  if (t in regs) return regs[t];
  for (const [addr, name] of Object.entries(IO)) {
    if (name.split("/").includes(t)) return Number(addr);
  }
  const digits = t.replace(/^(\$|0X)/, "");
  return /^[0-9A-F]{1,4}$/.test(digits) ? parseInt(digits, 16) : null;
}

/** A view start: a multiple of 16 that keeps the whole view in memory. */
export function clampStart(start) {
  return Math.min(Math.max(start, 0), LAST_START) & ~(COLS - 1);
}

/** Where to start the view so that `addr` sits a few rows down. */
export const startAround = (addr) => clampStart((addr & ~(COLS - 1)) - 4 * COLS);

/** A byte as a character, or "." if it isn't printable ASCII. */
export const asciiChar = (b) => (b >= 0x20 && b < 0x7f ? String.fromCharCode(b) : ".");

/** Region buttons: [label, address]. */
const JUMPS = [
  ["ROM 0", 0x0000],
  ["ROM bank", 0x4000],
  ["VRAM", 0x8000],
  ["Cart RAM", 0xa000],
  ["WRAM", 0xc000],
  ["OAM", 0xfe00],
  ["I/O", 0xff00],
  ["HRAM", 0xff80],
];

export class MemoryView {
  /** `els`: { view, addr, follow, prev, next, jumps, info }; `refresh` is
   * called after the controls change what to show. */
  constructor(els, refresh) {
    this.els = els;
    this.refresh = refresh;
    this.start = 0xc000;
    this.selected = null; // the address picked by clicking or typing
    this.regs = {}; // the last register values, for "PC" in the box and markers
    this.prev = null; // the bytes last shown, to spot changes
    this.prevStart = -1;
    this.changedAt = new Float64Array(VIEW_BYTES);
    this.build();
  }

  /** Makes the rows once; updates only change text and classes. */
  build() {
    const { view, jumps } = this.els;
    this.rowAddr = [];
    this.cells = [];
    this.ascii = [];
    for (let r = 0; r < ROWS; r++) {
      const row = document.createElement("div");
      const addr = document.createElement("span");
      addr.className = "addr";
      row.append(addr);
      this.rowAddr.push(addr);
      for (let c = 0; c < COLS; c++) {
        const cell = document.createElement("span");
        cell.className = "byte";
        cell.dataset.offset = r * COLS + c;
        row.append(cell);
        this.cells.push(cell);
      }
      const ascii = document.createElement("span");
      ascii.className = "ascii";
      row.append(ascii);
      this.ascii.push(ascii);
      view.append(row);
    }
    view.addEventListener("click", (e) => {
      const offset = e.target.dataset?.offset;
      if (offset === undefined) return;
      this.selected = (this.start + Number(offset)) & 0xffff;
      this.refresh();
    });
    view.addEventListener(
      "wheel",
      (e) => {
        e.preventDefault(); // scroll the memory, not the page
        this.goTo(this.start + Math.sign(e.deltaY) * COLS * 2);
      },
      { passive: false },
    );
    for (const [label, addr] of JUMPS) {
      const button = document.createElement("button");
      button.type = "button";
      button.textContent = label;
      button.title = `$${hex(addr, 4)}`;
      button.addEventListener("click", () => {
        this.selected = null;
        this.goTo(addr);
      });
      jumps.append(button);
    }
    this.els.prev.addEventListener("click", () => this.goTo(this.start - VIEW_BYTES));
    this.els.next.addEventListener("click", () => this.goTo(this.start + VIEW_BYTES));
    this.els.follow.addEventListener("change", () => this.refresh());
    this.els.addr.addEventListener("keydown", (e) => {
      if (e.key !== "Enter") return;
      const addr = parseAddress(e.target.value, this.regs);
      e.target.classList.toggle("invalid", addr === null);
      if (addr === null) return;
      this.selected = addr;
      this.goTo(addr);
    });
  }

  /** Shows the view from `start` and stops following a register. */
  goTo(start) {
    this.els.follow.value = "";
    this.start = clampStart(start);
    this.refresh();
  }

  /** Redraws from `emu`; `state` is its cpu_state(). */
  update(emu, state) {
    this.regs = {
      PC: state.pc,
      SP: state.sp,
      HL: (state.h << 8) | state.l,
      BC: (state.b << 8) | state.c,
      DE: (state.d << 8) | state.e,
    };
    const follow = this.els.follow.value;
    if (follow) this.start = startAround(this.regs[follow]);
    const bytes = emu.memory(this.start, VIEW_BYTES);
    const now = performance.now();
    const sameView = this.prevStart === this.start;
    const marks = { pc: this.regs.PC, sp: this.regs.SP, hl: this.regs.HL };

    for (let r = 0; r < ROWS; r++) {
      setText(this.rowAddr[r], hex(this.start + r * COLS, 4));
      let ascii = "";
      for (let c = 0; c < COLS; c++) ascii += asciiChar(bytes[r * COLS + c]);
      setText(this.ascii[r], ascii);
    }
    for (let i = 0; i < VIEW_BYTES; i++) {
      const cell = this.cells[i];
      const addr = this.start + i;
      const changedNow = sameView && bytes[i] !== this.prev[i];
      if (changedNow) this.changedAt[i] = now;
      else if (!sameView) this.changedAt[i] = 0;
      setText(cell, hex(bytes[i], 2));
      cell.classList.toggle("changed", changedNow || now - this.changedAt[i] < CHANGED_MS);
      for (const [cls, at] of Object.entries(marks)) cell.classList.toggle(cls, addr === at);
      cell.classList.toggle("selected", addr === this.selected);
    }
    this.prev = bytes;
    this.prevStart = this.start;
    this.showInfo(emu);
  }

  /** The line under the view: the selected byte, or the view's region. */
  showInfo(emu) {
    const bank = (addr) => (addr < 0x8000 ? emu.rom_bank(addr) : 0);
    let text = `$${hex(this.start, 4)}–$${hex(this.start + VIEW_BYTES - 1, 4)}: ${regionName(this.start, bank(this.start))}`;
    if (this.selected !== null) {
      const a = this.selected;
      const v = emu.memory(a, 1)[0];
      const name = ioName(a);
      text = `$${hex(a, 4)}${name ? ` ${name}` : ""} (${regionName(a, bank(a))}) = $${hex(v, 2)}, ${v}, %${v.toString(2).padStart(8, "0")}`;
    }
    setText(this.els.info, text);
  }
}

function setText(node, text) {
  if (node.textContent !== text) node.textContent = text;
}
