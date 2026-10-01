// Battery saves in the browser. The emulator hands over .sav bytes; these
// keep them in localStorage between visits, keyed by a hash of the ROM so
// different games (or versions of one) never share a save. Storage is passed
// in, so tests can use a fake.

const PREFIX = "gb-emu:save:";

/** Unix time in whole seconds, what the emulator stamps the MBC3 clock with. */
export const nowSeconds = () => Math.floor(Date.now() / 1000);

/** The storage key for a ROM: its SHA-256, shortened. */
export async function saveKey(romBytes) {
  const digest = new Uint8Array(await crypto.subtle.digest("SHA-256", romBytes));
  const hex = Array.from(digest.slice(0, 16), (b) => b.toString(16).padStart(2, "0")).join("");
  return PREFIX + hex;
}

/** Bytes → base64, since localStorage only holds strings. */
export function encode(bytes) {
  let binary = "";
  const CHUNK = 0x8000; // String.fromCharCode can't take 128 KiB of arguments at once
  for (let i = 0; i < bytes.length; i += CHUNK) {
    binary += String.fromCharCode(...bytes.subarray(i, i + CHUNK));
  }
  return btoa(binary);
}

/** base64 → bytes. */
export function decode(text) {
  return Uint8Array.from(atob(text), (c) => c.charCodeAt(0));
}

/** The stored save for `key`, or null if there is none (or storage is off). */
export function readSave(storage, key) {
  try {
    const text = storage?.getItem(key);
    return text ? decode(text) : null;
  } catch {
    return null;
  }
}

/** Stores a save. False if it couldn't be (storage full, disabled, private
 * browsing): the caller should tell the player to export instead. */
export function writeSave(storage, key, bytes) {
  try {
    storage.setItem(key, encode(bytes));
    return true;
  } catch {
    return false;
  }
}

/** A file name for an exported save, from the game's title. */
export function saveFileName(title) {
  const safe = title.replace(/[^\w\- ]+/g, "").trim() || "game";
  return `${safe}.sav`;
}
