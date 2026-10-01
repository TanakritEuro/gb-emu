// Serves web/ for local development. No dependencies, just Node.
//
//   node scripts/serve.js [port]        (default port 8765)
//
// Then open http://localhost:8765. Build web/pkg first with build-web.ps1/.sh.
//
// Why not port 8080: Windows often reserves it (Hyper-V / Docker excluded port
// ranges), and binding it fails. Why not `python -m http.server`: it works too,
// but this needs no Python, sends the application/wasm MIME type, and disables
// caching so a rebuilt web/pkg shows up on reload.

const http = require("http");
const fs = require("fs");
const path = require("path");

const root = path.resolve(__dirname, "..", "web");
const port = Number(process.argv[2] || 8765);
const types = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".json": "application/json",
  ".wasm": "application/wasm",
  ".png": "image/png",
  ".svg": "image/svg+xml",
  ".ico": "image/x-icon",
};

const server = http.createServer((req, res) => {
  let url;
  try {
    url = decodeURIComponent(new URL(req.url, "http://localhost").pathname);
  } catch {
    res.writeHead(400);
    return res.end("bad request");
  }
  const file = path.join(root, url.endsWith("/") ? url + "index.html" : url);
  // Refuse anything that resolves outside web/ (e.g. /../Cargo.toml).
  if (!file.startsWith(root + path.sep)) {
    res.writeHead(403);
    return res.end("forbidden");
  }
  fs.readFile(file, (err, data) => {
    if (err) {
      res.writeHead(404);
      return res.end("not found");
    }
    res.writeHead(200, {
      "Content-Type": types[path.extname(file)] || "application/octet-stream",
      "Cache-Control": "no-store",
    });
    res.end(data);
  });
});

server.on("error", (err) => {
  console.error(`can't serve on port ${port}: ${err.message}`);
  console.error("try another port: node scripts/serve.js 9000");
  process.exit(1);
});

// Localhost only: this is a dev server, not something to expose on the network.
server.listen(port, "127.0.0.1", () => {
  console.log(`serving ${root} at http://localhost:${port}`);
});
