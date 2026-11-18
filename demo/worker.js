// The engine's dedicated worker. Every tab starts one; the tab that wins the
// leader lock opens the database and syncs it, and the others forward their
// requests to it.
import init, { MoulWorker } from "./pkg/moul_web.js";

const db = new URL(self.location.href).searchParams.get("db") || "demo";
let moul;
const ready = init().then(() => {
  moul = new MoulWorker(db);
});

self.onmessage = async (event) => {
  const { id, req } = event.data;
  try {
    await ready;
    const body = await moul.request(req);
    const transfer = body instanceof Uint8Array ? [body.buffer] : [];
    self.postMessage({ id, ok: true, body, leader: moul.isLeader }, transfer);
  } catch (e) {
    self.postMessage({ id, ok: false, error: String(e?.message ?? e), leader: moul?.isLeader });
  }
};

ready.catch((e) => self.postMessage({ type: "moul:fatal", error: String(e?.message ?? e) }));
