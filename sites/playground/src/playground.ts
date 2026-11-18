import { createClient, type MoulClient, readJoinLink } from '@moul-dev/client';
import '@moul-dev/studio';
import { css, html, LitElement, nothing } from 'lit';
import { deleteDatabase, freshName, listDatabases, validName } from './databases';
import { loadSample } from './sample';
import { addSpreadsheet, planSheet } from './spreadsheet';

const LAST_KEY = 'moul.playground.last';

function remember(db: string): void {
  try {
    localStorage.setItem(LAST_KEY, db);
  } catch {
    // Only a convenience.
  }
}

function lastUsed(): string | null {
  try {
    return localStorage.getItem(LAST_KEY);
  } catch {
    return null;
  }
}

/** Forgets what this browser keeps about database `db` beside its files, once it is deleted. */
function forget(db: string): void {
  try {
    if (localStorage.getItem(LAST_KEY) === db) localStorage.removeItem(LAST_KEY);
    // Studio's note of the last download, and the sign-in the SDK keeps for the database.
    localStorage.removeItem(`moul.studio.${db}.exported`);
    localStorage.removeItem(`moul.auth.${db}`);
  } catch {
    // Only a convenience.
  }
}

/** What a dropped or chosen file holds, from its name or type. */
function fileKind(file: File): 'csv' | 'db' | undefined {
  const name = file.name.toLowerCase();
  if (name.endsWith('.csv') || file.type === 'text/csv') return 'csv';
  if (/\.(db|sqlite3?)$/.test(name) || /sqlite3/.test(file.type)) return 'db';
  return undefined;
}

/** Whether a drag event comes from or lands on a file input, which takes the file itself. */
function onFileInput(e: DragEvent): boolean {
  return e.composedPath().some((n) => n instanceof HTMLInputElement && n.type === 'file');
}

/** Opens database `db`: a new page load, so each page holds one engine. */
function open(db: string): void {
  const url = new URL(window.location.href);
  url.searchParams.set('db', db);
  url.hash = '';
  window.location.assign(url.toString());
}

function pickFile(accept: string): Promise<File | null> {
  return new Promise((resolve) => {
    const input = document.createElement('input');
    input.type = 'file';
    input.accept = accept;
    input.onchange = () => resolve(input.files?.[0] ?? null);
    input.oncancel = () => resolve(null);
    input.click();
  });
}

/** Runs `work` on a fresh database called `name`, then opens it. */
async function intoNewDatabase(name: string, work: (client: MoulClient) => Promise<void>): Promise<void> {
  const client = createClient({ db: name, owner: true });
  try {
    await work(client);
  } catch (err) {
    // Leave no empty database behind for a file that did not load.
    client.close();
    await new Promise((r) => setTimeout(r, 300));
    await deleteDatabase(name).catch(() => {});
    throw err;
  }
  client.close();
  open(name);
}

/**
 * The Playground: Studio on a database in this browser, and the ways to start one: empty, from the
 * sample app, from a `.db` file, or from a spreadsheet. Nothing leaves the browser unless you turn
 * on sync.
 */
