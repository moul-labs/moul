import { MoulError } from './errors.js';

/** A Moul Hub account. */
export interface HubAccount {
  id: string;
  email: string;
}

/** A device signed in to a hub: one token. */
export interface HubDevice {
  id: string;
  label: string;
  /** The one database a database token reaches; `null` for an account token. */
  database: string | null;
  /** In milliseconds since the epoch. */
  createdAt: number;
  /** In milliseconds since the epoch, or `null` if the device was never seen. */
  lastSeenAt: number | null;
  /** Whether this is the device whose token made the request. */
  current: boolean;
}

/** A replica's `moul/head.json`: the newest change and who pushed it. */
export interface HubHead {
  txid: number;
  /** The device that pushed it. */
  writer: string;
  generation: number;
  replica: string;
  /** In milliseconds since the epoch. */
  at: number;
}

/** A database on a hub. */
export interface HubDatabase {
  /** 1 to 63 of `a-z`, `0-9` and `-`, starting with a letter or digit. */
  name: string;
  /** In milliseconds since the epoch. */
  createdAt: number;
  bytes: number;
  /** `null` before the first push. */
  head: HubHead | null;
}

/** An account's limits, or what it uses of them. */
export interface HubAmounts {
  databases: number;
  bytes: number;
}

/** What `GET /api/hub/me` answers. */
export interface HubMe {
  account: HubAccount;
  device: HubDevice;
  limits: HubAmounts;
  usage: HubAmounts;
}

/** A signed-in device: an account token, which reaches every database of the account. */
export interface HubSession {
  token: string;
  device: HubDevice;
  account: HubAccount;
}

/** A device that joined from a link: a database token, which reaches that one database only. */
export interface HubJoined {
  token: string;
  device: HubDevice;
  /** The database the token reaches. */
  database: string;
}

/** A one-time ticket for one more device, valid 15 minutes. */
export interface HubJoinTicket {
  ticket: string;
  /** In milliseconds since the epoch. */
  expiresAt: number;
}

export interface HubClientOptions {
  /** The `fetch` to use; the global one by default. */
  fetch?: typeof fetch;
}

/** A database name a hub takes: 1 to 63 of `a-z`, `0-9` and `-`, starting with a letter or digit. */
export const HUB_DATABASE_NAME = /^[a-z0-9][a-z0-9-]{0,62}$/;

/** A time as the hub sends it (milliseconds, or an ISO date) in milliseconds. */
function ms(v: unknown): number {
  if (typeof v === 'number') return v;
  if (typeof v === 'string') {
    const n = /^\d+$/.test(v) ? Number(v) : Date.parse(v);
    return Number.isFinite(n) ? n : 0;
  }
  return 0;
}

function device(d: any): HubDevice {
  return { ...d, createdAt: ms(d?.createdAt), lastSeenAt: d?.lastSeenAt == null ? null : ms(d.lastSeenAt) };
}

function database(d: any): HubDatabase {
  return { ...d, createdAt: ms(d?.createdAt), head: d?.head ? { ...d.head, at: ms(d.head.at) } : null };
}

/**
 * Talks to a Moul Hub's account API: sign in with an email code, list and revoke devices, make and
 * delete databases, and make join tickets for more devices. Plain `fetch`, so it runs in any
 * JavaScript runtime. Syncing itself goes through `moul.sync.configure({ hub, database, token })`.
 *
 * ```ts
 * const hub = new HubClient('https://sync.moul.dev');
 * await hub.signIn('me@example.com');                  // a code arrives by mail
 * const { token } = await hub.verify('me@example.com', '123456', { label: 'Laptop' });
 * await hub.createDatabase(token, 'notes');
 * await moul.sync.configure({ hub: hub.url, database: 'notes', token, label: 'Laptop' });
 * ```
 *
 * Every failure is a `MoulError`: `status` is the hub's HTTP status (0 with `code: 'network'`
 * when it could not be reached) and `message` the hub's message.
 */
export class HubClient {
  /** The hub's address, without a trailing `/`. */
  readonly url: string;
  private readonly fetcher: typeof fetch;

  constructor(url: string | URL, options: HubClientOptions = {}) {
    let parsed: URL;
    try {
      parsed = new URL(String(url));
    } catch {
      throw new TypeError(`${String(url)} is not a hub address`);
    }
    if (!['https:', 'http:'].includes(parsed.protocol) || parsed.username || parsed.password) {
      throw new TypeError(`${String(url)} is not a plain http(s) address`);
    }
    this.url = (parsed.origin + parsed.pathname).replace(/\/+$/, '');
    this.fetcher = options.fetch ?? globalThis.fetch.bind(globalThis);
  }

