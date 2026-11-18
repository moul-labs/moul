// Regression tests for the second review of the SDK: sign-in races, owner streams, leader changes.
import { afterEach, describe, expect, it, vi } from 'vitest';
import { createHeadlessClient } from '../src/headless.js';
import {
  type EngineRequest,
  type EngineResponse,
  HttpTransport,
  type LiveState,
  MoulClient,
  MoulError,
  type Push,
  type RecordModel,
  type Transport,
} from '../src/index.js';

const schema = {
  format: 1,
  collections: [
    { name: 'users', type: 'auth' },
    { name: 'teams', fields: [{ name: 'name', type: 'text' }], rules: { listRule: '', viewRule: '', createRule: '', updateRule: '', deleteRule: '' } },
    {
      name: 'tasks',
      fields: [
        { name: 'title', type: 'text' },
        { name: 'owner', type: 'relation', relationConfig: { targetMoul: 'users', cardinality: '1:N' } },
      ],
      rules: {
        listRule: 'owner = @request.auth.id',
        viewRule: 'owner = @request.auth.id',
        createRule: "@request.auth.id != ''",
        updateRule: 'owner = @request.auth.id',
        deleteRule: 'owner = @request.auth.id',
      },
    },
  ],
};

const tick = (ms = 20) => new Promise((r) => setTimeout(r, ms));

afterEach(() => vi.restoreAllMocks());

/** A transport around another whose answers can be held back. */
function slow(inner: Transport, delay: () => number): Transport {
  return {
    kind: inner.kind,
    name: inner.name,
    async api(req) {
      const res = await inner.api(req);
      await tick(delay());
      return res;
    },
    op: (op, fields) => inner.op(op, fields),
    onPush: (l) => inner.onPush(l),
    close: () => inner.close(),
  };
}

async function setup() {
  const owner = await createHeadlessClient({ schema, owner: true });
  const app = new MoulClient(owner.transport, { authStorage: null });
  await app.auth('users').signUp({ email: 'ana@example.com', password: 'Password123!' });
  await app.auth('users').signInWithPassword('ana@example.com', 'Password123!');
  return { owner, app };
}

