// The link cable in the browser. A LinkSession carries bytes between this
// page's emulator and a partner's over any transport that can send small
// messages; TabLink pairs two tabs of this site over a BroadcastChannel (a
// WebRTC transport comes later). Plain logic, tested with node --test
// "web/*.test.js" using fake emulators and channels.
//
// Messages: { t: "byte", seq, v } carries a byte this side clocked out as
// master; the partner answers { t: "reply", seq, v } with what its Game Boy
// shifted back. See gb-core's serial module for the Game Boy side.

/** What Emulator.run_frame() returns. */
export const FRAME_DONE = 0;
export const FRAME_BREAKPOINT = 1;
export const FRAME_LINK_WAIT = 2;

export class LinkSession {
  /** `send(msg)` delivers a message to the partner. */
  constructor(send) {
    this.send = send;
    this.emu = null;
    this.connected = false;
    this.seq = 0;
    this.pending = null; // seq of the byte we're waiting to hear back about
    this.bytes = 0; // bytes swapped, for the status line
  }

  /** A new emulator (loaded, reset): plug it in if we're connected. */
  attach(emu) {
    this.emu = emu;
    this.pending = null;
    emu?.plug_link(this.connected);
  }

  /** The partner is there: plug the cable in. */
  connect() {
    this.connected = true;
    this.pending = null;
    this.emu?.plug_link(true);
  }

  /** The partner left: pull the cable, which frees a master waiting on it. */
  disconnect() {
    this.connected = false;
    this.pending = null;
    this.emu?.plug_link(false);
  }

  /** Call after running the emulator: sends what it clocked out. */
  pump() {
    if (!this.connected || !this.emu) return;
    for (let v = this.emu.take_link_out(); v !== undefined; v = this.emu.take_link_out()) {
      this.seq += 1;
      this.pending = this.seq;
      this.send({ t: "byte", seq: this.seq, v });
    }
  }

  /** A message from the partner. Returns true if it was the answer this
   * side's transfer was waiting for, so the emulator can carry on. */
  receive(msg) {
    if (!this.connected || !this.emu) return false;
    if (msg.t === "byte") {
      const back = this.emu.link_clocked(msg.v & 0xff);
      this.send({ t: "reply", seq: msg.seq, v: back });
      this.bytes += 1;
    } else if (msg.t === "reply" && msg.seq === this.pending) {
      this.pending = null;
      this.emu.link_answer(msg.v & 0xff);
      this.bytes += 1;
      return true;
    }
    return false;
  }
}

/** A linked tab pings this often, and counts its partner gone after this
 * long without a word (a closed tab can't always say "bye"). */
export const PING_MS = 1000;
export const TIMEOUT_MS = 10_000;

/**
 * Pairs this tab with one other tab that's also looking, over a
 * BroadcastChannel (same browser, same site). Three messages make sure only
 * two tabs pair when more are open: "hello" (anyone there?), "ack" (me!),
 * "confirm" (you, then). After that every message names its sender and
 * receiver; "bye" ends it, and so does silence (call `tick` regularly).
 */
export class TabLink {
  /** `channel`: a BroadcastChannel (or a stand-in); `events`: { paired(),
   * unpaired(), message(msg) }; `id` and `now` (ms) are for tests. */
  constructor(channel, events, { id = Math.random().toString(36).slice(2), now } = {}) {
    this.channel = channel;
    this.events = events;
    this.id = id;
    this.now = now ?? (() => performance.now());
    this.looking = false;
    this.partner = null;
    this.lastHeard = 0;
    this.lastSent = 0;
    channel.onmessage = (e) => this.onMessage(e.data);
  }

  /** Keeps a link alive (a ping when quiet) and notices a partner gone
   * silent; then unplugs and looks again, so it can come back. */
  tick() {
    if (!this.partner) return;
    const now = this.now();
    if (now - this.lastHeard > TIMEOUT_MS) {
      this.partner = null;
      this.start(); // looking again before anyone asks how things stand
      this.events.unpaired();
    } else if (now - this.lastSent >= PING_MS) {
      this.post({ t: "ping", to: this.partner });
    }
  }

  /** Starts looking for another tab. */
  start() {
    if (this.looking || this.partner) return;
    this.looking = true;
    this.post({ t: "hello" });
  }

  /** Stops looking, or leaves the partner. */
  stop() {
    if (this.partner) this.post({ t: "bye", to: this.partner });
    const was = this.partner;
    this.looking = false;
    this.partner = null;
    if (was) this.events.unpaired();
  }

  /** Sends a link message to the partner. */
  send(msg) {
    if (this.partner) this.post({ t: "link", to: this.partner, msg });
  }

  post(m) {
    this.channel.postMessage({ ...m, from: this.id });
    if (m.to && m.to === this.partner) this.lastSent = this.now();
  }

  onMessage(m) {
    if (!m || m.from === this.id || (m.to && m.to !== this.id)) return;
    if (m.from === this.partner) this.lastHeard = this.now();
    if (m.t === "hello" && this.looking && !this.partner) {
      this.post({ t: "ack", to: m.from });
    } else if (m.t === "ack" && this.looking && !this.partner) {
      this.pair(m.from);
      this.post({ t: "confirm", to: m.from });
    } else if (m.t === "confirm" && this.looking && !this.partner) {
      this.pair(m.from);
    } else if (m.t === "bye" && m.from === this.partner) {
      this.partner = null;
      this.start(); // keep looking, so the other tab can come back
      this.events.unpaired();
    } else if (m.t === "link" && m.from === this.partner) {
      this.events.message(m.msg);
    }
  }

  pair(id) {
    this.partner = id;
    this.looking = false;
    this.lastHeard = this.lastSent = this.now();
    this.events.paired();
  }
}
