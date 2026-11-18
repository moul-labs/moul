// Runs the workspace's wasm-bindgen tests in real browser engines.
//
// wasm-bindgen-test-runner drives browsers through WebDriver, which covers
// Chromium and Firefox but not WebKit on Linux. Instead this script starts the
// runner in interactive mode, where it only serves the test page, and opens that
// page in each engine with Playwright.
//
//   node run.mjs [--browsers chromium,firefox,webkit] [-- extra cargo test args]
//
// Needs `wasm-bindgen-test-runner` on PATH at the version of the `wasm-bindgen`
// crate in Cargo.lock, and the Playwright browsers (`npx playwright install`).

import { spawn, spawnSync } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { chromium, firefox, webkit } from "playwright";

const ENGINES = { chromium, firefox, webkit };
const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const TIMEOUT_MS = Number(process.env.MOUL_BROWSER_TEST_TIMEOUT_MS ?? 120_000);

function parseArgs(argv) {
  const opts = { browsers: Object.keys(ENGINES), cargo: [] };
  for (let i = 0; i < argv.length; i++) {
    if (argv[i] === "--browsers") opts.browsers = argv[++i].split(",");
    else if (argv[i] === "--") {
      opts.cargo = argv.slice(i + 1);
      break;
    } else throw new Error(`unknown argument ${argv[i]}`);
  }
  for (const b of opts.browsers) {
    if (!ENGINES[b]) throw new Error(`unknown browser ${b}`);
  }
  return opts;
}

// Build the browser tests and return the .wasm file of each test target.
function buildTests(extra) {
  const args = [
    "test",
    "--target",
    "wasm32-unknown-unknown",
    "--no-run",
    "--message-format=json",
    ...(extra.length ? extra : ["--workspace", "--tests"]),
  ];
  const res = spawnSync("cargo", args, {
    cwd: ROOT,
    encoding: "utf8",
    stdio: ["ignore", "pipe", "inherit"],
    maxBuffer: 256 << 20,
  });
  if (res.status !== 0) throw new Error(`cargo ${args.join(" ")} failed`);
  const wasm = [];
  for (const line of res.stdout.split("\n")) {
    if (!line.startsWith("{")) continue;
    const msg = JSON.parse(line);
    if (msg.reason === "compiler-artifact" && msg.profile?.test && msg.executable) {
      wasm.push({ name: `${msg.target.name} (${msg.package_id.split(/[#@ ]/).at(-1)})`, file: msg.executable });
    }
  }
  return wasm;
}

function freePort() {
  return new Promise((resolve, reject) => {
    const srv = createServer();
    srv.once("error", reject);
    srv.listen(0, "127.0.0.1", () => {
      const { port } = srv.address();
      srv.close(() => resolve(port));
    });
  });
}

// Start the runner in interactive mode and resolve with the page URL.
async function serve(file) {
  const port = await freePort();
  const child = spawn("wasm-bindgen-test-runner", [file], {
    cwd: ROOT,
    env: { ...process.env, NO_HEADLESS: "1", WASM_BINDGEN_TEST_ADDRESS: `127.0.0.1:${port}` },
    stdio: ["ignore", "pipe", "pipe"],
  });
  let log = "";
  const url = await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error(`runner did not start:\n${log}`)), 30_000);
    const onData = (d) => {
      log += d;
      const m = log.match(/available at (http:\/\/\S+)/);
      if (m) {
        clearTimeout(timer);
        resolve(m[1]);
      }
    };
    child.stdout.on("data", onData);
    child.stderr.on("data", onData);
    child.once("exit", (code) => {
      clearTimeout(timer);
      // A target with no tests for the browser exits at once with a summary.
      reject(Object.assign(new Error(log.trim() || `runner exited with ${code}`), { exitedEarly: true, code }));
    });
  });
  return { url, stop: () => child.kill() };
}

// Each run gets a fresh persistent profile. A default Playwright context is
// ephemeral, like a private window, and WebKit turns OPFS off there.
async function runIn(engine, url) {
  const profile = mkdtempSync(path.join(tmpdir(), `moul-${engine}-`));
  const context = await ENGINES[engine].launchPersistentContext(profile);
  try {
    const page = context.pages()[0] ?? (await context.newPage());
    const errors = [];
    page.on("pageerror", (e) => errors.push(String(e)));
    // MOUL_BROWSER_TEST_CONSOLE=1 prints the console, including panics in
    // callbacks that run outside a test and so never reach its output.
    if (process.env.MOUL_BROWSER_TEST_CONSOLE) {
      page.on("console", (m) => console.log(`    [${engine} console.${m.type()}] ${m.text()}`));
      page.on("worker", (w) => w.on("console", (m) => console.log(`    [${engine} worker console.${m.type()}] ${m.text()}`)));
    }
    await page.goto(url);
    const output = page.locator("#output");
    try {
      await page.waitForFunction(
        () => /test result: (ok|FAILED)/.test(document.getElementById("output")?.textContent ?? ""),
        null,
        { timeout: TIMEOUT_MS },
      );
    } catch {
      return { ok: false, text: `timed out after ${TIMEOUT_MS} ms\n${await output.textContent()}\n${errors.join("\n")}` };
    }
    const text = (await output.textContent()).trim();
    return { ok: /test result: ok/.test(text), text };
  } finally {
    await context.close();
    rmSync(profile, { recursive: true, force: true });
  }
}

const opts = parseArgs(process.argv.slice(2));
const targets = buildTests(opts.cargo);
const failures = [];

for (const target of targets) {
  let server;
  try {
    server = await serve(target.file);
  } catch (e) {
    if (e.exitedEarly && e.code === 0) {
      console.log(`- ${target.name}: no browser tests`);
      continue;
    }
    failures.push(`${target.name}: ${e.message}`);
    console.log(`✗ ${target.name}: runner failed\n${e.message}`);
    continue;
  }
  try {
    for (const engine of opts.browsers) {
      const res = await runIn(engine, server.url);
      const mark = res.ok ? "✓" : "✗";
      console.log(`${mark} ${target.name} in ${engine}`);
      console.log(res.text.replace(/^/gm, "    "));
      if (!res.ok) failures.push(`${target.name} in ${engine}`);
    }
  } finally {
    server.stop();
  }
}

if (failures.length) {
  console.log(`\n${failures.length} failed:\n  ${failures.join("\n  ")}`);
  process.exit(1);
}
console.log(`\nall browser tests passed in ${opts.browsers.join(", ")}`);
