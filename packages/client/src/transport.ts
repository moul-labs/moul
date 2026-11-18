import { MoulError, toMoulError } from './errors.js';
import type { ChangeNotice, EngineRequest, EngineResponse, RecordEvent } from './types.js';

/** What the engine pushes to a page. */
export type Push =
  | ChangeNotice
  | { type: 'event'; id: number; event: RecordEvent }
  | { type: 'closed'; id: number; reason: string }
  /**
   * Another tab now holds the database (or this one does): subscriptions start over. `opened`: the
   * same leading tab, which had answered with an error because the database would not open, has
   * opened it now.
   */
  | { type: 'leader'; tab: string; opened?: boolean }
  /** The page asked the browser to keep this site's storage. */
  | { type: 'persisted'; persisted: boolean };

/** A live record subscription over a remote transport. */
export interface RemoteSubscription {
  /** A collection's name, or `*` for every collection. */
  collection: string;
  /** Subscribe as the owner: the transport adds its admin key, if it has one. */
  owner?: boolean;
  headers: Record<string, string>;
  onEvent(event: RecordEvent): void;
  /** A connection failed; the stream tries again. */
  onError(error: MoulError): void;
  /** The stream connected (again, after a break). */
  onOpen?(): void;
  /** The server ended the stream for good (an expired sign-in, a refusal). */
  onClose?(reason: string): void;
}

const RECORD_ACTIONS = new Set(['create', 'update', 'delete']);

/**
 * How a client reaches an engine. `local` transports run the engine in this page (a worker, or the
 * headless engine in the same thread) and push change notices; `remote` ones talk HTTP to a server.
 */
export interface Transport {
  readonly kind: 'local' | 'remote';
  /** A name for the database or server, such as the database name or the base URL. */
  readonly name: string;
  /** Sends one engine request. */
  api(req: EngineRequest): Promise<EngineResponse>;
  /** Sends an operation other than `api`: `status`, `export`, `syncConfigure`, … Local only. */
  op(op: string, fields?: Record<string, unknown>): Promise<any>;
  /** Listens for pushes. Returns a function that stops listening. */
  onPush(listener: (push: Push) => void): () => void;
  /** Opens an event stream for a subscription. Remote only. */
  stream?(sub: RemoteSubscription): () => void;
  /** Stops the transport; later requests fail with code `closed`. */
  close(): void;
}

/** Listener bookkeeping shared by the transports. */
abstract class PushSource {
  private readonly listeners = new Set<(push: Push) => void>();

  onPush(listener: (push: Push) => void): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  protected emit(push: Push): void {
    for (const l of [...this.listeners]) {
      try {
        l(push);
      } catch (err) {
        // A listener's bug must not stop the others.
        queueMicrotask(() => {
          throw err;
        });
      }
    }
  }
}

/** Query parameters as pairs, without `undefined` and `null` values. */
export function queryPairs(query: EngineRequest['query']): [string, string][] {
  if (query === undefined) return [];
  if (typeof query === 'string') {
    return [...new URLSearchParams(query.replace(/^\?/, ''))];
  }
  if (Array.isArray(query)) return query.map(([k, v]) => [k, String(v)]);
  const out: [string, string][] = [];
  for (const [k, v] of Object.entries(query)) {
    if (v !== undefined && v !== null) out.push([k, String(v)]);
  }
  return out;
}

/** A request as the engine's message protocol takes it. */
function toMessage(req: EngineRequest): Record<string, unknown> {
  const msg: Record<string, unknown> = {
    op: 'api',
    method: (req.method ?? 'GET').toUpperCase(),
    path: req.path,
    query: queryPairs(req.query),
  };
  if (req.headers) msg.headers = Object.entries(req.headers);
  if (req.json !== undefined) msg.json = req.json;
  if (req.form) msg.form = { fields: req.form.fields ?? [], files: req.form.files ?? [] };
  if (req.bytes) {
    msg.bytes = req.bytes;
    msg.type = req.type ?? 'application/octet-stream';
  }
  if (req.owner) msg.owner = true;
  if (req.id) msg.id = req.id;
  return msg;
}

