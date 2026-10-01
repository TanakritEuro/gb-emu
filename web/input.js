// Keyboard and gamepad → Game Boy buttons. Pure functions, no DOM, so they
// can be tested with `node --test web/input.test.js`.
//
// Buttons are a bitmask indexed like gb_core::Button (and Emulator.set_button):
// 0 Right, 1 Left, 2 Up, 3 Down, 4 A, 5 B, 6 Select, 7 Start.

export const RIGHT = 0, LEFT = 1, UP = 2, DOWN = 3, A = 4, B = 5, SELECT = 6, START = 7;

export const KEYMAP = {
  ArrowRight: RIGHT, ArrowLeft: LEFT, ArrowUp: UP, ArrowDown: DOWN,
  KeyX: A, KeyZ: B, ShiftLeft: SELECT, ShiftRight: SELECT, Enter: START,
};

// Gamepad button indexes in the "standard" layout
// (https://w3c.github.io/gamepad/#remapping). The Game Boy has B lower-left
// and A upper-right, so the pad's bottom face button is B and its right one
// is A (as on Nintendo's own controllers).
const PAD_BUTTONS = [
  [12, UP], [13, DOWN], [14, LEFT], [15, RIGHT],
  [1, A], [0, B], [8, SELECT], [9, START],
];
const STICK_DEADZONE = 0.5;

const bit = (button) => 1 << button;

/** Buttons held on one gamepad (from navigator.getGamepads()), as a mask. */
export function gamepadMask(pad) {
  if (!pad || !pad.connected) return 0;
  let mask = 0;
  for (const [index, button] of PAD_BUTTONS) {
    if (pad.buttons[index]?.pressed) mask |= bit(button);
  }
  // The left stick works as a d-pad too.
  const [x = 0, y = 0] = pad.axes;
  if (x > STICK_DEADZONE) mask |= bit(RIGHT);
  if (x < -STICK_DEADZONE) mask |= bit(LEFT);
  if (y > STICK_DEADZONE) mask |= bit(DOWN);
  if (y < -STICK_DEADZONE) mask |= bit(UP);
  return mask;
}

/** Buttons held across all connected gamepads. */
export function gamepadsMask(pads) {
  let mask = 0;
  for (const pad of pads ?? []) mask |= gamepadMask(pad);
  return mask;
}

// On-screen d-pad: eight 45° sectors around the center, starting from Right
// and going clockwise (screen Y grows downward), so diagonals press two
// directions. A small dead zone in the middle presses nothing.
const DPAD_SECTORS = [
  [RIGHT], [DOWN, RIGHT], [DOWN], [DOWN, LEFT], [LEFT], [UP, LEFT], [UP], [UP, RIGHT],
].map((buttons) => buttons.reduce((mask, b) => mask | bit(b), 0));
const DPAD_DEADZONE = 0.2;

/** D-pad buttons for a touch at (dx, dy) from the pad's center; `radius` is
 * half the pad's width. Touches past the edge still count, so a thumb that
 * slides off keeps steering. */
export function dpadMask(dx, dy, radius) {
  if (Math.hypot(dx, dy) < radius * DPAD_DEADZONE) return 0;
  const sector = Math.round(Math.atan2(dy, dx) / (Math.PI / 4)) & 7;
  return DPAD_SECTORS[sector];
}

/** `mask` with `button` set or cleared. */
export function withButton(mask, button, pressed) {
  return pressed ? mask | bit(button) : mask & ~bit(button);
}

/** [button, pressed] for each button that differs between two masks. */
export function changes(before, after) {
  const out = [];
  for (let button = 0; button < 8; button++) {
    const was = (before >> button) & 1, now = (after >> button) & 1;
    if (was !== now) out.push([button, now === 1]);
  }
  return out;
}
