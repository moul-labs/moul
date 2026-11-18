// End-to-end checks of the SDK in real browsers: the engine in a worker, several tabs on one
// database, live queries across tabs, and a new leader when the leading tab closes.
//
//   node e2e.mjs [--browsers chromium,firefox,webkit]
//
// Needs packages/client built (scripts/build-wasm.sh, then `npm run build -w @moul-dev/client`).

import { createReadStream, existsSync, mkdtempSync, rmSync, statSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { createServer } from 'node:http';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { chromium, firefox, webkit } from 'playwright';

const ENGINES = { chromium, firefox, webkit };
const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const TYPES = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.mjs': 'text/javascript; charset=utf-8',
  '.wasm': 'application/wasm',
  '.json': 'application/json',
  '.css': 'text/css',
  '.svg': 'image/svg+xml',
};

/**
 * A static server over the repository, so pages load the built packages as they ship. With
 * `headers`, every response carries them (a site's `_headers`); with `fallback`, unknown paths get
 * that file, as a single-page app's host does.
 */
export function serve(root = ROOT, { headers = {}, fallback } = {}) {
  const server = createServer((req, res) => {
    const url = new URL(req.url, 'http://localhost');
    let file = path.join(root, decodeURIComponent(url.pathname));
    if (!file.startsWith(root)) {
      res.writeHead(403).end();
      return;
    }
    if (existsSync(file) && statSync(file).isDirectory()) file = path.join(file, 'index.html');
    if (!existsSync(file) && fallback) file = path.join(root, fallback);
    if (!existsSync(file)) {
      res.writeHead(404).end('not found');
      return;
    }
    res.writeHead(200, {
      'Content-Type': TYPES[path.extname(file)] ?? 'application/octet-stream',
      'Cache-Control': 'no-store',
      ...headers,
    });
    createReadStream(file).pipe(res);
  });
  return new Promise((resolve) => server.listen(0, '127.0.0.1', () => resolve(server)));
}

function check(ok, message) {
  if (!ok) throw new Error(message);
}

let step = '';
const at = (name) => {
  step = name;
  if (process.env.MOUL_E2E_STEPS) console.log(`    step: ${name}`);
};

async function scenario(context, base) {
  const db = `e2e${Date.now().toString(36)}`;
  const open = async () => {
    const page = await context.newPage();
    page.on('pageerror', (e) => console.log(`    page error: ${e.message}`));
    if (process.env.MOUL_BROWSER_TEST_CONSOLE) page.on('console', (m) => console.log(`    console: ${m.text()}`));
    await page.goto(`${base}/browser-tests/e2e/client.html?db=${db}`);
    await page.waitForFunction(() => window.ready === true);
    return page;
  };
  const a = await open();
  const b = await open();

  at('define and live query across tabs');
  // Tab A defines a collection and writes; tab B's live query sees it.
  await a.evaluate(async () => {
    await window.moul.admin.createCollection({ name: 'notes', fields: [{ name: 'text', type: 'text' }] });
  });
  await b.evaluate(() => {
    window.seen = [];
    window.moul
      .collection('notes')
      .live({ sort: 'text' })
      .subscribe((s) => window.seen.push(s.data ? s.data.items.map((n) => n.text).join(',') : null));
  });
  await a.evaluate(() => window.moul.collection('notes').create({ text: 'from a' }));
  await b.waitForFunction(() => window.seen.includes('from a'), null, { timeout: 10_000 });

  at('subscription across tabs');
  // Tab B writes; tab A's subscription hears it.
  await a.evaluate(async () => {
    window.events = [];
    await window.moul.collection('notes').subscribe((e) => window.events.push(`${e.action}:${e.record.text}`));
  });
  await b.evaluate(() => window.moul.collection('notes').create({ text: 'from b' }));
  await a.waitForFunction(() => window.events.includes('create:from b'), null, { timeout: 10_000 });
  await b.waitForFunction(() => window.seen.includes('from a,from b'), null, { timeout: 10_000 });

  const status = await b.evaluate(() => window.moul.database.status());
  check(status.durable === true, `the database is on OPFS: ${JSON.stringify(status)}`);

  at('leader closes');
  // Close the tab that opened the database first; the other takes over and keeps its data.
  await a.close();
  const after = await b.evaluate(async () => {
    const r = await window.moul.collection('notes').list({ sort: 'text' });
    return r.items.map((n) => n.text).join(',');
  });
  check(after === 'from a,from b', `the next tab reopens the database: got ${after}`);
  await b.evaluate(() => window.moul.collection('notes').create({ text: 'from b again' }));
  await b.waitForFunction(() => window.seen.includes('from a,from b,from b again'), null, { timeout: 10_000 });

  at('export');
  // The database file comes out as SQLite.
  const head = await b.evaluate(async () => {
    const bytes = await window.moul.database.export();
    return new TextDecoder().decode(bytes.slice(0, 15));
  });
  check(head === 'SQLite format 3', `export gives a SQLite file: ${head}`);
}

