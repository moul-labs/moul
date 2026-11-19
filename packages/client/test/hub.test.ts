import { describe, expect, it } from 'vitest';
import { HubClient, joinHub, joinLink, MoulClient, MoulError, readJoinLink, type Transport } from '../src/index.js';

interface Call {
  method: string;
  url: string;
  headers: Headers;
  body: any;
}

/** A fetch that records each call and answers from `routes`, keyed `METHOD /path`. */
function fakeHub(routes: Record<string, (call: Call) => Response | Promise<Response>>) {
  const calls: Call[] = [];
  const fetch: typeof globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = new URL(String(input));
    const call: Call = {
      method: init?.method ?? 'GET',
      url: String(input),
      headers: new Headers(init?.headers),
      body: typeof init?.body === 'string' ? JSON.parse(init.body) : init?.body,
    };
    calls.push(call);
    const route = routes[`${call.method} ${url.pathname}`];
    if (!route) return Response.json({ message: `no route ${call.method} ${url.pathname}` }, { status: 404 });
    return route(call);
  }) as typeof globalThis.fetch;
  return { fetch, calls };
}

const device = { id: 'd1', label: 'Laptop', database: null, createdAt: 1_763_000_000_000, lastSeenAt: null, current: true };

describe('HubClient', () => {
  it('signs in with an email code', async () => {
    const { fetch, calls } = fakeHub({
      'POST /api/hub/signin': () => Response.json({ sent: true }),
      'POST /api/hub/verify': (c) =>
        c.body.code === '123456'
          ? Response.json({ token: 'mh_account', device, account: { id: 'a1', email: c.body.email } })
          : Response.json({ message: 'wrong or expired code' }, { status: 400 }),
    });
    const hub = new HubClient('https://sync.moul.dev/', { fetch });
    expect(hub.url).toBe('https://sync.moul.dev');
    await hub.signIn('me@example.com', { invite: 'inv-1' });
    expect(calls[0]).toMatchObject({ method: 'POST', url: 'https://sync.moul.dev/api/hub/signin', body: { email: 'me@example.com', invite: 'inv-1' } });
    expect(calls[0].headers.get('Content-Type')).toBe('application/json');
    expect(calls[0].headers.has('Authorization')).toBe(false);

    const session = await hub.verify('me@example.com', '123456', { label: 'Laptop' });
    expect(session).toMatchObject({ token: 'mh_account', account: { email: 'me@example.com' }, device: { id: 'd1', lastSeenAt: null } });
    expect(calls[1].body).toEqual({ email: 'me@example.com', code: '123456', label: 'Laptop' });

    const err = await hub.verify('me@example.com', '000000').catch((e) => e);
    expect(err).toBeInstanceOf(MoulError);
    expect(err).toMatchObject({ status: 400, message: 'wrong or expired code', data: { message: 'wrong or expired code' } });
  });

  it('sends the token and reads databases, devices and the account', async () => {
    const head = { txid: 7, writer: 'dev1', generation: 1, replica: 'r1', at: '2025-11-13T02:13:20Z' };
    const { fetch, calls } = fakeHub({
      'GET /api/hub/me': () =>
        Response.json({ account: { id: 'a1', email: 'me@example.com' }, device, limits: { databases: 10, bytes: 1 << 30 }, usage: { databases: 1, bytes: 512 } }),
      'GET /api/hub/devices': () => Response.json({ items: [device, { ...device, id: 'd2', database: 'notes', current: false, lastSeenAt: '2025-11-13T02:13:20Z' }] }),
      'DELETE /api/hub/devices/d%202': () => new Response(null, { status: 204 }),
      'GET /api/hub/databases': () => Response.json({ items: [{ name: 'notes', createdAt: 1, bytes: 2048, head }, { name: 'empty', createdAt: 2, bytes: 0, head: null }] }),
      'POST /api/hub/databases': (c) =>
        c.body.name === 'notes'
          ? Response.json({ message: 'the account has a database by that name' }, { status: 409 })
          : Response.json({ name: c.body.name, createdAt: 3, bytes: 0, head: null }, { status: 201 }),
      'DELETE /api/hub/databases/notes': () => new Response(null, { status: 204 }),
      'POST /api/hub/databases/notes/joins': () => Response.json({ ticket: 'tk_1', expiresAt: 1_763_000_900_000 }, { status: 201 }),
    });
    const hub = new HubClient('https://sync.moul.dev', { fetch });
    const me = await hub.me('mh_account');
    expect(me.usage).toEqual({ databases: 1, bytes: 512 });
    expect(calls.at(-1)!.headers.get('Authorization')).toBe('Bearer mh_account');

    const devices = await hub.devices('mh_account');
    expect(devices[1]).toMatchObject({ id: 'd2', database: 'notes', lastSeenAt: 1_763_000_000_000 });
    await hub.revokeDevice('mh_account', 'd 2');
    expect(calls.at(-1)).toMatchObject({ method: 'DELETE', url: 'https://sync.moul.dev/api/hub/devices/d%202' });

    const dbs = await hub.databases('mh_account');
    expect(dbs.map((d) => d.name)).toEqual(['notes', 'empty']);
    expect(dbs[0].head).toMatchObject({ txid: 7, at: 1_763_000_000_000 });
    expect(dbs[1].head).toBeNull();

    expect(await hub.createDatabase('mh_account', 'todo')).toMatchObject({ name: 'todo', head: null });
    await expect(hub.createDatabase('mh_account', 'notes')).rejects.toMatchObject({ status: 409 });
    await hub.deleteDatabase('mh_account', 'notes');
    expect(await hub.createJoinTicket('mh_account', 'notes')).toEqual({ ticket: 'tk_1', expiresAt: 1_763_000_900_000 });
    expect(calls.every((c) => c.headers.get('Authorization') === 'Bearer mh_account')).toBe(true);
  });

  it('says when the hub could not be reached, and refuses an address that is not one', async () => {
    const hub = new HubClient('https://sync.moul.dev', {
      fetch: (async () => {
        throw new TypeError('Failed to fetch');
      }) as typeof fetch,
    });
    await expect(hub.databases('mh_x')).rejects.toMatchObject({ name: 'MoulError', status: 0, code: 'network', message: 'Failed to fetch' });
    const plain = new HubClient('https://sync.moul.dev', { fetch: (async () => new Response('Bad gateway', { status: 502 })) as typeof fetch });
    await expect(plain.me('mh_x')).rejects.toMatchObject({ status: 502, message: 'Bad gateway' });
    expect(() => new HubClient('ftp://sync.moul.dev')).toThrow(TypeError);
    expect(() => new HubClient('not an address')).toThrow(TypeError);
    expect(() => new HubClient('https://user:pw@sync.moul.dev')).toThrow(TypeError);
  });
});

