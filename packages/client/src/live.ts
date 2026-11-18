import { MoulError, toMoulError } from './errors.js';
import type { ChangeNotice, EngineResponse } from './types.js';

/**
 * What a read depends on. The engine names the collections each read looked up in `Moul-Reads`
 * (by id, so renames do not matter), or `*` when a rule it ran may read any table. A server that
 * does not report reads is followed by collection name.
 */
export type Reads =
  | { all: true }
  | { all: false; ids: ReadonlySet<string>; names: ReadonlySet<string> };

/** The first value of header `name`, compared without case. */
export function header(res: EngineResponse, name: string): string | undefined {
  const want = name.toLowerCase();
  return res.headers.find(([k]) => k.toLowerCase() === want)?.[1];
}

/**
 * The read set of a response. `name` is the collection read: it is kept by name too, so a server
 * that reports changes by collection name (a remote server's record events) can be followed.
 */
export function readsOf(res: EngineResponse, name?: string): Reads {
  const h = header(res, 'Moul-Reads');
  if (h === '*') return { all: true };
  const names = new Set(name ? [name] : []);
  if (h !== undefined) {
    return { all: false, ids: new Set(h.split(',').filter(Boolean)), names };
  }
  return { all: false, ids: new Set(), names };
}

/** Requests that change definitions: any read may depend on them. */
const DEFINITION_OPS = new Set(['define', 'undefine', 'schema']);

/** Whether `notice` may change the result of a read that depends on `reads`. */
export function affects(notice: ChangeNotice, reads: Reads): boolean {
  if (notice.all || reads.all) return true;
  for (const c of notice.changes) {
    if (DEFINITION_OPS.has(c.op)) return true;
    // A refused request changed nothing.
    if (c.status >= 400) continue;
    if (c.collection && reads.ids.has(c.collection)) return true;
    for (const e of c.effects ?? []) {
      if (reads.ids.has(e.collection)) return true;
    }
  }
  return false;
}

/** What a live query holds. */
export interface LiveState<T> {
  /** The latest result, kept while a refresh runs. */
  data: T | undefined;
  /** Why the latest run failed, if it did. `data` keeps the last good result. */
  error: MoulError | undefined;
  /** True until the first answer. Later refreshes run in the background and keep `data`. */
  loading: boolean;
}

/** What a live query runs: a read and what it depended on. */
export type LiveRun<T> = () => Promise<{ data: T; reads: Reads }>;

/** Where live queries register while someone listens. The client keeps one. */
export interface LiveRegistry {
  add(query: LiveQuery<any>): void;
  delete(query: LiveQuery<any>): void;
  /** A query's read set changed after a run. */
  readsChanged?(query: LiveQuery<any>): void;
  /** Counts changes of the signed-in person: data read under an older count is not shown. */
  generation?(): number;
}

/** Results compare by value, so an unchanged result keeps its identity and wakes nobody. */
function same(a: unknown, b: unknown): boolean {
  if (a === b) return true;
  try {
    return JSON.stringify(a) === JSON.stringify(b);
  } catch {
    return false;
  }
}

function sameReads(a: Reads | undefined, b: Reads): boolean {
  if (!a || a.all !== b.all) return false;
  if (a.all || b.all) return true;
  const eq = (x: ReadonlySet<string>, y: ReadonlySet<string>) => x.size === y.size && [...x].every((v) => y.has(v));
  return eq(a.ids, b.ids) && eq(a.names, b.names);
}

/**
 * Reports a listener's error without letting it stop the query or the other listeners: as an
 * uncaught error where the platform can report one (`reportError` in browsers), else on the
 * console.
 */
export function report(err: unknown): void {
  const g = globalThis as { reportError?: (e: unknown) => void };
  if (typeof g.reportError === 'function') g.reportError(err);
  else console.error(err);
}

/**
 * A read that stays current. It runs when the first listener subscribes and again whenever a
 * commit touches what it read, whoever made the change: this tab, another tab, another device
 * through sync, or a sign-in that changes what the rules allow. Runs are coalesced: a burst of
 * writes causes one refresh.
 *
 * ```ts
 * const open = moul.collection('tasks').live({ filter: 'done = false', sort: '-createdAt' });
 * const stop = open.subscribe(({ data, error, loading }) => render(data?.items ?? []));
 * ```
 */
export class LiveQuery<T> {
  private readonly listeners = new Set<(state: LiveState<T>) => void>();
  private current: LiveState<T> = { data: undefined, error: undefined, loading: true };
  private deps: Reads | undefined;
  private running: Promise<void> | undefined;
  private again = false;
  private scheduled = false;
  private active = false;
  /** Runs started and finished, so a caller can wait for a run that starts after it asked. */
  private started = 0;
  private finished = 0;
  private readonly waiters: { run: number; resolve: () => void }[] = [];
  /** The sign-in generation `current` was read under. */
  private readGeneration: number;
  private readonly registry: LiveRegistry;
  private readonly run: LiveRun<T>;

  /** Whether the query reads as the database's owner (a remote server's events are then followed as the owner). */
  readonly owner: boolean;