/** Ask the browser to keep this site's storage. Only pages can ask, so the worker asks the page. */
async function askToPersist(): Promise<boolean> {
  const storage = (globalThis as any).navigator?.storage;
  if (!storage?.persist) return false;
  try {
    if (await storage.persisted?.()) return true;
    return Boolean(await storage.persist());
  } catch {
    return false;
  }
}

export interface WorkerTransportOptions {
  /** The database name: one database per name per site. Letters, digits, `-` and `_`. */
  db: string;
  /**
   * The engine's worker. By default the package's own `worker.js`, which bundlers that understand
   * `new URL(…, import.meta.url)` (Vite, webpack, Rollup, esbuild) pick up. Pass a `Worker` or a
   * function that makes one to load it from elsewhere.
   */
  worker?: Worker | (() => Worker);
  /** Where the engine's `.wasm` file is served, when the worker cannot find it next to itself. */
  wasmUrl?: string | URL;
  /** Ask the browser to keep this site's storage after the first write. On by default. */
  persist?: boolean;
}

/** Runs the engine in a dedicated worker. Several tabs share one database; one of them holds it. */
export class WorkerTransport extends PushSource implements Transport {
  readonly kind = 'local' as const;
  readonly name: string;
  private readonly worker: Worker;
  private readonly pending = new Map<number, { resolve: (v: any) => void; reject: (e: unknown) => void }>();
  private next = 1;
  private failure: MoulError | undefined;
  private readonly persist: boolean;

  constructor(options: WorkerTransportOptions) {
    super();
    if (!/^[A-Za-z0-9_-]+$/.test(options.db)) {
      throw new MoulError(0, `a database name may hold only letters, digits, - and _: ${JSON.stringify(options.db)}`);
    }
    // `<db>-conflicts` is where database `<db>` keeps its conflict copies.
    if (options.db.endsWith('-conflicts')) {
      throw new MoulError(0, `a database name may not end in "-conflicts": ${JSON.stringify(options.db)}`);
    }
    this.name = options.db;
    this.persist = options.persist ?? true;
    const w = options.worker;
    this.worker =
      typeof w === 'function'
        ? w()
        : (w ?? new Worker(new URL('./worker.js', import.meta.url), { type: 'module', name: `moul:${options.db}` }));
    this.worker.onmessage = (e: MessageEvent) => this.receive(e.data);
    this.worker.onerror = (e: ErrorEvent) => {
      e.preventDefault?.();
      this.fail(new MoulError(0, `the engine's worker failed: ${e.message || 'it did not load'}`, undefined, 'engine'));
    };
    this.worker.postMessage({
      type: 'init',
      db: options.db,
      wasmUrl: options.wasmUrl === undefined ? undefined : String(options.wasmUrl),
    });
  }

  private receive(msg: any): void {
    if (msg?.type === 'push') {
      this.emit(msg.body as Push);
      return;
    }
    if (msg?.type === 'moul:persist') {
      if (this.persist) {
        void askToPersist().then((persisted) => this.emit({ type: 'persisted', persisted }));
      }
      return;
    }
    if (msg?.type === 'moul:fatal') {
      this.fail(new MoulError(0, `the engine did not start: ${msg.error}`, undefined, 'engine'));
      return;
    }
    const p = this.pending.get(msg?.id);
    if (!p) return;
    this.pending.delete(msg.id);
    if (msg.ok) p.resolve(msg.body);
    else p.reject(toMoulError(new Error(msg.error)));
  }

  private fail(err: MoulError): void {
    this.failure = err;
    for (const p of this.pending.values()) p.reject(err);
    this.pending.clear();
  }

  private call(req: Record<string, unknown>): Promise<any> {
    if (this.failure) return Promise.reject(this.failure);
    const id = this.next++;
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      // Bytes are copied, not transferred: the caller may still use them.
      this.worker.postMessage({ id, req });
    });
  }

  api(req: EngineRequest): Promise<EngineResponse> {
    return this.call(toMessage(req));
  }

  op(op: string, fields: Record<string, unknown> = {}): Promise<any> {
    return this.call({ ...fields, op });
  }

  close(): void {
    this.fail(new MoulError(0, 'the client was closed', undefined, 'closed'));
    this.worker.terminate();
  }
}

