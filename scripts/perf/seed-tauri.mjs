// V2-G13a: seed one test host into the data dir of the Z-SSH 0.2.14 (Tauri) perf copy.
// Needs node >= 22 (node:sqlite). The server address, user and key path come from ZSSH_TEST_* only and
// end up in the isolated data dir's database, never in any file of the repository.
//
//   ZSSH_DATA_DIR=C:\zssh-v2-data\V2-G13a\zssh node scripts/perf/seed-tauri.mjs [--scrollback 4000]
// Start the app once with the same data dir first so it creates data.db.
import { DatabaseSync } from "node:sqlite";
import { existsSync } from "node:fs";
import { join } from "node:path";

const dir = process.env.ZSSH_DATA_DIR ?? "";
if (!dir.split("\\").join("/").toLowerCase().startsWith("c:/zssh-v2-data/")) {
  console.error("ZSSH_DATA_DIR must be an isolated dir under C:\zssh-v2-data");
  process.exit(2);
}
const dbFile = join(dir, "data.db");
if (!existsSync(dbFile)) {
  console.error("data.db not found: start the app once with this data dir first");
  process.exit(2);
}
const need = (n) => {
  const v = process.env[n];
  if (!v) {
    console.error(`missing ${n}`);
    process.exit(2);
  }
  return v;
};
const host = need("ZSSH_TEST_HOST");
const user = need("ZSSH_TEST_USER");
const key = need("ZSSH_TEST_KEY_PATH");
const port = Number(process.env.ZSSH_TEST_PORT ?? "22");
const socks = process.env.ZSSH_TEST_SOCKS5 ?? "";
const sbIndex = process.argv.indexOf("--scrollback");
const scrollback = sbIndex >= 0 ? Number(process.argv[sbIndex + 1]) : 10000;

const now = Math.floor(Date.now() / 1000);
const db = new DatabaseSync(dbFile);
db.exec("BEGIN");
try {
  db.prepare("DELETE FROM hosts WHERE name = 'perf-host'").run();
  db.prepare("DELETE FROM identities WHERE name = 'perf-host-key'").run();
  db.prepare("DELETE FROM proxies WHERE name = 'test-proxy'").run();
  db.prepare("INSERT INTO identities (name, kind, key_path, secret_blob) VALUES ('perf-host-key', 'key_file', ?, NULL)").run(key);
  let proxyId = null;
  if (socks) {
    const [ph, pp] = socks.split(":");
    db.prepare("INSERT INTO proxies (name, kind, host, port, username, secret_blob) VALUES ('test-proxy', 'socks5', ?, ?, NULL, NULL)").run(ph, Number(pp));
    proxyId = db.prepare("SELECT id FROM proxies WHERE name = 'test-proxy'").get().id;
  }
  db.prepare(
    `INSERT INTO hosts (group_id, name, host, port, username, auth_method, identity_id, proxy_id,
       tags, sort, created_at, updated_at, legacy_algorithms, proxy_mode)
     VALUES (NULL, 'perf-host', ?, ?, ?, 'key', (SELECT id FROM identities WHERE name = 'perf-host-key'), ?,
       '[]', 0, ?, ?, 0, ?)`,
  ).run(host, port, user, proxyId, now, now, proxyId ? "custom" : "default");
  const set = (k, v) => db.prepare("INSERT OR REPLACE INTO settings (key, value) VALUES (?, ?)").run(k, JSON.stringify(v));
  set("update_auto_check", false); // the trial native build has no update traffic either
  set("close_action", "quit");
  set("terminal_font_size", 14); // the native default font size
  set("terminal_scrollback", scrollback);
  db.exec("COMMIT");
} catch (error) {
  db.exec("ROLLBACK");
  throw error;
}
console.log("seeded: 1 host, scrollback", scrollback);
db.close();
