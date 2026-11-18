// Regression tests for the final review: a leader that opens the database late, subscriptions on a
// follower (`visible`), unsubscribes scoped to the leader that issued the id, and small contract fixes.
import { describe, expect, it } from 'vitest';
import {
  type EngineRequest,
  joinLink,
  MoulClient,
  type Push,
  readJoinLink,
  type RecordModel,
  type Transport,
  WorkerTransport,
} from '../src/index.js';

const tick = (ms = 20) => new Promise((r) => setTimeout(r, ms));

/**
 * A fake local transport. Each leader numbers its subscriptions from 1, as a fresh engine does. A
 * leader can answer with an error (its database would not open) or refuse subscriptions.
 */
function fakeLocal() {
  const listeners = new Set<(p: Push) => void>();
  const subscribes: { leader: string; fields: Record<string, unknown> }[] = [];
  const unsubscribes: { leader: string; id: number }[] = [];
  const api: EngineRequest[] = [];
  const next = new Map<string, number>();
  const state = { leader: 'A', broken: false, refuse: false };
  let gate: Promise<void> = Promise.resolve();
  const t: Transport = {
    kind: 'local',
    name: 'fake',
    async api(req) {
      if (state.broken) throw new Error('the database could not be opened: NoModificationAllowedError');
      api.push(req);
      return { status: 200, headers: [], json: { items: [], totalItems: 0 } };
    },
    async op(op, fields = {}) {
      if (op === 'subscribe') {
        await gate;
        if (state.broken) throw new Error('the database could not be opened: NoModificationAllowedError');
        if (state.refuse) return { ok: false, status: 403, json: { message: 'you may not subscribe' } };
        const id = next.get(state.leader) ?? 1;
        next.set(state.leader, id + 1);
        subscribes.push({ leader: state.leader, fields });
        return { ok: true, id, leader: state.leader };
      }
      if (op === 'unsubscribe') unsubscribes.push({ leader: state.leader, id: fields.id as number });
      return { ok: true };
    },
    onPush(l) {
      listeners.add(l);
      return () => listeners.delete(l);
    },
    close() {},
  };
  return {
    t,
    state,
    subscribes,
    unsubscribes,
    api,
    push: (p: Push) => listeners.forEach((l) => l(p)),
    hold: () => {
      let release!: () => void;
      gate = new Promise((r) => (release = r));
      return release;
    },
  };
}

const event = (id: number, action: 'create' | 'update' | 'delete', record: string): Push => ({
  type: 'event',
  id,
  event: { action, moul: 'x', record: { id: record } as RecordModel, timestamp: '' },
});

describe('a leader that opens the database late', () => {
  it('makes the subscriptions it refused, without onClose, once it says the database is open', async () => {
    const f = fakeLocal();
    const c = new MoulClient(f.t, { authStorage: null, owner: true });
    f.push({ type: 'leader', tab: 'A' });
    const got: string[] = [];
    const closes: string[] = [];
    await c.collection('x').subscribe((e) => got.push(e.record.id), { onClose: (r) => closes.push(r) });

    // Tab A closes; B leads but cannot open the files yet, and answers with the error.
    f.state.leader = 'B';
    f.state.broken = true;
    f.push({ type: 'leader', tab: 'B' });
    await tick();
    expect(f.subscribes.map((s) => s.leader)).toEqual(['A']);
    expect(closes).toEqual([]);

    // B opens the database: same leader, so only the `opened` notice says so.
    f.state.broken = false;
    f.push({ type: 'leader', tab: 'B', opened: true });
    await tick();
    expect(f.subscribes.map((s) => s.leader)).toEqual(['A', 'B']);
    f.push(event(1, 'create', 'r1'));
    expect(got).toEqual(['r1']);
    expect(closes).toEqual([]);
  });

  it('reads live queries again when the database opens', async () => {
    const f = fakeLocal();
    const c = new MoulClient(f.t, { authStorage: null, owner: true });
    f.push({ type: 'leader', tab: 'A' });
    f.state.broken = true;
    const states: { error?: unknown }[] = [];
    const stop = c.collection('x').live().subscribe((s) => states.push(s));
    await tick();
    expect(states.at(-1)?.error).toBeTruthy();
    f.state.broken = false;
    f.push({ type: 'leader', tab: 'A', opened: true });
    await tick();
    expect(states.at(-1)?.error).toBeUndefined();
    stop();
  });

  it('applies the schema file once the database opens', async () => {
    const f = fakeLocal();
    f.state.broken = true;
    const c = new MoulClient(f.t, { authStorage: null, schema: { format: 1, collections: [] } });
    await expect(c.ready).rejects.toThrow(/could not be opened/);
    f.state.broken = false;
    f.push({ type: 'leader', tab: 'A', opened: true });
    await tick();
    expect(f.api.filter((r) => r.path === '/api/schema' && r.method === 'POST')).toHaveLength(1);
    c.close();
  });

  it('keeps trying the schema file while leaders still cannot open the database', async () => {
    const f = fakeLocal();
    f.state.broken = true;
    const c = new MoulClient(f.t, { authStorage: null, schema: { format: 1, collections: [] } });
    await expect(c.ready).rejects.toThrow(/could not be opened/);
    // B takes over and cannot open it either; then it does.
    f.push({ type: 'leader', tab: 'B' });
    await tick();
    expect(f.api).toHaveLength(0);
    f.state.broken = false;
    f.push({ type: 'leader', tab: 'B', opened: true });
    await tick();
    expect(f.api.filter((r) => r.path === '/api/schema')).toHaveLength(1);
    // Applied: later notices do not send it again.
    f.push({ type: 'leader', tab: 'C' });
    await tick();
    expect(f.api.filter((r) => r.path === '/api/schema')).toHaveLength(1);
    c.close();
  });

  it('ends a subscription the new leader refuses, with onClose', async () => {
    const f = fakeLocal();
    const c = new MoulClient(f.t, { authStorage: null, owner: true });
    f.push({ type: 'leader', tab: 'A' });
    const closes: string[] = [];
    await c.collection('x').subscribe(() => {}, { onClose: (r) => closes.push(r) });
    f.state.leader = 'B';
    f.state.refuse = true;
    f.push({ type: 'leader', tab: 'B' });
    await tick();
    expect(closes).toEqual(['you may not subscribe']);
  });
});

