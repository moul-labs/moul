// The packaged Studio in an app, in real browsers. A small app (studio-app/) imports
// '@moul-dev/studio' and '@moul-dev/client' by name and is bundled with Vite, so it gets Studio's
// dist build through the package's exports, with Lit, Moul UI, TanStack and the SDK from the app's
// own dependencies, as an app that installs Studio from npm does. The scenario: Studio on the
// app's client and a fresh database, the Overview, a collection made through the SDK showing up in
// Studio, and the database file from the header's download button.
//
//   node studio.mjs [--browsers chromium,firefox,webkit]
//
// Needs packages/client and packages/studio built (scripts/build-wasm.sh, then `npm run build`),
// and the workspace's dependencies installed (`npm ci` at the root), which hold Vite.

import { existsSync, mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { chromium, firefox, webkit } from 'playwright';
import { build } from 'vite';
import { serve } from './e2e.mjs';

const ENGINES = { chromium, firefox, webkit };
const HERE = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(HERE, '..');
const APP = path.join(HERE, 'studio-app');

const check = (ok, message) => {
  if (!ok) throw new Error(message);
};

/** Bundles the app into `out`, and checks it took the packages' builds, not their sources. */
async function bundle(out) {
  const result = await build({
    root: APP,
    configFile: false,
    // Rolldown warns of Moul UI's "use client" and of the chunk size; errors still fail.
    logLevel: 'error',
    build: {
      outDir: out,
      emptyOutDir: true,
      target: ['chrome123', 'edge123', 'firefox120', 'safari17.5', 'ios17.5'],
      cssTarget: ['chrome123', 'edge123', 'firefox120', 'safari17.5', 'ios17.5'],
    },
    worker: { format: 'es' },
  });
  const modules = (Array.isArray(result) ? result : [result])
    .flatMap((r) => r.output)
    .filter((o) => o.type === 'chunk')
    .flatMap((c) => Object.keys(c.modules))
    .map((id) => path.relative(ROOT, id.replace(/^\0/, '').split('?')[0]).split(path.sep).join('/'));
  for (const want of ['packages/studio/dist/moul-studio.js', 'packages/client/dist/index.js']) {
    check(modules.includes(want), `the bundle does not hold ${want}`);
  }
  const sources = modules.filter((m) => m.startsWith('packages/studio/src/') || m.startsWith('packages/client/src/'));
  check(sources.length === 0, `the bundle holds package sources: ${sources.slice(0, 3).join(', ')}`);
  // Studio and the app share one Lit.
  const lits = new Set(modules.filter((m) => m.endsWith('/lit-element.js')));
  check(lits.size === 1, `one copy of Lit, not ${[...lits].join(', ')}`);
}

let step = '';
async function scenario(context, base, problems) {
  const db = `studio${Date.now().toString(36)}`;
  const page = await context.newPage();
  page.on('pageerror', (e) => problems.push(e.message));
  page.on('console', (m) => {
    if (m.type() === 'error') problems.push(m.text());
  });
  step = 'the app loads';
  await page.goto(`${base}/?db=${db}`);
  await page.waitForFunction(() => window.ready === true);

  step = 'the Overview';
  const overview = page.locator('admin-overview-page');
  await overview.getByText('This database and its collections at a glance.').waitFor({ timeout: 20_000 });
  await overview.getByText('No collections yet').waitFor({ timeout: 10_000 });
  const shared = await page.evaluate(() => document.querySelector('moul-studio').client === window.moul);
  check(shared, "Studio works through the app's client");

  step = 'a collection made through the SDK';
  await page.evaluate(async () => {
    await window.moul.admin.createCollection({ name: 'books', fields: [{ name: 'title', type: 'text' }] });
    await window.moul.collection('books').create({ title: 'Solaris' });
  });
  const card = overview.locator('admin-collection-card').filter({ hasText: 'books' });
  await card.waitFor({ timeout: 10_000 });
  await card.getByText('1 records').waitFor({ timeout: 10_000 });

  step = 'the download button';
  const [download] = await Promise.all([
    page.waitForEvent('download', { timeout: 15_000 }),
    page.locator('admin-header moul-button[aria-label="Download the database file"]').click(),
  ]);
  check(download.suggestedFilename() === `${db}.db`, `the file is named ${download.suggestedFilename()}`);
  const file = await download.path();
  const head = readFileSync(file).subarray(0, 15).toString('latin1');
  check(head === 'SQLite format 3', `the download is a SQLite file: ${JSON.stringify(head)}`);
}

async function main() {
  const args = process.argv.slice(2);
  const i = args.indexOf('--browsers');
  const names = i >= 0 ? args[i + 1].split(',') : Object.keys(ENGINES);
  check(existsSync(path.join(ROOT, 'packages/studio/dist/moul-studio.js')), 'build packages/studio first');
  check(existsSync(path.join(ROOT, 'packages/client/dist/index.js')), 'build packages/client first');
  check(existsSync(path.join(ROOT, 'packages/client/wasm/moul_web_bg.wasm')), 'run scripts/build-wasm.sh first');

  const out = mkdtempSync(path.join(tmpdir(), 'moul-studio-app-'));
  const failed = [];
  try {
    await bundle(out);
    const server = await serve(out);
    const base = `http://127.0.0.1:${server.address().port}`;
    for (const name of names) {
      // A persistent profile: WebKit's ephemeral contexts refuse OPFS sync access handles.
      const profile = mkdtempSync(path.join(tmpdir(), `moul-studio-${name}-`));
      const context = await ENGINES[name].launchPersistentContext(profile, { acceptDownloads: true });
      const problems = [];
      const started = Date.now();
      try {
        await scenario(context, base, problems);
        check(problems.length === 0, `console errors: ${problems.join(' | ')}`);
        console.log(`ok   ${name} (${Date.now() - started} ms)`);
      } catch (e) {
        failed.push(name);
        const console_ = problems.length ? ` (console: ${problems.join(' | ')})` : '';
        console.log(`FAIL ${name} at "${step}": ${e.message.split('\n')[0]}${console_}`);
      } finally {
        await context.close();
        rmSync(profile, { recursive: true, force: true });
      }
    }
    server.close();
  } finally {
    rmSync(out, { recursive: true, force: true });
  }
  if (failed.length) {
    console.log(`${failed.length} failed: ${failed.join(', ')}`);
    process.exit(1);
  }
  console.log(`packaged Studio e2e passed in ${names.join(', ')}`);
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
