// Sound output, main-thread side: an AudioContext with the "gb-audio"
// worklet (audio-worklet.js), a gain node for volume, and an estimate of how
// much audio is queued so the frame loop can let audio set the pace.

/** Output volume: headroom, since the mixed signal can peak near full scale. */
const VOLUME = 0.6;
/** How much audio to keep queued: enough to ride out a late frame, short
 * enough that sound doesn't lag the picture. */
const TARGET_SECONDS = 0.06;

export class AudioOut {
  ctx = null;
  node = null;
  gain = null;
  muted = false;
  // The worklet's last report, when it arrived, and what's been sent since.
  report = { buffered: 0, underruns: 0, at: 0 };
  sentSinceReport = 0;
  #starting = null;

  /** Creates or resumes the AudioContext. Browsers only allow this from a
   * user gesture (click, tap, key press), so call it from one. */
  start() {
    this.#starting ??= this.#create();
    return this.#starting.then(() => this.ctx.resume());
  }

  async #create() {
    this.ctx = new AudioContext({ latencyHint: "interactive" });
    await this.ctx.audioWorklet.addModule("audio-worklet.js");
    this.node = new AudioWorkletNode(this.ctx, "gb-audio", {
      numberOfInputs: 0,
      outputChannelCount: [2],
    });
    this.gain = new GainNode(this.ctx, { gain: this.muted ? 0 : VOLUME });
    this.node.connect(this.gain).connect(this.ctx.destination);
    this.node.port.onmessage = (e) => {
      this.report = { ...e.data, at: performance.now() };
      this.sentSinceReport = 0;
    };
    this.report.at = performance.now(); // an empty queue, as of now
  }

  get running() {
    return this.node !== null && this.ctx.state === "running";
  }

  get sampleRate() {
    return this.ctx?.sampleRate ?? 48000;
  }

  get targetFrames() {
    return this.sampleRate * TARGET_SECONDS;
  }

  /** Sends interleaved stereo samples to the worklet (the buffer moves, no copy). */
  push(samples) {
    if (!samples.length) return;
    this.node.port.postMessage(samples, [samples.buffer]);
    this.sentSinceReport += samples.length / 2;
  }

  /** Frames queued in the worklet now: its last report, less what has
   * played since, plus what has been sent since. */
  buffered(now) {
    const played = ((now - this.report.at) / 1000) * this.sampleRate;
    return Math.max(0, this.report.buffered - played + this.sentSinceReport);
  }

  setMuted(muted) {
    this.muted = muted;
    if (this.gain) this.gain.gain.value = muted ? 0 : VOLUME;
  }
}