describe('subscriptions on a follower', () => {
  it('send the records the subscriber knows of, on the first open and every one after', async () => {
    const f = fakeLocal();
    const c = new MoulClient(f.t, { authStorage: null });
    f.push({ type: 'leader', tab: 'A' });
    await c.collection('x').subscribe(() => {}, { visible: ['a', 'b'] });
    expect(f.subscribes[0].fields.visible).toEqual(['a', 'b']);
    f.push(event(1, 'create', 'c'));
    f.push(event(1, 'delete', 'a'));
    f.state.leader = 'B';
    f.push({ type: 'leader', tab: 'B' });
    await tick();
    expect(f.subscribes[1].fields.visible).toEqual(['b', 'c']);
  });

  it('do not keep ids for a subscription that never hears deletes', async () => {
    const f = fakeLocal();
    const c = new MoulClient(f.t, { authStorage: null });
    f.push({ type: 'leader', tab: 'A' });
    await c.collection('x').subscribe(() => {}, { events: ['create'] });
    f.push(event(1, 'create', 'c'));
    f.state.leader = 'B';
    f.push({ type: 'leader', tab: 'B' });
    await tick();
    expect(f.subscribes[1].fields.visible).toBeUndefined();
  });

  it('send none when there are none', async () => {
    const f = fakeLocal();
    const c = new MoulClient(f.t, { authStorage: null });
    await c.collection('x').subscribe(() => {});
    expect(f.subscribes[0].fields.visible).toBeUndefined();
  });
});

describe('unsubscribing after a token refresh', () => {
  it('does not send the old leader’s id to a new leader, which may have given it to another subscription', async () => {
    const f = fakeLocal();
    const c = new MoulClient(f.t, { authStorage: null });
    const person = { record: { id: 'u1' } as RecordModel, collection: 'users' };
    c.authStore.set({ token: 't1', ...person });
    f.push({ type: 'leader', tab: 'A' });
    const got: string[] = [];
    await c.collection('x').subscribe((e) => got.push(e.record.id));
    expect(f.subscribes).toHaveLength(1);

    // A refreshed token makes the subscription again; while that is out, B takes over.
    const release = f.hold();
    c.authStore.set({ token: 't2', ...person });
    // Let the refresh start: it is now waiting for its new subscription.
    await tick(0);
    f.state.leader = 'B';
    f.push({ type: 'leader', tab: 'B' });
    release();
    await tick();
    // B issued id 1 to the remade subscription; A's id 1 must not be sent to B.
    expect(f.subscribes.map((s) => s.leader)).toEqual(['A', 'B']);
    expect(f.unsubscribes).toEqual([]);
    f.push(event(1, 'create', 'r1'));
    expect(got).toEqual(['r1']);
  });

  it('still ends the old subscription when the leader stayed', async () => {
    const f = fakeLocal();
    const c = new MoulClient(f.t, { authStorage: null });
    const person = { record: { id: 'u1' } as RecordModel, collection: 'users' };
    c.authStore.set({ token: 't1', ...person });
    f.push({ type: 'leader', tab: 'A' });
    await c.collection('x').subscribe(() => {});
    c.authStore.set({ token: 't2', ...person });
    await tick();
    expect(f.subscribes.map((s) => s.leader)).toEqual(['A', 'A']);
    expect(f.unsubscribes).toEqual([{ leader: 'A', id: 1 }]);
  });
});

describe('a stopped subscription', () => {
  it('does not hear onClose when the engine ends it afterwards', async () => {
    const f = fakeLocal();
    const c = new MoulClient(f.t, { authStorage: null, owner: true });
    f.push({ type: 'leader', tab: 'A' });
    const closes: string[] = [];
    const stop = await c.collection('x').subscribe(() => {}, { onClose: (r) => closes.push(r) });
    // Stopped while its unsubscribe waits; the engine ends it before that lands.
    const release = f.hold();
    stop();
    f.push({ type: 'closed', id: 1, reason: 'replaced' });
    release();
    await tick();
    expect(closes).toEqual([]);
  });
});

describe('contract details', () => {
  it('refuses database names that would share another database’s conflict folder', () => {
    expect(() => new WorkerTransport({ db: 'notes-conflicts' })).toThrow(/-conflicts/);
  });

  it('files.delete() sends as the owner, which the engine requires', async () => {
    const f = fakeLocal();
    const c = new MoulClient(f.t, { authStorage: null });
    await c.files.delete('f1');
    expect(f.api.at(-1)).toMatchObject({ method: 'DELETE', path: '/api/files/f1', owner: true });
  });

  it('a join link leaves out unset settings, and reading one takes null as unset', () => {
    const settings = { endpoint: 'https://s3.example', bucket: 'notes', accessKeyId: 'k', secretAccessKey: 's' };
    const link = joinLink({ ...settings, pathStyle: null, prefix: undefined } as any, 'https://play.moul.dev/');
    expect(readJoinLink(link)).toEqual(settings);
    const old = new URL('https://play.moul.dev/');
    const raw = JSON.stringify({ ...settings, pathStyle: null, region: null });
    old.hash = `moul-join=${btoa(raw).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '')}`;
    expect(readJoinLink(old)).toEqual(settings);
  });
});
