// Run with: node --test web/input.test.js
import test from "node:test";
import assert from "node:assert/strict";
import {
  RIGHT, LEFT, UP, DOWN, A, B, SELECT, START,
  KEYMAP, gamepadMask, gamepadsMask, dpadMask, withButton, changes,
} from "./input.js";

/** A fake "standard" gamepad with the given button indexes held. */
function pad({ held = [], axes = [0, 0, 0, 0], connected = true } = {}) {
  const buttons = Array.from({ length: 17 }, (_, i) => ({ pressed: held.includes(i), value: held.includes(i) ? 1 : 0 }));
  return { connected, mapping: "standard", buttons, axes };
}
const bits = (...buttons) => buttons.reduce((m, b) => m | (1 << b), 0);

test("button numbers match gb_core::Button", () => {
  assert.deepEqual([RIGHT, LEFT, UP, DOWN, A, B, SELECT, START], [0, 1, 2, 3, 4, 5, 6, 7]);
});

test("keyboard layout", () => {
  assert.equal(KEYMAP.KeyX, A);
  assert.equal(KEYMAP.KeyZ, B);
  assert.equal(KEYMAP.Enter, START);
  assert.equal(KEYMAP.ShiftLeft, SELECT);
  assert.equal(KEYMAP.ArrowUp, UP);
});

test("standard gamepad: d-pad, face buttons, select, start", () => {
  assert.equal(gamepadMask(pad({ held: [12] })), bits(UP));
  assert.equal(gamepadMask(pad({ held: [13, 15] })), bits(DOWN, RIGHT));
  assert.equal(gamepadMask(pad({ held: [14] })), bits(LEFT));
  assert.equal(gamepadMask(pad({ held: [1] })), bits(A), "right face button is A");
  assert.equal(gamepadMask(pad({ held: [0] })), bits(B), "bottom face button is B");
  assert.equal(gamepadMask(pad({ held: [8, 9] })), bits(SELECT, START));
  assert.equal(gamepadMask(pad({ held: [2, 3, 4, 5] })), 0, "unmapped buttons");
});

test("left stick acts as a d-pad past the deadzone", () => {
  assert.equal(gamepadMask(pad({ axes: [0.9, 0] })), bits(RIGHT));
  assert.equal(gamepadMask(pad({ axes: [-0.9, -0.9] })), bits(LEFT, UP));
  assert.equal(gamepadMask(pad({ axes: [0, 0.8] })), bits(DOWN));
  assert.equal(gamepadMask(pad({ axes: [0.3, -0.4] })), 0, "inside the deadzone");
});

test("missing or disconnected pads hold nothing", () => {
  assert.equal(gamepadMask(null), 0);
  assert.equal(gamepadMask(pad({ held: [0], connected: false })), 0);
  // getGamepads() returns sparse arrays with nulls
  assert.equal(gamepadsMask([null, pad({ held: [9] }), null]), bits(START));
  assert.equal(gamepadsMask(undefined), 0);
});

test("keyboard and gamepad combine without fighting", () => {
  // Hold Right on the pad, tap Right on the keyboard: still held after.
  const padHeld = gamepadMask(pad({ held: [15] }));
  let keys = withButton(0, RIGHT, true);
  keys = withButton(keys, RIGHT, false);
  assert.equal(keys | padHeld, bits(RIGHT));
});

test("on-screen d-pad: four directions, four diagonals", () => {
  const r = 70; // a 140px pad
  assert.equal(dpadMask(50, 0, r), bits(RIGHT));
  assert.equal(dpadMask(-50, 0, r), bits(LEFT));
  assert.equal(dpadMask(0, -50, r), bits(UP), "screen Y grows downward");
  assert.equal(dpadMask(0, 50, r), bits(DOWN));
  assert.equal(dpadMask(40, -40, r), bits(UP, RIGHT));
  assert.equal(dpadMask(-40, -40, r), bits(UP, LEFT));
  assert.equal(dpadMask(40, 40, r), bits(DOWN, RIGHT));
  assert.equal(dpadMask(-40, 40, r), bits(DOWN, LEFT));
});

test("on-screen d-pad: sector edges, dead zone, sliding off", () => {
  const r = 70;
  // 22.5° is the boundary between Right and Down-Right: just inside each.
  const at = (deg) => dpadMask(50 * Math.cos((deg * Math.PI) / 180), 50 * Math.sin((deg * Math.PI) / 180), r);
  assert.equal(at(20), bits(RIGHT));
  assert.equal(at(25), bits(DOWN, RIGHT));
  assert.equal(at(-20), bits(RIGHT), "wraps around at 0°");
  assert.equal(at(180), bits(LEFT));
  assert.equal(at(-180), bits(LEFT));
  assert.equal(dpadMask(5, 5, r), 0, "dead zone in the middle");
  assert.equal(dpadMask(300, 0, r), bits(RIGHT), "past the edge still steers");
});

test("changes lists only buttons that flipped", () => {
  assert.deepEqual(changes(bits(A, UP), bits(A, DOWN)), [[UP, false], [DOWN, true]]);
  assert.deepEqual(changes(bits(START), bits(START)), []);
});
