import { describe, expect, it } from 'vitest';
import { createHeadlessClient } from '../src/headless.js';
import {
  HttpTransport,
  type LiveState,
  type ListResult,
  MoulClient,
  MoulError,
  joinLink,
  readJoinLink,
  type RecordModel,
} from '../src/index.js';

const NOW = '2026-11-18T09:00:00Z';

/** A schema file: tasks a person owns, and their teams. */
const schema = {
  format: 1,
  collections: [
    { name: 'users', type: 'auth' },
    { name: 'teams', fields: [{ name: 'name', type: 'text', required: true }], rules: { listRule: '', viewRule: '' } },
    {
      name: 'tasks',
      fields: [
        { name: 'title', type: 'text', required: true },
        { name: 'done', type: 'bool' },
        { name: 'owner', type: 'relation', relationConfig: { targetMoul: 'users', cardinality: '1:N' } },
        { name: 'team', type: 'relation', relationConfig: { targetMoul: 'teams', cardinality: '1:N' } },
        { name: 'attachment', type: 'file' },
      ],
      rules: {
        listRule: "owner = @request.auth.id",
        viewRule: "owner = @request.auth.id",
        createRule: "@request.auth.id != '' && @request.body.owner = @request.auth.id",
        updateRule: 'owner = @request.auth.id',
        deleteRule: 'owner = @request.auth.id',
      },
    },
  ],
};

interface Task extends RecordModel {
  title: string;
  done: boolean;
  owner: string;
  team?: string;
}

type Schema = { users: RecordModel; teams: RecordModel & { name: string }; tasks: Task };

async function setup() {
  const owner = await createHeadlessClient<Schema>({ schema, now: NOW, seed: 7, owner: true });
  // An app's own client: same engine, no owner rights.
  const app = new MoulClient<Schema>(owner.transport, { authStorage: null });
  return { owner, app };
}

async function signUp(app: MoulClient<Schema>, email: string) {
  await app.auth('users').signUp({ email, username: email.split('@')[0], password: 'Password123!' });
  return app.auth('users').signInWithPassword(email, 'Password123!');
}

/** Waits until `query`'s state passes `ok`. */
function until<T>(query: { subscribe(l: (s: LiveState<T>) => void): () => void }, ok: (s: LiveState<T>) => boolean) {
  return new Promise<LiveState<T>>((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('timed out waiting for the live query')), 2000);
    let stop: (() => void) | undefined;
    stop = query.subscribe((s) => {
      if (ok(s)) {
        clearTimeout(timer);
        queueMicrotask(() => stop?.());
        resolve(s);
      }
    });
  });
}

const tick = () => new Promise((r) => setTimeout(r, 20));

