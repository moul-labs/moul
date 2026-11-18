import type { MoulClient } from './client.js';
import { MoulError } from './errors.js';
import { LiveQuery, readsOf } from './live.js';
import type { BaseRecord, GetOptions, ListOptions, ListResult, MoulFile, RecordEvent, RecordModel } from './types.js';

/**
 * The fields a write may set: the record's own fields (files may be `Blob`s), an `id` for a
 * create, and an auth collection's password fields. With typed records a misspelled field is a
 * compile error.
 */
export type RecordInput<T> = {
  [K in Exclude<keyof T, 'createdAt' | 'updatedAt' | 'expand'>]?: T[K] | Blob | Blob[] | null;
} & { password?: string; passwordConfirm?: string; oldPassword?: string };

/** Options of a subscription. */
export interface SubscribeOptions {
  /** Only this record. */
  record?: string;
  /** Only these actions. All by default. */
  events?: ('create' | 'update' | 'delete')[];
  /**
   * The ids of records you already show, read before subscribing. On a device that follows
   * another writer, changes arrive from the bucket and are told only to subscribers that knew the
   * record: name them here to hear when one of them is deleted, or changed so you may no longer
   * see it (that arrives as a delete). Records you learn of through events are tracked for you,
   * and the whole set is sent again whenever the subscription is made again. Ignored over HTTP.
   */
  visible?: string[];
  /** Called when the engine ends the subscription (a revoked token, a deleted user). */
  onClose?: (reason: string) => void;
  /** Aborting it ends the subscription, even before the returned promise settles. */
  signal?: AbortSignal;
  /**
   * Remote servers only: called when the event stream reconnects after a break. Events during the
   * break are lost, so read again what you need.
   */
  onReconnect?: () => void;
}

function listQuery(o: ListOptions = {}): Record<string, string | number | undefined> {
  return {
    filter: o.filter || undefined,
    sort: o.sort || undefined,
    expand: o.expand || undefined,
    search: o.search || undefined,
    page: o.page,
    perPage: o.perPage,
    ...o.query,
  };
}

/** Records of one collection: `moul.collection('tasks')`. */
export class CollectionClient<T extends BaseRecord = RecordModel> {
  constructor(
    private readonly client: MoulClient<any>,
    /** The collection's name. */
    readonly name: string,
  ) {}

  private path(id?: string): string {
    const base = `/api/moul/${encodeURIComponent(this.name)}/records`;
    if (id === '' || id === '.' || id === '..') throw new MoulError(400, `no record can have the id ${JSON.stringify(id)}`);
    return id === undefined ? base : `${base}/${encodeURIComponent(id)}`;
  }

  /** One page of records the caller may list. */
  list(options: ListOptions = {}): Promise<ListResult<T>> {
    return this.client.request({ path: this.path(), query: listQuery(options) });
  }

  /** Every record that matches, read 500 at a time. */
  async all(options: Omit<ListOptions, 'page' | 'perPage'> = {}): Promise<T[]> {
    const out: T[] = [];
    for (let page = 1; ; page++) {
      const r = await this.list({ ...options, page, perPage: 500 });
      out.push(...r.items);
      if (page >= r.totalPages || r.items.length === 0) return out;
    }
  }

  /** The first record that matches `filter`, or `null`. */
  async first(filter: string, options: Omit<ListOptions, 'filter' | 'page' | 'perPage'> = {}): Promise<T | null> {
    const r = await this.list({ ...options, filter, perPage: 1 });
    return r.items[0] ?? null;
  }

  /** One record. Fails with status 404 when it does not exist or the caller may not view it. */
  async get(id: string, options: GetOptions = {}): Promise<T> {
    return this.client.request({ path: this.path(id), query: { expand: options.expand || undefined } });
  }

  /**
   * Creates a record. Pass an `id` to choose it (a record made offline keeps its id). Values that
   * are `Blob`s or `File`s are uploaded first and stored as file references.
   */
  async create(data: RecordInput<T>, options: GetOptions = {}): Promise<T> {
    return this.client.request({
      method: 'POST',
      path: this.path(),
      query: { expand: options.expand || undefined },
      json: await this.client.files.resolve(data),
    });
  }

  /** Changes the fields given and leaves the others alone. */
  async update(id: string, data: RecordInput<T>, options: GetOptions = {}): Promise<T> {
    return this.client.request({
      method: 'PATCH',
      path: this.path(id),
      query: { expand: options.expand || undefined },
      json: await this.client.files.resolve(data),
    });
  }

