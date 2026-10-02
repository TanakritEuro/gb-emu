// The link cable over the internet: a WebRTC data channel straight between
// two browsers. There's no server to introduce them, so the players carry
// the introductions: the host's invite code (a WebRTC "offer") goes to the
// guest, whose reply code (the "answer") comes back. Each holds a session
// description, including the network addresses where that browser can be
// reached (found with a public STUN server), compressed into one line of
// text. After that, bytes travel directly between the two browsers.
//
// Codes are tested with node --test "web/*.test.js"; RtcLink and NetPanel
// need a browser.

/** Public STUN servers, which tell a browser how its address looks from
 * outside its router. (No TURN relay: networks that forbid direct
 * connections can't link.) */
export const ICE_SERVERS = [
  { urls: "stun:stun.l.google.com:19302" },
  { urls: "stun:stun.cloudflare.com:3478" },
];

const PREFIX = "GBLINK1.";
/** Longest wait for this side's network addresses before making a code. */
const GATHER_MS = 5000;

/** Turns a session description into a code: "GBLINK1." then base64url of
 * the deflated JSON. */
export async function encodeCode(kind, sdp) {
  const json = new TextEncoder().encode(JSON.stringify({ kind, sdp }));
  const packed = await pipe(json, new CompressionStream("deflate-raw"));
  let text = "";
  for (const b of packed) text += String.fromCharCode(b);
  return PREFIX + btoa(text).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

/** Reads a code back: { kind: "offer" | "answer", sdp }. Throws a readable
 * error for anything else. */
export async function decodeCode(code) {
  const text = String(code).replace(/\s+/g, "");
  if (!text.startsWith(PREFIX)) throw new Error("that isn't a link code");
  try {
    const b64 = text.slice(PREFIX.length).replace(/-/g, "+").replace(/_/g, "/");
    const bytes = Uint8Array.from(atob(b64), (c) => c.charCodeAt(0));
    const json = await pipe(bytes, new DecompressionStream("deflate-raw"));
    const { kind, sdp } = JSON.parse(new TextDecoder().decode(json));
    if ((kind !== "offer" && kind !== "answer") || typeof sdp !== "string") throw new Error();
    return { kind, sdp };
  } catch {
    throw new Error("that link code is damaged (copy all of it)");
  }
}

/** All of `bytes` through a (de)compression stream. */
async function pipe(bytes, stream) {
  const out = new Response(new Blob([bytes]).stream().pipeThrough(stream));
  return new Uint8Array(await out.arrayBuffer());
}

/**
 * One end of a link over WebRTC: the host calls invite() then finish(reply),
 * the guest join(invite). Once the data channel opens, `events.paired()`;
 * messages arrive at `events.message(msg)`; when it closes or the
 * connection fails, `events.unpaired()`.
 */
export class RtcLink {
  constructor(events, { RTCPeerConnection = globalThis.RTCPeerConnection } = {}) {
    this.events = events;
    this.Peer = RTCPeerConnection;
    this.pc = null;
    this.channel = null;
    this.connected = false;
  }

  /** Host: makes the invite code. */
  async invite() {
    this.close();
    const pc = this.newPeer();
    this.useChannel(pc.createDataChannel("link", { ordered: true }));
    await pc.setLocalDescription(await pc.createOffer());
    return encodeCode("offer", await this.gathered());
  }

  /** Guest: takes the host's invite code, makes the reply code. */
  async join(inviteCode) {
    const { kind, sdp } = await decodeCode(inviteCode);
    if (kind !== "offer") throw new Error("that's a reply code: paste it on the inviting side");
    this.close();
    const pc = this.newPeer();
    pc.ondatachannel = (e) => this.useChannel(e.channel);
    await pc.setRemoteDescription({ type: "offer", sdp });
    await pc.setLocalDescription(await pc.createAnswer());
    return encodeCode("answer", await this.gathered());
  }

  /** Host: takes the guest's reply code; the link opens soon after. */
  async finish(replyCode) {
    const { kind, sdp } = await decodeCode(replyCode);
    if (kind !== "answer") throw new Error("that's an invite code: paste it on the joining side");
    if (!this.pc) throw new Error("make an invite first");
    await this.pc.setRemoteDescription({ type: "answer", sdp });
  }

  send(msg) {
    if (this.channel?.readyState === "open") this.channel.send(JSON.stringify(msg));
  }

  /** Hangs up (the partner's channel closes too). */
  close() {
    const was = this.connected;
    this.connected = false;
    this.channel?.close();
    this.pc?.close();
    this.channel = null;
    this.pc = null;
    if (was) this.events.unpaired();
  }

  newPeer() {
    const pc = new this.Peer({ iceServers: ICE_SERVERS });
    pc.onconnectionstatechange = () => {
      if (pc === this.pc && (pc.connectionState === "failed" || pc.connectionState === "closed")) {
        this.close();
      }
    };
    this.pc = pc;
    return pc;
  }

  useChannel(channel) {
    this.channel = channel;
    channel.onopen = () => {
      this.connected = true;
      this.events.paired();
    };
    channel.onclose = () => {
      if (channel === this.channel) this.close();
    };
    channel.onmessage = (e) => {
      try {
        this.events.message(JSON.parse(e.data));
      } catch {
        // Not ours: ignore it.
      }
    };
  }

  /** This side's description once its network addresses are found (or
   * after GATHER_MS with what there is), so one code says it all. */
  gathered() {
    const pc = this.pc;
    return new Promise((resolve) => {
      const done = () => resolve(pc.localDescription.sdp);
      if (pc.iceGatheringState === "complete") return done();
      const timer = setTimeout(done, GATHER_MS);
      pc.addEventListener("icegatheringstatechange", () => {
        if (pc.iceGatheringState === "complete") {
          clearTimeout(timer);
          done();
        }
      });
    });
  }
}