  private async call<T>(method: string, path: string, options: { token?: string; json?: unknown } = {}): Promise<T> {
    const headers: Record<string, string> = { Accept: 'application/json' };
    if (options.token) headers.Authorization = `Bearer ${options.token}`;
    let body: string | undefined;
    if (options.json !== undefined) {
      body = JSON.stringify(options.json);
      headers['Content-Type'] = 'application/json';
    }
    let res: Response;
    try {
      res = await this.fetcher(`${this.url}/api/hub${path}`, { method, headers, body });
    } catch (err) {
      throw new MoulError(0, err instanceof Error ? err.message : String(err), undefined, 'network');
    }
    if (res.status === 204) return undefined as T;
    const text = await res.text().catch(() => '');
    let data: any;
    try {
      data = text ? JSON.parse(text) : undefined;
    } catch {
      data = { message: text };
    }
    if (!res.ok) {
      const message = typeof data?.message === 'string' && data.message ? data.message : `the hub answered ${res.status}`;
      throw new MoulError(res.status, message, data);
    }
    return data as T;
  }

  /**
   * Asks for a sign-in code by mail. The hub answers the same whether or not the email has an
   * account, so this says nothing about which do. `invite` is for a hub whose sign-up needs one.
   */
  async signIn(email: string, options: { invite?: string } = {}): Promise<void> {
    const json: Record<string, string> = { email };
    if (options.invite) json.invite = options.invite;
    await this.call('POST', '/signin', { json });
  }

  /** Trades the code from the mail for an account token: this device's, revocable. */
  async verify(email: string, code: string, options: { label?: string } = {}): Promise<HubSession> {
    const json: Record<string, string> = { email, code };
    if (options.label) json.label = options.label;
    const s = await this.call<any>('POST', '/verify', { json });
    return { ...s, device: device(s?.device) };
  }

  /**
   * Trades a join ticket (from a join link) for a database token: this device's own, reaching that
   * one database only. A ticket works once.
   */
  async join(ticket: string, options: { label?: string } = {}): Promise<HubJoined> {
    const json: Record<string, string> = { ticket };
    if (options.label) json.label = options.label;
    const j = await this.call<any>('POST', '/join', { json });
    const name = typeof j?.database === 'string' ? j.database : j?.database?.name;
    return { ...j, device: device(j?.device), database: name };
  }

  /** The account, this device, and the account's limits and usage. */
  async me(token: string): Promise<HubMe> {
    const m = await this.call<any>('GET', '/me', { token });
    return { ...m, device: device(m?.device) };
  }

  /** The account's devices. Account tokens only. */
  async devices(token: string): Promise<HubDevice[]> {
    const r = await this.call<{ items?: unknown[] }>('GET', '/devices', { token });
    return (r?.items ?? []).map(device);
  }

  /** Revokes a device: its token stops working at once. */
  async revokeDevice(token: string, id: string): Promise<void> {
    await this.call('DELETE', `/devices/${encodeURIComponent(id)}`, { token });
  }

  /** The databases the token reaches. */
  async databases(token: string): Promise<HubDatabase[]> {
    const r = await this.call<{ items?: unknown[] }>('GET', '/databases', { token });
    return (r?.items ?? []).map(database);
  }

  /** Makes a database. `409` if the account has one by that name, `403` over the account's limits. */
  async createDatabase(token: string, name: string): Promise<HubDatabase> {
    return database(await this.call('POST', '/databases', { token, json: { name } }));
  }

  /** Deletes a database and its replica. */
  async deleteDatabase(token: string, name: string): Promise<void> {
    await this.call('DELETE', `/databases/${encodeURIComponent(name)}`, { token });
  }

  /**
   * A one-time ticket that lets one more device join `name`, valid 15 minutes. Put it in a join
   * link with `joinLink({ hub: hub.url, database: name, ticket }, page)`.
   */
  async createJoinTicket(token: string, name: string): Promise<HubJoinTicket> {
    const t = await this.call<any>('POST', `/databases/${encodeURIComponent(name)}/joins`, { token });
    return { ...t, expiresAt: ms(t?.expiresAt) };
  }
}
