// Run with: node --test "web/*.test.js"
import test from "node:test";
import assert from "node:assert/strict";
import { SampleQueue, UNDERRUN_FADE, RESUME_FRAMES } from "./audio-queue.js";

/** A queue already playing at full level (past its first fade-in). */
function playing(maxFrames) {
  const q = new SampleQueue(maxFrames);
  q.gain = 1;
  return q;
}

/** Interleaved stereo: left = i, right = -i for i in [from, from + n). */
function ramp(from, n) {
  const out = new Float32Array(n * 2);
  for (let i = 0; i < n; i++) {
    out[i * 2] = from + i;
    out[i * 2 + 1] = -(from + i);
  }
  return out;
}

function pull(q, n) {
  const left = new Float32Array(n), right = new Float32Array(n);
  q.pull(left, right);
  return { left: [...left], right: [...right] };
}

test("samples come out in order, split into left and right, across chunks", () => {
  const q = playing(10_000);
  q.push(ramp(0, 3));
  q.push(ramp(3, 4));
  assert.equal(q.frames, 7);
  const { left, right } = pull(q, 5);
  assert.deepEqual(left, [0, 1, 2, 3, 4]);
  assert.deepEqual(right, [-0, -1, -2, -3, -4]);
  assert.deepEqual(pull(q, 2).left, [5, 6]);
  assert.equal(q.frames, 0);
  assert.equal(q.underruns, 0);
});

test("running dry fades from the last sample instead of jumping to 0", () => {
  const q = playing(10_000);
  q.push(ramp(0.5, 1)); // one frame: 0.5 / -0.5
  const { left } = pull(q, 4);
  assert.equal(left[0], 0.5);
  assert.ok(Math.abs(left[1] - 0.5 * UNDERRUN_FADE) < 1e-6, "the next sample barely moves");
  assert.ok(left[3] < left[2] && left[3] > 0.45, "a gentle glide");
  assert.equal(q.underruns, 1);
  // A long gap ends near silence.
  const later = pull(q, 4800).left;
  assert.ok(Math.abs(later.at(-1)) < 0.001);
});

test("too much queued drops the oldest audio, bounding the lag", () => {
  const q = playing(5);
  q.push(ramp(0, 4));
  q.push(ramp(4, 4)); // 8 frames, max 5: drop the first 3
  assert.equal(q.frames, 5);
  assert.deepEqual(pull(q, 5).left, [3, 4, 5, 6, 7]);
});

test("audio coming back after running dry fades in instead of jumping", () => {
  const q = playing(10_000);
  pull(q, 128); // dry: e.g. paused
  const constant = new Float32Array(RESUME_FRAMES * 4).fill(0.8); // a held level, both sides
  q.push(constant);
  const { left } = pull(q, RESUME_FRAMES * 2);
  assert.ok(left[0] < 0.01, "starts near silence");
  assert.ok(left[RESUME_FRAMES / 2] > 0.3 && left[RESUME_FRAMES / 2] < 0.5, "halfway up halfway in");
  assert.ok(Math.abs(left[RESUME_FRAMES] - 0.8) < 1e-6, "full level after the fade");
  for (let i = 1; i < left.length; i++) assert.ok(left[i] >= left[i - 1], "no step down on the way");
});

test("a brand-new queue fades in its first sound too", () => {
  const q = new SampleQueue(10_000);
  q.push(new Float32Array(20).fill(1));
  assert.ok(pull(q, 1).left[0] < 0.01);
});

test("empty chunks are ignored", () => {
  const q = new SampleQueue(100);
  q.push(new Float32Array(0));
  assert.equal(q.chunks.length, 0);
});
