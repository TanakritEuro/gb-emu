// Run with: node --test "web/*.test.js"
import test from "node:test";
import assert from "node:assert/strict";
import {
  FRAME_MS, SPEEDS, TURBO, MAX_CATCH_UP_MS, MAX_AUDIO_CATCH_UP, framesDue, nextSpeed, audioFramesDue,
} from "./timing.js";

/** Frames run over 10 seconds of real time on a `hz` monitor (one call per
 * refresh), and how many a perfect clock would run: 10 s x 59.7275 x speed. */
function tenSeconds(hz, speed) {
  let backlog = 0, frames = 0;
  for (let i = 0; i < 10 * hz; i++) {
    const due = framesDue(backlog, 1000 / hz, speed);
    frames += due.frames;
    backlog = due.backlog;
  }
  return { frames, ideal: (10_000 / FRAME_MS) * speed };
}

test("real speed is ~59.73 frames a second, whatever the monitor rate", () => {
  for (const hz of [60, 75, 120, 144]) {
    const { frames, ideal } = tenSeconds(hz, 1);
    assert.ok(Math.abs(frames - ideal) < 1, `${hz} Hz: ${frames} frames, ideal ${ideal}`);
  }
});

test("speed multiplies the frame rate", () => {
  for (const speed of [2, 4, TURBO]) {
    const { frames, ideal } = tenSeconds(60, speed);
    assert.ok(Math.abs(frames - ideal) < 1, `${speed}x: ${frames} frames, ideal ${ideal}`);
  }
});

test("the leftover carries over instead of being lost", () => {
  const first = framesDue(0, FRAME_MS * 1.5, 1);
  assert.equal(first.frames, 1);
  const second = framesDue(first.backlog, FRAME_MS * 0.5, 1);
  assert.equal(second.frames, 1, "half a frame + half a frame");
});

test("a long stall doesn't make the game sprint to catch up", () => {
  const { frames } = framesDue(0, 5000, 1);
  assert.equal(frames, Math.floor(MAX_CATCH_UP_MS / FRAME_MS));
  assert.equal(framesDue(0, -10, 1).frames, 0, "clock going backwards");
});

test("audio pacing tops the queue up to the target", () => {
  const perFrame = 48000 / 59.7275; // ~804 audio frames per Game Boy frame
  assert.equal(audioFramesDue(2880, 2880, perFrame), 0, "full: wait");
  assert.equal(audioFramesDue(3500, 2880, perFrame), 0, "over: wait");
  assert.equal(audioFramesDue(2500, 2880, perFrame), 1, "a little low: one frame");
  assert.equal(audioFramesDue(1200, 2880, perFrame), 3, "lower: enough frames to refill");
  assert.equal(audioFramesDue(0, 48000, perFrame), MAX_AUDIO_CATCH_UP, "drained: capped");
});

test("audio pacing settles at the Game Boy's real frame rate", () => {
  // Simulate a 60 Hz display and a 48 kHz sound card draining the queue,
  // with the page running audioFramesDue frames each refresh.
  const rate = 48000, perFrame = rate / 59.7275, target = 0.06 * rate;
  let queue = 0, frames = 0, low = Infinity, high = 0;
  for (let refresh = 0; refresh < 60 * 20; refresh++) {
    const n = audioFramesDue(queue, target, perFrame);
    frames += n;
    queue += n * perFrame;
    queue = Math.max(0, queue - rate / 60);
    if (refresh > 60) {
      low = Math.min(low, queue);
      high = Math.max(high, queue);
    }
  }
  const fps = frames / 20;
  assert.ok(Math.abs(fps - 59.7275) < 0.2, `${fps} fps`);
  assert.ok(low > 0, "never runs dry once going");
  assert.ok(high < target + 2 * perFrame, "never piles up");
});

test("speed button cycles 1x, 2x, 4x", () => {
  assert.deepEqual(SPEEDS, [1, 2, 4]);
  assert.equal(nextSpeed(1), 2);
  assert.equal(nextSpeed(2), 4);
  assert.equal(nextSpeed(4), 1);
});