describe('records and rules', () => {
  it('applies the schema, and rules decide who reads and writes', async () => {
    const { owner, app } = await setup();
    const cols = await owner.admin.listCollections();
    expect(cols.map((c) => c.name).sort()).toEqual(['tasks', 'teams', 'users']);

    // A guest sees nothing and cannot write.
    expect((await app.collection('tasks').list()).items).toEqual([]);
    await expect(app.collection('tasks').create({ title: 'x' })).rejects.toMatchObject({ status: 401 });

    const ana = await signUp(app, 'ana@example.com');
    expect(app.user?.id).toBe(ana.record.id);
    const t = await app.collection('tasks').create({ title: 'Buy oat milk', owner: ana.record.id });
    expect(t.title).toBe('Buy oat milk');
    expect(t.done).toBeFalsy();
    const got = await app.collection('tasks').get(t.id);
    expect(got.id).toBe(t.id);
    const changed = await app.collection('tasks').update(t.id, { done: true });
    expect(changed.done).toBe(true);
    expect(changed.title).toBe('Buy oat milk');
    expect(await app.collection('tasks').first('done = true')).toMatchObject({ id: t.id });

    // Another person cannot see Ana's task.
    await app.auth('users').signOut();
    await signUp(app, 'bo@example.com');
    expect((await app.collection('tasks').list()).totalItems).toBe(0);
    await expect(app.collection('tasks').get(t.id)).rejects.toBeInstanceOf(MoulError);

    // The owner sees everything.
    expect((await owner.collection('tasks').list()).totalItems).toBe(1);
    await owner.collection('tasks').delete(t.id);
    expect((await owner.collection('tasks').all()).length).toBe(0);
  });

  it('pages through every record', async () => {
    const { owner } = await setup();
    for (let i = 0; i < 12; i++) await owner.collection('teams').create({ name: `team ${i}` });
    const page = await owner.collection('teams').list({ perPage: 5, page: 3, sort: 'name' });
    expect(page).toMatchObject({ page: 3, perPage: 5, totalItems: 12, totalPages: 3 });
    expect(page.items.length).toBe(2);
    expect((await owner.collection('teams').all({ sort: '-name' })).length).toBe(12);
  });

  it('signs in with a code from the local inbox', async () => {
    const { owner, app } = await setup();
    await app.auth('users').requestCode('cy@example.com');
    const [mail] = await owner.admin.inbox({ to: 'cy@example.com' });
    expect(mail.kind).toBe('otp');
    const code = /\b(\d{6})\b/.exec(mail.body)?.[1];
    expect(code).toBeTruthy();
    const session = await app.auth('users').signInWithCode('cy@example.com', code!);
    expect(session.record.email).toBe('cy@example.com');
    const refreshed = await app.auth('users').refresh();
    expect(refreshed.token).not.toBe(session.token);
  });

  it('sends a write again with its request id and applies it once', async () => {
    const { owner } = await setup();
    const req = { method: 'POST', path: '/api/moul/teams/records', json: { name: 'once' }, id: 'test:1' } as const;
    const a = await owner.send(req);
    const b = await owner.send(req);
    expect(a.status).toBe(201);
    expect(b.status).toBe(201);
    expect(b.headers.find(([k]) => k.toLowerCase() === 'moul-replayed')?.[1]).toBe('true');
    expect((await owner.collection('teams').all()).length).toBe(1);
  });
});

describe('live queries', () => {
  it('refresh when a commit touches what they read, and only then', async () => {
    const { owner } = await setup();
    let runs = 0;
    const { readsOf } = await import('../src/live.js');
    const counted = owner.live(async () => {
      runs++;
      const res = await owner.send({ path: '/api/moul/teams/records', query: { sort: 'name' } });
      return { data: owner.unwrap<ListResult<RecordModel>>(res), reads: readsOf(res, 'teams') };
    });
    // Keep the query running for the whole test.
    const keep = counted.subscribe(() => {});
    const first = await until(counted, (s) => !s.loading);
    expect(first.data?.items).toEqual([]);
    expect(runs).toBe(1);

    await owner.collection('teams').create({ name: 'a' });
    const second = await until(counted, (s) => s.data?.items.length === 1);
    expect(second.data?.items[0].name).toBe('a');
    const after = runs;

    // A write to another collection does not touch this query.
    const ana = await owner.collection('users').create({ email: 'ana@example.com', username: 'ana', password: 'Password123!', passwordConfirm: 'Password123!' });
    await owner.collection('tasks').create({ title: 'x', owner: ana.id });
    await tick();
    expect(runs).toBe(after);

    // Several writes in a burst cause at most two refreshes: one for the first notice, one after it.
    await Promise.all([1, 2, 3, 4].map((i) => owner.collection('teams').create({ name: `b${i}` })));
    await until(counted, (st) => st.data?.items.length === 5);
    await tick();
    expect(runs - after).toBeLessThanOrEqual(2);
    keep();

    expect((await owner.collection('teams').live({ sort: 'name' }).value()).totalItems).toBe(5);
  });

  it('follow relations they expand and filter through', async () => {
    const { owner, app } = await setup();
    const ana = await signUp(app, 'ana@example.com');
    const team = await owner.collection('teams').create({ name: 'Home' });
    await app.collection('tasks').create({ title: 'Mop', owner: ana.record.id, team: team.id });
    const q = app.collection('tasks').live({ expand: 'team' });
    await until(q, (s) => s.data?.items[0]?.expand?.team?.name === 'Home');
    await owner.collection('teams').update(team.id, { name: 'House' });
    await until(q, (s) => s.data?.items[0]?.expand?.team?.name === 'House');
  });

  it('refresh when someone signs in or out', async () => {
    const { app } = await setup();
    const ana = await signUp(app, 'ana@example.com');
    await app.collection('tasks').create({ title: 'Mine', owner: ana.record.id });
    const q = app.collection('tasks').live();
    await until(q, (s) => s.data?.totalItems === 1);
    await app.auth('users').signOut();
    await until(q, (s) => s.data?.totalItems === 0);
  });

  it('follow one record until it is deleted', async () => {
    const { owner } = await setup();
    const team = await owner.collection('teams').create({ name: 'Solo' });
    const q = owner.collection('teams').liveGet(team.id);
    await until(q, (s) => s.data?.name === 'Solo');
    await owner.collection('teams').delete(team.id);
    await until(q, (s) => s.data === null && !s.loading);
  });

  it('keep the collection list current', async () => {
    const { owner } = await setup();
    const q = owner.admin.liveCollections();
    await until(q, (s) => s.data?.length === 3);
    await owner.admin.createCollection({ name: 'notes', fields: [{ name: 'text', type: 'text' }] });
    await until(q, (s) => s.data?.some((c) => c.name === 'notes') ?? false);
  });
});

