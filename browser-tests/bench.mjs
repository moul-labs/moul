// Time budgets of the SDK in real browsers: from createClient to the first answered request (the
// worker starting, the wasm compiling, the database opening on OPFS), both the first time in a
// fresh browser profile (nothing cached) and the median of several; and the mean time of one
// record created through the SDK. Chromium runs with its CPU slowed four times, as a slow phone;
// Firefox and WebKit run at full speed.
//
//   node bench.mjs [--browsers chromium,firefox,webkit] [--throttle 4]
//
// Chromium's throttling (Emulation.setCPUThrottlingRate) slows the page's main thread only: it
// refuses workers ("Operation is only supported for pages, not workers"), so the engine's worker,
// and the wasm compile, run at full speed there too.
//
// The budgets are about five times what an Apple M-series laptop measured (noted beside each), and
// at least 10 ms for a create.
//
// Needs packages/client built (scripts/build-wasm.sh, then `npm run build -w @moul-dev/client`).

import { existsSync, mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { chromium, firefox, webkit } from 'playwright';
import { serve } from './e2e.mjs';

const ENGINES = { chromium, firefox, webkit };
const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
/** Fresh databases opened, each in a new tab: the first is cold, the median of all counts too. */
const OPENS = 5;
/** Records created one after another; their mean counts. */
const CREATES = 200;

/** Milliseconds, per browser. */
const BUDGETS = {
  chromium: { first: 750, open: 500, create: 25 }, // measured: first 138 ms, open 91 ms, create 4.9 ms (unthrottled open 80, create 2.4)
  firefox: { first: 500, open: 500, create: 10 }, // measured: first 75 ms, open 72 ms, create 1.5 ms
  webkit: { first: 500, open: 500, create: 10 }, // measured: first 79 ms, open 53 ms, create 0.9 ms
};

const median = (xs) => [...xs].sort((a, b) => a - b)[Math.floor(xs.length / 2)];

async function measure(name, base, rate) {
  const profile = mkdtempSync(path.join(tmpdir(), `moul-bench-${name}-`));
  // A persistent profile: WebKit's ephemeral contexts refuse OPFS sync access handles.
  const context = await ENGINES[name].launchPersistentContext(profile);
  const problems = [];
  try {
    const opens = [];
    let durable = true;
    let create;
    for (let i = 0; i < OPENS; i++) {
      const page = await context.newPage();
      page.on('pageerror', (e) => problems.push(e.message));
      if (name === 'chromium' && rate > 1) {
        const cdp = await context.newCDPSession(page);
        await cdp.send('Emulation.setCPUThrottlingRate', { rate });
      }
      await page.goto(`${base}/browser-tests/e2e/bench.html`);
      await page.waitForFunction(() => window.ready === true);
      const db = `bench${i}${Date.now().toString(36)}`;
      const opened = await page.evaluate((db) => window.bench.open(db), db);
      opens.push(opened.ms);
      durable &&= opened.durable;
      if (i === OPENS - 1) create = await page.evaluate((n) => window.bench.creates(n), CREATES);
      await page.close();
    }
    if (problems.length) throw new Error(`page errors: ${problems.join(' | ')}`);
    if (!durable) throw new Error('the database was not on OPFS');
    return { first: opens[0], open: median(opens), create };
  } finally {
    await context.close();
    rmSync(profile, { recursive: true, force: true });
  }
}

async function main() {
  const args = process.argv.slice(2);
  const value = (flag) => {
    const i = args.indexOf(flag);
    return i >= 0 ? args[i + 1] : undefined;
  };
  const names = value('--browsers')?.split(',') ?? Object.keys(ENGINES);
  const rate = Number(value('--throttle') ?? 4);
  if (!existsSync(path.join(ROOT, 'packages/client/dist/index.js'))) throw new Error('build packages/client first');
  if (!existsSync(path.join(ROOT, 'packages/client/wasm/moul_web_bg.wasm'))) throw new Error('run scripts/build-wasm.sh first');
  const server = await serve();
  const base = `http://127.0.0.1:${server.address().port}`;
  const rows = [];
  const failed = [];
  for (const name of names) {
    const label = name === 'chromium' && rate > 1 ? `chromium, CPU / ${rate}` : name;
    try {
      const got = await measure(name, base, rate);
      const budget = BUDGETS[name];
      const over = [
        got.first > budget.first && 'first open',
        got.open > budget.open && 'open',
        got.create > budget.create && 'create',
      ].filter(Boolean);
      if (over.length) failed.push(`${label} (${over.join(', ')})`);
      rows.push({ label, ...got, budget, over });
    } catch (e) {
      failed.push(label);
      rows.push({ label, error: e.message.split('\n')[0] });
    }
  }
  server.close();

  const ms = (n) => `${n.toFixed(1)} ms`;
  const table = [
    ['', 'first open (cold)', `createClient to first answer (median of ${OPENS})`, `create a record (mean of ${CREATES})`],
    ...rows.map((r) =>
      r.error
        ? [r.label, `FAIL: ${r.error}`, '', '']
        : [
            r.label,
            `${ms(r.first)} / ${r.budget.first} ms${r.over.includes('first open') ? '  OVER' : ''}`,
            `${ms(r.open)} / ${r.budget.open} ms${r.over.includes('open') ? '  OVER' : ''}`,
            `${ms(r.create)} / ${r.budget.create} ms${r.over.includes('create') ? '  OVER' : ''}`,
          ],
    ),
  ];
  const widths = table[0].map((_, c) => Math.max(...table.map((row) => row[c].length)));
  for (const row of table) console.log(row.map((cell, c) => cell.padEnd(widths[c])).join('   ').trimEnd());
  console.log('(measured / budget)');
  if (failed.length) {
    console.log(`over budget or failed: ${failed.join(', ')}`);
    process.exit(1);
  }
  console.log(`bench passed in ${names.join(', ')}`);
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
