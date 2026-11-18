/**
 * `@moul-dev/client`: the Moul SDK. It starts the engine inside the page (a database, rules,
 * sign-in, files and realtime, stored as one SQLite file) or talks to a Moul server, with the same
 * requests either way.
 *
 * ```ts
 * import { createClient } from '@moul-dev/client';
 * import type { MoulSchema } from './moul-types';
 *
 * const moul = createClient<MoulSchema>({ db: 'notes' });
 * await moul.collection('notes').create({ text: 'Buy oat milk' });
 * moul.collection('notes').live({ sort: '-createdAt' }).subscribe(({ data }) => render(data?.items));
 * ```
 */

import { MoulClient, type MoulClientOptions } from './client.js';
import { HttpTransport, type Transport, WorkerTransport } from './transport.js';
import type { AnySchema } from './types.js';

export interface ClientOptions extends MoulClientOptions {
  /**
   * Run the engine in this page, on the database with this name (`app` by default). Each name is
   * its own database; the site's tabs share it.
   */
  db?: string;
  /** Talk to a Moul server at this base URL instead. */
  url?: string;
  /** Use this transport instead. */
  transport?: Transport;
  /** In-page only: the engine's worker, when it should not load from the package. */
  worker?: Worker | (() => Worker);
  /** In-page only: where the engine's `.wasm` file is served. */
  wasmUrl?: string | URL;
  /** In-page only: ask the browser to keep the site's storage after the first write. On by default. */
  persist?: boolean;
  /** Remote only: the `fetch` to use. */
  fetch?: typeof fetch;
  /** Remote only: sent as `X-Admin-Key` on owner requests. */
  adminKey?: string;
}

/**
 * Makes a client. With `db` (or nothing), the engine starts in a worker in this page; with `url`, it
 * talks to a server.
 */
export function createClient<S extends object = AnySchema>(options: ClientOptions = {}): MoulClient<S> {
  const transport =
    options.transport ??
    (options.url
      ? new HttpTransport({ url: options.url, fetch: options.fetch, adminKey: options.adminKey })
      : new WorkerTransport({
          db: options.db ?? 'app',
          worker: options.worker,
          wasmUrl: options.wasmUrl,
          persist: options.persist,
        }));
  return new MoulClient<S>(transport, options);
}

export { AuthClient, type AuthSession, AuthStore, type StorageLike } from './auth.js';
export {
  AdminClient,
  type ClientRequest,
  DatabaseClient,
  type DatabaseStatus,
  FilesClient,
  joinLink,
  MoulClient,
  type MoulClientOptions,
  readJoinLink,
  type RuleTest,
  SyncClient,
  type SyncSettings,
  type SyncStatus,
} from './client.js';
export { CollectionClient, type FileRef, type RecordInput, type SubscribeOptions } from './collection.js';
export { MoulError, type MoulErrorCode } from './errors.js';
export { affects, LiveQuery, type LiveRun, type LiveState, type Reads, readsOf } from './live.js';
export {
  EngineTransport,
  HttpTransport,
  type HttpTransportOptions,
  type InProcessEngine,
  type Push,
  type Transport,
  WorkerTransport,
  type WorkerTransportOptions,
} from './transport.js';
export type * from './types.js';
