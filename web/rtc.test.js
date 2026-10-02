// Run with: node --test "web/*.test.js"
import test from "node:test";
import assert from "node:assert/strict";
import { encodeCode, decodeCode } from "./rtc.js";

// A WebRTC offer of the size browsers make for a data channel.
const SDP = [
  "v=0",
  "o=- 4611731400430051336 2 IN IP4 127.0.0.1",
  "s=-",
  "t=0 0",
  "a=group:BUNDLE 0",
  "a=msid-semantic: WMS",
  "m=application 9 UDP/DTLS/SCTP webrtc-datachannel",
  "c=IN IP4 0.0.0.0",
  "a=candidate:1 1 udp 2113937151 3f1b2c4d-1111-2222-3333-444455556666.local 54321 typ host",
  "a=candidate:2 1 udp 1677729535 203.0.113.7 54321 typ srflx raddr 0.0.0.0 rport 0",
  "a=ice-ufrag:abcd",
  "a=ice-pwd:0123456789abcdefghijklmn",
  "a=fingerprint:sha-256 " + Array.from({ length: 32 }, (_, i) => (i * 7).toString(16).padStart(2, "0").toUpperCase()).join(":"),
  "a=setup:actpass",
  "a=mid:0",
  "a=sctp-port:5000",
  "a=max-message-size:262144",
  "",
].join("\r\n");

test("a description survives being a code, and the code is one shorter line", async () => {
  const code = await encodeCode("offer", SDP);
  assert.match(code, /^GBLINK1\.[A-Za-z0-9_-]+$/, "safe to paste anywhere");
  assert.ok(code.length < SDP.length, `${code.length} < ${SDP.length}`);
  assert.deepEqual(await decodeCode(code), { kind: "offer", sdp: SDP });
});

test("line breaks and spaces picked up in a chat app don't matter", async () => {
  const code = await encodeCode("answer", SDP);
  const mangled = ` ${code.slice(0, 40)}\n  ${code.slice(40)} \n`;
  assert.equal((await decodeCode(mangled)).kind, "answer");
});

test("anything else is refused with a readable reason", async () => {
  await assert.rejects(decodeCode("hello"), /isn't a link code/);
  const code = await encodeCode("offer", SDP);
  await assert.rejects(decodeCode(code.slice(0, -20)), /damaged/, "cut short");
  await assert.rejects(decodeCode("GBLINK1.!!!"), /damaged/);
});
