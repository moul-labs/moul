// The engine Studio works on: one owner client per page, set by <moul-studio>.
import type { MoulClient } from '@moul-dev/client';
import { Store } from './lib/store';

class StudioState extends Store {
  client: MoulClient | undefined;
  /** The client `client` was made from. */
  private source: MoulClient | undefined;

  /** Points Studio at `client`, as its owner. Returns whether that is another client than before. */
  use(client: MoulClient): boolean {
    if (client === this.source) return false;
    this.source = client;
    this.client = client.asOwner();
    this.notify();
    return true;
  }
}

export const studio = new StudioState();

/** The owner client. Studio's pages render only once one is set. */
export function moul(): MoulClient {
  if (!studio.client) throw new Error('Studio has no database: give <moul-studio> a client or a db');
  return studio.client;
}
