// TanStack DB collections over the admin API. Pages read them with live queries
// (`LiveQueryController`) and write through them: `insert`/`update`/`delete` apply optimistically,
// the handlers below persist the change, and a failed write rolls back on its own.
import { type Collection, createCollection } from '@tanstack/db';
import { hashKey, type QueryKey } from '@tanstack/query-core';
import { queryCollectionOptions } from '@tanstack/query-db-collection';
import type { MoulRules } from '../components/collections/rules-editor';
import { queryClient } from '../lib/query';
import type { MoulField } from '../utils/fields';
import { api } from './client';

type Row = Record<string, any>;

/** A collection schema as `/api/moul` returns it. */
export type Moul = { id?: string; name: string; type: string; fields?: MoulField[]; rules?: MoulRules; [key: string]: any };

/** Server-side paging, sorting and filtering of one records grid. */
export type RecordsQuery = { page?: number; perPage?: number; sort?: string; filter?: string; search?: string; expand?: string };

const VIRTUAL_PROPS = ['$key', '$origin', '$collectionId', '$hasPendingWrites'];

/** The row without the `$`-props TanStack DB adds, for JSON views, exports and form state. */
export function withoutVirtualProps<T extends Row>(row: T): T {
  const plain: Row = { ...row };
  for (const prop of VIRTUAL_PROPS) delete plain[prop];
  return plain as T;
}

/** List endpoints answer with a bare array or a `{ items, totalItems, … }` page. */
const rowsOf = (data: any): Row[] => (Array.isArray(data) ? data : Array.isArray(data?.items) ? data.items : []);

/** Waits for every write, then reports the first failure, so one bad row doesn't hide the others' success. */
async function firstFailure(writes: Promise<unknown>[]): Promise<unknown> {
  const results = await Promise.allSettled(writes);
  return results.find((r): r is PromiseRejectedResult => r.status === 'rejected')?.reason;
}

// Every collection, so signing out can drop their rows along with the query cache.
const live = new Set<Collection<any, any, any>>();

function register<T extends Collection<any, any, any>>(collection: T, onCleanedUp?: () => void): T {
  live.add(collection);
  if (onCleanedUp) {
    collection.on('status:change', ({ status }) => {
      if (status !== 'cleaned-up') return;
      live.delete(collection);
      onCleanedUp();
    });
  }
  return collection;
}

/** Stops every collection's sync and drops its rows, so the next session starts clean. */
export function resetCollections(): void {
  for (const collection of live) collection.cleanup({ dependents: true }).catch(() => {});
}

export const moulsCollection = register(
  createCollection(
    queryCollectionOptions({
      id: 'mouls',
      queryKey: ['mouls'],
      queryFn: (): Promise<Moul[]> => api.listMouls(),
      queryClient,
      getKey: (moul: Moul) => moul.name,
      onInsert: async ({ transaction, collection }) => {
        for (const { modified } of transaction.mutations) {
          await collection.utils.writeInsert(await api.createMoul(modified));
        }
      },
      onUpdate: async ({ transaction, collection }) => {
        // The endpoint is a partial update, so properties left out (webhooks, email templates) keep their values.
        for (const { key, changes } of transaction.mutations) await api.updateMoul(key, changes);
        // The saved schema can differ from what was sent (default auth rules); read it back.
        await collection.utils.refetch();
      },
      onDelete: async ({ transaction, collection }) => {
        for (const { key } of transaction.mutations) await api.deleteMoul(key);
        // Deleting a collection can rewrite relations in the others.
        await collection.utils.refetch();
      },
    }),
  ),
);

/** Marks every cached view of `moul`'s records stale (other grid pages, pickers, counts); those on screen refetch. */
export function invalidateRecords(moul: string, except?: QueryKey): void {
  const skip = except && hashKey(except);
  void queryClient.invalidateQueries({
    predicate: (q) =>
      ['records', 'collectionRecordCount'].includes(q.queryKey[0] as string) &&
      q.queryKey[1] === moul &&
      q.queryHash !== skip,
  });
}

export const recordsQueryKey = (moul: string, query: RecordsQuery): QueryKey => ['records', moul, query];

function createRecordsCollection(moul: string, query: RecordsQuery, onCleanedUp: () => void) {
  const queryKey = recordsQueryKey(moul, query);
  return register(
    createCollection(
      queryCollectionOptions({
        id: `records:${hashKey(queryKey)}`,
        queryKey,
        queryFn: () => api.listRecords(moul, query),
        select: rowsOf,
        queryClient,
        getKey: (record: Row) => String(record.id),
        onInsert: async ({ transaction, collection }) => {
          const error = await firstFailure(
            transaction.mutations.map(({ modified }) => {
              // The key is a placeholder until the server assigns the real ID.
              const { id: _placeholder, ...data } = modified;
              return api.createRecord(moul, data);
            }),
          );
          await collection.utils.refetch();
          invalidateRecords(moul, queryKey);
          if (error) throw error;
        },
        onUpdate: async ({ transaction, collection }) => {
          const error = await firstFailure(
            transaction.mutations.map((m) => api.updateRecord(moul, String(m.key), m.changes)),
          );
          // A refetch rather than the PATCH response: the grid's rows carry expanded relations.
          await collection.utils.refetch();
          invalidateRecords(moul, queryKey);
          if (error) throw error;
        },
        onDelete: async ({ transaction, collection }) => {
          const error = await firstFailure(transaction.mutations.map(({ key }) => api.deleteRecord(moul, String(key))));
          // Pulls the next page's first rows up, and drops the rows that did delete before a failure rolls back the rest.
          await collection.utils.refetch();
          invalidateRecords(moul, queryKey);
          if (error) throw error;
        },
      }),
    ),
    onCleanedUp,
  );
}

export type RecordsCollection = ReturnType<typeof createRecordsCollection>;

const recordViews = new Map<string, RecordsCollection>();

/**
 * The records of one grid page (one `query`), keyed by record ID. Views are shared by query and
 * dropped once their collection is garbage collected.
 */
export function recordsCollection(moul: string, query: RecordsQuery = {}) {
  const id = hashKey(recordsQueryKey(moul, query));
  let collection = recordViews.get(id);
  if (!collection) {
    collection = createRecordsCollection(moul, query, () => recordViews.delete(id));
    recordViews.set(id, collection);
  }
  return collection;
}

/**
 * Puts jobs of a worker collection back in the queue: `available` at once. Workers run jobs on the
 * native server; in a tab this only changes the records.
 */
export function retryJobs(records: RecordsCollection, ids: string[]) {
  const scheduled_at = new Date().toISOString();
  return records.update(ids, (drafts) => {
    for (const draft of drafts) Object.assign(draft, { state: 'available', scheduled_at, attempt: 0 });
  });
}

/** `totalItems`/`totalPages` of a records list response; a collection only keeps its rows. */
export function pageTotals(data: unknown): { totalItems?: number; totalPages?: number } {
  const page = data as { totalItems?: unknown; totalPages?: unknown } | undefined;
  return {
    totalItems: typeof page?.totalItems === 'number' ? page.totalItems : undefined,
    totalPages: typeof page?.totalPages === 'number' ? page.totalPages : undefined,
  };
}

/**
 * Keeps every view current: after each commit, from this tab, another tab or another device, the
 * queries on screen fetch again. Returns a function that stops following.
 */
export function followEngine(client: { onChange(listener: () => void): () => void }): () => void {
  return client.onChange(() => {
    void queryClient.invalidateQueries();
  });
}