  constructor(registry: LiveRegistry, run: LiveRun<T>, options: { owner?: boolean } = {}) {
    this.registry = registry;
    this.run = run;
    this.owner = options.owner ?? false;
    this.readGeneration = this.generationNow();
  }

  private generationNow(): number {
    return this.registry.generation?.() ?? 0;
  }

  /** Forgets data read for a person who is no longer signed in. */
  private dropStale(notify: boolean): void {
    const now = this.generationNow();
    if (now === this.readGeneration) return;
    this.readGeneration = now;
    const empty = { data: undefined, error: undefined, loading: true };
    if (notify) this.set(empty);
    else this.current = empty;
  }

  /** The state now. */
  get state(): LiveState<T> {
    return this.current;
  }

  /** What the last run read; `undefined` before the first run. */
  get reads(): Reads | undefined {
    return this.deps;
  }

  /** Whether anyone listens. */
  get isActive(): boolean {
    return this.active;
  }

  /**
   * Calls `listener` with the state now and after every change, until the returned function is
   * called. The query runs while it has listeners. An error a listener throws is reported, and
   * stops neither the query nor the other listeners.
   */
  subscribe(listener: (state: LiveState<T>) => void): () => void {
    // Idle through a sign-out, it may still hold the previous person's results.
    this.dropStale(true);
    this.listeners.add(listener);
    if (!this.active) {
      this.active = true;
      this.registry.add(this);
      this.invalidate();
    }
    try {
      listener(this.current);
    } catch (err) {
      report(err);
    }
    return () => {
      this.listeners.delete(listener);
      if (this.listeners.size === 0 && this.active) {
        this.active = false;
        this.registry.delete(this);
      }
    };
  }

  /**
   * Runs the query and resolves with a result read after this call (rejecting if that run
   * failed), whether or not anyone listens.
   */
  async value(): Promise<T> {
    await this.refresh();
    if (this.current.error) throw this.current.error;
    return this.current.data as T;
  }

  /** Runs the query again, whether or not anyone listens, and resolves when that run is done. */
  refresh(): Promise<void> {
    // A run that has not started yet will see everything committed before this call.
    const run = this.started + 1;
    const done = new Promise<void>((resolve) => this.waiters.push({ run, resolve }));
    this.invalidate();
    return done;
  }

  /**
   * Called by the client when a commit may have changed the result, or with `auth` when the
   * signed-in person changed: their previous results are dropped at once, not shown to the next
   * person while the query runs again.
   */
  notify(notice: ChangeNotice | 'all' | 'auth'): void {
    if (!this.active) return;
    if (notice === 'auth') {
      this.dropStale(true);
      this.invalidate();
      return;
    }
    if (notice === 'all' || this.deps === undefined || affects(notice, this.deps)) this.invalidate();
  }

  /** Called by the client when collection `name` changed on a server that reports by name. */
  notifyName(name: string): void {
    if (!this.active) return;
    const d = this.deps;
    // A read that reached other collections (relations it expanded or filtered through) cannot
    // be matched by name: any change may touch it.
    if (!d || d.all || d.names.has(name) || d.ids.size > d.names.size) this.invalidate();
  }

  private invalidate(): void {
    if (this.running) {
      this.again = true;
      return;
    }
    if (this.scheduled) return;
    this.scheduled = true;
    // Coalesce a burst of notices into one run.
    setTimeout(() => {
      this.scheduled = false;
      if (this.active || this.waiters.length) this.running = this.execute();
    }, 0);
  }

  private set(next: LiveState<T>): void {
    const prev = this.current;
    if (prev.data === next.data && prev.error === next.error && prev.loading === next.loading) return;
    this.current = next;
    for (const l of [...this.listeners]) {
      try {
        l(next);
      } catch (err) {
        report(err);
      }
    }
  }

  private async execute(): Promise<void> {
    try {
      do {
        this.again = false;
        const run = ++this.started;
        const generation = this.generationNow();
        let dropped = false;
        try {
          const { data, reads } = await this.run();
          if (generation !== this.generationNow()) {
            // Read for the person signed in before: never show it; read again for the new one.
            this.again = dropped = true;
          } else {
            const changed = !sameReads(this.deps, reads);
            this.deps = reads;
            const keep = this.current.data !== undefined && same(this.current.data, data);
            this.readGeneration = generation;
            this.set({ data: keep ? (this.current.data as T) : data, error: undefined, loading: false });
            if (changed) this.registry.readsChanged?.(this);
          }
        } catch (err) {
          if (generation !== this.generationNow()) {
            this.again = dropped = true;
          } else {
            // Keep the last good result only if it was read for this same person.
            const data = this.readGeneration === generation ? this.current.data : undefined;
            this.readGeneration = generation;
            this.set({ data, error: toMoulError(err), loading: false });
          }
        }
        // A dropped run answers nobody waiting: the next one, for the new person, does.
        if (!dropped) {
          this.finished = run;
          for (let i = this.waiters.length - 1; i >= 0; i--) {
            if (this.waiters[i].run <= this.finished) this.waiters.splice(i, 1)[0].resolve();
          }
        }
      } while (this.again && (this.active || this.waiters.length));
    } finally {
      this.running = undefined;
    }
  }
}
