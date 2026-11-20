import { AuthClient, type AuthSession, AuthStore, samePerson, type StorageLike } from './auth.js';
import { CollectionClient, type FileRef, fileUrl, type SubscribeOptions } from './collection.js';
import { MoulError, toMoulError } from './errors.js';
import { HUB_DATABASE_NAME, HubClient } from './hub.js';
import { LiveQuery, type LiveRegistry, type LiveRun, type Reads, readsOf, report } from './live.js';
import type { Push, Transport } from './transport.js';
import type {
  AnySchema,
  BaseRecord,
  Change,
  ChangeNotice,
  CollectionModel,
  EngineRequest,
  EngineResponse,
  InboxMessage,
  MoulFile,
  RecordEvent,
  RecordModel,
} from './types.js';

/** A request from the client: an engine request, plus whether to leave the sign-in out. */
export interface ClientRequest extends EngineRequest {
  /** Send without the signed-in token, as a guest would (sign-in requests do this). */
  anonymous?: boolean;
}

/** Options for {@link MoulClient}. */
export interface MoulClientOptions {
  /**
   * Send every request as the database's owner, so rules do not apply. Studio does this. In a tab
   * the person at the keyboard owns the data; rules are there to build and test apps that others
   * will use once the database is served by a hub.
   */
  owner?: boolean;
  /** Where to keep the sign-in. `localStorage` in a browser, nowhere (`null`) elsewhere. */
  authStorage?: StorageLike | null;
  /** The storage key of the sign-in. `moul.auth.<name>` by default. */
  authKey?: string;
  /**
   * A schema file (`moul.schema.json`) to apply before the first request: collections are created
   * and changed to match it, and none are deleted. A device that follows another writer skips it.
   * The outcome is in {@link MoulClient.ready}.
   */
  schema?: object;
}

/** The parts several clients on one transport share. */
interface Shared {
  authStore: AuthStore;
  hub: LiveHub;
  realtime: Realtime;
  /** Requests wait for this: the schema file being applied. */
  gate: { promise: Promise<unknown> | undefined };
  /** Settles when the schema file is applied (at once without one). */
  ready: Promise<void>;
  /** Blob URLs handed out for files, by file URL. */
  objectUrls: Map<string, Promise<string>>;
}

const WRITE = new Set(['POST', 'PUT', 'PATCH', 'DELETE']);

/**
 * Whether the engine logs `method path` as a named request, and so applies it once however often
 * it is sent with one request id. Mirrors `Route::is_logged_write` in moul-core. Other writes
 * (sign-in, token refresh, sign-out) are not deduplicated and must not be sent twice.
 */
export function loggedWrite(method: string, path: string): boolean {
  const s = path.split('?')[0].split('/').filter(Boolean);
  if (s[0] !== 'api') return false;
  const m = method.toUpperCase();
  if (s[1] === 'moul') {
    if (s.length === 2) return m === 'POST';
    if (s.length === 3) return m === 'PATCH' || m === 'PUT' || m === 'DELETE';
    if (s.length === 4) {
      return (s[3] === 'records' && m === 'POST') || (s[3] === 'email-templates' && m === 'PUT') || (s[3] === 'import' && m === 'POST');
    }
    if (s.length === 5) {
      return (s[3] === 'records' && (m === 'PATCH' || m === 'DELETE')) || (s[3] === 'otp' && s[4] === 'request' && m === 'POST');
    }
    return false;
  }
  if (s[1] === 'upload' && s.length === 2) return m === 'POST';
  if ((s[1] === 'upload' || s[1] === 'files') && s.length > 2) return m === 'DELETE';
  if (s[1] === 'schema' && s.length === 2) return m === 'POST' || m === 'PUT';
  return false;
}

/**
 * Refuses a path with a `.` or `..` step. A URL resolves them: `/records/..` is the collection
 * itself, so a record with such an id would turn "delete this record" into "delete the
 * collection" on a server.
 */
