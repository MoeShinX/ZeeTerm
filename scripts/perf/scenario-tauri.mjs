// V2-G13a: the same scenario as scenario-zeeterm.ps1, driven in Z-SSH 0.2.14 (Tauri, WebView2) over CDP.
// Run by scenario-tauri.ps1 (which starts the exe with --remote-debugging-port, the memory sampler and the
// window sizing). This file only drives the page and writes marks / probe results (epoch ms).
//
//   node scenario-tauri.mjs --out <dir> [--port 9222] [--idle-probes 20] [--flood-probes 40] [--lines 12400000]
//
// No server address appears here: the host ("perf-host") already exists in the isolated data dir.
import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const arg = (name, dflt) => {
  const i = process.argv.indexOf(`--${name}`);
  return i >= 0 ? process.argv[i + 1] : dflt;
};
const OUT = arg("out", "");
const PORT = Number(arg("port", "9222"));
const IDLE_PROBES = Number(arg("idle-probes", "20"));
const FLOOD_PROBES = Number(arg("flood-probes", "40"));
const LINES = Number(arg("lines", "12400000"));
const IDLE_SECONDS = Number(arg("idle-seconds", "30"));
const POST_SECONDS = Number(arg("post-seconds", "60"));
if (!OUT) throw new Error("--out is required");
mkdirSync(OUT, { recursive: true });

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const marks = [];
const mark = (name) => {
  marks.push({ name, epoch_ms: Date.now() });
  console.log(`mark ${name}`);
};

// ---------------------------------------------------------------- CDP
let ws;
let nextId = 1;
const pending = new Map();
async function connect() {
  for (let i = 0; i < 100; i += 1) {
    try {
      const targets = await (await fetch(`http://127.0.0.1:${PORT}/json`)).json();
      const page = targets.find((t) => t.type === "page" && t.url.includes("localhost"));
      if (page) {
        ws = new WebSocket(page.webSocketDebuggerUrl);
        await new Promise((resolve, reject) => {
          ws.addEventListener("open", resolve, { once: true });
          ws.addEventListener("error", reject, { once: true });
        });
        ws.addEventListener("message", (event) => {
          const msg = JSON.parse(event.data);
          if (msg.id && pending.has(msg.id)) {
            const { resolve, reject } = pending.get(msg.id);
            pending.delete(msg.id);
            if (msg.error) reject(new Error(JSON.stringify(msg.error)));
            else resolve(msg.result);
          }
        });
        return;
      }
    } catch {
      // not up yet
    }
    await sleep(200);
  }
  throw new Error("no page target");
}
function send(method, params = {}) {
  const id = nextId++;
  return new Promise((resolve, reject) => {
    pending.set(id, { resolve, reject });
    ws.send(JSON.stringify({ id, method, params }));
    setTimeout(() => {
      if (pending.has(id)) {
        pending.delete(id);
        reject(new Error(`CDP timeout: ${method}`));
      }
    }, 30000);
  });
}
async function evaluate(expression) {
  const r = await send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true });
  if (r.exceptionDetails) throw new Error("page error: " + JSON.stringify(r.exceptionDetails.exception?.description ?? r.exceptionDetails));
  return r.result.value;
}
async function waitFor(expr, timeout = 30000, label = expr) {
  const end = Date.now() + timeout;
  let lastError = '';
  while (Date.now() < end) {
    try {
      if (await evaluate(`(()=>{try{return Boolean(${expr})}catch(e){return false}})()`)) return true;
    } catch (error) {
      lastError = String(error).slice(0, 200);
    }
    await sleep(100);
  }
  throw new Error(`timeout: ${label} (last error: ${lastError})`);
}

// ---------------------------------------------------------------- page helpers
// Collect every xterm Terminal the app constructs: the public Terminal assigns `this._core = ...` in its
// constructor, so an inherited setter on Object.prototype sees each instance exactly once. Injected before
// the bundle runs (Page.addScriptToEvaluateOnNewDocument) and then the page is reloaded.
const HOOK = `(() => {
  if (globalThis.__zzTerms) return;
  globalThis.__zzTerms = [];
  Object.defineProperty(Object.prototype, '_core', {
    configurable: true,
    set(v) {
      Object.defineProperty(this, '_core', { value: v, writable: true, configurable: true, enumerable: true });
      if (this && typeof this.write === 'function' && this.buffer) globalThis.__zzTerms.push(this);
    },
  });
})()`;

