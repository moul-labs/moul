import { describe, expect, it } from 'vitest';
import { DEFAULT_HUB, databaseNameProblem, HubFlow, suggestDatabaseName, syncTarget } from '../src/lib/hub-flow';

/** A hub with one account (code 123456), answering the account calls the Sync page makes. */
function fakeHub() {
  const calls: { method: string; url: string; auth: string | null; body: any }[] = [];
  const names = ['notes'];
  const fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = new URL(String(input));
    const method = init?.method ?? 'GET';
    const body = typeof init?.body === 'string' ? JSON.parse(init.body) : undefined;
    const auth = new Headers(init?.headers).get('Authorization');
    calls.push({ method, url: String(input), auth, body });
    const route = `${method} ${url.pathname}`;
    if (route === 'POST /api/hub/signin') return Response.json({ sent: true });
    if (route === 'POST /api/hub/verify') {
      if (body.code !== '123456') return Response.json({ message: 'wrong or expired code' }, { status: 400 });
      return Response.json({ token: 'mh_account', device: { id: 'd1', label: body.label ?? '', database: null, createdAt: 1, lastSeenAt: null, current: true }, account: { id: 'a1', email: body.email } });
    }
    if (auth !== 'Bearer mh_account') return Response.json({ message: 'sign in' }, { status: 401 });
    if (route === 'GET /api/hub/databases') return Response.json({ items: names.map((name) => ({ name, createdAt: 1, bytes: 0, head: null })) });
    if (route === 'POST /api/hub/databases') {
      if (names.includes(body.name)) return Response.json({ message: 'the account has a database by that name' }, { status: 409 });
      names.push(body.name);
      return Response.json({ name: body.name, createdAt: 2, bytes: 0, head: null }, { status: 201 });
    }
    return Response.json({ message: 'no such route' }, { status: 404 });
  }) as typeof globalThis.fetch;
  return { fetch, calls };
}

describe('signing in to a hub from the Sync page', () => {
  it('goes from an email to a code to a database, and hands over the token only to connect', async () => {
    const { fetch, calls } = fakeHub();
    const flow = new HubFlow(fetch);
    expect(flow.address).toBe(DEFAULT_HUB);
    flow.address = 'hub.example/';
    flow.email = 'not an email';
    await expect(flow.sendCode()).rejects.toThrow(TypeError);
    expect(calls).toEqual([]);

    flow.email = ' me@example.com ';
    await flow.sendCode();
    expect(flow.step).toBe('code');
    expect(calls[0]).toMatchObject({ method: 'POST', url: 'https://hub.example/api/hub/signin', body: { email: 'me@example.com' } });

    flow.code = '000000';
    await expect(flow.verify('Laptop')).rejects.toMatchObject({ status: 400, message: 'wrong or expired code' });
    expect(flow.step).toBe('code');
    expect(() => flow.settings('Laptop')).toThrow('Sign in first.');

    flow.code = '123456';
    await flow.verify('Laptop');
    expect(flow.step).toBe('database');
    expect(flow.code).toBe('');
    expect(calls.find((c) => c.url.endsWith('/verify') && c.body.code === '123456')?.body.label).toBe('Laptop');
    expect(flow.databases.map((d) => d.name)).toEqual(['notes']);
    expect(flow.database).toBe('notes');

    await expect(flow.create('Bad Name')).rejects.toThrow(TypeError);
    await expect(flow.create('notes')).rejects.toMatchObject({ status: 409 });
    await flow.create('todo');
    expect(flow.databases.map((d) => d.name)).toEqual(['notes', 'todo']);
    expect(flow.database).toBe('todo');

    expect(flow.settings(' Laptop ')).toEqual({ hub: 'https://hub.example', database: 'todo', token: 'mh_account', label: 'Laptop' });
    expect(flow.settings('')).not.toHaveProperty('label');

    flow.reset();
    expect(flow.step).toBe('email');
    expect(flow.databases).toEqual([]);
    expect(() => flow.settings('Laptop')).toThrow('Sign in first.');
  });

  it('sends an invite code only when there is one', async () => {
    const { fetch, calls } = fakeHub();
    const flow = new HubFlow(fetch);
    flow.email = 'me@example.com';
    flow.invite = '  ';
    await flow.sendCode();
    expect(calls[0].body).toEqual({ email: 'me@example.com' });
    flow.reset();
    flow.invite = 'beta-42';
    await flow.sendCode();
    expect(calls[1].body).toEqual({ email: 'me@example.com', invite: 'beta-42' });
    expect(calls[1].url).toBe(`${DEFAULT_HUB}/api/hub/signin`);
  });
});

describe('hub database names', () => {
  it('are suggested from the database name', () => {
    expect(suggestDatabaseName('notes')).toBe('notes');
    expect(suggestDatabaseName('My Notes_2')).toBe('my-notes-2');
    expect(suggestDatabaseName('--x--')).toBe('x');
    expect(suggestDatabaseName('日本')).toBe('app');
    expect(suggestDatabaseName('a'.repeat(80))).toHaveLength(63);
  });

  it('say what is wrong with one the hub would refuse', () => {
    expect(databaseNameProblem('notes-2')).toBeUndefined();
    for (const bad of ['', '-a', 'Notes', 'a_b', 'a'.repeat(64)]) expect(databaseNameProblem(bad), bad).toBeTypeOf('string');
  });
});

describe('the sync target', () => {
  it('names the hub database or the bucket', () => {
    expect(syncTarget({ hub: 'https://sync.moul.dev', database: 'notes' })).toBe('notes on sync.moul.dev');
    expect(syncTarget({ endpoint: 'https://s3.example', bucket: 'b', accessKeyId: 'k' })).toBe('b');
    expect(syncTarget(undefined)).toBe('A bucket');
  });
});
