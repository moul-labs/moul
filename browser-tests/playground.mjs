// The built Playground in real browsers, served with its own _headers (so its content security
// policy is in force): the welcome screen, the sample app, Studio on it, offline reloads, dropped
// files, deleting a database, and a deploy while a tab is open.
//
//   node playground.mjs [--browsers chromium,firefox,webkit]
//
// Needs sites/playground built (`npm run build -w moul-playground`).

import { cpSync, existsSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { chromium, firefox, webkit } from 'playwright';
import { serve } from './e2e.mjs';

const ENGINES = { chromium, firefox, webkit };
const SITE = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../sites/playground/dist');

/** The headers `_headers` gives every path. */
function siteHeaders() {
  const out = {};
  let inAll = false;
  for (const line of readFileSync(path.join(SITE, '_headers'), 'utf8').split('\n')) {
    if (line.startsWith('#') || !line.trim()) continue;
    if (!line.startsWith(' ')) {
      inAll = line.trim() === '/*';
      continue;
    }
    if (inAll) {
      const [k, ...v] = line.trim().split(':');
      out[k.trim()] = v.join(':').trim();
    }
  }
  return out;
}

const check = (ok, message) => {
  if (!ok) throw new Error(message);
};

/** Text inside Studio's shadow roots, which Playwright's text selectors pierce. */
let step = '';
async function seeText(page, text, timeout = 15_000) {
  step = `waiting for "${text}"`;
  try {
    await page.getByText(text, { exact: false }).filter({ visible: true }).first().waitFor({ timeout });
  } catch (e) {
    if (process.env.MOUL_E2E_SHOTS) await page.screenshot({ path: process.env.MOUL_E2E_SHOTS });
    throw e;
  }
}

/** Drops a file on the Playground, as a file dragged from the desktop. */
async function dropFile(page, name, type, text) {
  step = `dropping ${name}`;
  await page.evaluate(
    ([name, type, text]) => {
      const dataTransfer = new DataTransfer();
      dataTransfer.items.add(new File([text], name, { type }));
      const target = document.querySelector('moul-playground');
      for (const kind of ['dragover', 'drop']) {
        const e = new DragEvent(kind, { dataTransfer, bubbles: true, cancelable: true, composed: true });
        if (!e.dataTransfer) Object.defineProperty(e, 'dataTransfer', { value: dataTransfer });
        target.dispatchEvent(e);
      }
    },
    [name, type, text],
  );
}

const BOOKS = 'Title,Pages,Code\nDune,412,0441013597\nSolaris,204,0156027607\n';

async function scenario(context, base, problems) {
  const page = await context.newPage();
  page.on('console', (m) => {
    if (m.type() === 'error' || /Content.Security.Policy|CSP/i.test(m.text())) problems.push(m.text());
  });
  page.on('pageerror', (e) => problems.push(e.message));
  await page.goto(base);
  await seeText(page, 'A backend in your browser tab');
  await page.getByText('Try the sample app').click();
  await page.waitForURL(/db=sample/, { timeout: 20_000 });
  await seeText(page, 'Collections');
  await seeText(page, 'Tasks');
  // The records grid of tasks, through Studio's hash routing.
  await page.goto(`${base}/?db=sample#/records/tasks`);
  await seeText(page, 'Buy oat milk');
  await seeText(page, 'Not backed up');
  // Sign-in codes land in the inbox.
  await page.evaluate(async () => {
    const studio = document.querySelector('moul-playground').shadowRoot.querySelector('moul-studio');
    const { MoulClient } = studio.client.constructor === undefined ? {} : { MoulClient: studio.client.constructor };
    const guest = new MoulClient(studio.client.transport, { authStorage: null });
    await guest.auth('users').requestCode('ana@example.com');
  });
  await page.goto(`${base}/?db=sample#/inbox`);
  await seeText(page, 'ana@example.com');

  // Offline: the service worker serves the page and the engine from its cache.
  step = 'service worker';
  await page.evaluate(() => navigator.serviceWorker.ready);
  await page.reload();
  await seeText(page, 'ana@example.com');
  await context.setOffline(true);
  try {
    await page.goto(`${base}/?db=sample#/records/tasks`);
    await seeText(page, 'Call the plumber');
  } finally {
    await context.setOffline(false);
  }

  // A spreadsheet dropped on Studio becomes a collection of the open database.
  await dropFile(page, 'Books.csv', 'text/csv', BOOKS);
  await page.waitForURL(/db=sample#\/records\/books\b/, { timeout: 20_000 });
  await seeText(page, 'Solaris');
  // Leading zeros keep a column as text.
  await seeText(page, '0441013597');

  // Dropped on the welcome screen, it starts a database.
  step = 'the welcome screen';
  await page.evaluate(() => localStorage.removeItem('moul.playground.last'));
  await page.goto(base);
  await seeText(page, 'A backend in your browser tab');
  await dropFile(page, 'notes.txt', 'text/plain', 'hello');
  await seeText(page, 'Drop a .csv spreadsheet or a .db database file');
  await dropFile(page, 'Books.csv', 'text/csv', BOOKS);
  await page.waitForURL(/db=sheets\b/, { timeout: 20_000 });
  await seeText(page, 'Books');

  // Deleting a database forgets what the browser kept beside it, so a new one by its name starts clean.
  step = 'deleting a database';
  await page.evaluate(() => {
    localStorage.setItem('moul.studio.sheets.exported', '1');
    localStorage.setItem('moul.auth.sheets', '{}');
  });
  page.once('dialog', (d) => void d.accept());
  await page.locator('moul-playground header nav a.link', { hasText: 'Delete' }).click();
  await page.waitForURL(`${base}/`, { timeout: 20_000 });
  await seeText(page, 'A backend in your browser tab');
  const left = await page.evaluate(() => [
    localStorage.getItem('moul.studio.sheets.exported'),
    localStorage.getItem('moul.auth.sheets'),
    localStorage.getItem('moul.playground.last'),
  ]);
  check(left.every((v) => v === null), `deleting leaves keys behind: ${JSON.stringify(left)}`);
}

/**
 * A deploy while a tab is open. The new service worker waits until no tab uses the old version, so
 * the open tab still loads code it had not needed yet, from the old version's cache, after the
 * server has dropped it. Code gone from both loads the page again, once.
 */
async function deployScenario(engine, problems) {
  const site = mkdtempSync(path.join(tmpdir(), 'moul-play-site-'));
  cpSync(SITE, site, { recursive: true });
  const server = await serve(site, { headers: siteHeaders(), fallback: 'index.html' });
  const base = `http://127.0.0.1:${server.address().port}`;
  const profile = mkdtempSync(path.join(tmpdir(), 'moul-play-deploy-'));
  const context = await engine.launchPersistentContext(profile);
  try {
    const page = await context.newPage();
    page.on('pageerror', (e) => problems.push(e.message));
    await page.goto(`${base}/?db=deploy`);
    await seeText(page, 'Collections');
    step = 'service worker';
    await page.evaluate(() => navigator.serviceWorker.ready);

    step = 'deploy';
    const assets = path.join(site, 'assets');
    const chunk = (page) => readdirSync(assets).find((f) => f.startsWith(`${page}-`) && f.endsWith('.js'));
    const realtime = chunk('realtime');
    const schema = chunk('schema');
    rmSync(path.join(assets, realtime));
    const sw = readFileSync(path.join(site, 'sw.js'), 'utf8');
    const old = sw.match(/moul-playground-[0-9a-f]+/)[0];
    writeFileSync(
      path.join(site, 'sw.js'),
      sw.replace(old, 'moul-playground-next').replace(new RegExp(`"/assets/${realtime.replace(/[.$]/g, '\\$&')}",?`), ''),
    );
    const update = await page.evaluate(async () => {
      const reg = await navigator.serviceWorker.getRegistration();
      await reg.update();
      for (let i = 0; i < 200 && reg.installing; i++) await new Promise((r) => setTimeout(r, 50));
      return { waiting: reg.waiting !== null, caches: await caches.keys() };
    });
    check(update.waiting, `the new worker waits for the open tab: ${JSON.stringify(update)}`);
    check(update.caches.includes(old) && update.caches.includes('moul-playground-next'), `both versions are cached: ${JSON.stringify(update)}`);

    step = 'a page the open tab had not loaded';
    await page.evaluate(() => (window.location.hash = '#/realtime'));
    await page.waitForFunction(() => customElements.get('admin-realtime-page') !== undefined, null, { timeout: 15_000 });
    check(problems.length === 0, `page errors: ${problems.join(' | ')}`);

    step = 'code gone from the server and the cache';
    rmSync(path.join(assets, schema));
    await page.evaluate(async (file) => {
      for (const name of await caches.keys()) await (await caches.open(name)).delete(`/assets/${file}`);
      window.beforeReload = true;
      window.location.hash = '#/schema';
    }, schema);
    await page.waitForFunction(() => window.beforeReload === undefined, null, { timeout: 15_000 });
    // Once: the page that loaded again fails the same way, and stays.
    await page.evaluate(() => (window.afterReload = true));
    await page.waitForTimeout(3_000);
    check(await page.evaluate(() => window.afterReload === true), 'the page loads again only once');
  } finally {
    await context.close();
    server.close();
    rmSync(profile, { recursive: true, force: true });
    rmSync(site, { recursive: true, force: true });
  }
}

/** The settings of the bucket the sync scenario uses, from MOUL_S3_* (as the sync suite reads them). */
function bucket(name) {
  const endpoint = process.env.MOUL_S3_ENDPOINT;
  if (!endpoint) return null;
  return {
    endpoint,
    bucket: process.env.MOUL_S3_BUCKET ?? 'moul-test',
    region: process.env.MOUL_S3_REGION ?? '',
    prefix: `e2e/${name}-${Date.now().toString(36)}`,
    accessKeyId: process.env.MOUL_S3_ACCESS_KEY_ID ?? 'moul-test',
    secretAccessKey: process.env.MOUL_S3_SECRET_ACCESS_KEY ?? 'moul-test-secret',
    pathStyle: process.env.MOUL_S3_PATH_STYLE !== 'false',
  };
}

/** A join link, as the SDK's joinLink writes it. */
function joinLink(settings, page) {
  return `${page}#moul-join=${Buffer.from(JSON.stringify(settings)).toString('base64url')}`;
}

/**
 * Two devices: the first syncs the sample app to a bucket; the second opens a join link, connects
 * from the Sync screen, gets the data, and follows a change the first makes.
 */
async function syncScenario(engine, base, settings, problems) {
  const profiles = [0, 1].map((i) => mkdtempSync(path.join(tmpdir(), `moul-sync-${i}-`)));
  const [a, b] = await Promise.all(profiles.map((p) => engine.launchPersistentContext(p)));
  try {
    const laptop = await a.newPage();
    laptop.on('pageerror', (e) => problems.push(`a: ${e.message}`));
    await laptop.goto(base);
    await laptop.getByText('Try the sample app').click();
    await laptop.waitForURL(/db=sample/, { timeout: 20_000 });
    await seeText(laptop, 'Tasks');
    step = 'the first device connects';
    const status = await laptop.evaluate(async (s) => {
      const studio = document.querySelector('moul-playground').shadowRoot.querySelector('moul-studio');
      await studio.client.sync.configure({ ...s, label: 'Laptop' });
      return studio.client.sync.now();
    }, settings);
    check(status.role === 'writer' && !status.pending, `the first device writes: ${JSON.stringify(status)}`);

    step = 'the second device joins';
    const phone = await b.newPage();
    phone.on('pageerror', (e) => problems.push(`b: ${e.message}`));
    // A database of the phone's own, which a join link naming it must leave alone.
    await phone.goto(`${base}/?db=mine`);
    await seeText(phone, 'Collections');
    await phone.goto('about:blank');
    await phone.goto(joinLink(settings, `${base}/?db=mine`));
    await phone.waitForURL(/db=synced/, { timeout: 20_000 });
    await seeText(phone, 'Connect a bucket');
    check(!phone.url().includes('moul-join'), `the key leaves the address bar: ${phone.url()}`);
    await phone.getByText('Turn on sync').filter({ visible: true }).last().click();
    // Settings from a link connect only once the person agrees, and then only to follow.
    await seeText(phone, 'Follow this bucket?');
    await phone.getByText('Follow the bucket', { exact: true }).filter({ visible: true }).last().click();
    await seeText(phone, 'Following another device', 30_000);
    await phone.goto(`${base}/?db=synced#/records/tasks`);
    await seeText(phone, 'Buy oat milk', 30_000);
    await seeText(phone, 'Read-only');

    step = 'a change follows';
    await laptop.evaluate(async () => {
      const studio = document.querySelector('moul-playground').shadowRoot.querySelector('moul-studio');
      const [project] = (await studio.client.collection('projects').list()).items;
      await studio.client.collection('tasks').create({ title: 'Synced from the laptop', owner: project.owner, project: project.id });
      await studio.client.sync.now();
    });
    await seeText(phone, 'Synced from the laptop', 30_000);

    step = 'a join link opened in an open tab';
    const tab = await b.newPage();
    tab.on('pageerror', (e) => problems.push(`b: ${e.message}`));
    await tab.goto(`${base}/?db=mine`);
    await seeText(tab, 'Collections');
    await tab.evaluate((hash) => (window.location.hash = hash), new URL(joinLink(settings, `${base}/`)).hash);
    await tab.waitForURL(/db=synced-2/, { timeout: 20_000 });
    await seeText(tab, 'Connect a bucket');
    check(!tab.url().includes('moul-join'), `the key leaves the address bar: ${tab.url()}`);
  } finally {
    await Promise.all([a.close(), b.close()]);
    for (const p of profiles) rmSync(p, { recursive: true, force: true });
  }
}

async function main() {
  check(existsSync(path.join(SITE, 'index.html')), 'build sites/playground first');
  const args = process.argv.slice(2);
  const i = args.indexOf('--browsers');
  const names = i >= 0 ? args[i + 1].split(',') : Object.keys(ENGINES);
  const server = await serve(SITE, { headers: siteHeaders(), fallback: 'index.html' });
  const base = `http://127.0.0.1:${server.address().port}`;
  const failed = [];
  for (const name of names) {
    const profile = mkdtempSync(path.join(tmpdir(), `moul-play-${name}-`));
    const context = await ENGINES[name].launchPersistentContext(profile);
    const problems = [];
    const started = Date.now();
    try {
      await scenario(context, base, problems);
      check(problems.length === 0, `console errors: ${problems.join(' | ')}`);
      console.log(`ok   ${name} (${Date.now() - started} ms)`);
    } catch (e) {
      failed.push(name);
      console.log(`FAIL ${name} ${step}: ${e.message.split('\n')[0]}${problems.length ? ` (console: ${problems.join(' | ')})` : ''}`);
    } finally {
      await context.close();
      rmSync(profile, { recursive: true, force: true });
    }
  }
  for (const name of names) {
    const problems = [];
    const started = Date.now();
    try {
      await deployScenario(ENGINES[name], problems);
      console.log(`ok   ${name} deploy with a tab open (${Date.now() - started} ms)`);
    } catch (e) {
      failed.push(`${name} deploy`);
      console.log(`FAIL ${name} deploy, ${step}: ${e.message.split('\n')[0]}`);
    }
  }
  for (const name of names) {
    const settings = bucket(name);
    if (!settings) {
      console.log('skip sync between two devices: set MOUL_S3_ENDPOINT (and MOUL_S3_BUCKET, keys) to run it');
      break;
    }
    const problems = [];
    const started = Date.now();
    try {
      await syncScenario(ENGINES[name], base, settings, problems);
      check(problems.length === 0, `page errors: ${problems.join(' | ')}`);
      console.log(`ok   ${name} sync between two devices (${Date.now() - started} ms)`);
    } catch (e) {
      failed.push(`${name} sync`);
      console.log(`FAIL ${name} sync, ${step}: ${e.message.split('\n')[0]}`);
    }
  }
  server.close();
  if (failed.length) {
    console.log(`${failed.length} failed: ${failed.join(', ')}`);
    process.exit(1);
  }
  console.log(`playground e2e passed in ${names.join(', ')}`);
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
