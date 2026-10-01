// The debugger panel: CPU registers, flags and the next few instructions,
// plus the memory view (memview.js) and the VRAM viewer (vramview.js).
// The formatting helpers are plain functions, tested with
// node --test "web/*.test.js"; only DebugPanel touches the page.

/** `n` as upper-case hex, zero-padded to `digits`. */
export const hex = (n, digits) => n.toString(16).toUpperCase().padStart(digits, "0");

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
   * MemoryView), vram (a VramView) }. */
  constructor(els) {
    this.els = els;
  }

  get open() {
    return this.els.panel.open;
  }

  /** Shows `emu`'s current state; does nothing while the panel is closed. */
  update(emu) {
    if (!this.open) return;
    if (!emu) {
      this.els.regs.replaceChildren();
      this.els.flags.replaceChildren();
      this.els.disasm.replaceChildren();
      return;
    }
    const state = emu.cpu_state();
    try {
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
      const lines = emu.disassemble(state.pc, DISASM_LINES);
      this.els.disasm.replaceChildren(...lines.map((line) => el("div", line)));
      this.els.memory.update(emu, state);
      this.els.vram.update(emu);
    } finally {
      state.free();
    }
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
