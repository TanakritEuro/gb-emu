// Runs on the browser's audio thread: plays the emulator's samples from a
// queue, one 128-frame block at a time, and tells the page how full the
// queue is so the page can run just enough Game Boy frames to keep it there.
import { SampleQueue } from "./audio-queue.js";

/** Report the queue level to the page every this many blocks (~21 ms at 48 kHz). */
const REPORT_EVERY = 8;

class GameBoyAudio extends AudioWorkletProcessor {
  constructor() {
    super();
    // `sampleRate` is a global here. Keep at most half a second queued.
    this.queue = new SampleQueue(sampleRate / 2);
    this.blocks = 0;
    this.port.onmessage = (e) => this.queue.push(e.data);
  }

  process(_inputs, outputs) {
    const [left, right] = outputs[0];
    if (right) {
      this.queue.pull(left, right);
    } else {
      this.queue.pull(left, new Float32Array(left.length)); // mono output: left only
    }
    if (++this.blocks % REPORT_EVERY === 0) {
      this.port.postMessage({ buffered: this.queue.frames, underruns: this.queue.underruns });
    }
    return true;
  }
}

registerProcessor("gb-audio", GameBoyAudio);
