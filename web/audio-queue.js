// The AudioWorklet's sample queue: interleaved stereo chunks from the
// emulator go in, and each 128-frame render block takes them out as separate
// left and right arrays. Plain JS with no audio APIs, so it can be tested
// with node --test "web/*.test.js".

/** Per-sample fade when the queue runs dry: the output glides from the last
 * sample to silence (about -40 dB in 20 ms) instead of jumping, which clicks. */
export const UNDERRUN_FADE = 0.995;
/** When audio comes back after running dry (unpausing, leaving fast-forward),
 * it fades in over this many frames (5 ms at 48 kHz) rather than starting
 * mid-waveform, which would also click. */
export const RESUME_FRAMES = 240;

export class SampleQueue {
  /** `maxFrames`: beyond this the oldest audio is dropped, to bound latency. */
  constructor(maxFrames) {
    this.maxFrames = maxFrames;
    this.chunks = [];
    this.offset = 0; // read position in chunks[0], in samples
    this.frames = 0; // stereo frames queued
    this.last = [0, 0];
    this.underruns = 0; // render blocks that ran short
    this.gain = 0; // fade-in level: starts silent, rises to 1
  }

  /** Queues an interleaved left/right Float32Array. */
  push(chunk) {
    if (!chunk.length) return;
    this.chunks.push(chunk);
    this.frames += chunk.length / 2;
    if (this.frames > this.maxFrames) this.drop(this.frames - this.maxFrames);
  }

  /** Fills `left` and `right` (same length) from the queue. */
  pull(left, right) {
    let short = false;
    for (let i = 0; i < left.length; i++) {
      if (this.frames > 0) {
        const chunk = this.chunks[0];
        this.gain = Math.min(1, this.gain + 1 / RESUME_FRAMES);
        this.last[0] = chunk[this.offset] * this.gain;
        this.last[1] = chunk[this.offset + 1] * this.gain;
        this.offset += 2;
        this.frames--;
        if (this.offset >= chunk.length) {
          this.chunks.shift();
          this.offset = 0;
        }
      } else {
        short = true;
        this.gain = 0; // fade back in when audio returns
        this.last[0] *= UNDERRUN_FADE;
        this.last[1] *= UNDERRUN_FADE;
      }
      left[i] = this.last[0];
      right[i] = this.last[1];
    }
    if (short) this.underruns++;
  }

  drop(frames) {
    while (frames > 0 && this.chunks.length) {
      const chunk = this.chunks[0];
      const take = Math.min((chunk.length - this.offset) / 2, frames);
      this.offset += take * 2;
      this.frames -= take;
      frames -= take;
      if (this.offset >= chunk.length) {
        this.chunks.shift();
        this.offset = 0;
      }
    }
  }
}
