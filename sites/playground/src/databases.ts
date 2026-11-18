// The databases this site keeps in the browser: one folder per database under `moul/` in the
// origin private file system (OPFS).

const NAME = /^[A-Za-z0-9_-]{1,64}$/;

/** Conflict copies live beside their database as `<db>-conflicts`, so no database may end so. */
const CONFLICTS = '-conflicts';

/** Whether `name` can name a database. */
export function validName(name: string): boolean {
  return NAME.test(name) && !name.endsWith(CONFLICTS);
}

async function moulFolder(create = false): Promise<FileSystemDirectoryHandle | null> {
  try {
    const root = await navigator.storage.getDirectory();
    return await root.getDirectoryHandle('moul', { create });
  } catch {
    return null;
  }
}

/** The names of the databases kept in this browser, sorted. */
export async function listDatabases(): Promise<string[]> {
  const folder = await moulFolder();
  if (!folder) return [];
  const names: string[] = [];
  for await (const [name, handle] of (folder as any).entries() as AsyncIterable<[string, FileSystemHandle]>) {
    if (handle.kind === 'directory' && validName(name)) names.push(name);
  }
  return names.sort();
}

/**
 * Deletes a database and its conflict copies. Fails while a tab has it open, because the engine
 * holds its files.
 */
export async function deleteDatabase(name: string): Promise<void> {
  const folder = await moulFolder();
  if (!folder) return;
  await folder.removeEntry(name, { recursive: true });
  await folder.removeEntry(`${name}${CONFLICTS}`, { recursive: true }).catch(() => {});
}

/** A name for a new database that is not taken yet. */
export async function freshName(base = 'db'): Promise<string> {
  const taken = new Set(await listDatabases());
  if (!taken.has(base) && validName(base)) return base;
  for (let i = 2; ; i++) if (!taken.has(`${base}-${i}`)) return `${base}-${i}`;
}