describe('subscriptions', () => {
  it('deliver record events the subscriber may see', async () => {
    const { owner } = await setup();
    const events: string[] = [];
    const stop = await owner.collection('teams').subscribe((e) => events.push(`${e.action}:${e.record.name}`));
    const t = await owner.collection('teams').create({ name: 'one' });
    await owner.collection('teams').update(t.id, { name: 'two' });
    await owner.collection('teams').delete(t.id);
    await tick();
    expect(events).toEqual(['create:one', 'update:two', 'delete:two']);
    stop();
    await owner.collection('teams').create({ name: 'unheard' });
    await tick();
    expect(events.length).toBe(3);
  });
});

describe('files, the database file and owner tools', () => {
  it('uploads Blob values and fetches them back', async () => {
    const { app } = await setup();
    const ana = await signUp(app, 'ana@example.com');
    const t = await app.collection('tasks').create({
      title: 'With a file',
      owner: ana.record.id,
      attachment: new Blob(['hello file'], { type: 'text/plain' }),
    });
    expect(t.attachment?.url).toMatch(/\/storage\//);
    const blob = await app.files.fetch(t.attachment);
    expect(await blob.text()).toBe('hello file');
  });

  it('exports the database file and imports it into a fresh engine', async () => {
    const { owner } = await setup();
    await owner.collection('teams').create({ name: 'kept' });
    const status = await owner.database.status();
    expect(status.role).toBe('writer');
    expect(status.durable).toBe(false);
    const file = await owner.database.export();
    expect(new TextDecoder().decode(file.slice(0, 15))).toBe('SQLite format 3');
    const other = await createHeadlessClient({ owner: true });
    await other.database.import(file);
    expect((await other.collection('teams').all()).map((t) => t.name)).toEqual(['kept']);
  });

  it('writes the schema file and the types', async () => {
    const { owner } = await setup();
    const file = await owner.admin.schema();
    expect(file.collections.map((c) => c.name)).toEqual(['tasks', 'teams', 'users']);
    const types = await owner.admin.types();
    expect(types).toContain('export interface TasksRecord');
    expect(types).toContain('export interface MoulSchema');
    const test = await owner.admin.testRule({ rule: "owner = @request.auth.id", record: { owner: 'u1' }, auth: { id: 'u1' } });
    expect(test).toMatchObject({ valid: true, matched: true });
    const csv = await owner.collection('teams').export('csv');
    expect(await csv.text()).toContain('name');
    await owner.collection('teams').import('name\nImported\n', 'text/csv');
    expect((await owner.collection('teams').all()).map((t) => t.name)).toContain('Imported');
  });
});

describe('over HTTP', () => {
  it('sends the same requests to a server', async () => {
    const { owner } = await setup();
    // A fake server: the headless engine behind fetch.
    const fetcher: typeof fetch = async (input, init) => {
      const url = new URL(String(input));
      const headers = Object.fromEntries(new Headers(init?.headers).entries());
      const body = init?.body;
      const res = await owner.transport.api({
        method: init?.method,
        path: url.pathname,
        query: url.search,
        headers,
        json: typeof body === 'string' ? JSON.parse(body) : undefined,
        owner: headers['x-admin-key'] === 'secret',
      });
      const out = res.json !== undefined ? JSON.stringify(res.json) : (res.bytes ?? null);
      return new Response(out as BodyInit, {
        status: res.status,
        headers: [...res.headers, ...(res.json !== undefined ? [['Content-Type', 'application/json'] as [string, string]] : [])],
      });
    };
    const remote = new MoulClient<Schema>(new HttpTransport({ url: 'https://app.example', fetch: fetcher, adminKey: 'secret' }), {
      authStorage: null,
    });
    await signUp(remote, 'ana@example.com');
    const t = await remote.collection('tasks').create({ title: 'over http', owner: remote.user!.id });
    expect(t.title).toBe('over http');
    expect((await remote.collection('tasks').list()).totalItems).toBe(1);
    expect((await remote.asOwner().admin.listCollections()).length).toBe(3);
    await expect(remote.database.status()).rejects.toMatchObject({ code: 'unsupported' });
  });
});

describe('sign-up', () => {
  it('makes a username from the email when none is given, and another if it is taken', async () => {
    const { app } = await setup();
    const a = await app.auth('users').signUp({ email: 'sam@example.com', password: 'Password123!' });
    expect(a.username).toBe('sam');
    const b = await app.auth('users').signUp({ email: 'sam@example.org', password: 'Password123!' });
    expect(b.username).toMatch(/^sam_[a-z0-9]{1,4}$/);
  });
});

describe('a schema in the options', () => {
  it('is applied before the first request', async () => {
    const { createHeadlessClient: make } = await import('../src/headless.js');
    const base = await make({ owner: true });
    const app = new MoulClient(base.transport, { owner: true, authStorage: null, schema });
    // The first request already sees the collections.
    expect((await app.admin.listCollections()).map((c) => c.name).sort()).toEqual(['tasks', 'teams', 'users']);
    await app.ready;
    // A bad schema rejects `ready` but leaves the client working.
    const bad = new MoulClient(base.transport, { owner: true, authStorage: null, schema: { format: 99, collections: [] } });
    await expect(bad.ready).rejects.toBeInstanceOf(MoulError);
    expect((await bad.admin.listCollections()).length).toBe(3);
  });
});

describe('join links', () => {
  it('carry sync settings in the fragment', () => {
    const settings = {
      endpoint: 'https://acct.r2.cloudflarestorage.com',
      bucket: 'notes',
      accessKeyId: 'AK',
      secretAccessKey: 'SK',
      label: 'Laptop',
    };
    const link = joinLink(settings, 'https://play.moul.dev/?db=notes');
    expect(new URL(link).search).toBe('?db=notes');
    expect(link).not.toContain('SK');
    const back = readJoinLink(link);
    expect(back).toMatchObject({ endpoint: settings.endpoint, bucket: 'notes', secretAccessKey: 'SK' });
    expect(back?.label).toBeUndefined();
    expect(readJoinLink('https://play.moul.dev/#other')).toBeNull();
  });
});