const HELPERS = `(() => {
  if (window.__zz) return true;
  const visible = (el) => { const r = el.getBoundingClientRect(); return r.width > 0 && r.height > 0 && getComputedStyle(el).visibility !== 'hidden'; };
  window.__zz = {
    byText(text, scope) {
      const root = scope ? document.querySelector(scope) : document;
      const m = Array.from((root ?? document).querySelectorAll('*')).filter((el) => visible(el) && (el.textContent ?? '').trim() === text);
      return m.sort((a, b) => a.querySelectorAll('*').length - b.querySelectorAll('*').length)[0] ?? null;
    },
    dblclickCard(text) {
      // the host name also appears in the left tree (a button): take the one inside a clickable card div
      const root = document.querySelector('[data-section=vault]');
      const hit = Array.from((root ?? document).querySelectorAll('*')).filter((el) => visible(el) && (el.textContent ?? '').trim() === text && el.closest('div[class*=cursor-pointer]'))[0];
      const card = hit && hit.closest('div[class*=cursor-pointer]');
      if (!card) return false;
      card.dispatchEvent(new MouseEvent('dblclick', { bubbles: true, cancelable: true, detail: 2 }));
      return true;
    },
    clickTab(text) {
      const el = window.__zz.byText(text, '[data-section=top-tabs]');
      if (!el) return false;
      (el.closest('div[class*=cursor-pointer]') ?? el).click();
      return true;
    },
    clickText(text) { const el = window.__zz.byText(text); if (!el) return false; el.click(); return true; },
    termVisible(i) {
      const t = globalThis.__zzTerms[i]; if (!t || !t.element) return false;
      let n = t.element; while (n) { const s = getComputedStyle(n); if (s.display === 'none' || s.visibility === 'hidden') return false; n = n.parentElement; }
      return true;
    },
    lastLine(i) {
      const t = globalThis.__zzTerms[i]; if (!t) return null; const b = t.buffer.active;
      for (let y = b.length - 1; y >= 0; y -= 1) { const l = b.getLine(y); const s = l ? l.translateToString(true) : ''; if (s.trim()) return s; }
      return '';
    },
    tail(i, n) {
      const t = globalThis.__zzTerms[i]; if (!t) return ''; const b = t.buffer.active; const out = [];
      for (let y = b.length - 1; y >= 0 && out.length < n; y -= 1) { const l = b.getLine(y); const s = l ? l.translateToString(true) : ''; if (s.trim()) out.unshift(s); }
      return out.join('\\n');
    },
  };
  return true;
})()`;

const ENTER = { key: "Enter", code: "Enter", windowsVirtualKeyCode: 13, text: "\r" };
async function pressEnter() {
  await send("Input.dispatchKeyEvent", { type: "keyDown", ...ENTER });
  await send("Input.dispatchKeyEvent", { type: "char", text: "\r", key: "Enter", unmodifiedText: "\r" });
  await send("Input.dispatchKeyEvent", { type: "keyUp", ...ENTER });
}
async function pressCtrl(letter) {
  const upper = letter.toUpperCase();
  const base = { key: letter, code: `Key${upper}`, windowsVirtualKeyCode: upper.charCodeAt(0), modifiers: 2 };
  await send("Input.dispatchKeyEvent", { type: "rawKeyDown", ...base });
  await send("Input.dispatchKeyEvent", { type: "keyUp", ...base });
}
async function typeFast(text) {
  for (const ch of text) {
    await send("Input.dispatchKeyEvent", { type: "keyDown", text: ch, unmodifiedText: ch, key: ch });
    await send("Input.dispatchKeyEvent", { type: "keyUp", key: ch });
  }
}
async function runLine(text) {
  await typeFast(text);
  await pressEnter();
}

// ---------------------------------------------------------------- scenario
async function openHostTab(expected) {
  await evaluate(`window.__zz.clickTab('主机')`);
  await sleep(300);
  if (!(await evaluate(`window.__zz.dblclickCard('perf-host')`))) throw new Error("host card not found");
  for (let i = 0; i < 80; i += 1) {
    const text = await evaluate("document.body.innerText");
    if (text.includes("第一次连接这台主机")) await evaluate("window.__zz.clickText('信任并保存')");
    if ((await evaluate("globalThis.__zzTerms.length")) >= expected) {
      const last = await evaluate(`window.__zz.lastLine(${expected - 1})`);
      if (last) return;
    }
    await sleep(250);
  }
  throw new Error(`tab ${expected} did not come up`);
}
async function selectTab(index) {
  const ok = await evaluate(`(() => {
    const tabs = Array.from(document.querySelectorAll('[data-section=top-tabs] div[class*=cursor-pointer]')).filter((el) => (el.textContent ?? '').includes('perf-host'));
    const el = tabs[${index}]; if (!el) return false; el.click(); return true; })()`);
  if (!ok) throw new Error(`tab ${index} not found`);
  await waitFor(`window.__zz.termVisible(${index})`, 5000, `terminal ${index} visible`);
  await evaluate(`globalThis.__zzTerms[${index}].focus()`);
  await sleep(150);
}