/** WebKit's ephemeral contexts behave like private windows: the engine runs in memory and says so. */
async function privateWindow(base) {
  const browser = await webkit.launch();
  try {
    const page = await (await browser.newContext()).newPage();
    await page.goto(`${base}/browser-tests/e2e/client.html?db=private`);
    await page.waitForFunction(() => window.ready === true);
    const status = await page.evaluate(() => window.moul.database.status());
    check(status.durable === false && /refused/.test(status.note ?? ''), `memory with a note: ${JSON.stringify(status)}`);
    const n = await page.evaluate(async () => {
      await window.moul.admin.createCollection({ name: 'notes', fields: [{ name: 'text', type: 'text' }] });
      await window.moul.collection('notes').create({ text: 'kept in memory' });
      return (await window.moul.collection('notes').list()).totalItems;
    });
    check(n === 1, `the engine works in memory: ${n}`);
  } finally {
    await browser.close();
  }
}

async function main() {
  const args = process.argv.slice(2);
  const i = args.indexOf('--browsers');
  const names = i >= 0 ? args[i + 1].split(',') : Object.keys(ENGINES);
  check(existsSync(path.join(ROOT, 'packages/client/dist/index.js')), 'build packages/client first');
  check(existsSync(path.join(ROOT, 'packages/client/wasm/moul_web_bg.wasm')), 'run scripts/build-wasm.sh first');
  const server = await serve();
  const base = `http://127.0.0.1:${server.address().port}`;
  const failed = [];
  for (const name of names) {
    // A persistent profile: WebKit's ephemeral contexts refuse OPFS sync access handles, as
    // private windows do.
    const profile = mkdtempSync(path.join(tmpdir(), `moul-e2e-${name}-`));
    const context = await ENGINES[name].launchPersistentContext(profile);
    const started = Date.now();
    try {
      await scenario(context, base);
      console.log(`ok   ${name} (${Date.now() - started} ms)`);
    } catch (e) {
      failed.push(name);
      console.log(`FAIL ${name} at "${step}": ${e.message}`);
    } finally {
      await context.close();
      rmSync(profile, { recursive: true, force: true });
    }
  }
  if (names.includes('webkit')) {
    step = 'private window';
    try {
      await privateWindow(base);
      console.log('ok   webkit private window');
    } catch (e) {
      failed.push('webkit private window');
      console.log(`FAIL webkit private window: ${e.message}`);
    }
  }
  server.close();
  if (failed.length) {
    console.log(`${failed.length} failed: ${failed.join(', ')}`);
    process.exit(1);
  }
  console.log(`client e2e passed in ${names.join(', ')}`);
}

if (import.meta.url === `file://${process.argv[1]}`) {
  main().catch((e) => {
    console.error(e);
    process.exit(1);
  });
}
