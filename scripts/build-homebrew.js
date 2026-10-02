// Builds the games in homebrew/ with RGBDS (https://rgbds.gbdev.io) into
// web/games/, where the page offers them. The ROMs are build products, not
// committed (like web/pkg).
//
// RGBDS's tools are found on PATH, or in the folder named by the RGBDS
// environment variable, or in ~/rgbds.
//
// Usage: node scripts/build-homebrew.js
import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync } from "node:fs";
import { homedir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const exe = process.platform === "win32" ? ".exe" : "";

/** Where an RGBDS tool is: a full path, or just its name to find on PATH. */
function tool(name) {
  for (const dir of [process.env.RGBDS, join(homedir(), "rgbds")]) {
    if (dir && existsSync(join(dir, name + exe))) return join(dir, name + exe);
  }
  return name;
}

/** Each game: its source, the ROM it makes, and the title in its header. */
const games = [{ src: "homebrew/link-pong/link-pong.asm", out: "web/games/link-pong.gb", title: "LINKPONG" }];

const objDir = join(root, "target", "homebrew");
mkdirSync(objDir, { recursive: true });
mkdirSync(join(root, "web", "games"), { recursive: true });

const run = (name, args) => execFileSync(tool(name), args, { cwd: root, stdio: "inherit" });

try {
  for (const g of games) {
    const obj = join(objDir, g.title + ".o");
    run("rgbasm", ["-Wall", "-o", obj, g.src]);
    run("rgblink", ["-o", g.out, "-n", join(objDir, g.title + ".sym"), obj]);
    // A valid header: Nintendo logo, title, checksums; padded to 32 KiB.
    run("rgbfix", ["-v", "-p", "0xFF", "-t", g.title, g.out]);
    console.log(`built ${g.out}`);
  }
} catch (e) {
  if (e.code === "ENOENT") {
    console.error("RGBDS not found: install it (https://rgbds.gbdev.io) or set RGBDS to its folder");
  }
  process.exit(1);
}
