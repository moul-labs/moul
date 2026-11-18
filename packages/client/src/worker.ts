// The engine's dedicated worker. Every tab starts one; the tab that wins the database's lock opens
// it (and syncs it), and the others forward their requests to it.
//
// Protocol with the page:
//   page → worker   { type: "init", db, wasmUrl? }    first, once
//   page → worker   { id, req }                        a request; see the engine's `op`s
//   worker → page   { id, ok: true, body } | { id, ok: false, error }
//   worker → page   { type: "push", body }             notices, events, leader changes
//   worker → page   { type: "moul:persist" }           ask the browser to keep the storage
//   worker → page   { type: "moul:fatal", error }      the engine did not start

import init, { MoulWorker } from '../wasm/moul_web.js';

const scope = globalThis as unknown as {
  onmessage: ((e: MessageEvent) => void) | null;
  postMessage(msg: unknown, transfer?: Transferable[]): void;
};

let start!: (msg: { db: string; wasmUrl?: string }) => void;
const ready: Promise<MoulWorker> = new Promise<{ db: string; wasmUrl?: string }>((resolve) => {
  start = resolve;
}).then(async ({ db, wasmUrl }) => {
  await init(wasmUrl ? { module_or_path: wasmUrl } : undefined);
  const moul = new MoulWorker(db);
  moul.onPush((body: unknown) => scope.postMessage({ type: 'push', body }));
  return moul;
});
ready.catch((e: unknown) => scope.postMessage({ type: 'moul:fatal', error: String((e as Error)?.message ?? e) }));

function transferables(body: unknown): Transferable[] {
  if (body instanceof Uint8Array) return [body.buffer as ArrayBuffer];
  const bytes = (body as { bytes?: unknown } | null)?.bytes;
  return bytes instanceof Uint8Array ? [bytes.buffer as ArrayBuffer] : [];
}

scope.onmessage = async (event: MessageEvent) => {
  const msg = event.data;
  if (msg?.type === 'init') {
    start(msg);
    return;
  }
  const { id, req } = msg ?? {};
  try {
    const moul = await ready;
    const body = await moul.request(req);
    scope.postMessage({ id, ok: true, body }, transferables(body));
  } catch (e) {
    scope.postMessage({ id, ok: false, error: String((e as Error)?.message ?? e) });
  }
};