export class MoulPlayground extends LitElement {
  static override styles = css`
    :host {
      display: flex;
      flex-direction: column;
      height: 100%;
      font-family: var(--font-family-base, system-ui, sans-serif);
      color: var(--color-fg);
      background: var(--color-bg);
    }
    header {
      display: flex;
      align-items: center;
      gap: 0.75rem;
      padding: 0.5rem 1rem;
      border-bottom: 1px solid var(--color-border);
      flex-wrap: wrap;
    }
    .brand {
      display: flex;
      align-items: center;
      gap: 0.5rem;
      font-weight: 700;
      letter-spacing: -0.01em;
      color: inherit;
      text-decoration: none;
    }
    .brand img {
      width: 22px;
      height: 22px;
    }
    .brand small {
      font-weight: 500;
      color: var(--color-fg-subtle);
    }
    .spacer {
      flex: 1;
    }
    select {
      font: inherit;
      font-size: 0.875rem;
      padding: 0.25rem 0.5rem;
      border-radius: 6px;
      border: 1px solid var(--color-border);
      background: var(--color-bg);
      color: inherit;
      max-width: 14rem;
    }
    nav {
      display: flex;
      gap: 0.25rem;
      flex-wrap: wrap;
    }
    a.link {
      font-size: 0.875rem;
      color: var(--color-fg-subtle);
      text-decoration: none;
      padding: 0.25rem 0.5rem;
    }
    a.link:hover {
      color: var(--color-fg);
    }
    main {
      flex: 1;
      min-height: 0;
    }
    moul-studio {
      height: 100%;
    }
    .welcome {
      max-width: 52rem;
      margin: 0 auto;
      padding: 3rem 1.25rem;
      overflow: auto;
      height: 100%;
      box-sizing: border-box;
    }
    .welcome h1 {
      font-size: clamp(1.75rem, 4vw, 2.5rem);
      letter-spacing: -0.03em;
      margin: 0 0 0.75rem;
    }
    .welcome p.lead {
      font-size: 1.0625rem;
      color: var(--color-fg-subtle);
      margin: 0 0 2rem;
      max-width: 40rem;
      line-height: 1.55;
    }
    .choices {
      display: grid;
      grid-template-columns: repeat(auto-fill, minmax(min(15rem, 100%), 1fr));
      gap: 1rem;
    }
    button.choice {
      all: unset;
      box-sizing: border-box;
      cursor: pointer;
      display: flex;
      flex-direction: column;
      gap: 0.375rem;
      padding: 1.125rem;
      border: 1px solid var(--color-border);
      border-radius: 12px;
      background: var(--color-bg-subtle);
    }
    button.choice:hover,
    button.choice:focus-visible {
      border-color: var(--color-primary-500);
      outline: none;
    }
    button.choice strong {
      font-size: 1rem;
    }
    button.choice span {
      font-size: 0.875rem;
      color: var(--color-fg-subtle);
      line-height: 1.45;
    }
    .fine {
      margin-top: 2rem;
      font-size: 0.8125rem;
      color: var(--color-fg-subtle);
      line-height: 1.55;
    }
    .error {
      margin-top: 1rem;
      color: var(--color-error-600, #b91c1c);
      font-size: 0.875rem;
    }
    .busy {
      opacity: 0.6;
      pointer-events: none;
    }
    .drop {
      position: fixed;
      inset: 0.5rem;
      z-index: 1000;
      display: grid;
      place-items: center;
      padding: 1rem;
      text-align: center;
      font-weight: 600;
      border: 2px dashed var(--color-primary-500);
      border-radius: 12px;
      background: color-mix(in srgb, var(--color-bg) 85%, transparent);
      pointer-events: none;
    }
  `;

  static override properties = {
    db: { state: true },
    databases: { state: true },
    busy: { state: true },
    error: { state: true },
    dropping: { state: true },
  };

  declare db: string | undefined;
  declare databases: string[];
  declare busy: boolean;
  declare error: string | undefined;
  /** Whether a file is being dragged over the page. */
  declare dropping: boolean;
  private client: MoulClient | undefined;
  private dropTimer: ReturnType<typeof setTimeout> | undefined;

  constructor() {
    super();
    this.databases = [];
    this.busy = false;
    this.dropping = false;
    // A file dropped anywhere on the page: a spreadsheet, or a database file. Studio's own drop
    // zones (a file field, importing records) take theirs first.
    this.addEventListener('dragover', this.onDragOver);
    this.addEventListener('dragleave', this.onDragLeave);
    this.addEventListener('drop', this.onDrop);
  }

  override connectedCallback(): void {
    super.connectedCallback();
    // Before Studio starts, so this sees a join link ahead of Studio's hash routing.
    window.addEventListener('hashchange', this.onHashChange);
    void this.start();
  }

  override disconnectedCallback(): void {
    super.disconnectedCallback();
    window.removeEventListener('hashchange', this.onHashChange);
  }