describe('hub join links', () => {
  const page = 'https://play.moul.dev/?db=notes';
  const ticket = { hub: 'https://sync.moul.dev', database: 'notes', ticket: 'tk_abc-123' };

  it('carry the hub, the database and a ticket, never a token', () => {
    const link = joinLink({ ...ticket, token: 'mh_secret', expiresAt: 5 } as any, page);
    expect(new URL(link).search).toBe('?db=notes');
    const payload = new URLSearchParams(new URL(link).hash.slice(1)).get('moul-join')!;
    const raw = atob(payload.replace(/-/g, '+').replace(/_/g, '/'));
    expect(JSON.parse(raw)).toEqual(ticket);
    expect(readJoinLink(link)).toEqual(ticket);
    expect(() => joinLink({ hub: ticket.hub, database: 'notes', ticket: '' }, page)).toThrow(TypeError);
  });

  it('accept only a well-formed hub, database and ticket', () => {
    // Made by hand: a link may come from anywhere, not only from joinLink.
    const link = (s: object) => {
      const url = new URL(page);
      url.hash = `moul-join=${btoa(JSON.stringify(s)).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '')}`;
      return url.toString();
    };
    expect(readJoinLink(link({ ...ticket, hub: 'https://sync.moul.dev/' }))).toEqual(ticket);
    expect(readJoinLink(link({ ...ticket, hub: 'http://localhost:8091' }))).toMatchObject({ hub: 'http://localhost:8091' });
    expect(readJoinLink(link({ ...ticket, hub: 'http://sync.example' }))).toBeNull();
    expect(readJoinLink(link({ ...ticket, hub: 'http://169.254.169.254' }))).toBeNull();
    expect(readJoinLink(link({ ...ticket, hub: 'javascript:alert(1)' }))).toBeNull();
    expect(readJoinLink(link({ ...ticket, hub: 'https://user:pw@evil.example' }))).toBeNull();
    expect(readJoinLink(link({ ...ticket, hub: 42 }))).toBeNull();
    for (const database of ['Notes', '-notes', 'a/b', '../x', 'a'.repeat(64), '', 7]) {
      expect(readJoinLink(link({ ...ticket, database })), String(database)).toBeNull();
    }
    for (const t of ['', 'has space', 'x'.repeat(513), 'line\nbreak', {}]) {
      expect(readJoinLink(link({ ...ticket, ticket: t })), String(t)).toBeNull();
    }
    // A hub link never reads as a bucket's, whatever else it carries.
    const both = readJoinLink(link({ ...ticket, endpoint: 'https://s3.example', bucket: 'b', accessKeyId: 'k', secretAccessKey: 's' }));
    expect(both).toEqual(ticket);
  });

  it('turn into settings by trading the ticket for a database token', async () => {
    const { fetch, calls } = fakeHub({
      'POST /api/hub/join': (c) =>
        c.body.ticket === 'tk_abc-123'
          ? Response.json({ token: 'mh_database', device: { ...device, database: 'notes' }, database: { name: 'notes', createdAt: 1, bytes: 0, head: null } })
          : Response.json({ message: 'the ticket was used or expired' }, { status: 400 }),
    });
    const settings = await joinHub(readJoinLink(joinLink(ticket, page)) as typeof ticket, { label: 'Phone', fetch });
    expect(settings).toEqual({ hub: 'https://sync.moul.dev', database: 'notes', token: 'mh_database', label: 'Phone', followOnly: true });
    expect(calls[0]).toMatchObject({ method: 'POST', url: 'https://sync.moul.dev/api/hub/join', body: { ticket: 'tk_abc-123', label: 'Phone' } });
    await expect(joinHub({ ...ticket, ticket: 'used' }, { fetch })).rejects.toMatchObject({ status: 400, message: 'the ticket was used or expired' });
  });
});

