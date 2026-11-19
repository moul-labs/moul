import { type BucketSyncSettings, type HubJoinLink, readJoinLink } from '@moul-dev/client';
import { Store } from '../lib/store';

/**
 * Settings from a join link (`#moul-join=…`), from the address the page was opened with or one
 * opened in the tab since: a bucket's, or a hub database's with a one-time ticket. They are taken
 * out of the address at once, so the key or ticket does not stay in the address bar or the
 * history. The Sync page connects with them only to follow the bucket or the database.
 */
class JoinState extends Store {
  private pending: BucketSyncSettings | HubJoinLink | null = null;

  /** Reads and removes a join link from the page's address. Returns whether there was one. */
  capture(): boolean {
    const settings = readJoinLink(window.location.href);
    if (!settings) return false;
    this.pending = settings;
    // Keep the page's own history state (an app router's), only drop the fragment.
    window.history.replaceState(window.history.state, '', `${window.location.pathname}${window.location.search}`);
    this.notify();
    return true;
  }

  /** The settings, once. */
  take(): BucketSyncSettings | HubJoinLink | null {
    const s = this.pending;
    this.pending = null;
    return s;
  }
}

export const joinState = new JoinState();
