/**
 * The collections that applying `file` with `prune` deletes: those it does not list. It matches
 * the way the engine does: a collection is listed by an entry with its ID, or, for an entry
 * without an ID, its exact name. A file without a `collections` array deletes nothing; the engine
 * refuses it.
 */
export function collectionsPrunedBy(file: unknown, stored: readonly { id?: string; name: string }[]): string[] {
  const entries = (file as { collections?: unknown } | null)?.collections;
  if (!Array.isArray(entries)) return [];
  const ids = new Set<string>();
  const names = new Set<string>();
  for (const entry of entries) {
    const { id, name } = (entry ?? {}) as { id?: unknown; name?: unknown };
    if (typeof id === 'string' && id !== '') ids.add(id);
    else if (typeof name === 'string') names.add(name.trim());
  }
  return stored.filter((c) => !(c.id && ids.has(c.id)) && !names.has(c.name)).map((c) => c.name);
}