  private async start() {
    // A join link always opens a fresh database that follows the bucket, never the one `?db=`
    // names: joining would replace that database's data, or send it to the bucket. Studio then asks
    // before it connects.
    const joining = readJoinLink(window.location.href) !== null;
    const asked = joining ? null : new URL(window.location.href).searchParams.get('db');
    this.databases = await listDatabases();
    let db = asked && validName(asked) ? asked : undefined;
    if (joining) db = await freshName('synced');
    if (!db) {
      const last = lastUsed();
      if (last && this.databases.includes(last)) db = last;
    }
    if (db) {
      remember(db);
      if (db !== asked) {
        const url = new URL(window.location.href);
        url.searchParams.set('db', db);
        window.history.replaceState(null, '', url);
      }
      this.client = createClient({ db, owner: true });
      this.db = db;
      if (!this.databases.includes(db)) this.databases = [...this.databases, db].sort();
    }
  }

  private async run(work: () => Promise<void>) {
    this.busy = true;
    this.error = undefined;
    try {
      await work();
    } catch (e) {
      this.error = e instanceof Error ? e.message : String(e);
    } finally {
      this.busy = false;
    }
  }

  private async newDatabase() {
    const name = window.prompt('Name the new database (letters, digits, - and _):', await freshName('app'));
    if (name === null) return;
    if (!validName(name)) {
      this.error = name.endsWith('-conflicts')
        ? 'A database name may not end in -conflicts: that is where Moul keeps conflict copies.'
        : 'A database name may hold only letters, digits, - and _.';
      return;
    }
    open(name);
  }

  /**
   * A join link opened in this tab after it loaded changes only the address's fragment. Load the
   * page again, so that it opens a fresh database for the link as any page load does; Studio's
   * routing must not take the link for one of its pages meanwhile.
   */
  private readonly onHashChange = (e: HashChangeEvent) => {
    if (!readJoinLink(window.location.href)) return;
    e.stopImmediatePropagation();
    window.location.reload();
  };

  private readonly onDragOver = (e: DragEvent) => {
    if (e.defaultPrevented || onFileInput(e) || !Array.from(e.dataTransfer?.types ?? []).includes('Files')) return;
    e.preventDefault();
    e.dataTransfer!.dropEffect = 'copy';
    clearTimeout(this.dropTimer);
    this.dropping = true;
  };

  private readonly onDragLeave = () => {
    // Moving between elements leaves one and enters the next; only leaving the page lasts.
    clearTimeout(this.dropTimer);
    this.dropTimer = setTimeout(() => (this.dropping = false), 100);
  };

  private readonly onDrop = (e: DragEvent) => {
    clearTimeout(this.dropTimer);
    this.dropping = false;
    const file = e.dataTransfer?.files?.[0];
    if (e.defaultPrevented || onFileInput(e) || !file) return;
    // Not the browser's own handling, which would leave the page to show the file.
    e.preventDefault();
    if (this.busy) return;
    const kind = fileKind(file);
    if (kind === 'csv') this.spreadsheet(file);
    else if (kind === 'db') this.openFile(file);
    else this.error = `Drop a .csv spreadsheet or a .db database file (not “${file.name}”).`;
  };

  private openFile(dropped?: File) {
    void this.run(async () => {
      const file = dropped ?? (await pickFile('.db,.sqlite,.sqlite3,application/vnd.sqlite3,application/x-sqlite3'));
      if (!file) return;
      const base = file.name.replace(/\.[^.]+$/, '').replace(/[^A-Za-z0-9_-]+/g, '-').replace(/^-+|-+$/g, '') || 'imported';
      const name = await freshName(base.slice(0, 48));
      await intoNewDatabase(name, (c) => c.database.import(file));
    });
  }

  private spreadsheet(dropped?: File) {
    void this.run(async () => {
      const file = dropped ?? (await pickFile('.csv,text/csv'));
      if (!file) return;
      if (this.client) {
        const name = await addSpreadsheet(this.client, file);
        window.location.hash = `#/records/${name}`;
        return;
      }
      // Read the file first: one that is not a spreadsheet makes no database.
      planSheet(await file.text());
      await intoNewDatabase(await freshName('sheets'), async (c) => {
        await addSpreadsheet(c, file);
      });
    });
  }

  private sample() {
    void this.run(async () => {
      await intoNewDatabase(await freshName('sample'), loadSample);
    });
  }

