// The debugger panel: CPU registers, flags and the next few instructions,
// breakpoints, plus the memory view (memview.js) and the VRAM viewer
// (vramview.js).
// The formatting helpers are plain functions, tested with
// node --test "web/*.test.js"; only DebugPanel touches the page.

import { hex } from "./format.js";
import { parseAddress } from "./memview.js";

/** The register pairs as [name, "1234"], from an Emulator.cpu_state(). */
export function registerPairs(s) {
  const pair = (hi, lo) => hex((hi << 8) | lo, 4);
  return [
    ["AF", pair(s.a, s.f)],
    ["BC", pair(s.b, s.c)],
    ["DE", pair(s.d, s.e)],
    ["HL", pair(s.h, s.l)],
    ["SP", hex(s.sp, 4)],
    ["PC", hex(s.pc, 4)],
  ];
}

/** F's flags as [name, set?]: Z zero, N subtract, H half carry, C carry
 * (bits 7 to 4; the low four bits of F are always 0). */
export function flagStates(f) {
  return ["Z", "N", "H", "C"].map((name, i) => [name, Boolean(f & (0x80 >> i))]);
}

/** How many instructions to show, starting at PC. */
export const DISASM_LINES = 12;

export class DebugPanel {
  /** `els`: { panel (the <details>), regs, flags, disasm, note, memory (a
   * MemoryView), vram (a VramView), breakpoints (a BreakpointList), bpAddr
   * (an input), bpList (a <ul>) }. */
  constructor(els) {
    this.els = els;
    this.emu = null; // the one last shown, for breakpoint edits
    this.regs = {}; // its registers, so "PC" can be typed as an address
    // Clicking an instruction toggles a breakpoint on it.
    els.disasm.addEventListener("click", (e) => {
      const line = e.target.closest("[data-addr]");
      if (line) this.editBreakpoint(Number(line.dataset.addr), "toggle");
    });
    els.bpAddr.addEventListener("keydown", (e) => {
      if (e.key !== "Enter") return;
      const addr = parseAddress(e.target.value, this.regs);
      e.target.classList.toggle("invalid", addr === null);
      if (addr === null) return;
      e.target.value = "";
      this.editBreakpoint(addr, true);
    });
    els.bpList.addEventListener("click", (e) => {
      const button = e.target.closest("button[data-addr]");
      if (button) this.editBreakpoint(Number(button.dataset.addr), false);
    });
  }

  /** Sets (true), clears (false) or toggles ("toggle") the breakpoint at `addr`. */
  editBreakpoint(addr, on) {
    const list = this.els.breakpoints;
    if (on === "toggle") list.toggle(addr, this.emu);
    else list.set(addr, on, this.emu);
    this.update(this.emu);
  }

  get open() {
    return this.els.panel.open;
  }

  /** Shows `emu`'s current state; does nothing while the panel is closed. */
  update(emu) {
    this.emu = emu;
    if (!this.open) return;
    this.showBreakpoints();
    if (!emu) {
      this.els.regs.replaceChildren();
      this.els.flags.replaceChildren();
      this.els.disasm.replaceChildren();
      return;
    }
    const state = emu.cpu_state();
    try {
      this.regs = {
        PC: state.pc,
        SP: state.sp,
        HL: (state.h << 8) | state.l,
        BC: (state.b << 8) | state.c,
        DE: (state.d << 8) | state.e,
      };
      this.els.regs.replaceChildren(
        ...registerPairs(state).flatMap(([name, value]) => [el("dt", name), el("dd", value)]),
      );
      const flags = [...flagStates(state.f), ["IME", state.ime], ["HALT", state.halted]];
      this.els.flags.replaceChildren(
        ...flags.map(([name, on]) => {
          const badge = el("span", name);
          badge.classList.toggle("on", on);
          return badge;
        }),
      );
      // Each line starts with its address: "0150  3E 01     LD A,$01".
      const lines = emu.disassemble(state.pc, DISASM_LINES).map((text, i) => {
        const line = el("div", text);
        const addr = parseInt(text.slice(0, 4), 16);
        line.dataset.addr = addr;
        line.classList.toggle("current", i === 0);
        line.classList.toggle("bp", this.els.breakpoints.has(addr));
        line.title = "Click to set or clear a breakpoint";
        return line;
      });
      this.els.disasm.replaceChildren(...lines);
      this.els.memory.update(emu, state);
      this.els.vram.update(emu);
    } finally {
      state.free();
    }
  }

  /** The list under the flags: one row per breakpoint, with a remove button. */
  showBreakpoints() {
    const addrs = this.els.breakpoints.sorted();
    if (!addrs.length) {
      const hint = el("li", "none: click an instruction");
      hint.className = "muted";
      this.els.bpList.replaceChildren(hint);
      return;
    }
    this.els.bpList.replaceChildren(
      ...addrs.map((addr) => {
        const row = el("li", `$${hex(addr, 4)}`);
        const remove = el("button", "✕");
        remove.type = "button";
        remove.dataset.addr = addr;
        remove.title = `Remove the breakpoint at $${hex(addr, 4)}`;
        row.append(remove);
        return row;
      }),
    );
  }

  /** A short message under the buttons, e.g. how long the last step took. */
  note(text) {
    this.els.note.textContent = text;
  }
}

function el(tag, text) {
  const node = document.createElement(tag);
  node.textContent = text;
  return node;
}
