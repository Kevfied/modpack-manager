// CDP driver for UI verification — talks to a running app's WebView2
// (launched with WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=9222).
// Usage: node scripts/cdp.mjs <eval-file> [screenshot.png]
//   eval-file: JS body for Runtime.evaluate (returns a value)
//   screenshot.png: optional — Page.captureScreenshot output
import WebSocket from "ws";

const evalFile = process.argv[2];
const shotPath = process.argv[3];
const fs = await import("fs");
const expression =
  evalFile && evalFile !== "-"
    ? fs.readFileSync(evalFile, "utf8")
    : process.argv[4] ?? "";

const list = await (await fetch("http://127.0.0.1:9222/json/list")).json();
const page = list.find((t) => t.type === "page");
if (!page) throw new Error("no page target");
const ws = new WebSocket(page.webSocketDebuggerUrl, { maxPayload: 64 * 1024 * 1024 });
let id = 0;
const pend = new Map();
const send = (method, params = {}) =>
  new Promise((res, rej) => {
    const mid = ++id;
    pend.set(mid, { res, rej });
    ws.send(JSON.stringify({ id: mid, method, params }));
  });
ws.on("message", (d) => {
  const m = JSON.parse(d);
  if (m.id && pend.has(m.id)) {
    pend.get(m.id).res(m.result ?? m.error);
    pend.delete(m.id);
  }
});
await new Promise((r) => ws.on("open", r));
if (expression) {
  const r = await send("Runtime.evaluate", {
    expression,
    returnByValue: true,
    awaitPromise: true,
  });
  console.log(JSON.stringify(r.result?.value ?? r, null, 2));
}
if (shotPath) {
  const shot = await send("Page.captureScreenshot", { format: "png" });
  fs.writeFileSync(shotPath, Buffer.from(shot.data, "base64"));
  console.log("saved", shotPath);
}
ws.close();
process.exit(0);
