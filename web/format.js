// Number formatting shared by the debugger's modules.

/** `n` as upper-case hex, zero-padded to `digits`. */
export const hex = (n, digits) => n.toString(16).toUpperCase().padStart(digits, "0");
