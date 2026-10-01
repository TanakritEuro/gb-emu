// The debugger's breakpoints. The page keeps the list, because a reset or a
// save import swaps in a new Emulator, which starts with none: applyTo()
// hands it the list again. Tested with node --test "web/*.test.js".

export class BreakpointList {
  constructor() {
    this.addrs = new Set();
  }

  has(addr) {
    return this.addrs.has(addr);
  }

  /** Lowest address first. */
  sorted() {
    return [...this.addrs].sort((a, b) => a - b);
  }

  /** Adds or removes `addr`, here and in `emu`. */
  set(addr, on, emu) {
    if (on) this.addrs.add(addr);
    else this.addrs.delete(addr);
    emu?.set_breakpoint(addr, on);
  }

  toggle(addr, emu) {
    this.set(addr, !this.has(addr), emu);
  }

  clear(emu) {
    for (const addr of this.addrs) emu?.set_breakpoint(addr, false);
    this.addrs.clear();
  }

  /** Gives a newly made emulator the whole list. */
  applyTo(emu) {
    for (const addr of this.addrs) emu.set_breakpoint(addr, true);
  }
}
