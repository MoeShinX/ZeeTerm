// V2-G13a: turn one scenario run directory (samples.csv, marks.csv, steps.csv / probes.csv) into summary.json.
//
//   node scripts/perf/analyze.mjs --kind zeeterm|tauri --dir <run dir>
//
// samples.csv: epoch_ms,t_s,root_mb,webview_mb,other_mb,total_mb,procs,cpu_s  (process tree, every 500 ms)
// Memory figure = private working set summed over the whole process tree (WebView2 children included).
import { readFileSync, writeFileSync, existsSync } from "node:fs";
import { join } from "node:path";

const arg = (n, d) => {
  const i = process.argv.indexOf(`--${n}`);
  return i >= 0 ? process.argv[i + 1] : d;
};
const kind = arg("kind", "");
const dir = arg("dir", "");
if (!kind || !dir) throw new Error("--kind and --dir are required");

const csv = (file) => {
  const text = readFileSync(file, "utf8").replace(/^﻿/, "").trim().split(/\r?\n/);
  const head = text[0].split(",").map((h) => h.replace(/"/g, ""));
  return text.slice(1).map((l) => {
    const cells = l.match(/("([^"]|"")*"|[^,]*)(,|$)/g).map((c) => c.replace(/,$/, "").replace(/^"|"$/g, "").replace(/""/g, '"'));
    return Object.fromEntries(head.map((h, i) => [h, cells[i]]));
  });
};
const median = (v) => {
  if (!v.length) return null;
  const s = [...v].sort((a, b) => a - b);
  const n = s.length;
  return n % 2 ? s[(n - 1) / 2] : (s[n / 2 - 1] + s[n / 2]) / 2;
};
const r1 = (x) => (x === null || x === undefined ? null : Math.round(x * 10) / 10);
const pct = (v, p) => {
  if (!v.length) return null;
  const s = [...v].sort((a, b) => a - b);
  return s[Math.min(s.length - 1, Math.max(0, Math.ceil(p * s.length) - 1))];
};

const samples = csv(join(dir, "samples.csv")).map((r) => ({
  t: Number(r.epoch_ms), root: Number(r.root_mb), web: Number(r.webview_mb), other: Number(r.other_mb), total: Number(r.total_mb), procs: Number(r.procs), cpu: Number(r.cpu_s),
}));

const marks = {};
const probes = [];
const out = { kind };
if (kind === "tauri") {
  for (const m of csv(join(dir, "marks.csv"))) marks[m.name] = Number(m.epoch_ms);
  for (const p of csv(join(dir, "probes.csv"))) probes.push({ phase: p.phase, token: p.token, ms: Number(p.echo_ms) });
} else {
  // steps.csv lines look like: [12] Type("ZQa01") ok ; phases are Sleep steps (arrival time = end of the sleep)
  const steps = csv(join(dir, "steps.csv")).map((s) => ({ idx: Number(s.idx), t: Number(s.epoch_ms), line: s.line }));
  const find = (re, from = 0) => steps.find((s, i) => i >= from && re.test(s.line));
  const typeOf = new Map();
  for (const s of steps) {
    let m = s.line.match(/Type\("(ZQ[ab]\d\d)"\)/);
    if (m) typeOf.set(m[1], s.t);
    m = s.line.match(/Expect\("(ZQ[ab]\d\d)"\)/);
    if (m && typeOf.has(m[1])) probes.push({ phase: m[1].startsWith("ZQa") ? "idle" : "flood", token: m[1], ms: s.t - typeOf.get(m[1]) });
  }
  const sleeps = steps.filter((s) => /Sleep\(\d+\)/.test(s.line)).map((s) => ({ ...s, ms: Number(s.line.match(/Sleep\((\d+)\)/)[1]) }));
  const settle = sleeps.find((s) => s.ms === 12000);
  const idleSleep = sleeps.find((s) => s !== settle && s.idx > (settle?.idx ?? -1));
  if (settle && idleSleep) {
    marks.idle_start = settle.t;
    marks.idle_end = idleSleep.t;
  }
  const seq = find(/Type\("seq 1 \d+ && echo/);
  if (seq) marks.flood_start = seq.t;
  const done = find(/Expect\("ZDONE2"\)/);
  if (done) {
    marks.flood_done_observed = done.t;
    marks.post_start = done.t;
    const postSleep = sleeps.find((s) => s.idx > done.idx);
    if (postSleep) marks.post_end = postSleep.t;
  }
  const ctrlc = steps.find((s) => /Key\("ctrl-c"\)/.test(s.line));
  if (ctrlc) {
    marks.ctrlc_sent = ctrlc.t;
    const back = steps.find((s) => s.idx > ctrlc.idx && /Expect\("ZP>"\)/.test(s.line));
    if (back) marks.ctrlc_prompt_back = back.t;
  }
  const fails = steps.filter((s) => /FAIL/.test(s.line));
  if (fails.length) out.driver_failures = fails.map((f) => f.line);
}

const between = (a, b) => samples.filter((s) => s.t >= a && s.t <= b);
const stat = (arr, key) => {
  const v = arr.map((s) => s[key]);
  return { n: v.length, median: r1(median(v)), min: r1(Math.min(...v)), max: r1(Math.max(...v)) };
};

if (marks.idle_start && marks.idle_end) {
  const idle = between(marks.idle_start, marks.idle_end);
  out.idle = {
    seconds: r1((marks.idle_end - marks.idle_start) / 1000),
    samples: idle.length,
    total_mb: stat(idle, "total"),
    root_mb: stat(idle, "root"),
    webview_mb: stat(idle, "web"),
    other_mb: stat(idle, "other"),
    procs: stat(idle, "procs"),
  };
}

if (marks.flood_start) {
  const endFlood = marks.flood_done_observed ?? marks.post_start ?? marks.flood_start + 120000;
  const fl = between(marks.flood_start, endFlood);
  out.flood = {
    observed_done_after_s: marks.flood_done_observed ? r1((marks.flood_done_observed - marks.flood_start) / 1000) : null,
    samples: fl.length,
    peak_total_mb: fl.length ? r1(Math.max(...fl.map((s) => s.total))) : null,
    peak_root_mb: fl.length ? r1(Math.max(...fl.map((s) => s.root))) : null,
    peak_webview_mb: fl.length ? r1(Math.max(...fl.map((s) => s.web))) : null,
  };
  // CPU-activity end as an estimate of when the flood stopped being consumed: first sample (2 s after the start)
  // from which the tree's CPU rate stays below 0.04 core for 6 consecutive samples (3 s).
  let settle = null;
  const rate = (a, b) => (b.cpu - a.cpu) / ((b.t - a.t) / 1000);
  for (let i = 1; i < samples.length - 6; i += 1) {
    if (samples[i].t < marks.flood_start + 2000) continue;
    let quiet = true;
    for (let k = 0; k < 6; k += 1) if (rate(samples[i + k], samples[i + k + 1]) >= 0.04) quiet = false;
    if (quiet) {
      settle = samples[i].t;
      break;
    }
  }
  out.flood.cpu_settle_after_s = settle ? r1((settle - marks.flood_start) / 1000) : null;
  out.flood.cpu_settle_epoch_ms = settle;
  const busy = samples.filter((s, i) => i > 0 && s.t >= marks.flood_start && s.t <= (settle ?? endFlood));
  if (busy.length > 1) {
    const first = samples.find((s) => s.t >= marks.flood_start);
    const last = busy[busy.length - 1];
    out.flood.avg_cpu_cores = r1((last.cpu - first.cpu) / ((last.t - first.t) / 1000));
  }
}

if (marks.post_start) {
  const base = out.flood?.cpu_settle_epoch_ms && out.flood.cpu_settle_epoch_ms < marks.post_start ? out.flood.cpu_settle_epoch_ms : marks.post_start;
  const at = (sec) => {
    const target = base + sec * 1000;
    return samples.reduce((best, s) => (Math.abs(s.t - target) < Math.abs(best.t - target) ? s : best), samples[0]);
  };
  const series = {};
  for (const sec of [0, 5, 10, 20, 30, 45, 60]) series[`t+${sec}s`] = r1(at(sec).total);
  const post = between(base, base + 60000);
  out.post_flood = { total_mb_at: series, min_mb: post.length ? r1(Math.min(...post.map((s) => s.total))) : null, root_at_end_mb: r1(at(60).root), webview_at_end_mb: r1(at(60).web) };
}

if (marks.ctrlc_sent && marks.ctrlc_prompt_back) out.ctrl_c_to_prompt_ms = marks.ctrlc_prompt_back - marks.ctrlc_sent;
else if (marks.ctrlc_sent) out.ctrl_c_to_prompt_ms = null;

for (const phase of ["idle", "flood"]) {
  const v = probes.filter((p) => p.phase === phase && p.ms >= 0).map((p) => p.ms);
  const lost = probes.filter((p) => p.phase === phase && p.ms < 0).length;
  out[`echo_${phase}_ms`] = { n: v.length, lost, median: r1(median(v)), p90: r1(pct(v, 0.9)), max: r1(v.length ? Math.max(...v) : null) };
}
out.session_peak_total_mb = r1(Math.max(...samples.map((s) => s.total)));
out.marks_epoch_ms = marks;
writeFileSync(join(dir, "summary.json"), JSON.stringify(out, null, 2));
console.log(JSON.stringify(out, null, 2));