/** The engine's in-process interface: `MoulEngine` from the wasm package. */
export interface InProcessEngine {
  request(req: unknown): any;
  onPush(listener: ((push: unknown) => void) | null): void;
  free?(): void;
}

/** Runs the engine in this thread: the headless engine for tests and tools. */
export class EngineTransport extends PushSource implements Transport {
  readonly kind = 'local' as const;
  readonly name: string;
  private closed = false;

  constructor(
    private readonly engine: InProcessEngine,
    name = 'headless',
  ) {
    super();
    this.name = name;
    // Pushes arrive while the engine is still answering; hand them on once it is done.
    engine.onPush((push) => queueMicrotask(() => this.emit(push as Push)));
  }

  private async call(req: Record<string, unknown>): Promise<any> {
    if (this.closed) throw new MoulError(0, 'the client was closed', undefined, 'closed');
    try {
      return await this.engine.request(req);
    } catch (err) {
      throw toMoulError(err);
    }
  }

  api(req: EngineRequest): Promise<EngineResponse> {
    return this.call(toMessage(req));
  }

  op(op: string, fields: Record<string, unknown> = {}): Promise<any> {
    return this.call({ ...fields, op });
  }

  close(): void {
    this.closed = true;
    this.engine.onPush(null);
  }
}

export interface HttpTransportOptions {
  /** The server's base URL, such as `https://notes.example.com`. Paths start with `/api`. */
  url: string;
  /** The `fetch` to use; the global one by default. */
  fetch?: typeof fetch;
  /** Sent as `X-Admin-Key` on requests that claim the owner, for a server that has an admin key. */
  adminKey?: string;
}

const textTypes = /^(application\/(json|problem\+json)|text\/json)/i;

/** Talks HTTP to a Moul server or hub, or to the Go engine. */
export class HttpTransport extends PushSource implements Transport {
  readonly kind = 'remote' as const;
  readonly name: string;
  private readonly base: string;
  private readonly fetcher: typeof fetch;
  private readonly adminKey: string | undefined;
  private readonly aborts = new Set<AbortController>();
  private closed = false;

  constructor(options: HttpTransportOptions) {
    super();
    this.base = options.url.replace(/\/+$/, '');
    this.name = this.base;
    this.fetcher = options.fetch ?? globalThis.fetch.bind(globalThis);
    this.adminKey = options.adminKey;
  }

  private url(path: string, query: [string, string][]): string {
    const qs = new URLSearchParams(query).toString();
    const sep = path.includes('?') ? '&' : '?';
    return `${this.base}${path}${qs ? sep + qs : ''}`;
  }

  async api(req: EngineRequest): Promise<EngineResponse> {
    if (this.closed) throw new MoulError(0, 'the client was closed', undefined, 'closed');
    const headers = new Headers(req.headers ?? {});
    if (req.owner && this.adminKey && !headers.has('X-Admin-Key')) headers.set('X-Admin-Key', this.adminKey);
    if (req.id) headers.set('Moul-Request-Id', req.id);
    let body: BodyInit | undefined;
    if (req.json !== undefined) {
      body = JSON.stringify(req.json);
      headers.set('Content-Type', 'application/json');
    } else if (req.form) {
      const form = new FormData();
      for (const [k, v] of req.form.fields ?? []) form.append(k, v);
      for (const [k, f] of req.form.files ?? []) {
        form.append(k, new Blob([f.data as BlobPart], { type: f.type }), f.name);
      }
      body = form;
    } else if (req.bytes) {
      body = req.bytes as BlobPart as BodyInit;
      headers.set('Content-Type', req.type ?? 'application/octet-stream');
    }
    let res: Response;
    try {
      res = await this.fetcher(this.url(req.path, queryPairs(req.query)), {
        method: (req.method ?? 'GET').toUpperCase(),
        headers,
        body,
      });
    } catch (err) {
      throw new MoulError(0, err instanceof Error ? err.message : String(err), undefined, 'network');
    }
    const type = res.headers.get('Content-Type') ?? '';
    const out: EngineResponse = { status: res.status, headers: [...res.headers] };
    if (res.status === 204 || req.method?.toUpperCase() === 'HEAD') return out;
    if (textTypes.test(type)) {
      const text = await res.text();
      if (text) {
        try {
          out.json = JSON.parse(text);
        } catch {
          out.json = { message: text };
        }
      }
    } else {
      const buf = new Uint8Array(await res.arrayBuffer());
      if (buf.length || type) {
        out.bytes = buf;
        out.type = type;
      }
    }
    return out;
  }

