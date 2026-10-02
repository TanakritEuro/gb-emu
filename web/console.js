// The Console panel: which console original Game Boy games run on, and, on
// the Game Boy Color, which of its palettes they show in. Both choices are
// remembered in this browser: the console for every game, the palette per
// game (keyed like battery saves, by a hash of the ROM). Storage is passed
// in, so tests can use a fake; plain logic plus a small DOM builder, tested
// with node --test "web/*.test.js".

const CONSOLE_KEY = "gb-emu:console";
const PALETTE_PREFIX = "gb-emu:palette:";

/** Palette choice -1: whatever the Color's boot ROM picks for the game. */
export const AUTO_PALETTE = -1;

/** Which console to run original games on: "dmg" (the default) or "cgb". */
export function readConsole(storage) {
  try {
    return storage?.getItem(CONSOLE_KEY) === "cgb" ? "cgb" : "dmg";
  } catch {
    return "dmg";
  }
}

export function writeConsole(storage, value) {
  try {
    storage?.setItem(CONSOLE_KEY, value === "cgb" ? "cgb" : "dmg");
  } catch {
    // Not remembered; it still applies until the page closes.
  }
}

/** The palette key for a game, from its battery save key ("gb-emu:save:<hash>"). */
export function paletteKey(saveKey) {
  return PALETTE_PREFIX + saveKey.slice(saveKey.lastIndexOf(":") + 1);
}

/** The palette picked for a game: AUTO_PALETTE, or 0 to `count - 1`. */
export function readPalette(storage, saveKey, count) {
  let n = NaN;
  try {
    n = Number(storage?.getItem(paletteKey(saveKey)) ?? NaN);
  } catch {
    // Unreadable: the boot ROM's choice.
  }
  return Number.isInteger(n) && n >= 0 && n < count ? n : AUTO_PALETTE;
}

export function writePalette(storage, saveKey, choice) {
  try {
    if (choice === AUTO_PALETTE) storage?.removeItem(paletteKey(saveKey));
    else storage?.setItem(paletteKey(saveKey), String(choice));
  } catch {
    // Not remembered; it still applies until the page closes.
  }
}

/** An RGB555 color (bits 0-4 red, 5-9 green, 10-14 blue) as CSS, widened
 * to 8 bits a channel the way the emulator draws it. */
export function rgb555ToCss(color) {
  const widen = (c) => (c << 3) | (c >> 2);
  const r = widen(color & 0x1f), g = widen((color >> 5) & 0x1f), b = widen((color >> 10) & 0x1f);
  return `rgb(${r}, ${g}, ${b})`;
}

/**
 * A grid of palette buttons, each showing its colors: the background's four
 * on top, the two sprite palettes' below. `onPick(choice)` runs on a click.
 */
export class PalettePicker {
  constructor(container, onPick) {
    this.container = container;
    container.addEventListener("click", (e) => {
      const button = e.target.closest("button[data-choice]");
      if (button) onPick(Number(button.dataset.choice));
    });
  }

  /** `options`: [{ choice, label, colors: 12 RGB555 values }]. */
  render(options, selected) {
    const doc = this.container.ownerDocument;
    this.container.replaceChildren(
      ...options.map(({ choice, label, colors }) => {
        const button = doc.createElement("button");
        button.type = "button";
        button.dataset.choice = String(choice);
        button.setAttribute("aria-pressed", String(choice === selected));
        const swatches = doc.createElement("span");
        swatches.className = "swatches";
        colors.forEach((c, i) => {
          const s = doc.createElement("i");
          s.style.background = rgb555ToCss(c);
          if (i >= 4) s.className = "obj"; // sprite colors: the smaller row
          swatches.append(s);
        });
        const name = doc.createElement("span");
        name.textContent = label;
        button.append(swatches, name);
        return button;
      }),
    );
  }
}
