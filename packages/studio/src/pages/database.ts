import type { Change } from '@moul-dev/client';
import { css, html, LitElement, nothing } from 'lit';
import { repeat } from 'lit/directives/repeat.js';
import { formatFileSize } from '../components/records/record-helpers';
import { onPress } from '../lib/press';
import { StoreController } from '../lib/store';
import { toast } from '../lib/toast';
import { databaseState } from '../state/database';
import { moul } from '../studio';
import { baseStyles } from '../styles/base';
import { pageHeader, pageStyles } from '../styles/page';

/**
 * The database file: where it is kept, downloading it, replacing it with another file, and the
 * change log. The data is one SQLite file the builder can take anywhere.
 */
export class AdminDatabasePage extends LitElement {
  static override styles = [
    baseStyles,
    pageStyles,
    css`
      .grid {
        display: grid;
        grid-template-columns: repeat(auto-fill, minmax(min(320px, 100%), 1fr));
        gap: var(--spacing-4);
      }
      dl {
        display: grid;
        grid-template-columns: max-content 1fr;
        gap: var(--spacing-2) var(--spacing-4);
        margin: 0;
        font-size: var(--font-size-sm);
      }
      dt {
        color: var(--color-fg-subtle);
      }
      dd {
        margin: 0;
        word-break: break-word;
      }
      .actions {
        display: flex;
        flex-wrap: wrap;
        gap: var(--spacing-2);
        margin-top: var(--spacing-4);
      }
      p {
        margin: 0 0 var(--spacing-3);
        font-size: var(--font-size-sm);
        color: var(--color-fg-subtle);
      }
      table {
        width: 100%;
        border-collapse: collapse;
        font-size: var(--font-size-sm);
      }
      th,
      td {
        text-align: left;
        padding: var(--spacing-2);
        border-bottom: 1px solid var(--color-border-subtle);
        vertical-align: top;
      }
      th {
        color: var(--color-fg-subtle);
        font-weight: 500;
      }
      code {
        font-family: var(--font-mono);
        font-size: 0.75rem;
      }
    `,
  ];

  static override properties = {
    estimate: { state: true },
    changes: { state: true },
    confirmImport: { state: true },
  };

  declare estimate: { usage: number; quota: number } | null;
  declare changes: Change[];
  declare confirmImport: File | undefined;
  private stop: (() => void) | undefined;
  private timer: ReturnType<typeof setTimeout> | undefined;

  constructor() {
    super();
    this.estimate = null;
    this.changes = [];
    new StoreController(this, databaseState);
  }

  override connectedCallback(): void {
    super.connectedCallback();
    void this.load();
    // A burst of commits (an import, a sync pull) reloads once.
    this.stop = moul().onChange(() => {
      clearTimeout(this.timer);
      this.timer = setTimeout(() => void this.load(), 250);
    });
  }

  override disconnectedCallback(): void {
    super.disconnectedCallback();
    this.stop?.();
    this.stop = undefined;
    clearTimeout(this.timer);
  }

  private async load() {
    const client = moul();
    const [estimate, log] = await Promise.all([
      client.database.estimate().catch(() => null),
      client.admin.changes({ tail: 50 }).catch(() => ({ items: [] as Change[], last: 0 })),
    ]);
    if (!this.isConnected) return;
    this.estimate = estimate;
    // The newest 50, newest first.
    this.changes = [...log.items].reverse();
  }

  private async download() {
    await databaseState.download(moul());
    toast.success('Downloaded', 'Open it with any SQLite tool, or load it here later.');
  }

  private pickFile() {
    const input = document.createElement('input');
    input.type = 'file';
    input.accept = '.db,.sqlite,.sqlite3,application/vnd.sqlite3,application/x-sqlite3';
    input.onchange = () => {
      const file = input.files?.[0];
      if (file) this.confirmImport = file;
    };
    input.click();
  }

  private async importFile(file: File) {
    this.confirmImport = undefined;
    try {
      await moul().database.import(file);
      await databaseState.refresh();
      toast.success('Database replaced', `${file.name} is now this database.`);
    } catch (e) {
      toast.error('Import failed', e instanceof Error ? e.message : String(e));
    }
  }