  op(op: string): Promise<any> {
    return Promise.reject(
      new MoulError(0, `${op} is available only when the engine runs in this page`, undefined, 'unsupported'),
    );
  }

  /** Server-sent events of `GET /api/moul/:name/subscribe`, read with `fetch` so credentials stay in headers. */
  stream(sub: RemoteSubscription): () => void {
    if (this.closed) {
      queueMicrotask(() => sub.onClose?.('the client was closed'));
      return () => {};
    }
    const abort = new AbortController();
    this.aborts.add(abort);
    let active = true;
    const finish = (reason: string) => {
      if (!active) return;
      active = false;
      abort.abort();
      this.aborts.delete(abort);
      sub.onClose?.(reason);
    };
    const path = sub.collection === '*' ? '/api/moul/subscribe' : `/api/moul/${encodeURIComponent(sub.collection)}/subscribe`;
    const run = async () => {
      let delay = 1000;
      while (active) {
        try {
          const headers: Record<string, string> = { Accept: 'text/event-stream', ...sub.headers };
          if (sub.owner && this.adminKey) headers['X-Admin-Key'] = this.adminKey;
          const res = await this.fetcher(this.url(path, []), { headers, signal: abort.signal });
          if (!res.ok || !res.body) {
            const text = await res.text().catch(() => '');
            let data: any;
            try {
              data = JSON.parse(text);
            } catch {
              data = { message: text };
            }
            const message = data?.message || `subscribe failed with ${res.status}`;
            sub.onError(new MoulError(res.status, message, data));
            // A refusal will not change by trying again.
            if (res.status >= 400 && res.status < 500 && res.status !== 408 && res.status !== 429) {
              finish(message);
              return;
            }
          } else {
            delay = 1000;
            sub.onOpen?.();
            const reader = res.body.getReader();
            const decoder = new TextDecoder();
            let buffer = '';
            read: while (active) {
              const { done, value } = await reader.read();
              if (done) break;
              buffer += decoder.decode(value, { stream: true });
              const parts = buffer.split('\n\n');
              buffer = parts.pop() ?? '';
              for (const part of parts) {
                // Stopped by an earlier event of this same chunk.
                if (!active) return;
                let event = 'message';
                const data: string[] = [];
                for (const line of part.split('\n')) {
                  if (line.startsWith('event:')) event = line.slice(6).trim();
                  else if (line.startsWith('data:')) data.push(line.slice(5).trimStart());
                }
                if (event === 'connected' || !data.length) continue;
                if (event === 'auth_expired') {
                  finish('the sign-in expired');
                  return;
                }
                // The server dropped events for a slow reader: connect again (and read again).
                if (event === 'slow_consumer') {
                  void reader.cancel().catch(() => {});
                  break read;
                }
                try {
                  const parsed = JSON.parse(data.join('\n'));
                  const action = parsed.action ?? event;
                  if (!RECORD_ACTIONS.has(action)) continue;
                  sub.onEvent({
                    action,
                    moul: parsed.moul ?? sub.collection,
                    record: parsed.record ?? parsed,
                    old_record: parsed.old_record,
                    timestamp: parsed.timestamp ?? new Date().toISOString(),
                  });
                } catch {
                  // Not an event: a comment or a keep-alive.
                }
              }
            }
          }
        } catch (err) {
          if (!active || (err as any)?.name === 'AbortError') return;
          sub.onError(new MoulError(0, err instanceof Error ? err.message : String(err), undefined, 'network'));
        }
        if (!active) return;
        await new Promise((r) => setTimeout(r, delay));
        delay = Math.min(delay * 2, 30_000);
      }
    };
    void run();
    return () => {
      active = false;
      abort.abort();
      this.aborts.delete(abort);
    };
  }

  close(): void {
    this.closed = true;
    for (const a of this.aborts) a.abort();
    this.aborts.clear();
  }
}
