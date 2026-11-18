import type { DatabaseStatus, MoulClient, SyncStatus } from '@moul-dev/client';
import { Store } from '../lib/store';

function exportKey(client: MoulClient): string {
  return `moul.studio.${client.transport.name}.exported`;
}

/**
 * Where the database stands, for the banner and the Database and Sync pages: whether the browser
 * keeps it, whether it is backed up (synced, or downloaded since the last change), and sync.
 */
class DatabaseState extends Store {
  status: DatabaseStatus | undefined;
  sync: SyncStatus | undefined;
  /** Whether the browser promised to keep the site's storage. */
  persisted: boolean | undefined;
  /** The commit the last downloaded file held. */
  exportedAt: number | undefined;
  error: string | undefined;
  /** How many collections there are: an empty database needs no backup. */
  collections = 0;
  private client: MoulClient | undefined;
  private stops: (() => void)[] = [];
  private timer: ReturnType<typeof setTimeout> | undefined;

  /** Follows `client`'s database. */
  follow(client: MoulClient): void {
    this.stop();
    if (client !== this.client) {
      // Another database: nothing known of the last one holds for it.
      this.status = undefined;
      this.sync = undefined;
      this.persisted = undefined;
      this.error = undefined;
      this.collections = 0;
      this.notify();
    }
    this.client = client;
    try {
      const v = localStorage.getItem(exportKey(client));
      this.exportedAt = v ? Number(v) : undefined;
    } catch {
      this.exportedAt = undefined;
    }
    this.stops = [
      client.onChange(() => this.soon()),
      client.onPersist(() => this.soon()),
      client.transport.onPush((p) => {
        if (p.type === 'leader') this.soon();
      }),
    ];
    void this.refresh();
    // Sync rounds run in the background and push nothing when nothing changed.
    const poll = setInterval(() => void this.refresh(), 5000);
    this.stops.push(() => clearInterval(poll));
  }

  /** Stops polling and listening, until the next `follow`. */
  stop(): void {
    for (const s of this.stops) s();
    this.stops = [];
    clearTimeout(this.timer);
    this.timer = undefined;
  }

  private soon(): void {
    clearTimeout(this.timer);
    this.timer = setTimeout(() => void this.refresh(), 150);
  }

  async refresh(): Promise<void> {
    const client = this.client;
    if (!client || client.transport.kind !== 'local') return;
    try {
      const [status, sync, persisted, collections] = await Promise.all([
        client.database.status(),
        client.sync.status(),
        client.database.persisted(),
        client.admin.listCollections(),
      ]);
      // Studio moved to another database while this was on its way: it would show the last one's.
      if (client !== this.client) return;
      this.collections = collections.length;
      this.status = status;
      this.sync = sync;
      this.persisted = persisted;
      this.error = undefined;
    } catch (e) {
      if (client !== this.client) return;
      this.error = e instanceof Error ? e.message : String(e);
    }
    this.notify();
  }

  /**
   * Downloads `client`'s database file and records the export. Every download button goes through
   * here, so none leaves the "not backed up" notice behind.
   */
  async download(client: MoulClient): Promise<void> {
    // The file holds what was there when the download began; a commit landing meanwhile is not in it.
    const at = client === this.client ? this.status?.lastCommit : undefined;
    await client.database.download();
    if (client === this.client) this.noteExport(at);
  }

  /** Records that the file was downloaded as of `at` (by default, the last commit known now). */
  noteExport(at?: number): void {
    if (!this.client || !this.status) return;
    this.exportedAt = at ?? this.status.lastCommit;
    try {
      localStorage.setItem(exportKey(this.client), String(this.exportedAt));
    } catch {
      // The note is a convenience; without storage it lasts for this visit.
    }
    this.notify();
  }

  /** Synced, or downloaded since the last change. */
  get backedUp(): boolean {
    if (this.sync?.configured && this.sync.connected) return true;
    return this.status !== undefined && this.exportedAt !== undefined && this.exportedAt >= this.status.lastCommit;
  }

  /** Whether the builder made anything yet: an empty database needs no backup. */
  get hasData(): boolean {
    return this.collections > 0;
  }
}

export const databaseState = new DatabaseState();
