// Frame pacing: how many Game Boy frames to run for the real time that has
// passed, at a given speed. Pure functions, tested with node --test "web/*.test.js".

export const FRAME_MS = 1000 / 59.7275; // DMG refresh rate
/** Speeds the speed button cycles through. */
export const SPEEDS = [1, 2, 4];
/** Speed while fast-forward is held. */
export const TURBO = 8;
/** After a stall (a background tab, a debugger pause), don't try to catch up
 * on more than this much real time; the game just resumes. */
export const MAX_CATCH_UP_MS = 250;

/**
 * Frames due after `elapsedMs` of real time at `speed`, carrying the
 * fractional leftover in `backlog` (ms of game time) to the next call.
 */
export function framesDue(backlog, elapsedMs, speed) {
  const total = backlog + Math.min(Math.max(elapsedMs, 0), MAX_CATCH_UP_MS) * speed;
  const frames = Math.floor(total / FRAME_MS);
  return { frames, backlog: total - frames * FRAME_MS };
}

/** At most this many frames per refresh when audio sets the pace, so a
 * drained queue (after a pause or a hidden tab) refills over a few
 * refreshes instead of one long stall. */
export const MAX_AUDIO_CATCH_UP = 6;

/**
 * Audio-driven pacing: how many frames to run now so the audio queue gets
 * back up to `target` frames, given each Game Boy frame adds
 * `framesPerVideoFrame` audio frames (sampleRate / 59.7275). The sound
 * card's clock then sets the game's speed, so the queue neither runs dry
 * (crackle) nor grows (lag).
 */
export function audioFramesDue(buffered, target, framesPerVideoFrame, max = MAX_AUDIO_CATCH_UP) {
  const missing = target - buffered;
  if (missing <= 0) return 0;
  return Math.min(max, Math.ceil(missing / framesPerVideoFrame));
}

/** The speed after `speed` in SPEEDS, wrapping around. */
export function nextSpeed(speed) {
  return SPEEDS[(SPEEDS.indexOf(speed) + 1) % SPEEDS.length];
}