describe('sign-in races', () => {
  it('a refresh that answers after sign-out does not sign the person back in', async () => {
    const { owner } = await setup();
    let hold = 0;
    const app = new MoulClient(slow(owner.transport, () => hold), { authStorage: null });
    await app.auth('users').signInWithPassword('ana@example.com', 'Password123!');
    hold = 40;
    const refreshing = app.auth('users').refresh();
    refreshing.catch(() => {});
    await tick(5);
    await app.auth('users').signOut();
    await expect(refreshing).rejects.toBeInstanceOf(MoulError);
    expect(app.authStore.session).toBeNull();
  });

  it('a refused refresh does not sign out someone who signed in meanwhile', async () => {
    const { owner, app } = await setup();
    await owner.collection('users').create({ email: 'bo@example.com', username: 'bo', password: 'Password123!', passwordConfirm: 'Password123!' });
    let hold = 0;
    const slowApp = new MoulClient(slow(owner.transport, () => hold), { authStorage: null });
    slowApp.authStore.set({ token: 'forged.token.x', record: { id: 'x' } as RecordModel, collection: 'users' });
    hold = 40;
    const refreshing = slowApp.auth('users').refresh();
    refreshing.catch(() => {});
    await tick(5);
    hold = 0;
    await slowApp.auth('users').signInWithPassword('bo@example.com', 'Password123!');
    await expect(refreshing).rejects.toMatchObject({ status: 401 });
    expect(slowApp.authStore.record?.email).toBe('bo@example.com');
    void app;
  });

  it('a live query run in flight at sign-out never shows the previous person’s data', async () => {
    const { owner } = await setup();
    let hold = 0;
    let failGuests = false;
    const base = slow(owner.transport, () => hold);
    const transport: Transport = {
      ...base,
      async api(req: EngineRequest): Promise<EngineResponse> {
        if (failGuests && !req.headers?.Authorization) throw new MoulError(0, 'offline', undefined, 'network');
        return base.api(req);
      },
    };
    const app = new MoulClient(transport, { authStorage: null });
    const ana = await app.auth('users').signInWithPassword('ana@example.com', 'Password123!');
    await app.collection('tasks').create({ title: 'mine', owner: ana.record.id });
    const q = app.collection('tasks').live();
    const states: LiveState<any>[] = [];
    const stop = q.subscribe((s) => states.push(s));
    await tick(50);
    expect(q.state.data?.totalItems).toBe(1);
    hold = 60;
    failGuests = true;
    void q.refresh();
    await tick(10);
    const mark = states.length;
    app.authStore.set(null);
    await tick(150);
    const afterSignOut = states.slice(mark);
    expect(afterSignOut.length).toBeGreaterThan(0);
    expect(afterSignOut.every((s) => s.data === undefined)).toBe(true);
    expect(q.state.error?.message).toBe('offline');
    stop();
  });

  it('an idle query does not hand the previous person’s data to its next subscriber', async () => {
    const { app } = await setup();
    await app.collection('tasks').create({ title: 'mine', owner: app.user!.id });
    const q = app.collection('tasks').live();
    expect((await q.value()).totalItems).toBe(1);
    app.authStore.set(null);
    let first: LiveState<any> | undefined;
    const stop = q.subscribe((s) => (first ??= s));
    expect(first).toMatchObject({ data: undefined, loading: true });
    stop();
  });

  it('a token refresh for the same person keeps what is shown', async () => {
    const { app } = await setup();
    await app.collection('tasks').create({ title: 'mine', owner: app.user!.id });
    const q = app.collection('tasks').live();
    const states: LiveState<any>[] = [];
    const stop = q.subscribe((s) => states.push(s));
    await tick(50);
    const shown = q.state.data;
    states.length = 0;
    await app.auth('users').refresh();
    await tick(50);
    expect(states.some((s) => s.data === undefined)).toBe(false);
    expect(q.state.data).toBe(shown);
    stop();
  });

  it('a throwing sign-in listener does not stop sign-out from revoking the token', async () => {
    const { owner, app } = await setup();
    vi.spyOn(console, 'error').mockImplementation(() => {});
    const token = app.authStore.token!;
    app.onAuthChange(() => {
      throw new Error('app bug');
    });
    await app.auth('users').signOut();
    expect(app.authStore.session).toBeNull();
    const probe = new MoulClient(owner.transport, { authStorage: null });
    probe.authStore.set({ token, record: { id: 'x' } as RecordModel, collection: 'users' });
    await expect(probe.auth('users').refresh()).rejects.toMatchObject({ status: 401 });
  });

  it('an owner client refreshes the sign-in it holds', async () => {
    const { app } = await setup();
    const before = app.authStore.token;
    const s = await app.asOwner().auth('users').refresh();
    expect(s.token).not.toBe(before);
  });
});

/** A fake remote server: the headless engine behind fetch, plus controllable event streams. */
function fakeServer(owner: MoulClient, opts: { schemaDelay?: number } = {}) {
  const calls: { path: string; method: string; headers: Headers; at: number }[] = [];
  const streams: { path: string; push(text: string): void; headers: Headers }[] = [];
  let clock = 0;
  const fetcher = (async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = new URL(String(input));
    const headers = new Headers(init?.headers);
    calls.push({ path: url.pathname, method: init?.method ?? 'GET', headers, at: clock++ });
    if (url.pathname.endsWith('/subscribe')) {
      let push!: (t: string) => void;
      const body = new ReadableStream<Uint8Array>({
        start(c) {
          push = (t) => c.enqueue(new TextEncoder().encode(t));
          init?.signal?.addEventListener('abort', () => c.error(new DOMException('aborted', 'AbortError')));
        },
      });
      streams.push({ path: url.pathname, push: (t) => push(t), headers });
      return new Response(body, { status: 200, headers: { 'Content-Type': 'text/event-stream' } });
    }
    if (url.pathname === '/api/schema' && opts.schemaDelay) await tick(opts.schemaDelay);
    const body = init?.body;
    const res = await owner.transport.api({
      method: init?.method,
      path: url.pathname,
      query: url.search,
      headers: Object.fromEntries(headers.entries()),
      json: typeof body === 'string' ? JSON.parse(body) : undefined,
      owner: headers.get('x-admin-key') === 'k',
    });
    const out = res.json !== undefined ? JSON.stringify(res.json) : (res.bytes ?? null);
    return new Response(out as BodyInit, {
      status: res.status,
      headers: [...res.headers, ...(res.json !== undefined ? [['Content-Type', 'application/json'] as [string, string]] : [])],
    });
  }) as typeof fetch;
  return { fetcher, calls, streams };
}

