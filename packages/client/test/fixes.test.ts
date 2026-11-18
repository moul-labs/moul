// Regression tests for the review of the SDK: each test names the problem it guards against.
import { afterEach, describe, expect, it, vi } from 'vitest';
import { loggedWrite } from '../src/client.js';
import { createHeadlessClient } from '../src/headless.js';
import { HttpTransport, type LiveState, MoulClient, MoulError, joinLink, readJoinLink, type RecordModel } from '../src/index.js';

const schema = {
  format: 1,
  collections: [
    { name: 'users', type: 'auth' },
    { name: 'teams', fields: [{ name: 'name', type: 'text', required: true }], rules: { listRule: '', viewRule: '', createRule: '', updateRule: '', deleteRule: '' } },
    {
      name: 'tasks',
      fields: [
        { name: 'title', type: 'text', required: true },
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

function until<T>(q: { subscribe(l: (s: LiveState<T>) => void): () => void }, ok: (s: LiveState<T>) => boolean, ms = 2000) {
  return new Promise<LiveState<T>>((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('timed out')), ms);
    let stop: (() => void) | undefined;
    stop = q.subscribe((s) => {
      if (ok(s)) {
        clearTimeout(timer);
        queueMicrotask(() => stop?.());
        resolve(s);
      }
    });
  });
}

async function setup() {
  const owner = await createHeadlessClient({ schema, owner: true });
  const app = new MoulClient(owner.transport, { authStorage: null });
  return { owner, app };
}

async function signIn(app: MoulClient, email: string) {
  await app.auth('users').signUp({ email, password: 'Password123!' });
  return app.auth('users').signInWithPassword(email, 'Password123!');
}

afterEach(() => vi.restoreAllMocks());

/** A fake remote server: the headless engine behind fetch, plus a controllable event stream. */
function fakeServer(owner: MoulClient) {
  const streams: { path: string; push(text: string): void; headers: Headers }[] = [];
  const fetcher = (async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = new URL(String(input));
    const headers = new Headers(init?.headers);
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
  return { fetcher, streams };
}

describe('live queries over HTTP', () => {
  it('follow the collection they read and refresh on this client’s writes and on server events', async () => {
    const { owner } = await setup();
    const server = fakeServer(owner);
    const remote = new MoulClient(new HttpTransport({ url: 'https://app.example', fetch: server.fetcher, adminKey: 'k' }), {
      owner: true,
      authStorage: null,
    });
    const q = remote.collection('teams').live();
    const stop = q.subscribe(() => {});
    await until(q, (s) => s.data?.totalItems === 0);
    await tick();
    expect(server.streams.map((s) => s.path)).toEqual(['/api/moul/teams/subscribe']);
    // This client's own write.
    await remote.collection('teams').create({ name: 'a' });
    await until(q, (s) => s.data?.totalItems === 1);
    // Someone else's write, announced by the server.
    await owner.collection('teams').create({ name: 'b' });
    server.streams[0].push('event: create\ndata: {"action":"create","moul":"teams","record":{"id":"x"}}\n\n');
    await until(q, (s) => s.data?.totalItems === 2);
    stop();
    remote.close();
  });
});

describe('live query robustness', () => {
  it('keeps running when a listener throws', async () => {
    const { owner } = await setup();
    const errors: unknown[] = [];
    vi.spyOn(console, 'error').mockImplementation((e) => errors.push(e));
    const q = owner.collection('teams').live();
    const stopBad = q.subscribe(() => {
      throw new Error('render bug');
    });
    let seen = -1;
    const stopGood = q.subscribe((s) => {
      if (s.data) seen = s.data.totalItems;
    });
    for (const name of ['a', 'b', 'c']) {
      await owner.collection('teams').create({ name });
      await tick();
    }
    await tick(50);
    expect(seen).toBe(3);
    expect(q.state.error).toBeUndefined();
    expect(errors.length).toBeGreaterThan(0);
    stopBad();
    stopGood();
  });

  it('value() reads after the call, even while others listen; refresh() runs without listeners', async () => {
    const { owner } = await setup();
    const q = owner.collection('teams').live();
    const stop = q.subscribe(() => {});
    await until(q, (s) => !s.loading);
    await owner.collection('teams').create({ name: 'x' });
    expect((await q.value()).totalItems).toBe(1);
    stop();
    let runs = 0;
    const counted = owner.live(async () => {
      runs++;
      return { data: runs, reads: { all: true } };
    });
    await counted.refresh();
    expect(runs).toBe(1);
    expect(counted.state.data).toBe(1);
  });

  it('drops the previous person’s results at once when the sign-in changes', async () => {
    const { app } = await setup();
    const ana = await signIn(app, 'ana@example.com');
    await app.collection('tasks').create({ title: 'mine', owner: ana.record.id });
    const q = app.collection('tasks').live();
    const states: LiveState<any>[] = [];
    const stop = q.subscribe((s) => states.push(s));
    await until(q, (s) => s.data?.totalItems === 1);
    states.length = 0;
    app.authStore.set(null);
    expect(states[0]).toMatchObject({ data: undefined, loading: true });
    await until(q, (s) => s.data?.totalItems === 0);
    stop();
  });
});

describe('subscriptions', () => {
  it('are not duplicated by quick sign-in changes, and stop for good', async () => {
    const { owner } = await setup();
    const got: string[] = [];
    const stop = await owner.collection('teams').subscribe((e) => got.push(e.record.name));
    owner.authStore.set({ token: 'nope', record: { id: 'u' } as RecordModel, collection: 'users' });
    owner.authStore.set(null);
    await tick(50);
    await owner.collection('teams').create({ name: 'once' });
    await tick(50);
    expect(got).toEqual(['once']);
    stop();
    await tick(20);
    await owner.collection('teams').create({ name: 'after' });
    await tick(50);
    expect(got).toEqual(['once']);
  });

  it('made through asOwner() see what only the owner may see', async () => {
    const { owner, app } = await setup();
    await signIn(app, 'ana@example.com');
    const got: string[] = [];
    const stop = await app.asOwner().collection('tasks').subscribe((e) => got.push(e.record.title));
    const bo = await owner.collection('users').create({ email: 'bo@example.com', username: 'bo', password: 'Password123!', passwordConfirm: 'Password123!' });
    await owner.collection('tasks').create({ title: "bo's", owner: bo.id });
    await tick(50);
    expect(got).toEqual(["bo's"]);
    stop();
  });

  it('can be cancelled with a signal before they are made', async () => {
    const { owner } = await setup();
    const got: string[] = [];
    const ac = new AbortController();
    const pending = owner.collection('teams').subscribe((e) => got.push(e.record.name), { signal: ac.signal });
    ac.abort();
    await pending;
    await owner.collection('teams').create({ name: 'x' });
    await tick(50);
    expect(got).toEqual([]);
  });
});

describe('requests', () => {
  it('are sent again only when the engine applies them once', () => {
    expect(loggedWrite('POST', '/api/moul/tasks/records')).toBe(true);
    expect(loggedWrite('PATCH', '/api/moul/tasks/records/t1')).toBe(true);
    expect(loggedWrite('DELETE', '/api/moul/tasks')).toBe(true);
    expect(loggedWrite('POST', '/api/schema')).toBe(true);
    expect(loggedWrite('POST', '/api/moul/users/otp/request')).toBe(true);
    expect(loggedWrite('POST', '/api/moul/users/refresh')).toBe(false);
    expect(loggedWrite('POST', '/api/moul/users/auth-with-password')).toBe(false);
    expect(loggedWrite('POST', '/api/moul/users/logout')).toBe(false);
  });

  it('refuse a path with a dot step, which a URL would resolve to the collection', async () => {
    const { owner } = await setup();
    await expect(owner.collection('teams').delete('..')).rejects.toMatchObject({ status: 400 });
    await expect(owner.send({ method: 'DELETE', path: '/api/moul/teams/records/%2e%2e' })).rejects.toBeInstanceOf(MoulError);
    expect((await owner.admin.listCollections()).length).toBe(3);
  });

  it('from asOwner() wait for the schema too', async () => {
    const base = await createHeadlessClient({ owner: true });
    const app = new MoulClient(base.transport, { authStorage: null, schema });
    const names = (await app.asOwner().admin.listCollections()).map((c) => c.name).sort();
    expect(names).toEqual(['tasks', 'teams', 'users']);
  });

  it('clear the session when a refresh is refused', async () => {
    const { app } = await setup();
    await signIn(app, 'ana@example.com');
    app.authStore.set({ ...app.authStore.session!, token: 'revoked.or.forged' });
    await expect(app.auth('users').refresh()).rejects.toMatchObject({ status: 401 });
    expect(app.authStore.session).toBeNull();
  });
});

describe('remote files and join links', () => {
  it('keep a server path in file URLs', async () => {
    const t = new HttpTransport({ url: 'https://example.com/moul', fetch: (async () => new Response('{}')) as typeof fetch });
    const c = new MoulClient(t, { authStorage: null });
    expect(await c.files.url('/storage/abc/a.png')).toBe('https://example.com/moul/storage/abc/a.png');
    expect(await c.files.url('https://cdn.example/x.png')).toBe('https://cdn.example/x.png');
    c.close();
    await expect(t.api({ path: '/api/moul' })).rejects.toMatchObject({ code: 'closed' });
  });

  it('accept only well-formed settings from a join link', () => {
    const good = { endpoint: 'https://acct.r2.cloudflarestorage.com/', bucket: 'notes', accessKeyId: 'AK', secretAccessKey: 'SK' };
    const link = (s: object) => joinLink(s as any, 'https://play.moul.dev/');
    expect(readJoinLink(link(good))).toEqual({ ...good, endpoint: 'https://acct.r2.cloudflarestorage.com' });
    expect(readJoinLink(link({ ...good, extra: 'x' }))).not.toHaveProperty('extra');
    expect(readJoinLink(link({ ...good, endpoint: 'http://169.254.169.254' }))).toBeNull();
    expect(readJoinLink(link({ ...good, endpoint: 'http://127.0.0.1:9000' }))?.endpoint).toBe('http://127.0.0.1:9000');
    expect(readJoinLink(link({ ...good, endpoint: 'javascript:alert(1)' }))).toBeNull();
    expect(readJoinLink(link({ ...good, endpoint: 'https://user:pw@evil.example' }))).toBeNull();
    expect(readJoinLink(link({ ...good, pathStyle: 'yes' }))).toBeNull();
    expect(readJoinLink(link({ ...good, region: {} }))).toBeNull();
    expect(readJoinLink(link({ ...good, prefix: 'a/../b' }))).toBeNull();
    expect(readJoinLink(link({ ...good, bucket: '../x' }))).toBeNull();
  });
});