describe('sync through a hub', () => {
  it('configures with the hub settings, and makes a link from a ticket the engine asks for', async () => {
    const ops: [string, Record<string, unknown> | undefined][] = [];
    const t: Transport = {
      kind: 'local',
      name: 'fake',
      async api() {
        return { status: 200, headers: [] };
      },
      async op(op, fields) {
        ops.push([op, fields]);
        if (op === 'syncJoinTicket') return { hub: 'https://sync.moul.dev', database: 'notes', ticket: 'tk_9', expiresAt: 1 };
        return { configured: true, connected: true, role: 'writer', readOnly: false, position: 0, lastCommit: 0, settings: { hub: 'https://sync.moul.dev', database: 'notes' } };
      },
      onPush() {
        return () => {};
      },
      close() {},
    };
    const moul = new MoulClient(t, { authStorage: null });
    const status = await moul.sync.configure({ hub: 'https://sync.moul.dev', database: 'notes', token: 'mh_account', label: 'Laptop' });
    expect(ops[0]).toEqual(['syncConfigure', { hub: 'https://sync.moul.dev', database: 'notes', token: 'mh_account', label: 'Laptop' }]);
    expect(status.settings && 'hub' in status.settings ? status.settings.database : undefined).toBe('notes');
    const link = joinLink(await moul.sync.joinTicket(), 'https://play.moul.dev/');
    expect(ops[1][0]).toBe('syncJoinTicket');
    expect(readJoinLink(link)).toEqual({ hub: 'https://sync.moul.dev', database: 'notes', ticket: 'tk_9' });
    moul.close();
  });
});