describe('over HTTP', () => {
  it('owner subscriptions carry the admin key', async () => {
    const { owner } = await setup();
    const server = fakeServer(owner);
    const remote = new MoulClient(new HttpTransport({ url: 'https://x.example', fetch: server.fetcher, adminKey: 'k' }), { owner: true, authStorage: null });
    const stop = await remote.collection('tasks').subscribe(() => {});
    await tick();
    expect(server.streams[0].headers.get('x-admin-key')).toBe('k');
    stop();
    remote.close();
  });

  it('a live query refreshes when its stream first connects, and follows again after a sign-in', async () => {
    const { owner } = await setup();
    const server = fakeServer(owner);
    const remote = new MoulClient(new HttpTransport({ url: 'https://x.example', fetch: server.fetcher }), { authStorage: null });
    const q = remote.collection('teams').live();
    const stop = q.subscribe(() => {});
    await tick(50);
    expect(server.streams.length).toBe(1);
    // The server ends the stream: the sign-in expired.
    server.streams[0].push('event: auth_expired\ndata: {}\n\n');
    await tick(20);
    await remote.auth('users').signInWithPassword('ana@example.com', 'Password123!');
    await tick(50);
    // Followed again: as the signed-in person the read also depends on their own account, so the
    // query now follows every collection.
    expect(server.streams.length).toBeGreaterThanOrEqual(2);
    expect(server.streams.at(-1)?.path).toBe('/api/moul/subscribe');
    stop();
    remote.close();
  });

  it('a live query that reads related collections follows all of them', async () => {
    const t = new HttpTransport({ url: 'https://x.example', fetch: (async () => new Response('{}')) as typeof fetch });
    const streams: string[] = [];
    (t as any).stream = (sub: { collection: string }) => {
      streams.push(sub.collection);
      return () => {};
    };
    const c = new MoulClient(t, { authStorage: null });
    const q = c.live(async () => ({ data: 1, reads: { all: false, ids: new Set(['a', 'b']), names: new Set(['tasks']) } }));
    const stop = q.subscribe(() => {});
    await tick(30);
    expect(streams).toEqual(['*']);
    stop();
  });

  it('subscriptions wait for the schema file', async () => {
    const base = await createHeadlessClient({ owner: true });
    const server = fakeServer(base, { schemaDelay: 40 });
    const remote = new MoulClient(new HttpTransport({ url: 'https://x.example', fetch: server.fetcher, adminKey: 'k' }), {
      owner: true,
      authStorage: null,
      schema,
    });
    const stop = await remote.collection('teams').subscribe(() => {});
    const schemaAt = server.calls.find((c) => c.path === '/api/schema')!.at;
    const streamAt = server.calls.find((c) => c.path.endsWith('/subscribe'))!.at;
    expect(streamAt).toBeGreaterThan(schemaAt);
    stop();
    remote.close();
  });

  it('a stopped subscriber hears nothing more, even from the same chunk', async () => {
    const { owner } = await setup();
    const server = fakeServer(owner);
    const remote = new MoulClient(new HttpTransport({ url: 'https://x.example', fetch: server.fetcher }), { authStorage: null });
    const got: string[] = [];
    let stop!: () => void;
    stop = await remote.collection('teams').subscribe((e) => {
      got.push(e.record.id);
      stop();
    });
    await tick();
    server.streams[0].push(
      'data: {"action":"create","moul":"teams","record":{"id":"1"}}\n\n' + 'data: {"action":"create","moul":"teams","record":{"id":"2"}}\n\n',
    );
    await tick();
    expect(got).toEqual(['1']);
    remote.close();
    // After close, a new subscription opens no stream.
    const before = server.streams.length;
    await expect(remote.collection('teams').subscribe(() => {})).rejects.toMatchObject({ code: 'closed' });
    expect(server.streams.length).toBe(before);
  });

  it('fetches a file by the full URL url() gave, and refuses another server’s', async () => {
    const { owner } = await setup();
    const server = fakeServer(owner);
    const remote = new MoulClient(new HttpTransport({ url: 'https://x.example/base', fetch: server.fetcher }), { authStorage: null });
    await remote.files.fetch('https://x.example/base/storage/none/a.png').catch(() => {});
    expect(server.calls.at(-1)?.path).toBe('/base/storage/none/a.png');
    await expect(remote.files.fetch('https://cdn.example/a.png')).rejects.toMatchObject({ status: 400 });
    remote.close();
  });
});