function checkPath(path: string): void {
  const p = path.split('?')[0];
  // A URL parser reads `\\` as `/`, ends the path at `#` and drops tabs and line breaks, so each
  // could turn into a `..` step on a server; ids and names reach a path encoded and never hold them.
  if (/[\\#\u0000-\u001f\u007f]/.test(p)) {
    throw new MoulError(400, `a path may not hold '\\', '#' or control characters: ${JSON.stringify(path)}`);
  }
  for (const seg of p.split('/')) {
    let decoded = seg;
    try {
      decoded = decodeURIComponent(seg);
    } catch {
      // Not an escape sequence the URL parser would read either.
    }
    if (decoded === '.' || decoded === '..') {
      throw new MoulError(400, `a path may not have a '.' or '..' step: ${path}`);
    }
  }
}

function randomId(length: number): string {
  const alphabet = 'abcdefghijklmnopqrstuvwxyz0123456789';
  const bytes = new Uint8Array(length);
  globalThis.crypto.getRandomValues(bytes);
  return Array.from(bytes, (b) => alphabet[b % alphabet.length]).join('');
}

function defaultStorage(): StorageLike | null {
  try {
    const s = (globalThis as any).localStorage as StorageLike | undefined;
    return s ?? null;
  } catch {
    return null;
  }
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/**
 * A connection to one Moul database: records, sign-in, files, live queries and, when the engine runs
 * in this page, the database file and sync. Make one with `createClient`.
 *
 * `S` maps collection names to record types, such as the `MoulSchema` that `moul typegen` writes:
 * `createClient<MoulSchema>({ db: 'notes' })`.
 */
export class MoulClient<S extends object = AnySchema> {
  /** How requests reach the engine. */
  readonly transport: Transport;
  /** Whether requests go as the database's owner. */
  readonly owner: boolean;
  /** The sign-in this client sends. */
  readonly authStore: AuthStore;
  /** Uploads and files. */
  readonly files: FilesClient;
  /** The owner's tools: collections, the schema file, types, the change log, the local inbox, rules. */
  readonly admin: AdminClient;
  /** The database file, when the engine runs in this page. */
  readonly database: DatabaseClient;
  /** Sync with a bucket, when the engine runs in this page. */
  readonly sync: SyncClient;
  /** @internal */
  readonly realtime: Realtime;
  private readonly hub: LiveHub;
  private readonly gate: Shared['gate'];
  private readonly shared: Shared;
  /** Made by `asOwner()`: it shares its parent's connection and does not close it. */
  private readonly derived: boolean;
  private readonly prefix = `c${randomId(10)}`;
  private counter = 0;
  /** Stops waiting to apply the schema file again, when the database had not opened. */
  private schemaRetry: (() => void) | undefined;
  /**
   * Settles once the client is ready: at once, or after the schema file given in the options is
   * applied. Rejects if applying it failed; requests still run. If it failed because the database
   * would not open, the file is applied again once it opens.
   */
  readonly ready: Promise<void>;

  constructor(transport: Transport, options: MoulClientOptions = {}, shared?: Shared) {
    this.transport = transport;
    this.owner = options.owner ?? false;
    this.authStore =
      shared?.authStore ??
      new AuthStore(
        options.authStorage === undefined ? defaultStorage() : options.authStorage,
        options.authKey ?? `moul.auth.${transport.name}`,
      );
    this.derived = shared !== undefined;
    this.gate = shared?.gate ?? { promise: undefined };
    this.hub = shared?.hub ?? new LiveHub(this);
    this.realtime = shared?.realtime ?? new Realtime(this, this.gate);
    this.files = new FilesClient(this, shared?.objectUrls ?? new Map());
    this.admin = new AdminClient(this);
    this.database = new DatabaseClient(this);
    this.sync = new SyncClient(this);
    if (options.schema) {
      const schema = options.schema;
      const apply = () =>
        this.sendNow({ method: 'POST', path: '/api/schema', json: schema, owner: true }).then((res) => {
          // A follower cannot write: the writer's definitions arrive with its data.
          if (res.status === 409) return;
          this.unwrap(res);
        });
      const applying = apply();
      this.gate.promise = applying.catch(() => {});
      this.ready = applying;
      this.ready.catch((err) => {
        // The database did not open: apply the file again whenever a tab says it leads or has
        // opened the database, until it applies (`ready` stays rejected).
        if (toMoulError(err).code !== 'engine' || transport.kind !== 'local') return;
        let running = false;
        let missed = false;
        const finish = () => {
          stop();
          this.schemaRetry = undefined;
        };
        const attempt = () => {
          running = true;
          missed = false;
          const applied = apply();
          this.gate.promise = applied.catch(() => {});
          applied.then(finish, (e) => {
            running = false;
            const code = toMoulError(e).code;
            if (code === 'engine') {
              // Still not open. A notice that came meanwhile may be the one saying it is now.
              if (missed) attempt();
              return;
            }
            finish();
            if (code !== 'closed') report(e);
          });
        };
        const stop = transport.onPush((p) => {
          if (p.type !== 'leader') return;
          if (running) missed = true;
          else attempt();
        });
        this.schemaRetry = stop;
      });
    } else {
      this.ready = shared?.ready ?? Promise.resolve();
    }
    this.shared = {
      authStore: this.authStore,
      hub: this.hub,
      realtime: this.realtime,
      gate: this.gate,
      ready: this.ready,
      objectUrls: this.files.objectUrls,
    };
  }

  /**
   * A client on the same connection, sign-in and schema gate that sends requests as the owner.
   * Closing it does nothing: close the client it came from.
   */
  asOwner(): MoulClient<S> {
    if (this.owner) return this;
    return new MoulClient<S>(this.transport, { owner: true }, this.shared);
  }

  /** Records of collection `name`. */
  collection<K extends keyof S & string>(name: K): CollectionClient<S[K] extends BaseRecord ? S[K] : RecordModel>;
  collection<T extends BaseRecord = RecordModel>(name: string): CollectionClient<T>;
  collection(name: string): CollectionClient<any> {
    return new CollectionClient(this, name);
  }

  /** Sign-in for auth collection `collection` (`users` by default). */
  auth<K extends keyof S & string>(collection: K): AuthClient<S[K] extends RecordModel ? S[K] : RecordModel>;
  auth(collection?: string): AuthClient;
  auth(collection = 'users'): AuthClient<any> {
    return new AuthClient(this, collection);
  }

  /** The signed-in session, or `null`. */
  get session(): AuthSession | null {
    return this.authStore.session;
  }

  /** The signed-in record, or `null`. */
  get user(): RecordModel | null {
    return this.authStore.record;
  }

  /** Calls `listener` when someone signs in or out, in this tab or another. */
  onAuthChange(listener: (session: AuthSession | null) => void): () => void {
    return this.authStore.onChange(listener);
  }

  /** Calls `listener` after every commit, with the change-log rows it added. Local engines only. */
  onChange(listener: (notice: ChangeNotice) => void): () => void {
    return this.transport.onPush((p) => {
      if (p.type === 'changes') listener(p);
    });
  }

  /** Calls `listener` when the page asked the browser to keep this site's storage. */
  onPersist(listener: (persisted: boolean) => void): () => void {
    return this.transport.onPush((p) => {
      if (p.type === 'persisted') listener(p.persisted);
    });
  }

  /** The request as it is sent: owner, sign-in, and a request id for writes. */
  private prepare(req: ClientRequest): EngineRequest {
    const { anonymous, ...out } = req;
    const headers = { ...(out.headers ?? {}) };
    const owner = out.owner ?? this.owner;
    const token = this.authStore.token;
    if (!anonymous && !owner && token && !Object.keys(headers).some((k) => k.toLowerCase() === 'authorization')) {
      headers.Authorization = `Bearer ${token}`;
    }
    const method = (out.method ?? 'GET').toUpperCase();
    checkPath(out.path);
    // A logged write with an id can be sent again safely. The engine takes ids only from the
    // owner and signed-in callers.
    const signedIn = !anonymous && (owner || Object.keys(headers).some((k) => k.toLowerCase() === 'authorization'));
    const logged = WRITE.has(method) && loggedWrite(method, out.path);
    const id = out.id ?? (logged && signedIn ? `${this.prefix}:${++this.counter}` : undefined);
    return { ...out, method, headers, owner: owner || undefined, id };
  }

  /**
   * Sends a request and returns the engine's answer, whatever its status. A read, or a logged
   * write with a request id, is sent again if the tab holding the database closed before
   * answering; the engine applies such a write once.
   */
  async send(req: ClientRequest): Promise<EngineResponse> {
    if (this.gate.promise) await this.gate.promise;
    return this.sendNow(req);
  }

  /** {@link send} without waiting for the schema file. */
  private async sendNow(req: ClientRequest): Promise<EngineResponse> {
    const prepared = this.prepare(req);
    const method = prepared.method ?? 'GET';
    const retry = method === 'GET' || method === 'HEAD' || (prepared.id !== undefined && loggedWrite(method, prepared.path));
    for (let attempt = 0; ; attempt++) {
      try {
        const res = await this.transport.api(prepared);
        // A remote server pushes no change notices: this client's own writes refresh its live
        // queries directly.
        if (this.transport.kind === 'remote' && WRITE.has(method) && res.status < 400) this.hub.wrote(prepared.path);
        return res;
      } catch (err) {
        const e = toMoulError(err);
        if (e.code === 'leader-lost' && retry && attempt < 5) {
          await sleep(50 * 2 ** attempt);
          continue;
        }
        throw e;
      }
    }
  }

  /** The body of a response, or a `MoulError` for a status of 400 or more. */
  unwrap<T>(res: EngineResponse): T {
    if (res.status >= 400) {
      const data = res.json;
      throw new MoulError(res.status, data?.message || data?.error || `the request failed with status ${res.status}`, data);
    }
    return (res.json !== undefined ? res.json : res.bytes) as T;
  }

  /** Sends a request and returns its body: JSON, or bytes for a file. Fails with a `MoulError`. */
  async request<T = any>(req: ClientRequest): Promise<T> {
    return this.unwrap<T>(await this.send(req));
  }

  /** A query that stays current. `run` returns the data and what it read. */
  live<T>(run: LiveRun<T>): LiveQuery<T> {
    return new LiveQuery<T>(this.hub, run, { owner: this.owner });
  }

  /** A `GET` that stays current. `name` is the collection to follow on servers that do not report reads. */
  liveRequest<T = any>(req: ClientRequest, name?: string): LiveQuery<T> {
    return this.live(async () => {
      const res = await this.send(req);
      return { data: this.unwrap<T>(res), reads: readsOf(res, name) };
    });
  }

  /** What a read of collection `name` depends on, for a record that is not there (yet). */
  async collectionReads(name: string): Promise<Reads> {
    const res = await this.send({ path: `/api/moul/${encodeURIComponent(name)}/records`, query: { perPage: 1 } });
    return res.status < 400 ? readsOf(res, name) : { all: true };
  }

  /**
   * Stops the client and its engine connection. On a client made by `asOwner()` it does nothing:
   * close the client it came from.
   */
  close(): void {
    if (this.derived) return;
    this.schemaRetry?.();
    this.hub.close();
    this.realtime.close();
    this.files.releaseAll();
    this.authStore.close();
    this.transport.close();
  }
}

/** The collection a request path names, if any: `/api/moul/<name>/…`. */
function collectionOfPath(path: string): string | undefined {
  const s = path.split('?')[0].split('/').filter(Boolean);
  return s[0] === 'api' && s[1] === 'moul' && s[2] ? decodeURIComponent(s[2]) : undefined;
}

/** Keeps live queries current from the engine's notices, or a remote server's record events. */
class LiveHub implements LiveRegistry {
  private readonly queries = new Set<LiveQuery<any>>();
  private readonly stops: (() => void)[] = [];
  /**
   * Remote only: one record subscription per collection the live queries read (`*` for all), per
   * owner flag, keyed `owner|name`.
   */
  private readonly watched = new Map<string, { stop: (() => void) | undefined; closed: boolean }>();
  /** Counts changes of the signed-in person. */
  private people = 0;
  private readonly client: MoulClient<any>;

  constructor(client: MoulClient<any>) {
    this.client = client;
    this.stops.push(
      client.transport.onPush((p: Push) => {
        if (p.type === 'changes') for (const q of this.queries) q.notify(p);
        // A new leader may hold newer data (it reopened the files).
        else if (p.type === 'leader') for (const q of this.queries) q.notify('all');
      }),
      client.authStore.onChange((session, previous) => {
        if (samePerson(session, previous)) {
          // A refreshed token for the same person: read again, keep what is shown.
          for (const q of this.queries) q.notify('all');
        } else {
          // Someone else: drop what the previous person saw, everywhere.
          this.people++;
          for (const q of this.queries) q.notify('auth');
        }
        // Streams a server ended (an expired sign-in) are made again with the new credentials.
        this.follow();
      }),
    );
  }

  generation(): number {
    return this.people;
  }

  add(query: LiveQuery<any>): void {
    this.queries.add(query);
    this.follow();
  }

  delete(query: LiveQuery<any>): void {
    this.queries.delete(query);
    this.follow();
  }

  readsChanged(): void {
    this.follow();
  }

  /** A write this client made on a remote server succeeded. */
  wrote(path: string): void {
    const name = collectionOfPath(path);
    const s = path.split('?')[0].split('/').filter(Boolean);
    // Definitions (`/api/moul`, `/api/moul/<name>`, `/api/schema`) can change any read.
    const definition = (s[1] === 'moul' && s.length <= 3) || s[1] === 'schema';
    for (const q of this.queries) {
      if (definition || !name) q.notify('all');
      else q.notifyName(name);
    }
  }

  private notifyFollowed(name: string): void {
    for (const q of this.queries) {
      if (name === '*') q.notify('all');
      else q.notifyName(name);
    }
  }

  /**
   * A remote server pushes no notices: follow the collections the live queries read through
   * record subscriptions instead, as the owner for the owner's queries.
   */
  private follow(): void {
    if (this.client.transport.kind !== 'remote') return;
    const want = new Map<string, { name: string; owner: boolean }>();
    for (const q of this.queries) {
      const r = q.reads;
      if (!r) continue;
      // A read that reached other collections (expanded or filtered relations) has ids the client
      // cannot name: follow every collection.
      const names = r.all || r.ids.size > r.names.size ? ['*'] : [...r.names];
      for (const name of names) want.set(`${q.owner ? 1 : 0}|${name}`, { name, owner: q.owner });
    }
    for (const [key, w] of this.watched) {
      if (!want.has(key)) {
        w.closed = true;
        w.stop?.();
        this.watched.delete(key);
      }
    }
    for (const [key, { name, owner }] of want) {
      if (this.watched.has(key)) continue;
      const entry: { stop: (() => void) | undefined; closed: boolean } = { stop: undefined, closed: false };
      this.watched.set(key, entry);
      this.client.realtime
        .subscribe(name, () => this.notifyFollowed(name), {
          // Writes before the stream connected, or during a break, sent no event: read again.
          onOpen: () => this.notifyFollowed(name),
          onClose: () => {
            if (this.watched.get(key) === entry) this.watched.delete(key);
          },
        }, owner)
        .then(
          (stop) => {
            if (entry.closed) stop();
            else entry.stop = stop;
          },
          () => {
            if (this.watched.get(key) === entry) this.watched.delete(key);
          },
        );
    }
  }

  close(): void {
    for (const s of this.stops) s();
    for (const w of this.watched.values()) {
      w.closed = true;
      w.stop?.();
    }
    this.watched.clear();
    this.queries.clear();
  }
}

interface Sub {
  collection: string;
  listener: (event: RecordEvent) => void;
  options: SubscribeOptions & { onOpen?: () => void };
  /** Sent as the owner. */
  owner: boolean;
  /** The engine's id, while subscribed. */
  id: number | undefined;
  /** The leading tab that holds the engine subscription, when the engine says. */
  leader: string | undefined;
  /** The sign-in token it was opened with (null for none), once open. */
  token: string | null | undefined;
  /** A remote stream, while open. */
  stop: (() => void) | undefined;
  /** The subscriber stopped it. */
  closed: boolean;
  /** The engine ended it. */
  ended: boolean;
  /** Opens and closes run one after another, so they cannot race. */
  chain: Promise<void>;
  /** Records the subscriber knows of: its `visible` ids, then what events told it. Sent on each open. */
  seen: Set<string>;
}

/** Record subscriptions, kept across leader changes and sign-ins. */
class Realtime {
  private readonly subs = new Set<Sub>();
  private readonly byId = new Map<number, Sub>();
  private readonly stops: (() => void)[] = [];
  /** Counts leader changes, for engines that do not name the leader in their answers. */
  private epoch = 0;
  /** The leading tab, as last announced, and the ones that led before it. */
  private leader: string | undefined;
  private readonly retired = new Set<string>();
  /**
   * A `closed` that arrived before the answer naming its id (the engine ended a subscription it had
   * just made, say after its data was replaced): applied when that answer comes.
   */
  private readonly closedEarly = new Map<number, string>();
  private closed = false;
  private readonly client: MoulClient<any>;
  private readonly gate: Shared['gate'];

  constructor(client: MoulClient<any>, gate: Shared['gate']) {
    this.client = client;
    this.gate = gate;
    this.stops.push(
      client.transport.onPush((p) => {
        if (p.type === 'event') {
          const sub = this.byId.get(p.id);
          if (sub && !sub.closed && !sub.ended) {
            const rid = (p.event.record as { id?: unknown } | undefined)?.id;
            // Without deletes the set could only grow, and would tell the engine nothing.
            const hearsDeletes = !sub.options.events?.length || sub.options.events.includes('delete');
            if (typeof rid === 'string' && hearsDeletes) {
              if (p.event.action === 'delete') sub.seen.delete(rid);
              else sub.seen.add(rid);
            }
            try {
              sub.listener(p.event);
            } catch (err) {
              report(err);
            }
          }
        } else if (p.type === 'closed') {
          const sub = this.byId.get(p.id);
          if (sub) this.end(sub, p.reason);
          else {
            this.closedEarly.set(p.id, p.reason);
            // Keep only recent ones: an id the engine reuses later is a new subscription.
            if (this.closedEarly.size > 64) this.closedEarly.delete(this.closedEarly.keys().next().value!);
          }
        } else if (p.type === 'leader') {
          this.newLeader(p.tab, p.opened === true);
        }
      }),
      client.authStore.onChange((session, previous) => {
        const same = samePerson(session, previous);
        for (const s of this.subs) {
          // The owner's subscriptions do not depend on the sign-in.
          if (s.owner) continue;
          void this.queue(s, async () => {
            // Opened (or made) after this sign-in already: nothing to change.
            if (s.token === undefined || s.token === this.client.authStore.token) return;
            if (same) {
              // A refreshed token: make the new subscription before ending the old one, so no
              // event falls between them.
              const old = s.id;
              const oldLeader = s.leader;
              const epoch = this.epoch;
              if (old !== undefined) this.byId.delete(old);
              const oldStop = s.stop;
              s.id = undefined;
              s.leader = undefined;
              s.stop = undefined;
              try {
                await this.open(s);
              } finally {
                oldStop?.();
                // Ids are the issuing leader's: another leader may have given the same number to a
                // different subscription, and the old leader's engine is gone with its ones.
                const sameLeader = oldLeader !== undefined ? oldLeader === this.leader && !this.retired.has(oldLeader) : epoch === this.epoch;
                if (old !== undefined && sameLeader) await this.client.transport.op('unsubscribe', { id: old }).catch(() => {});
              }
            } else {
              await this.close1(s);
              await this.open(s);
            }
          }).catch((err) => this.failed(s, err));
        }
      }),
    );
  }

  /**
   * Another tab leads now: its engine has none of the subscriptions the last leader held. Or, with
   * `opened`, the same tab opened the database after answering with an error: make again what it
   * refused.
   */
  private newLeader(tab: string, opened = false): void {
    if (tab === this.leader) {
      if (opened) for (const s of this.subs) if (s.id === undefined) this.reopen(s);
      return;
    }
    this.closedEarly.clear();
    const first = this.leader === undefined;
    if (this.leader !== undefined) this.retired.add(this.leader);
    this.leader = tab;
    this.epoch++;
    // Requests sent before any leader was known wait for this one: nothing to make again.
    if (first && !opened) return;
    for (const s of this.subs) {
      // Answered by the new leader already, before this news arrived.
      if (s.leader === tab) continue;
      if (s.id !== undefined) this.byId.delete(s.id);
      s.id = undefined;
      s.leader = undefined;
      this.reopen(s);
    }
  }

  /** Makes `sub` again on the current leader. */
  private reopen(sub: Sub): void {
    void this.queue(sub, () => this.open(sub)).catch((err) => this.failed(sub, err));
  }

  /**
   * Making `sub` again failed. If the engine refused it (a rule, a deleted collection), it is over:
   * `onClose` says why. If no engine answered (the database would not open, the leader went away),
   * it waits: the next leader, or the same one once it opens the database, makes it again.
   */
  private failed(sub: Sub, err: unknown): void {
    const e = toMoulError(err);
    if (!sub.closed && e.status > 0) this.end(sub, e.message);
  }

  /** Runs `op` after the subscription's earlier opens and closes; skipped once it is over. */
  private queue(sub: Sub, op: () => Promise<void>, always = false): Promise<void> {
    const next = sub.chain.then(() => (always || (!sub.closed && !sub.ended) ? op() : undefined));
    sub.chain = next.catch(() => {});
    return next;
  }

  /** The engine (or a remote server) ended `sub`. */
  private end(sub: Sub, reason: string): void {
    if (sub.ended) return;
    sub.ended = true;
    if (sub.id !== undefined) this.byId.delete(sub.id);
    sub.id = undefined;
    this.subs.delete(sub);
    // The subscriber stopped it already: nobody is listening for why it ended.
    if (sub.closed) return;
    try {
      sub.options.onClose?.(reason);
    } catch (err) {
      report(err);
    }
  }

  private headers(): Record<string, string> {
    const token = this.client.authStore.token;
    return token ? { Authorization: `Bearer ${token}` } : {};
  }

  private async open(sub: Sub): Promise<void> {
    if (sub.closed || sub.ended) return;
    if (this.gate.promise) await this.gate.promise;
    if (sub.closed || sub.ended) return;
    const transport = this.client.transport;
    if (transport.kind === 'remote' && transport.stream) {
      if (sub.stop) return;
      sub.token = this.client.authStore.token;
      let opened = 0;
      sub.stop = transport.stream({
        collection: sub.collection,
        owner: sub.owner,
        headers: this.headers(),
        onEvent: (event) => {
          if (sub.closed || sub.ended) return;
          const want = sub.options;
          if (want.record && (event.record as any)?.id !== want.record) return;
          if (want.events?.length && !want.events.includes(event.action)) return;
          try {
            sub.listener(event);
          } catch (err) {
            report(err);
          }
        },
        onOpen: () => {
          try {
            sub.options.onOpen?.();
            if (opened++ > 0) sub.options.onReconnect?.();
          } catch (err) {
            report(err);
          }
        },
        onError: () => {},
        onClose: (reason) => this.end(sub, reason),
      });
      return;
    }
    if (sub.id !== undefined) return;
    const token = this.client.authStore.token;
    let answer: any;
    let epoch = this.epoch;
    for (let attempt = 0; ; attempt++) {
      // Read just before sending: a retry after a lost leader goes to the new one.
      epoch = this.epoch;
      try {
        answer = await transport.op('subscribe', {
          collection: sub.collection,
          record: sub.options.record,
          events: sub.options.events,
          headers: Object.entries(sub.owner ? {} : this.headers()),
          owner: sub.owner,
          visible: sub.seen.size > 0 ? [...sub.seen] : undefined,
        });
        break;
      } catch (err) {
        const e = toMoulError(err);
        if (e.code === 'leader-lost' && attempt < 5) {
          await sleep(50 * 2 ** attempt);
          continue;
        }
        throw e;
      }
    }
    if (!answer?.ok) {
      const data = answer?.json;
      throw new MoulError(answer?.status ?? 0, data?.message || 'the subscription was refused', data);
    }
    const leader: string | undefined = typeof answer.leader === 'string' ? answer.leader : undefined;
    // Made on a leader that has since gone: the reopen queued for the new leader makes it again.
    const stale = leader !== undefined ? this.retired.has(leader) : epoch !== this.epoch;
    if (stale) return;
    if (leader !== undefined && this.leader === undefined) this.leader = leader;
    if (sub.closed || sub.ended) {
      void transport.op('unsubscribe', { id: answer.id }).catch(() => {});
      return;
    }
    sub.id = answer.id;
    sub.leader = leader;
    sub.token = token;
    this.byId.set(answer.id, sub);
    const early = this.closedEarly.get(answer.id);
    if (early !== undefined) {
      this.closedEarly.delete(answer.id);
      this.end(sub, early);
    }
  }

  private async close1(sub: Sub): Promise<void> {
    sub.stop?.();
    sub.stop = undefined;
    if (sub.id !== undefined) {
      this.byId.delete(sub.id);
      const id = sub.id;
      sub.id = undefined;
      sub.leader = undefined;
      await this.client.transport.op('unsubscribe', { id }).catch(() => {});
    }
  }

  /** Subscribes `listener` to `collection`'s record events, as the owner when `owner` is set. */
  async subscribe(
    collection: string,
    listener: (event: RecordEvent) => void,
    options: SubscribeOptions & { onOpen?: () => void } = {},
    owner = this.client.owner,
  ): Promise<() => void> {
    if (this.closed) throw new MoulError(0, 'the client was closed', undefined, 'closed');
    const sub: Sub = {
      collection,
      listener,
      options,
      owner,
      id: undefined,
      leader: undefined,
      token: undefined,
      stop: undefined,
      closed: false,
      ended: false,
      chain: Promise.resolve(),
      seen: new Set(options.visible ?? []),
    };
    const stop = () => {
      if (sub.closed) return;
      sub.closed = true;
      this.subs.delete(sub);
      void this.queue(sub, () => this.close1(sub), true);
    };
    if (options.signal?.aborted) return () => {};
    options.signal?.addEventListener('abort', stop, { once: true });
    // In the set before it opens, so a sign-in or a new leader meanwhile reaches it.
    this.subs.add(sub);
    try {
      await this.queue(sub, () => this.open(sub));
    } catch (err) {
      stop();
      throw err;
    }
    return stop;
  }

  close(): void {
    this.closed = true;
    for (const s of this.stops) s();
    for (const s of [...this.subs]) {
      s.closed = true;
      void this.queue(s, () => this.close1(s), true);
    }
    this.subs.clear();
  }
}

/** Media types a blob URL may keep: images, sound, video, PDF and plain text. Not SVG or HTML. */
const SAFE_TYPES = /^(image\/(png|jpeg|gif|webp|avif|bmp|x-icon|vnd\.microsoft\.icon)|audio\/[\w.+-]+|video\/[\w.+-]+|application\/pdf|text\/plain)(;|$)/i;

/** Uploads and the files records hold. */
export class FilesClient {
  /** @internal Shared by a client and the clients `asOwner()` makes from it. */
  readonly objectUrls: Map<string, Promise<string>>;
  private readonly client: MoulClient<any>;

  constructor(client: MoulClient<any>, objectUrls: Map<string, Promise<string>> = new Map()) {
    this.client = client;
    this.objectUrls = objectUrls;
  }

  /** @internal Frees every blob URL this client handed out (on close). */
  releaseAll(): void {
    for (const p of this.objectUrls.values()) void p.then((u) => URL.revokeObjectURL(u), () => {});
    this.objectUrls.clear();
  }

  /** Uploads a file and returns its reference, ready to store in a file field. */
  async upload(file: Blob, name?: string): Promise<MoulFile> {
    const data = new Uint8Array(await file.arrayBuffer());
    const info = await this.client.request<MoulFile[]>({
      method: 'POST',
      path: '/api/upload',
      form: { files: [['file', { name: name ?? ((file as File).name || 'file'), type: file.type || 'application/octet-stream', data }]] },
    });
    const first = Array.isArray(info) ? info[0] : (info as any);
    if (!first?.id) throw new MoulError(0, 'the upload answered without a file');
    return first;
  }

  /** Replaces `Blob` values in record data with uploaded file references. */
  async resolve(data: Record<string, unknown>): Promise<Record<string, unknown>> {
    const out: Record<string, unknown> = {};
    for (const [k, v] of Object.entries(data)) {
      if (typeof Blob !== 'undefined' && v instanceof Blob) out[k] = await this.upload(v);
      else if (Array.isArray(v) && v.length && typeof Blob !== 'undefined' && v.every((x) => x instanceof Blob)) {
        out[k] = await Promise.all(v.map((b) => this.upload(b)));
      } else out[k] = v;
    }
    return out;
  }

  /** The files uploaded so far. Owner only. */
  list(): Promise<MoulFile[]> {
    return this.client.request({ path: '/api/files', owner: true });
  }

  /**
   * Deletes an uploaded file. Only the owner may, so this always sends as the owner (in a tab, the
   * person at the keyboard; a server needs the client's admin key).
   */
  async delete(file: MoulFile | string): Promise<void> {
    const id = typeof file === 'string' ? file : file.id;
    await this.client.request({ method: 'DELETE', path: `/api/files/${encodeURIComponent(id)}`, owner: true });
  }

  /** A file's bytes. */
  async fetch(ref: FileRef): Promise<Blob> {
    let raw = fileUrl(ref);
    if (/^https?:\/\//i.test(raw)) {
      // A full URL (as `url()` gives for a remote server) must be one of this server's.
      const base = this.client.transport.kind === 'remote' ? this.client.transport.name : '';
      if (!base || !raw.startsWith(`${base}/`)) throw new MoulError(400, `not a file of this server: ${raw}`);
      raw = raw.slice(base.length);
    }
    const url = new URL(raw, 'http://moul.invalid');
    const res = await this.client.send({ path: url.pathname, query: url.search });
    if (res.status >= 400) this.client.unwrap(res);
    return new Blob([(res.bytes ?? new Uint8Array()) as BlobPart], { type: res.type || 'application/octet-stream' });
  }

  /**
   * A URL an `<img>` or `<a>` can use. A local engine has no address, so this is a blob URL, kept
   * until {@link release}; a remote server's file URL is returned as it is. Blob URLs of types
   * that could run script (HTML, SVG) are served as `application/octet-stream`, so they download
   * instead of opening.
   */
  async url(ref: FileRef): Promise<string> {
    const raw = fileUrl(ref);
    if (this.client.transport.kind === 'remote') {
      // The server's base URL may have a path (`https://example.com/moul`): keep it.
      if (/^https?:\/\//i.test(raw)) return raw;
      return `${this.client.transport.name}${raw.startsWith('/') ? '' : '/'}${raw}`;
    }
    let p = this.objectUrls.get(raw);
    if (!p) {
      // A blob URL runs in the page's own origin, so only types that cannot run script keep theirs:
      // an HTML or SVG upload opened from it would otherwise act as the app.
      p = this.fetch(raw).then((b) => URL.createObjectURL(SAFE_TYPES.test(b.type) ? b : new Blob([b], { type: 'application/octet-stream' })));
      this.objectUrls.set(raw, p);
      p.catch(() => this.objectUrls.delete(raw));
    }
    return p;
  }

  /** Frees a blob URL made by {@link url}. */
  async release(ref: FileRef): Promise<void> {
    const raw = fileUrl(ref);
    const p = this.objectUrls.get(raw);
    if (!p) return;
    this.objectUrls.delete(raw);
    URL.revokeObjectURL(await p);
  }
}

/** A rule test, as `POST /api/rules/test` answers it. */
export interface RuleTest {
  valid: boolean;
  matched: boolean;
  translated: string;
  error?: string;
}

/** The owner's tools. Every request goes as the owner. */
export class AdminClient {
  constructor(private readonly client: MoulClient<any>) {}

  private req<T>(req: ClientRequest): Promise<T> {
    return this.client.request<T>({ ...req, owner: true });
  }

  /** Every collection. */
  listCollections(): Promise<CollectionModel[]> {
    return this.req({ path: '/api/moul' });
  }

  /** Every collection, kept current. */
  liveCollections(): LiveQuery<CollectionModel[]> {
    return this.client.liveRequest({ path: '/api/moul', owner: true });
  }

  /** One collection. */
  getCollection(name: string): Promise<CollectionModel> {
    return this.req({ path: `/api/moul/${encodeURIComponent(name)}` });
  }

  /** Creates a collection. */
  createCollection(definition: Partial<CollectionModel> & { name: string }): Promise<CollectionModel> {
    return this.req({ method: 'POST', path: '/api/moul', json: definition });
  }

  /** Changes a collection: its name, fields (matched by id) or rules. */
  updateCollection(name: string, changes: Partial<CollectionModel>): Promise<CollectionModel> {
    return this.req({ method: 'PATCH', path: `/api/moul/${encodeURIComponent(name)}`, json: changes });
  }

  /** Deletes a collection and its records. */
  async deleteCollection(name: string): Promise<void> {
    await this.req({ method: 'DELETE', path: `/api/moul/${encodeURIComponent(name)}` });
  }

  /** The schema file: every collection, its fields and rules, for git, review and agents. */
  schema(): Promise<{ version?: number; collections: CollectionModel[]; [key: string]: unknown }> {
    return this.req({ path: '/api/schema' });
  }

  /**
   * Applies a schema file: creates and changes collections to match. With `prune`, deletes the
   * collections it does not list.
   */
  applySchema(schema: object, options: { prune?: boolean } = {}): Promise<any> {
    return this.req({ method: 'POST', path: '/api/schema', query: { prune: options.prune ? 'true' : undefined }, json: schema });
  }

  /** TypeScript types for the schema, as `moul typegen` writes them. */
  async types(): Promise<string> {
    const r = await this.client.send({ path: '/api/types', owner: true });
    const body = this.client.unwrap<any>(r);
    if (typeof body === 'string') return body;
    if (r.bytes) return new TextDecoder().decode(r.bytes);
    return body?.types ?? String(body);
  }

  /** The change log after position `after`, or with `tail`, its newest rows (oldest first). */
  changes(options: { after?: number; limit?: number; tail?: number } = {}): Promise<{ items: Change[]; last: number }> {
    return this.req({ path: '/api/changes', query: { after: options.after, limit: options.limit, tail: options.tail } });
  }

  /**
   * Mail the engine kept instead of sending: sign-in codes and reset links. A tab cannot send
   * mail, so Studio shows these.
   */
  async inbox(options: { to?: string; collection?: string } = {}): Promise<InboxMessage[]> {
    const r = await this.req<{ items: InboxMessage[] }>({ path: '/api/inbox', query: options });
    return r.items;
  }

  /** The local inbox, kept current. */
  liveInbox(options: { to?: string; collection?: string } = {}): LiveQuery<InboxMessage[]> {
    return this.client.live(async () => {
      const res = await this.client.send({ path: '/api/inbox', query: options, owner: true });
      // Codes are written to the writer-only store, which the change log does not describe.
      return { data: this.client.unwrap<{ items: InboxMessage[] }>(res).items, reads: { all: true } };
    });
  }

  /** Evaluates a rule against a sample record, signed-in person and request. */
  testRule(test: { rule: string; record?: object; auth?: object | null; request?: object }): Promise<RuleTest> {
    return this.req({ method: 'POST', path: '/api/rules/test', json: test });
  }
}

/** Where the database stands in this browser. */
export interface DatabaseStatus {
  /** Whether the data survives the tab closing: false in a private window or without OPFS. */
  durable: boolean;
  /** Why it is not durable. */
  note: string | null;
  lastCommit: number;
  /** The device follows another writer through sync, so it cannot write. */
  readOnly: boolean;
  role: 'writer' | 'follower';
  /** This device's id. */
  device: string;
  /** Whether sync is set up. */
  syncing: boolean;
}

function needLocal(client: MoulClient<any>, what: string): void {
  if (client.transport.kind !== 'local') {
    throw new MoulError(0, `${what} is available only when the engine runs in this page`, undefined, 'unsupported');
  }
}

/** The database file, when the engine runs in this page. */
export class DatabaseClient {
  constructor(private readonly client: MoulClient<any>) {}

  /** Where the database stands. */
  async status(): Promise<DatabaseStatus> {
    needLocal(this.client, 'status');
    return this.client.transport.op('status');
  }

  /** The shared store as a SQLite file: the builder's data, without secrets or this device's own state. */
  async export(): Promise<Uint8Array> {
    needLocal(this.client, 'export');
    return this.client.transport.op('export');
  }

  /**
   * Replaces the data with a SQLite file: one `export` wrote, or one the Go engine made (converted on
   * the way in). Sign-ins and this device's state are kept.
   */
  async import(file: Uint8Array | Blob): Promise<void> {
    needLocal(this.client, 'import');
    const bytes = file instanceof Uint8Array ? file : new Uint8Array(await file.arrayBuffer());
    await this.client.transport.op('import', { bytes });
  }

  /** Saves the database file through the browser's download, named `<name>.db`. */
  async download(name = this.client.transport.name): Promise<void> {
    const bytes = await this.export();
    const url = URL.createObjectURL(new Blob([bytes as BlobPart], { type: 'application/vnd.sqlite3' }));
    const a = document.createElement('a');
    a.href = url;
    a.download = name.endsWith('.db') ? name : `${name}.db`;
    document.body.append(a);
    a.click();
    a.remove();
    setTimeout(() => URL.revokeObjectURL(url), 10_000);
  }

  /** Whether the browser promised to keep this site's storage. */
  async persisted(): Promise<boolean> {
    try {
      return Boolean(await (globalThis as any).navigator?.storage?.persisted?.());
    } catch {
      return false;
    }
  }

  /** Asks the browser to keep this site's storage. Some browsers ask the person; Safari decides by use. */
  async persist(): Promise<boolean> {
    try {
      return Boolean(await (globalThis as any).navigator?.storage?.persist?.());
    } catch {
      return false;
    }
  }

  /** How much storage the site uses and may use, in bytes. */
  async estimate(): Promise<{ usage: number; quota: number } | null> {
    try {
      const e = await (globalThis as any).navigator?.storage?.estimate?.();
      return e ? { usage: e.usage ?? 0, quota: e.quota ?? 0 } : null;
    } catch {
      return null;
    }
  }

  /** Runs raw SQL. Writes made this way bypass the change log and cannot be replayed. Owner's tool. */
  async exec(sql: string, params?: unknown[]): Promise<{ changes: number; lastCommit: number }> {
    needLocal(this.client, 'exec');
    return this.client.transport.op('exec', { sql, params });
  }

  /** Runs a raw SQL query. */
  async query(sql: string, params?: unknown[]): Promise<{ columns: string[]; rows: unknown[][] }> {
    needLocal(this.client, 'query');
    return this.client.transport.op('query', { sql, params });
  }
}

/** Settings for syncing with an S3-compatible bucket. */
export interface BucketSyncSettings {
  /** Such as `https://<account>.r2.cloudflarestorage.com`. */
  endpoint: string;
  bucket: string;
  /** A folder in the bucket, for several databases in one bucket. */
  prefix?: string;
  /** `auto` for R2, the bucket's region for S3. Guessed when left out. */
  region?: string;
  /** A key scoped to this bucket. */
  accessKeyId: string;
  secretAccessKey: string;
  /** Path-style addressing (`endpoint/bucket/key`): on by default, which R2 and RustFS need. */
  pathStyle?: boolean;
  /** A name for this device, such as "Laptop", shown to the others. */
  label?: string;
  /**
   * Never become the writer, not even of an empty bucket: the device only follows a replica that
   * exists, and keeps any data of its own as a conflict copy. Joining from a link should use this,
   * so a link cannot make a device upload its data to a bucket the link chose. Taking over writing
   * later is still possible.
   */
  followOnly?: boolean;
}

/** Settings for syncing through a Moul Hub. */
export interface HubSyncSettings {
  /** The hub's address, such as `https://sync.moul.dev`. */
  hub: string;
  /** The database on the hub. */
  database: string;
  /** This device's token, from {@link HubClient.verify} or {@link joinHub}. Kept in this device's store. */
  token: string;
  /** A name for this device, such as "Laptop", shown to the others. */
  label?: string;
  /** As for a bucket: never become the writer. Joining from a link should use this. */
  followOnly?: boolean;
}

/** Where to sync: a bucket you own, or a database on a Moul Hub. */
export type SyncSettings = BucketSyncSettings | HubSyncSettings;

/** What a join link for a hub database carries: a one-time ticket, never a token. */
export interface HubJoinLink {
  hub: string;
  database: string;
  ticket: string;
}

/** Where sync stands. */
export interface SyncStatus {
  configured: boolean;
  connected: boolean;
  /** `writer` pushes, `follower` pulls and cannot write, `none` before connecting. */
  role: 'writer' | 'follower' | 'none';
  device?: string;
  /** The writing device, as last read from the bucket. */
  writer?: { device: string; label: string; generation: number; since: number; at: number } | null;
  /** The last transaction in the local database. */
  position: number;
  /** The last transaction this device knows the bucket holds. */
  remote?: number;
  /** Local transactions not yet in the bucket. */
  pending?: number;
  /** When the device last finished a push or pull, in milliseconds since the epoch. */
  lastSyncAt?: number | null;
  lastError?: string | null;
  /**
   * The names of the conflict copies kept since the device started, as {@link SyncClient.conflicts}
   * lists them and `exportConflict` and `deleteConflict` take them.
   */
  conflicts?: string[];
  readOnly: boolean;
  lastCommit: number;
  /** The settings, without the secret key or the token. */
  settings?: Omit<BucketSyncSettings, 'secretAccessKey'> | Omit<HubSyncSettings, 'token'>;
  /** The replica, without credentials. */
  replica?: string;
}

/**
 * Sync with a bucket or through a Moul Hub, as a Litestream-compatible replica. Local engines only.
 */
export class SyncClient {
  constructor(private readonly client: MoulClient<any>) {}

  private async op<T>(op: string, fields?: Record<string, unknown>): Promise<T> {
    needLocal(this.client, 'sync');
    return this.client.transport.op(op, fields);
  }

  /** Where sync stands. */
  status(): Promise<SyncStatus> {
    return this.op('syncStatus');
  }

  /**
   * Connects to a bucket, or to a database on a Moul Hub. A device with data and an empty replica
   * becomes the writer; a device that finds a replica follows it. The settings, keys and tokens
   * included, stay in this device's own store. Through a hub, the device pulls as soon as the hub
   * says another device pushed, and leaves compaction to the hub.
   */
  configure(settings: SyncSettings): Promise<SyncStatus> {
    return this.op('syncConfigure', { ...settings });
  }

  /**
   * A one-time ticket for one more device, from the hub this device syncs through, made with this
   * device's own token (which stays in the engine). Valid 15 minutes. Put it in a link with
   * `joinLink(await moul.sync.joinTicket(), page)`.
   */
  joinTicket(): Promise<HubJoinLink & { expiresAt: number }> {
    return this.op('syncJoinTicket');
  }

  /** Pushes or pulls now instead of waiting for the next round. */
  now(): Promise<SyncStatus> {
    return this.op('syncNow');
  }

  /** Makes this device the writer. The previous writer becomes a follower on its next push. */
  takeOver(): Promise<SyncStatus> {
    return this.op('takeOver');
  }

  /**
   * Stops syncing and forgets the settings. The data stays. The device keeps the changes it makes
   * from now on, to push them when it connects to the same bucket again; with `forget`, it leaves
   * the bucket for good and stops keeping them.
   */
  disconnect(options: { forget?: boolean } = {}): Promise<SyncStatus> {
    return this.op('syncDisconnect', { forget: options.forget === true });
  }

  /** Names of the conflict copies kept: databases this device had changed when it lost the writer role. */
  conflicts(): Promise<string[]> {
    return this.op('conflicts');
  }

  /** A conflict copy as a SQLite file. */
  exportConflict(name: string): Promise<Uint8Array> {
    return this.op('conflictExport', { name });
  }

  /** Deletes a conflict copy. */
  async deleteConflict(name: string): Promise<void> {
    await this.op('conflictDelete', { name });
  }
}

const JOIN = 'moul-join';

function toBase64Url(text: string): string {
  const bytes = new TextEncoder().encode(text);
  let bin = '';
  for (const b of bytes) bin += String.fromCharCode(b);
  return btoa(bin).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
}

function fromBase64Url(text: string): string {
  const b64 = text.replace(/-/g, '+').replace(/_/g, '/');
  const bin = atob(b64 + '='.repeat((4 - (b64.length % 4)) % 4));
  return new TextDecoder().decode(Uint8Array.from(bin, (c) => c.charCodeAt(0)));
}

/**
 * A link that lets another device join this database's replica: `page` with the settings in its
 * fragment, which browsers never send to a server. A bucket link carries the bucket key, so share
 * it only with your own devices. A hub link carries the hub, the database and a one-time ticket
 * (from {@link SyncClient.joinTicket} or {@link HubClient.createJoinTicket}), never a token.
 */
export function joinLink(settings: BucketSyncSettings | HubJoinLink, page: string | URL): string {
  const url = new URL(String(page));
  let rest: Record<string, unknown>;
  if ('hub' in settings) {
    const { hub, database, ticket } = settings;
    if (!hub || !database || !ticket) throw new TypeError('a hub join link needs the hub, the database and a ticket');
    // Only these three: never a token, whatever else the object holds.
    rest = { hub, database, ticket };
  } else {
    const { label: _label, ...given } = settings;
    // Unset settings stay out of the link (a status may report one as null).
    rest = Object.fromEntries(Object.entries(given).filter(([, v]) => v !== undefined && v !== null));
  }
  url.hash = `${JOIN}=${toBase64Url(JSON.stringify(rest))}`;
  return url.toString();
}

/** A link may come from anywhere (a QR code): text of at most `max` characters, without control characters. */
function linkText(v: unknown, max = 256): string | undefined {
  return typeof v === 'string' && v.length <= max && !/[\u0000-\u001f\u007f]/.test(v) ? v : undefined;
}

/** HTTPS, or plain HTTP to this computer (a development server); never credentials in the URL. */
function linkAddress(v: unknown): string | undefined {
  const text = linkText(v, 2048);
  if (!text) return undefined;
  let url: URL;
  try {
    url = new URL(text);
  } catch {
    return undefined;
  }
  const local = ['localhost', '127.0.0.1', '[::1]'].includes(url.hostname);
  if (!(url.protocol === 'https:' || (url.protocol === 'http:' && local)) || url.username || url.password) return undefined;
  return url.origin + url.pathname.replace(/\/+$/, '');
}

/**
 * The settings in a {@link joinLink}, or `null` if the URL has none: a bucket's settings, or a hub
 * link's ticket (`'hub' in result`), which {@link joinHub} trades for settings.
 */
export function readJoinLink(link: string | URL): BucketSyncSettings | HubJoinLink | null {
  let s: any;
  try {
    const hash = new URL(String(link)).hash.replace(/^#/, '');
    const value = new URLSearchParams(hash).get(JOIN);
    if (!value) return null;
    s = JSON.parse(fromBase64Url(value));
  } catch {
    return null;
  }
  if (!s || typeof s !== 'object') return null;
  // A link may come from anywhere (a QR code): keep only well-formed settings.
  if (s.hub !== undefined) {
    const hub = linkAddress(s.hub);
    const database = linkText(s.database, 63);
    const ticket = linkText(s.ticket, 512);
    if (!hub || !database || !HUB_DATABASE_NAME.test(database) || !ticket || !/^[\x21-\x7e]+$/.test(ticket)) return null;
    return { hub, database, ticket };
  }
  const endpoint = linkAddress(s.endpoint);
  const bucket = linkText(s.bucket, 63);
  const accessKeyId = linkText(s.accessKeyId);
  const secretAccessKey = linkText(s.secretAccessKey);
  if (!endpoint || !bucket || !accessKeyId || !secretAccessKey) return null;
  if (!/^[A-Za-z0-9][A-Za-z0-9._-]*$/.test(bucket)) return null;
  const out: BucketSyncSettings = { endpoint, bucket, accessKeyId, secretAccessKey };
  // An unset setting may come as null.
  for (const k of ['prefix', 'region', 'pathStyle']) if (s[k] === null) delete s[k];
  if (s.prefix !== undefined) {
    const prefix = linkText(s.prefix, 512);
    if (prefix === undefined || prefix.split('/').some((p) => p === '.' || p === '..')) return null;
    out.prefix = prefix;
  }
  if (s.region !== undefined) {
    const region = linkText(s.region, 64);
    if (region === undefined || !/^[A-Za-z0-9-]*$/.test(region)) return null;
    out.region = region;
  }
  if (s.pathStyle !== undefined) {
    if (typeof s.pathStyle !== 'boolean') return null;
    out.pathStyle = s.pathStyle;
  }
  return out;
}

/**
 * Trades a hub join link's ticket for a database token of this device's own, and answers the
 * settings to connect with: `moul.sync.configure(await joinHub(link, { label: 'Phone' }))`. The
 * settings follow only (`followOnly`), so a link cannot make this device upload its data to a
 * database the link chose; take over writing later if this device should write. A ticket works
 * once.
 */
export async function joinHub(link: HubJoinLink, options: { label?: string; fetch?: typeof fetch } = {}): Promise<HubSyncSettings> {
  const hub = new HubClient(link.hub, { fetch: options.fetch });
  const joined = await hub.join(link.ticket, { label: options.label });
  const database = typeof joined.database === 'string' && HUB_DATABASE_NAME.test(joined.database) ? joined.database : link.database;
  if (typeof joined.token !== 'string' || !joined.token) throw new MoulError(0, 'the hub answered the join without a token', joined);
  const out: HubSyncSettings = { hub: hub.url, database, token: joined.token, followOnly: true };
  if (options.label) out.label = options.label;
  return out;
}

export type { FileRef };
