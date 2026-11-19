import { HubClient, type HubDatabase, type HubSyncSettings, type SyncStatus } from '@moul-dev/client';

/** The hosted hub, offered first. */
export const DEFAULT_HUB = 'https://sync.moul.dev';

/** A database name a hub takes: 1 to 63 of `a-z`, `0-9` and `-`, starting with a letter or digit. */
const NAME = /^[a-z0-9][a-z0-9-]{0,62}$/;

/** Where a sync status's settings point, for people: `notes on sync.moul.dev`, or the bucket. */
export function syncTarget(settings: SyncStatus['settings']): string {
  if (settings && 'hub' in settings) {
    let host = settings.hub;
    try {
      host = new URL(settings.hub).host;
    } catch {
      // Shown as given.
    }
    return `${settings.database} on ${host}`;
  }
  return settings?.bucket ?? 'A bucket';
}

/** Where signing in to a hub stands: the email to send a code to, the code, then a database. */
export type HubStep = 'email' | 'code' | 'database';

/** A hub database name made from a Moul database name, such as `My Notes` → `my-notes`. */
export function suggestDatabaseName(name: string): string {
  const s = name
    .toLowerCase()
    .replace(/[^a-z0-9-]+/g, '-')
    .replace(/^-+|-+$/g, '')
    .slice(0, 63)
    .replace(/-+$/, '');
  return NAME.test(s) ? s : 'app';
}

/** Why `name` is not a database name a hub takes, or `undefined` if it is one. */
export function databaseNameProblem(name: string): string | undefined {
  if (NAME.test(name)) return undefined;
  return 'Use 1 to 63 lowercase letters, digits and dashes, starting with a letter or digit.';
}

/**
 * Signing in to a Moul Hub from the Sync page and choosing a database: an email, the code the hub
 * mails, then a database to connect. The account token lives here only while the page is open;
 * connecting hands it to the engine, which keeps it in this device's store.
 */
export class HubFlow {
  address = DEFAULT_HUB;
  email = '';
  code = '';
  /** For a hub whose sign-up needs an invite. */
  invite = '';
  step: HubStep = 'email';
  databases: HubDatabase[] = [];
  /** The database chosen. */
  database = '';
  private token: string | undefined;
  private client: { address: string; hub: HubClient } | undefined;
  private readonly fetcher: typeof fetch | undefined;

  /** `fetcher` stands in for `fetch`, in tests. */
  constructor(fetcher?: typeof fetch) {
    this.fetcher = fetcher;
  }

  /** The hub at `address` (`https://` when it names none). Throws a `TypeError` for one that is not an address. */
  private hub(): HubClient {
    let address = this.address.trim() || DEFAULT_HUB;
    if (!/^[a-z][a-z0-9+.-]*:\/\//i.test(address)) address = `https://${address}`;
    if (this.client?.address !== address) this.client = { address, hub: new HubClient(address, { fetch: this.fetcher }) };
    return this.client.hub;
  }

  /** Asks the hub to mail a code. */
  async sendCode(): Promise<void> {
    const email = this.email.trim();
    if (!/^[^\s@]+@[^\s@]+$/.test(email)) throw new TypeError('Enter your email address.');
    await this.hub().signIn(email, { invite: this.invite.trim() || undefined });
    this.email = email;
    this.code = '';
    this.step = 'code';
  }

  /** Trades the code for an account token, then lists the account's databases. */
  async verify(label: string): Promise<void> {
    const code = this.code.trim();
    if (!code) throw new TypeError('Enter the code from the mail.');
    const session = await this.hub().verify(this.email, code, { label: label.trim() || undefined });
    this.token = session.token;
    this.code = '';
    this.step = 'database';
    await this.refresh();
  }

  /** Lists the databases again, keeping the choice if it is still there. */
  async refresh(): Promise<void> {
    if (!this.token) return;
    this.databases = (await this.hub().databases(this.token)).sort((a, b) => a.name.localeCompare(b.name));
    if (!this.databases.some((d) => d.name === this.database)) this.database = this.databases[0]?.name ?? '';
  }

  /** Makes a database on the hub and chooses it. */
  async create(name: string): Promise<void> {
    const problem = databaseNameProblem(name);
    if (problem) throw new TypeError(problem);
    if (!this.token) throw new TypeError('Sign in first.');
    await this.hub().createDatabase(this.token, name);
    this.database = name;
    await this.refresh();
    this.database = name;
  }

  /** The settings to connect with, token included: hand them to `moul.sync.configure` and forget them. */
  settings(label: string): HubSyncSettings {
    if (!this.token) throw new TypeError('Sign in first.');
    if (!this.database) throw new TypeError('Choose or create a database.');
    const out: HubSyncSettings = { hub: this.hub().url, database: this.database, token: this.token };
    if (label.trim()) out.label = label.trim();
    return out;
  }

  /** Back to the email: forgets the code, the token and the databases. */
  reset(): void {
    this.step = 'email';
    this.code = '';
    this.token = undefined;
    this.databases = [];
    this.database = '';
  }
}