describe('paths and handles', () => {
  it('refuse characters a URL would turn into a step up', async () => {
    const { owner } = await setup();
    for (const path of ['/api/moul/teams/records\\..', '/api/moul/teams/records/..#', '/api/moul/teams/records/.\t.']) {
      await expect(owner.send({ method: 'DELETE', path })).rejects.toMatchObject({ status: 400 });
    }
    await expect(owner.collection('teams').get('..')).rejects.toMatchObject({ status: 400 });
    await expect(owner.collection('teams').get('')).rejects.toMatchObject({ status: 400 });
    expect((await owner.admin.listCollections()).length).toBe(3);
  });

  it('asOwner() shares blob URLs and the schema outcome', async () => {
    const base = await createHeadlessClient({ owner: true });
    const bad = new MoulClient(base.transport, { authStorage: null, schema: { format: 99, collections: [] } });
    await expect(bad.ready).rejects.toBeInstanceOf(MoulError);
    await expect(bad.asOwner().ready).rejects.toBeInstanceOf(MoulError);
    expect(bad.asOwner().files.objectUrls).toBe(bad.files.objectUrls);
  });
});

describe('leader changes', () => {
  /** A fake local transport: answers subscribe with the leader's tab, and lets the test push. */
  function fakeLocal() {
    const listeners = new Set<(p: Push) => void>();
    const subscribes: string[] = [];
    const unsubscribes: number[] = [];
    let leader = 'A';
    let next = 1;
    let gate: Promise<void> = Promise.resolve();
    const t: Transport = {
      kind: 'local',
      name: 'fake',
      api: async () => ({ status: 200, headers: [], json: {} }),
      async op(op, fields) {
        if (op === 'subscribe') {
          await gate;
          subscribes.push(leader);
          return { ok: true, id: next++, leader };
        }
        if (op === 'unsubscribe') unsubscribes.push(fields?.id as number);
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
      subscribes,
      unsubscribes,
      push: (p: Push) => listeners.forEach((l) => l(p)),
      setLeader: (tab: string) => (leader = tab),
      hold: () => {
        let release!: () => void;
        gate = new Promise((r) => (release = r));
        return release;
      },
    };
  }

  it('a subscription answered by the first leader after its news arrived is kept, not made twice', async () => {
    const f = fakeLocal();
    const c = new MoulClient(f.t, { authStorage: null, owner: true });
    const release = f.hold();
    const pending = c.collection('x').subscribe(() => {});
    f.push({ type: 'leader', tab: 'A' });
    release();
    await pending;
    await tick();
    expect(f.subscribes).toEqual(['A']);
  });

  it('a new leader gets the subscription again, and a late answer from the old one is dropped', async () => {
    const f = fakeLocal();
    const c = new MoulClient(f.t, { authStorage: null, owner: true });
    f.push({ type: 'leader', tab: 'A' });
    const got: string[] = [];
    await c.collection('x').subscribe((e) => got.push(e.record.id));
    f.setLeader('B');
    f.push({ type: 'leader', tab: 'B' });
    await tick();
    expect(f.subscribes).toEqual(['A', 'B']);
    f.push({ type: 'event', id: 2, event: { action: 'create', moul: 'x', record: { id: 'r1' } as RecordModel, timestamp: '' } });
    expect(got).toEqual(['r1']);
  });

  it('a subscription the engine ends before its answer arrives is reported closed', async () => {
    const f = fakeLocal();
    const c = new MoulClient(f.t, { authStorage: null, owner: true });
    f.push({ type: 'leader', tab: 'A' });
    const release = f.hold();
    const closes: string[] = [];
    const pending = c.collection('x').subscribe(() => {}, { onClose: (r) => closes.push(r) });
    // The engine makes id 1, ends it (its data was replaced), and only then answers.
    f.push({ type: 'closed', id: 1, reason: 'replaced' });
    release();
    await pending;
    expect(closes).toEqual(['replaced']);
  });
});