const probes = [];
async function probe(phase, prefix, tabIndex, n) {
  const now = () => performance.timeOrigin + performance.now();
  for (let i = 1; i <= n; i += 1) {
    const tok = `${prefix}${String(i).padStart(2, "0")}`;
    const t0 = now();
    await typeFast(tok);
    const sent = now();
    let seen = null;
    const end = sent + 15000;
    while (now() < end) {
      const tail = await evaluate(`window.__zz.tail(${tabIndex}, 12)`);
      if (tail.includes(tok)) {
        seen = now();
        break;
      }
    }
    if (seen === null && !probe.debugged) {
      probe.debugged = true;
      console.log("probe lost; terminal tail: " + JSON.stringify(await evaluate(`window.__zz.tail(${tabIndex}, 6)`)) + " visible=" + (await evaluate(`window.__zz.termVisible(${tabIndex})`)) + " active=" + (await evaluate(`document.activeElement && document.activeElement.className`)));
    }
    probes.push({ phase, token: tok, send_ms: +(sent - t0).toFixed(1), echo_ms: seen === null ? -1 : +(seen - sent).toFixed(1) });
    await pressCtrl("u");
    await sleep(30);
  }
}

await connect();
// let the first load finish before reloading with the hook (reloading mid-navigation leaves about:blank)
await waitFor(`document.querySelector('[data-section=vault]')`, 30000, "first load");
await sleep(1500);
await send("Page.enable");
await send("Page.addScriptToEvaluateOnNewDocument", { source: HOOK });
await send("Page.reload", { ignoreCache: true });
await sleep(2500);
await waitFor(`document.querySelector('[data-section=vault]')`, 30000, "vault page").catch(async (error) => {
  console.log("vault page missing; body text starts: " + JSON.stringify(await evaluate("location.href + ' | ' + document.body.innerText.slice(0, 200)")));
  throw error;
});
await evaluate(HELPERS);
console.log("hook ready, terms", await evaluate(`globalThis.__zzTerms ? globalThis.__zzTerms.length : -1`));

for (let i = 1; i <= 5; i += 1) await openHostTab(i);
mark("tabs_open");
await selectTab(0);
await runLine("PS1='ZP> '");
await waitFor(`window.__zz.lastLine(0).trim() === 'ZP>'`, 10000, "prompt 0");
await selectTab(1);
await runLine("PS1='ZP> '");
await waitFor(`window.__zz.lastLine(1).trim() === 'ZP>'`, 10000, "prompt 1");

// idle phase: detach CDP so the debugging session does not add its own load, then reattach
ws.close();
await sleep(12000);
mark("idle_start");
await sleep(IDLE_SECONDS * 1000);
mark("idle_end");
await connect();
await evaluate(HELPERS);

await selectTab(1);
await probe("idle", "ZQa", 1, IDLE_PROBES);

// flood in tab 0, tab 1 stays in front (background-flood case), probes in tab 1 meanwhile
await selectTab(0);
mark("flood_start");
await runLine(`seq 1 ${LINES} && echo ZDONE$((1+1))`);
await selectTab(1);
let doneAt = null;
const watcher = (async () => {
  for (;;) {
    const last = await evaluate(`window.__zz.tail(0, 4)`).catch(() => "");
    if (last && last.includes("ZDONE2")) {
      doneAt = Date.now();
      return;
    }
    await sleep(25);
  }
})();
await probe("flood", "ZQb", 1, FLOOD_PROBES);
await Promise.race([watcher, sleep(120000)]);
if (doneAt) marks.push({ name: "flood_done_observed", epoch_ms: doneAt });
else console.log("flood did not finish within the wait");
mark("post_start");
ws.close();
await sleep(POST_SECONDS * 1000);
mark("post_end");
await connect();
await evaluate(HELPERS);

// Ctrl+C in the foreground flood
await selectTab(0);
await runLine(`seq 1 ${LINES}`);
await sleep(3000);
await pressCtrl("c");
const c1 = Date.now();
let back = null;
const endC = Date.now() + 40000;
while (Date.now() < endC) {
  const last = await evaluate(`window.__zz.lastLine(0)`).catch(() => "");
  if (last && last.trim() === "ZP>") {
    back = Date.now();
    break;
  }
}
marks.push({ name: "ctrlc_sent", epoch_ms: c1 });
if (back) marks.push({ name: "ctrlc_prompt_back", epoch_ms: back });
console.log(`ctrl-c -> prompt: ${back ? back - c1 : "never"} ms`);
mark("end_start");
await sleep(8000);
mark("end_end");

writeFileSync(join(OUT, "marks.csv"), "name,epoch_ms\n" + marks.map((m) => `${m.name},${m.epoch_ms}`).join("\n") + "\n");
writeFileSync(join(OUT, "probes.csv"), "phase,token,send_ms,echo_ms\n" + probes.map((p) => `${p.phase},${p.token},${p.send_ms},${p.echo_ms}`).join("\n") + "\n");
ws.close();
console.log("scenario finished");
process.exit(0);
