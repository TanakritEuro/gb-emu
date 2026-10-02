// Run with: node --test "web/*.test.js"
import test from "node:test";
import assert from "node:assert/strict";
import { LinkSession, TabLink, PING_MS, TIMEOUT_MS } from "./link.js";

/** Stands in for an Emulator's link cable: `sb` is what it sends back when
 * clocked as slave, `out` the bytes it clocks out as master. */
function fakeEmu(sb = 0xff) {
  return {
    plugged: false,
    sb,
    out: [],
    answers: [],
    clocked: [],
    plug_link(on) {
      this.plugged = on;
    },
    take_link_out() {
      return this.out.shift();
    },
    link_answer(v) {
      this.answers.push(v);
    },
    link_clocked(v) {
      this.clocked.push(v);
      return this.sb;
    },
  };
}

/** Two sessions wired straight to each other. */
function pair() {
  const a = new LinkSession((m) => b.receive(m));
  const b = new LinkSession((m) => a.receive(m));
  return [a, b];
}

test("a master's byte reaches the partner and its answer comes back", () => {
  const [a, b] = pair();
  const ea = fakeEmu(), eb = fakeEmu(0x99);
  a.attach(ea);
  b.attach(eb);
  a.connect();
  b.connect();
  assert.ok(ea.plugged && eb.plugged, "connecting plugs the cable in");
  ea.out.push(0x42);
  a.pump();
  assert.deepEqual(eb.clocked, [0x42], "the slave was clocked");
  assert.deepEqual(ea.answers, [0x99], "the master got the slave's byte");
  assert.equal(a.pending, null);
  assert.equal(a.bytes + b.bytes, 2);
  assert.equal(a.receive({ t: "reply", seq: a.seq, v: 1 }), false, "already answered");
  assert.equal(b.receive({ t: "byte", seq: 5, v: 1 }), false, "a byte isn't an answer");
});

test("nothing is sent while disconnected, and disconnecting pulls the cable", () => {
  const sent = [];
  const s = new LinkSession((m) => sent.push(m));
  const emu = fakeEmu();
  s.attach(emu);
  emu.out.push(1);
  s.pump();
  assert.deepEqual(sent, [], "not connected");
  s.connect();
  s.pump();
  assert.equal(sent.length, 1);
  s.disconnect();
  assert.equal(emu.plugged, false);
  s.receive({ t: "byte", seq: 9, v: 5 });
  assert.deepEqual(emu.clocked, [], "ignored once disconnected");
});

test("an answer to an old byte is ignored", () => {
  const sent = [];
  const s = new LinkSession((m) => sent.push(m));
  const emu = fakeEmu();
  s.attach(emu);
  s.connect();
  emu.out.push(1);
  s.pump();
  const first = sent[0].seq;
  s.attach(emu); // e.g. Reset: the old transfer is gone
  s.receive({ t: "reply", seq: first, v: 7 });
  assert.deepEqual(emu.answers, []);
});

test("a new emulator is plugged in if the session is connected", () => {
  const s = new LinkSession(() => {});
  s.connect();
  const emu = fakeEmu();
  s.attach(emu);
  assert.ok(emu.plugged);
});

/** BroadcastChannel stand-ins on one hub; messages queue until flush(),
 * like the real thing delivering them later. */
function hub() {
  const channels = [];
  const queue = [];
  return {
    channel() {
      const c = {
        onmessage: null,
        postMessage(data) {
          for (const other of channels) {
            if (other !== c) queue.push([other, structuredClone(data)]);
          }
        },
      };
      channels.push(c);
      return c;
    },
    flush() {
      while (queue.length) {
        const [c, data] = queue.shift();
        c.onmessage?.({ data });
      }
    },
  };
}

/** A tab on hub `h`, with a clock the test moves (`clock.t`, ms). */
function tab(h, id, clock = { t: 0 }) {
  const log = { paired: 0, unpaired: 0, messages: [] };
  const link = new TabLink(
    h.channel(),
    {
      paired: () => log.paired++,
      unpaired: () => log.unpaired++,
      message: (m) => log.messages.push(m),
    },
    { id, now: () => clock.t },
  );
  return { link, log };
}

test("two tabs that both look pair up and can talk", () => {
  const h = hub();
  const a = tab(h, "a"), b = tab(h, "b");
  a.link.start();
  h.flush(); // nobody else looking yet
  assert.equal(a.link.partner, null);
  b.link.start();
  h.flush();
  assert.equal(a.link.partner, "b");
  assert.equal(b.link.partner, "a");
  a.link.send({ t: "byte", seq: 1, v: 0x42 });
  h.flush();
  assert.deepEqual(b.log.messages, [{ t: "byte", seq: 1, v: 0x42 }]);
});

test("with three tabs looking, only two pair", () => {
  const h = hub();
  const a = tab(h, "a"), b = tab(h, "b"), c = tab(h, "c");
  b.link.start();
  c.link.start();
  h.flush(); // b and c find each other
  a.link.start();
  h.flush();
  assert.equal(b.link.partner, "c");
  assert.equal(c.link.partner, "b");
  assert.equal(a.link.partner, null, "a waits for someone free");
  // b leaves: c goes back to looking and finds a.
  b.link.stop();
  h.flush();
  assert.equal(c.link.partner, "a");
  assert.equal(a.link.partner, "c");
  assert.equal(c.log.unpaired, 1);
});

test("messages for someone else are ignored", () => {
  const h = hub();
  const a = tab(h, "a"), b = tab(h, "b"), c = tab(h, "c");
  a.link.start();
  b.link.start();
  h.flush();
  c.link.start();
  h.flush();
  a.link.send({ t: "byte", seq: 1, v: 1 });
  h.flush();
  assert.deepEqual(c.log.messages, []);
});

test("linked tabs keep pinging, and a silent partner counts as gone", () => {
  const h = hub();
  const clock = { t: 0 };
  const a = tab(h, "a", clock), b = tab(h, "b", clock);
  a.link.start();
  b.link.start();
  h.flush();
  // Both tick regularly: the pings keep the link up long past the timeout.
  for (clock.t = 0; clock.t <= 3 * TIMEOUT_MS; clock.t += PING_MS / 2) {
    a.link.tick();
    b.link.tick();
    h.flush();
  }
  assert.equal(a.link.partner, "b");
  assert.equal(b.link.partner, "a");
  // b closes without a word (a crashed or killed tab): a gives up on it.
  const silentFrom = clock.t;
  while (clock.t <= silentFrom + TIMEOUT_MS + PING_MS) {
    a.link.tick();
    h.flush();
    clock.t += PING_MS / 2;
  }
  assert.equal(a.link.partner, null);
  assert.equal(a.log.unpaired, 1);
  assert.ok(a.link.looking, "and looks for a partner again");
});

test("when a partner goes, the tab is already looking again as it's told", () => {
  const h = hub();
  const seenLooking = [];
  const a = new TabLink(h.channel(), {
    paired() {},
    unpaired() {
      seenLooking.push(a.looking);
    },
    message() {},
  }, { id: "a" });
  const b = tab(h, "b");
  a.start();
  b.link.start();
  h.flush();
  b.link.stop(); // says bye
  h.flush();
  assert.deepEqual(seenLooking, [true]);
});