  /** Deletes a record. */
  async delete(id: string): Promise<void> {
    await this.client.request({ method: 'DELETE', path: this.path(id) });
  }

  /** A page of records that stays current. See {@link LiveQuery}. */
  live(options: ListOptions = {}): LiveQuery<ListResult<T>> {
    return this.client.liveRequest<ListResult<T>>({ path: this.path(), query: listQuery(options) }, this.name);
  }

  /** Every record that matches, kept current. Meant for small collections. */
  liveAll(options: Omit<ListOptions, 'page' | 'perPage'> = {}): LiveQuery<T[]> {
    return this.client.live(async () => {
      const out: T[] = [];
      let reads: ReturnType<typeof readsOf> | undefined;
      for (let page = 1; ; page++) {
        const res = await this.client.send({ path: this.path(), query: listQuery({ ...options, page, perPage: 500 }) });
        const body = this.client.unwrap<ListResult<T>>(res);
        reads = mergeReads(reads, readsOf(res, this.name));
        out.push(...body.items);
        if (page >= body.totalPages || body.items.length === 0) break;
      }
      return { data: out, reads: reads! };
    });
  }

  /** One record that stays current: `null` once it is deleted or hidden from the caller. */
  liveGet(id: string, options: GetOptions = {}): LiveQuery<T | null> {
    return this.client.live(async () => {
      const res = await this.client.send({ path: this.path(id), query: { expand: options.expand || undefined } });
      if (res.status === 404) {
        // Missing now; it may appear later, so follow the collection.
        const reads = await this.client.collectionReads(this.name);
        return { data: null, reads };
      }
      return { data: this.client.unwrap<T>(res), reads: readsOf(res, this.name) };
    });
  }

  /**
   * Calls `listener` with each change to the collection's records that the caller may see. Prefer
   * {@link live} for showing data: events say what changed, live queries say what is there.
   */
  subscribe(listener: (event: RecordEvent<T>) => void, options: SubscribeOptions = {}): Promise<() => void> {
    return this.client.realtime.subscribe(this.name, listener as (e: RecordEvent) => void, options, this.client.owner);
  }

  /** The collection as CSV or JSON. Owner only. */
  async export(format: 'csv' | 'json' = 'csv'): Promise<Blob> {
    const res = await this.client.send({
      path: `/api/moul/${encodeURIComponent(this.name)}/export`,
      query: { format },
      owner: true,
    });
    const body = this.client.unwrap<any>(res);
    if (res.bytes) return new Blob([res.bytes as BlobPart], { type: res.type || 'application/octet-stream' });
    return new Blob([JSON.stringify(body, null, 2)], { type: 'application/json' });
  }

  /** Imports CSV or JSON rows into the collection. Owner only. */
  async import(file: Blob | string, type?: 'text/csv' | 'application/json'): Promise<any> {
    const blob = typeof file === 'string' ? new Blob([file], { type: type ?? guessType(file) }) : file;
    const bytes = new Uint8Array(await blob.arrayBuffer());
    const name = (blob as File).name || (blob.type.includes('json') ? 'import.json' : 'import.csv');
    return this.client.request({
      method: 'POST',
      path: `/api/moul/${encodeURIComponent(this.name)}/import`,
      form: { files: [['file', { name, type: type ?? (blob.type || guessType(name)), data: bytes }]] },
      owner: true,
    });
  }
}

function guessType(text: string): 'text/csv' | 'application/json' {
  const t = text.trimStart();
  return t.startsWith('[') || t.startsWith('{') || t.endsWith('.json') ? 'application/json' : 'text/csv';
}

function mergeReads(a: ReturnType<typeof readsOf> | undefined, b: ReturnType<typeof readsOf>) {
  if (!a) return b;
  if (a.all || b.all) return { all: true as const };
  return { all: false as const, ids: new Set([...a.ids, ...b.ids]), names: new Set([...a.names, ...b.names]) };
}

/** A `MoulFile` or a URL, for the files client. */
export type FileRef = MoulFile | string;

export function fileUrl(ref: FileRef): string {
  const url = typeof ref === 'string' ? ref : ref?.url;
  if (!url) throw new MoulError(0, 'a file reference needs a url');
  return url;
}