  private async deleteCurrent() {
    const db = this.db;
    if (!db) return;
    if (!window.confirm(`Delete the database “${db}” from this browser? This cannot be undone. Download it first if you may want it.`)) return;
    await this.run(async () => {
      this.client?.close();
      this.client = undefined;
      // The engine lets go of the files once its worker stops.
      await new Promise((r) => setTimeout(r, 300));
      try {
        await deleteDatabase(db);
      } catch {
        throw new Error('Close the other tabs that have this database open, then try again.');
      }
      forget(db);
      const url = new URL(window.location.href);
      url.search = '';
      url.hash = '';
      window.location.assign(url.toString());
    });
  }

  private onSelect(e: Event) {
    const value = (e.target as HTMLSelectElement).value;
    if (value === '__new') {
      (e.target as HTMLSelectElement).value = this.db ?? '';
      void this.newDatabase();
    } else if (value && value !== this.db) open(value);
  }

  private renderWelcome() {
    return html`<div class="welcome ${this.busy ? 'busy' : ''}">
      <h1>A backend in your browser tab</h1>
      <p class="lead">
        A database, access rules, sign-in, files and live queries, running in this page. The data is one SQLite file in
        your browser: no account, no server. Sync it to your own bucket when you want it on another device.
      </p>
      <div class="choices">
        <button class="choice" @click=${() => void this.newDatabase()}>
          <strong>Start empty</strong>
          <span>Create collections, set who can read and write, add records.</span>
        </button>
        <button class="choice" @click=${() => this.sample()}>
          <strong>Try the sample app</strong>
          <span>People, projects and tasks, with rules that keep each person's tasks to themselves.</span>
        </button>
        <button class="choice" @click=${() => this.spreadsheet()}>
          <strong>Start from a spreadsheet</strong>
          <span>Choose a CSV file, or drop one on this page: each column becomes a field, each row a record.</span>
        </button>
        <button class="choice" @click=${() => this.openFile()}>
          <strong>Open a database file</strong>
          <span>A .db file you downloaded from Moul, or one the earlier Moul server made. You can drop it here too.</span>
        </button>
      </div>
      ${this.error ? html`<div class="error">${this.error}</div>` : nothing}
      <p class="fine">
        The browser keeps this site's data until you clear it; Studio asks it to keep it for good, and shows when
        nothing is backed up. Databases made here stay on play.moul.dev: to use one in your own app, download the
        file or sync it, or build the app with <a href="https://www.npmjs.com/package/@moul-dev/client">@moul-dev/client</a>.
      </p>
    </div>`;
  }

  override render() {
    return html`
      <header>
        <a class="brand" href="./" @click=${(e: Event) => {
          e.preventDefault();
          window.location.assign(new URL('./', window.location.href).toString());
        }}><img src="/icon.svg" alt="" /> Moul <small>Playground</small></a>
        ${this.db
          ? html`<select aria-label="Database" @change=${this.onSelect}>
              ${this.databases.map((d) => html`<option value=${d} ?selected=${d === this.db}>${d}</option>`)}
              <option value="__new">New database…</option>
            </select>`
          : nothing}
        <span class="spacer"></span>
        <nav class=${this.busy ? 'busy' : ''}>
          ${this.db
            ? html`<a class="link" href="#" @click=${(e: Event) => {
                  e.preventDefault();
                  this.spreadsheet();
                }}>Add a spreadsheet</a>
                <a class="link" href="#" @click=${(e: Event) => {
                  e.preventDefault();
                  this.openFile();
                }}>Open a file</a>
                <a class="link" href="#" @click=${(e: Event) => {
                  e.preventDefault();
                  void this.deleteCurrent();
                }}>Delete</a>`
            : nothing}
          <a class="link" href="https://github.com/moul-labs/moul#readme" target="_blank" rel="noopener">Docs</a>
        </nav>
      </header>
      <main>
        ${this.db && this.client
          ? html`<moul-studio routing="hash" .client=${this.client}></moul-studio>`
          : this.renderWelcome()}
      </main>
      ${this.db && this.error ? html`<div class="error" style="padding: 0 1rem">${this.error}</div>` : nothing}
      ${this.dropping
        ? html`<div class="drop" aria-hidden="true">
            ${this.db ? 'Drop a CSV file to add it as a collection, or a .db file to open it' : 'Drop a CSV or .db file to start from it'}
          </div>`
        : nothing}
    `;
  }
}

customElements.define('moul-playground', MoulPlayground);