  override render() {
    const s = databaseState.status;
    const persisted = databaseState.persisted;
    return html`
      ${pageHeader(
        'Database',
        'Your data is one SQLite file in this browser. Download it any time.',
        html`<moul-button variant="primary" @click=${onPress(() => this.download())}>
          <admin-icon slot="prefix" name="download-simple" size="14"></admin-icon>
          Download the file
        </moul-button>`,
      )}
      <div class="grid">
        <moul-card>
          <moul-card-body>
            <div class="card-title"><admin-icon name="hard-drives" size="18"></admin-icon> Where it is kept</div>
            <dl style="margin-top: var(--spacing-3)">
              <dt>Name</dt>
              <dd class="mono">${moul().transport.name}</dd>
              <dt>Storage</dt>
              <dd>${!s ? '…' : s.durable ? 'This browser (origin private file system)' : `Memory only: ${s.note ?? ''}`}</dd>
              <dt>Kept by the browser</dt>
              <dd>
                ${persisted === undefined ? '…' : persisted ? 'Yes: it stays until you clear it' : 'Best effort: it may be cleared when space runs low'}
              </dd>
              <dt>Space used</dt>
              <dd>${this.estimate ? `${formatFileSize(this.estimate.usage)} of ${formatFileSize(this.estimate.quota)}` : '…'}</dd>
              <dt>This device</dt>
              <dd class="mono">${s?.device ?? '…'}</dd>
              <dt>Role</dt>
              <dd>${s ? (s.role === 'writer' ? 'Writes the data' : 'Follows another device (read-only)') : '…'}</dd>
              <dt>Commits</dt>
              <dd>${s?.lastCommit ?? '…'}</dd>
            </dl>
            <div class="actions">
              ${persisted === false
                ? html`<moul-button variant="secondary" @click=${onPress(async () => {
                    const ok = await moul().database.persist();
                    await databaseState.refresh();
                    if (ok) toast.success('The browser will keep this data');
                    else toast.warning('The browser said no', 'Safari decides by use: add the page to the Home Screen.');
                  })}>Ask the browser to keep it</moul-button>`
                : nothing}
            </div>
          </moul-card-body>
        </moul-card>

        <moul-card>
          <moul-card-body>
            <div class="card-title"><admin-icon name="floppy-disk" size="18"></admin-icon> Back up and restore</div>
            <p style="margin-top: var(--spacing-3)">
              The file holds your collections, records and rules. Passwords, keys and this device's own state stay
              out of it. The <code>moul</code> command line reads it, and so does any SQLite tool.
            </p>
            <p>
              Loading a file replaces the data here. A file the earlier Moul server made is converted on the way in.
            </p>
            <div class="actions">
              <moul-button variant="secondary" @click=${onPress(() => this.download())}>
                <admin-icon slot="prefix" name="download-simple" size="14"></admin-icon>
                Download
              </moul-button>
              <moul-button variant="outline" ?is-disabled=${s?.readOnly ?? false} @click=${onPress(() => this.pickFile())}>
                <admin-icon slot="prefix" name="upload-simple" size="14"></admin-icon>
                Load a file…
              </moul-button>
            </div>
          </moul-card-body>
        </moul-card>
      </div>

      <moul-card>
        <moul-card-body>
          <div class="card-title"><admin-icon name="list-dashes" size="18"></admin-icon> Recent changes</div>
          <p style="margin-top: var(--spacing-3)">
            Every write is a named request, kept in the change log with who sent it. Live queries and your other devices
            read it.
          </p>
          ${this.changes.length === 0
            ? html`<div class="muted">No changes yet.</div>`
            : html`<table>
                <thead>
                  <tr>
                    <th>#</th>
                    <th>When</th>
                    <th>What</th>
                    <th>Record</th>
                    <th>Who</th>
                    <th>Result</th>
                  </tr>
                </thead>
                <tbody>
                  ${repeat(
                    this.changes,
                    (c) => c.seq,
                    (c) => html`<tr>
                      <td>${c.seq}</td>
                      <td>${new Date((c as any).at ?? Date.now()).toLocaleString()}</td>
                      <td>${c.op}</td>
                      <td><code>${c.record ?? ''}</code></td>
                      <td><code>${(c as any).actor || 'guest'}</code></td>
                      <td>${c.status < 400 ? 'applied' : html`<span title=${(c as any).error ?? ''}>refused (${c.status})</span>`}</td>
                    </tr>`,
                  )}
                </tbody>
              </table>`}
        </moul-card-body>
      </moul-card>

      <moul-alert-dialog
        tone="warning"
        size="sm"
        ?is-open=${Boolean(this.confirmImport)}
        @openchange=${(e: CustomEvent<{ isOpen: boolean }>) => {
          if (!e.detail.isOpen) this.confirmImport = undefined;
        }}
      >
        <span slot="header">Replace this database?</span>
        <p slot="body" style="margin: 0; font-size: var(--font-size-sm)">
          The data here is replaced with <strong>${this.confirmImport?.name}</strong>. Download the current file first if
          you may want it back.
        </p>
        <moul-button slot="footer" variant="outline" @click=${onPress(() => (this.confirmImport = undefined))}>Cancel</moul-button>
        <moul-button slot="footer" variant="danger" @click=${onPress(() => this.importFile(this.confirmImport!))}>Replace</moul-button>
      </moul-alert-dialog>
    `;
  }
}

customElements.define('admin-database-page', AdminDatabasePage);

declare global {
  interface HTMLElementTagNameMap {
    'admin-database-page': AdminDatabasePage;
  }
}
